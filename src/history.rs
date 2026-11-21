//! Bounded, workspace-owned disk history. One writer thread owns every pane's archive and each
//! workspace's byte budget; searches, reads and copies read finished segments on their own threads.
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::ops::{Range, RangeInclusive};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

use crate::core::{Cell, Extra, Flags, Row, SelectionType};
use crate::search::{Matcher, SearchResult};
use crate::sys::notify;
use crate::vt::{Color, NamedColor, Rgb};

pub const HOT_ROWS: usize = 512;
pub(crate) const BATCH: usize = 32;
const SEGMENT_BYTES: u64 = 64 * 1024;
const MAX_PENDING_APPENDS: usize = 8;

/// Segment files of every pane of one workspace share this byte capacity.
pub struct HistoryBudget {
    directory: PathBuf,
    capacity: u64,
    notification: OwnedFd,
}
impl HistoryBudget {
    pub fn new(directory: PathBuf, capacity: u64) -> io::Result<Arc<Self>> {
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Arc::new(Self {
            directory,
            capacity,
            notification: crate::sys::new()?,
        }))
    }
    pub fn fd(&self) -> RawFd {
        self.notification.as_raw_fd()
    }
    /// Wakes the reactor that owns this workspace's panes.
    pub(crate) fn notify(&self) {
        notify(self.notification.as_raw_fd());
    }
}
impl Drop for HistoryBudget {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.directory)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("clean history {}: {e}", self.directory.display());
        }
    }
}

pub struct HistoryCopy {
    pub file: File,
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub epoch: u64,
    pub row: u64,
    pub offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub text: String,
    pub cursor: HistoryCursor,
    pub has_more: bool,
    pub matched: Option<bool>,
    pub incomplete: bool,
    pub command: Option<crate::core::CommandBlock>,
}

pub(crate) struct HistoryRead {
    pub start: (u64, usize),
    pub end: (u64, usize),
    pub hot_start: u64,
    pub hot: Vec<Row>,
    pub cursor: HistoryCursor,
    pub query: Option<String>,
    pub incomplete: bool,
    pub command: Option<crate::core::CommandBlock>,
    pub cancelled: Arc<AtomicBool>,
}

pub(crate) struct HistorySearch {
    pub query: String,
    pub hot_start: u64,
    pub hot: Vec<Row>,
    pub columns: usize,
    pub alternate: bool,
    pub cancelled: Arc<AtomicBool>,
}

pub(crate) struct HistoryCopyRequest {
    pub start: (u64, usize),
    pub end: (u64, usize),
    pub hot_start: u64,
    pub hot: Vec<Row>,
    pub kind: Option<SelectionType>,
    pub incomplete: bool,
    // The archive failed: the copy holds hot rows only, and the history directory is not trusted.
    pub in_memory: bool,
}

#[derive(Clone)]
struct Segment {
    id: u64,
    start: u64,
    end: u64,
    // Allocated filesystem bytes, so many tiny rows cannot outgrow the budget.
    bytes: u64,
}
fn segment_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(format!("{id}.rows"))
}

/// What the writer reports back to a pane; consumed on the reactor thread.
pub(crate) enum Reply {
    /// Row buffers of an appended batch, for reuse.
    Stored(Vec<Row>),
    /// The oldest retained row changed because the budget evicted a segment.
    First(u64),
    Page {
        epoch: u64,
        start: u64,
        rows: Vec<Row>,
    },
    Cleared,
    Error(io::Error),
}

struct Pane {
    budget: Arc<HistoryBudget>,
    replies: mpsc::Sender<Reply>,
    segments: VecDeque<Segment>,
    end: u64,
    bytes: u64,
    encoded: Vec<u8>,
    // Append handle of the last segment.
    current: Option<File>,
}
impl Pane {
    fn first(&self) -> u64 {
        self.segments
            .front()
            .map_or(self.end, |segment| segment.start)
    }
    fn page(&mut self, start: u64, count: usize) -> io::Result<(u64, Vec<Row>)> {
        let end = start.saturating_add(count as u64).min(self.end);
        let start = start.max(self.first()).min(end);
        let mut rows = Vec::new();
        if start < end {
            let segments = self.segments.make_contiguous();
            let directory = &self.budget.directory;
            walk(
                directory,
                segments,
                (end, &[]),
                start..=end - 1,
                0..0,
                |item| {
                    if let Item::Row(_, row) = item {
                        rows.push(row.clone());
                    }
                    Ok(false)
                },
            )?;
        }
        Ok((start, rows))
    }
    fn clear(&mut self) -> io::Result<()> {
        self.current = None;
        self.end = 0;
        self.bytes = 0;
        for segment in self.segments.drain(..) {
            remove_segment(&segment_path(&self.budget.directory, segment.id))?;
        }
        Ok(())
    }
    fn reply(&self, reply: io::Result<Reply>, wake: bool) {
        if self
            .replies
            .send(reply.unwrap_or_else(Reply::Error))
            .is_ok()
            && wake
        {
            self.budget.notify();
        }
    }
}

#[derive(Default)]
struct Writer {
    panes: HashMap<u64, Pane>,
    next_segment: u64,
}
impl Writer {
    fn append(&mut self, id: u64, start: u64, rows: &[Row]) -> io::Result<()> {
        let budget = self.panes[&id].budget.clone();
        // Batches already in flight when an earlier append failed must not land out of place.
        if start != self.panes[&id].end {
            return Err(io::Error::other("noncontiguous disk history"));
        }
        for chunk in rows.chunks(BATCH) {
            let pane = self.panes.get_mut(&id).unwrap();
            pane.encoded.clear();
            for row in chunk {
                write_row(&mut pane.encoded, row);
            }
            let end = pane.end + chunk.len() as u64;
            let size = pane.encoded.len() as u64;
            let fits = pane
                .segments
                .back()
                .is_some_and(|last| last.bytes + size <= SEGMENT_BYTES);
            if !(fits && pane.current.is_some()) {
                let path = segment_path(&budget.directory, self.next_segment);
                pane.current = Some(
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)?,
                );
                pane.segments.push_back(Segment {
                    id: self.next_segment,
                    start: pane.end,
                    end: pane.end,
                    bytes: 0,
                });
                self.next_segment += 1;
            }
            let file = pane.current.as_mut().unwrap();
            file.write_all(&pane.encoded)?;
            let bytes = (file.metadata()?.blocks() * 512).max(4096);
            let segment = pane.segments.back_mut().unwrap();
            pane.bytes = pane.bytes - segment.bytes + bytes;
            (segment.bytes, segment.end) = (bytes, end);
            pane.end = end;
            self.evict(&budget)?;
        }
        Ok(())
    }

    /// Removes the oldest segment of any of the workspace's panes until the budget holds.
    fn evict(&mut self, budget: &Arc<HistoryBudget>) -> io::Result<()> {
        loop {
            let mut used = 0;
            let mut oldest: Option<&mut Pane> = None;
            for pane in self
                .panes
                .values_mut()
                .filter(|pane| Arc::ptr_eq(&pane.budget, budget))
            {
                used += pane.bytes;
                if let Some(front) = pane.segments.front()
                    && oldest
                        .as_ref()
                        .is_none_or(|oldest| front.id < oldest.segments.front().unwrap().id)
                {
                    oldest = Some(pane);
                }
            }
            if used <= budget.capacity {
                return Ok(());
            }
            let pane = oldest.unwrap();
            let segment = pane.segments.pop_front().unwrap();
            pane.bytes -= segment.bytes;
            if pane.segments.is_empty() {
                pane.current = None;
            }
            remove_segment(&segment_path(&budget.directory, segment.id))?;
            pane.reply(Ok(Reply::First(pane.first())), true);
        }
    }
}

type Job = Box<dyn FnOnce(&mut Writer) + Send>;

static WRITER: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
static NEXT_HISTORY: AtomicU64 = AtomicU64::new(1);

fn writer() -> &'static mpsc::Sender<Job> {
    WRITER.get_or_init(|| {
        let (sender, jobs) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("history-io".into())
            .spawn(move || {
                let mut writer = Writer::default();
                while let Ok(job) = jobs.recv() {
                    job(&mut writer);
                }
            })
            .expect("cannot start history writer");
        sender
    })
}

/// A pane's handle on the writer thread. Nothing here blocks.
pub(crate) struct History {
    id: u64,
    budget: Arc<HistoryBudget>,
    replies: mpsc::Receiver<Reply>,
    pending: usize,
}
impl History {
    pub fn new(budget: Arc<HistoryBudget>) -> Self {
        let id = NEXT_HISTORY.fetch_add(1, Ordering::Relaxed);
        let (replies, receiver) = mpsc::channel();
        let history = Self {
            id,
            budget: budget.clone(),
            replies: receiver,
            pending: 0,
        };
        history.send(move |writer| {
            let pane = Pane {
                budget,
                replies,
                segments: VecDeque::new(),
                end: 0,
                bytes: 0,
                encoded: Vec::new(),
                current: None,
            };
            writer.panes.insert(id, pane);
        });
        history
    }
    fn send(&self, job: impl FnOnce(&mut Writer) + Send + 'static) {
        writer()
            .send(Box::new(job))
            .expect("history writer stopped");
    }
    /// Runs `job` on the writer thread, after everything sent before it.
    fn with_pane(&self, job: impl FnOnce(&mut Pane) + Send + 'static) {
        let id = self.id;
        self.send(move |writer| job(writer.panes.get_mut(&id).unwrap()));
    }
    pub fn budget(&self) -> &Arc<HistoryBudget> {
        &self.budget
    }
    pub fn poll(&mut self) -> impl Iterator<Item = Reply> + '_ {
        let pending = &mut self.pending;
        self.replies.try_iter().inspect(move |reply| {
            if matches!(reply, Reply::Stored(_)) {
                *pending -= 1;
            }
        })
    }
    pub fn busy(&self) -> bool {
        self.pending == MAX_PENDING_APPENDS
    }
    pub fn append(&mut self, start: u64, rows: Vec<Row>) {
        self.pending += 1;
        // The pane stops sending while busy, so only then does it need waking.
        let (id, wake) = (self.id, self.busy());
        self.send(move |writer| {
            let result = writer.append(id, start, &rows);
            writer.panes[&id].reply(result.map(|()| Reply::Stored(rows)), wake);
        });
    }
    pub fn page(&self, epoch: u64, start: u64, count: usize) {
        self.with_pane(move |pane| {
            let result = pane.page(start, count);
            pane.reply(
                result.map(|(start, rows)| Reply::Page { epoch, start, rows }),
                true,
            );
        });
    }
    pub fn clear(&self) {
        self.with_pane(|pane| {
            let result = pane.clear();
            pane.reply(result.map(|()| Reply::Cleared), true);
        });
    }
    /// Runs `run` on its own thread over the segments written so far and sends it the result.
    fn snapshot<T: Send + 'static>(
        &self,
        sender: mpsc::Sender<io::Result<T>>,
        run: impl FnOnce(&Path, &[Segment]) -> io::Result<T> + Send + 'static,
    ) {
        self.with_pane(move |pane| {
            let segments: Vec<_> = pane.segments.iter().cloned().collect();
            let budget = pane.budget.clone();
            let task = crate::sys::task(move || {
                if sender.send(run(&budget.directory, &segments)).is_ok() {
                    budget.notify();
                }
            });
            if let Err(error) = task {
                eprintln!("start history task: {error}");
            }
        });
    }
    pub fn search(&self, search: HistorySearch, sender: mpsc::Sender<io::Result<SearchResult>>) {
        self.snapshot(sender, move |directory, segments| {
            search_archive(directory, segments, &search)
        });
    }
    pub fn copy(&self, request: HistoryCopyRequest, sender: mpsc::Sender<io::Result<HistoryCopy>>) {
        self.snapshot(sender, move |directory, segments| {
            copy_archive(directory, segments, request)
        });
    }
    pub fn read(&self, read: HistoryRead, sender: mpsc::Sender<io::Result<HistoryPage>>) {
        self.snapshot(sender, move |directory, segments| {
            read_archive(directory, segments, &read)
        });
    }
}
impl Drop for History {
    fn drop(&mut self) {
        let id = self.id;
        self.send(move |writer| {
            let mut pane = writer.panes.remove(&id).unwrap();
            if let Err(error) = pane.clear() {
                eprintln!("clean pane history: {error}");
            }
        });
    }
}

enum Item<'a> {
    /// Rows are missing up to this id, e.g. evicted by the budget.
    Gap(u64),
    /// A row stored without styles, one character per column, and its wrap flag.
    Plain(u64, &'a str, bool),
    Row(u64, &'a Row),
}

/// Visits rows `ids` in order, cold ones from the segments first and then the hot ones.
/// Only rows in `plain` may be handed over undecoded. `visit` returns true to stop.
fn walk(
    directory: &Path,
    segments: &[Segment],
    (hot_start, hot): (u64, &[Row]),
    ids: RangeInclusive<u64>,
    plain: Range<u64>,
    mut visit: impl FnMut(Item) -> io::Result<bool>,
) -> io::Result<()> {
    let (mut next, last) = (*ids.start(), *ids.end());
    let cold_end = hot_start.min(last.saturating_add(1));
    let mut cells = Vec::new();
    for segment in &segments[segments.partition_point(|segment| segment.end <= next)..] {
        if segment.start >= cold_end {
            break;
        }
        if segment.start > next {
            if visit(Item::Gap(segment.start))? {
                return Ok(());
            }
            next = segment.start;
        }
        let end = segment.end.min(cold_end);
        // Evicted after the snapshot: the next gap report covers it.
        let Some(mut reader) = open_segment(directory, segment)? else {
            continue;
        };
        skip_rows(&mut reader, next - segment.start)?;
        while next < end {
            let buffer = reader.fill_buf()?;
            let stop = if let Some((length, text, wrapped)) =
                plain.contains(&next).then(|| plain_row(buffer)).flatten()
            {
                let stop = visit(Item::Plain(next, text, wrapped))?;
                reader.consume(length);
                stop
            } else {
                let row = read_row(&mut reader, cells)?;
                let stop = visit(Item::Row(next, &row))?;
                cells = row.cells;
                stop
            };
            if stop {
                return Ok(());
            }
            next += 1;
        }
    }
    if next < cold_end {
        if visit(Item::Gap(cold_end))? {
            return Ok(());
        }
        next = cold_end;
    }
    if next <= last {
        for (id, row) in (next..=last).zip(&hot[(next - hot_start) as usize..]) {
            if visit(Item::Row(id, row))? {
                break;
            }
        }
    }
    Ok(())
}

/// The unstyled row at the start of `bytes`: its stored length, text and wrap flag.
fn plain_row(bytes: &[u8]) -> Option<(usize, &str, bool)> {
    let header = bytes.get(..9)?;
    let field = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let (columns, used, stored, extras) = (field(0), field(2), field(4), field(7));
    if columns == 0 || used != stored || extras != 0 {
        return None;
    }
    let wrapped = header[6] != 0;
    if stored == 0 {
        return Some((9, "", wrapped));
    }
    let body = bytes.get(9..19)?;
    let length = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    let text_bytes = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    let runs = u16::from_le_bytes([body[8], body[9]]);
    if runs != 0 || length != 6 + text_bytes {
        return None;
    }
    let text = std::str::from_utf8(bytes.get(19..13 + length)?).ok()?;
    Some((13 + length, text, wrapped))
}

fn open_segment(directory: &Path, segment: &Segment) -> io::Result<Option<BufReader<File>>> {
    match File::open(segment_path(directory, segment.id)) {
        Ok(file) => Ok(Some(BufReader::new(file))),
        // The writer evicted the segment after this reader's snapshot.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn remove_segment(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn search_archive(
    directory: &Path,
    segments: &[Segment],
    search: &HistorySearch,
) -> io::Result<SearchResult> {
    let mut matcher = Matcher::new(&search.query);
    let finder = memchr::memmem::Finder::new(search.query.as_bytes());
    // The previous row soft-wraps into this one, so a match may continue across them.
    let mut joined = false;
    let first = segments
        .first()
        .map_or(search.hot_start, |segment| segment.start)
        .min(search.hot_start);
    walk(
        directory,
        segments,
        (search.hot_start, &search.hot),
        first..=u64::MAX,
        0..u64::MAX,
        |item| {
            if search.cancelled.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            match item {
                Item::Gap(_) => {
                    matcher.gap();
                    joined = false;
                }
                Item::Plain(_, text, false)
                    if !joined && finder.find(text.as_bytes()).is_none() =>
                {
                    matcher.line_break();
                }
                Item::Plain(id, text, wrapped) => {
                    matcher.plain(id, text, wrapped);
                    joined = wrapped;
                }
                Item::Row(id, row) => {
                    matcher.row(id, row);
                    joined = row.wrapped;
                }
            }
            Ok(false)
        },
    )?;
    Ok(matcher.finish(search.columns, search.alternate))
}

fn copy_archive(
    directory: &Path,
    segments: &[Segment],
    request: HistoryCopyRequest,
) -> io::Result<HistoryCopy> {
    let HistoryCopyRequest {
        mut start,
        mut end,
        hot_start,
        hot,
        kind,
        mut incomplete,
        in_memory,
    } = request;
    if let Some(kind) = kind
        && kind != SelectionType::Simple
    {
        expand_selection(
            directory,
            segments,
            &mut start,
            &mut end,
            (hot_start, &hot),
            kind,
        )?;
    }
    let mut file = BufWriter::new(copy_file(directory, in_memory)?);
    walk(
        directory,
        segments,
        (hot_start, &hot),
        start.0..=end.0,
        start.0 + 1..end.0,
        |item| {
            match item {
                Item::Gap(_) => incomplete = true,
                Item::Plain(_, text, wrapped) => {
                    file.write_all(text.as_bytes())?;
                    if !wrapped {
                        file.write_all(b"\n")?;
                    }
                }
                Item::Row(id, row) => {
                    let from = if id == start.0 { start.1 } else { 0 };
                    let to = if id == end.0 { end.1 } else { row.used() };
                    write_chars(&mut file, row.text(from, to, kind.is_some()))?;
                    if id != end.0 && !row.wrapped {
                        file.write_all(b"\n")?;
                    }
                }
            }
            Ok(false)
        },
    )?;
    let mut file = file.into_inner()?;
    file.rewind()?;
    Ok(HistoryCopy { file, incomplete })
}

/// An unlinked file for the copied text.
fn copy_file(directory: &Path, in_memory: bool) -> io::Result<File> {
    if in_memory {
        let fd = unsafe { libc::memfd_create(c"nextty-copy".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_TMPFILE)
        .mode(0o600)
        .open(directory)
}

fn write_chars(file: &mut impl Write, chars: impl Iterator<Item = char>) -> io::Result<()> {
    let mut bytes = [0; 512];
    let mut length = 0;
    for ch in chars {
        if length + 4 > bytes.len() {
            file.write_all(&bytes[..length])?;
            length = 0;
        }
        length += ch.encode_utf8(&mut bytes[length..]).len();
    }
    file.write_all(&bytes[..length])
}

/// Widens a line or word selection over rows that may be cold.
fn expand_selection(
    directory: &Path,
    segments: &[Segment],
    start: &mut (u64, usize),
    end: &mut (u64, usize),
    (hot_start, hot): (u64, &[Row]),
    kind: SelectionType,
) -> io::Result<()> {
    const WINDOW: u64 = 64;
    let first = segments
        .first()
        .map_or(hot_start, |segment| segment.start)
        .min(hot_start);
    // Cold rows come in windows around the requested row, as the selection grows both ways.
    let mut window = (0, Vec::new());
    let mut source_row = |id: i64| -> io::Result<Option<Cow<'_, Row>>> {
        if id < first as i64 {
            return Ok(None);
        }
        let id = id as u64;
        if id >= hot_start {
            return Ok(hot.get((id - hot_start) as usize).map(Cow::Borrowed));
        }
        if !(window.0..window.0 + window.1.len() as u64).contains(&id) {
            let from = id.saturating_sub(WINDOW / 2).max(first);
            window = (from, Vec::new());
            let last = (from + WINDOW - 1).min(hot_start - 1);
            walk(
                directory,
                segments,
                (hot_start, &[]),
                from..=last,
                0..0,
                |item| {
                    if let Item::Row(_, row) = item {
                        window.1.push(row.clone());
                    }
                    Ok(matches!(item, Item::Gap(_)))
                },
            )?;
        }
        Ok(window
            .1
            .get((id - window.0) as usize)
            .cloned()
            .map(Cow::Owned))
    };
    let mut first_point = (start.0 as i64, start.1);
    let mut last_point = (end.0 as i64, end.1.saturating_sub(1));
    crate::core::expand_selection(kind, &mut first_point, &mut last_point, &mut source_row)?;
    *start = (first_point.0 as u64, first_point.1);
    let row = source_row(last_point.0)?;
    *end = (
        last_point.0 as u64,
        last_point.1
            + row
                .as_ref()
                .map_or(1, |row| row.cells[last_point.1].width()),
    );
    Ok(())
}

pub(crate) const READ_LIMIT: usize = 16 * 1024;
struct ReadLine {
    cursor: HistoryCursor,
    length: usize,
}
struct ReadText<'a> {
    read: &'a HistoryRead,
    finder: Option<memchr::memmem::Finder<'a>>,
    cursor: HistoryCursor,
    text: String,
    pending: VecDeque<ReadLine>,
    pending_text: String,
    found: bool,
    has_more: bool,
}
impl ReadText<'_> {
    fn append(&mut self, cursor: HistoryCursor, input: &str) -> bool {
        let count = input.floor_char_boundary(READ_LIMIT - self.text.len());
        self.text.push_str(&input[..count]);
        self.advance(cursor, count, input.len());
        count < input.len() || self.text.len() == READ_LIMIT
    }
    fn advance(&mut self, cursor: HistoryCursor, count: usize, length: usize) {
        self.cursor = HistoryCursor {
            offset: cursor.offset + count as u64,
            ..cursor
        };
        self.has_more = cursor.row < self.read.end.0 || count < length;
        if count == length && cursor.row < self.read.end.0 {
            self.cursor.row += 1;
            self.cursor.offset = 0;
        }
    }
    fn skip_to(&mut self, row: u64) {
        self.pending.clear();
        self.pending_text.clear();
        self.cursor.row = row.min(self.read.end.0);
        self.cursor.offset = 0;
        self.has_more = row <= self.read.end.0;
    }
    fn row(&mut self, id: u64, row: &Row, rendered: &mut String) -> io::Result<bool> {
        rendered.clear();
        let from = if id == self.read.start.0 {
            self.read.start.1
        } else {
            0
        };
        let to = if id == self.read.end.0 {
            self.read.end.1
        } else {
            row.used()
        };
        rendered.extend(row.text(from, to, false));
        if id != self.read.end.0 && !row.wrapped {
            rendered.push('\n');
        }
        self.text(id, rendered)
    }
    fn text(&mut self, id: u64, rendered: &str) -> io::Result<bool> {
        let offset = if id == self.cursor.row {
            self.cursor.offset as usize
        } else {
            0
        };
        let input = rendered.get(offset..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "History row changed or cursor offset is outside the requested output",
            )
        })?;
        let cursor = HistoryCursor {
            row: id,
            offset: offset as u64,
            ..self.cursor
        };
        if self.finder.is_none() {
            return Ok(self.append(cursor, input));
        }
        if self.found {
            self.append(cursor, input);
            return Ok(true);
        }
        self.advance(cursor, input.len(), input.len());
        self.pending_text.push_str(input);
        self.pending.push_back(ReadLine {
            cursor,
            length: input.len(),
        });
        let finder = self.finder.as_ref().unwrap();
        let query_length = finder.needle().len();
        // Search new bytes plus the old suffix that could begin a cross-row match.
        let search_start = self
            .pending_text
            .len()
            .saturating_sub(input.len() + query_length.saturating_sub(1));
        if let Some(found) = finder.find(&self.pending_text.as_bytes()[search_start..]) {
            let found = search_start + found;
            let pending = std::mem::take(&mut self.pending_text);
            self.found = true;
            let mut row_start = 0;
            let mut previous_start = 0;
            let mut excerpt_start = 0;
            for line in &self.pending {
                if found < row_start + line.length {
                    excerpt_start = previous_start;
                    break;
                }
                previous_start = row_start;
                row_start += line.length;
            }
            // Preserve the match before spending the byte budget on preceding context.
            let excerpt_start = pending.ceil_char_boundary(
                excerpt_start.max(found.saturating_sub(READ_LIMIT.saturating_sub(query_length))),
            );
            let mut skipped = 0;
            while let Some(line) = self.pending.pop_front() {
                if skipped + line.length <= excerpt_start {
                    skipped += line.length;
                    continue;
                }
                let from = excerpt_start.saturating_sub(skipped);
                let full = self.append(
                    HistoryCursor {
                        offset: line.cursor.offset + from as u64,
                        ..line.cursor
                    },
                    &pending[skipped + from..skipped + line.length],
                );
                skipped += line.length;
                if full {
                    return Ok(true);
                }
            }
        } else {
            let suffix_start = self
                .pending_text
                .len()
                .saturating_sub(query_length.saturating_sub(1));
            let mut removed = 0;
            let mut skipped = 0;
            let mut rows = 0usize;
            for line in &self.pending {
                if skipped + line.length > suffix_start {
                    break;
                }
                // Keep the last complete row before the query suffix as context.
                removed = skipped;
                skipped += line.length;
                rows += 1;
            }
            self.pending.drain(..rows.saturating_sub(1));
            self.pending_text.drain(..removed);
        }
        Ok(false)
    }
}

fn read_archive(
    directory: &Path,
    segments: &[Segment],
    read: &HistoryRead,
) -> io::Result<HistoryPage> {
    let mut output = ReadText {
        read,
        finder: read
            .query
            .as_ref()
            .map(|query| memchr::memmem::Finder::new(query.as_bytes())),
        cursor: read.cursor,
        text: String::new(),
        pending: VecDeque::new(),
        pending_text: String::new(),
        found: false,
        has_more: true,
    };
    let mut incomplete = read.incomplete;
    let mut rendered = String::new();
    walk(
        directory,
        segments,
        (read.hot_start, &read.hot),
        read.cursor.row..=read.end.0,
        read.start.0 + 1..read.end.0,
        |item| {
            if read.cancelled.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            match item {
                Item::Gap(next) => {
                    incomplete = true;
                    output.skip_to(next);
                    Ok(output.found)
                }
                Item::Plain(id, text, wrapped) => {
                    rendered.clear();
                    rendered.push_str(text);
                    if !wrapped {
                        rendered.push('\n');
                    }
                    output.text(id, &rendered)
                }
                Item::Row(id, row) => output.row(id, row, &mut rendered),
            }
        },
    )?;
    Ok(HistoryPage {
        text: output.text,
        cursor: output.cursor,
        has_more: output.has_more,
        matched: read.query.as_ref().map(|_| output.found),
        incomplete,
        command: read.command,
    })
}

fn color_bytes(color: Color) -> [u8; 4] {
    match color {
        Color::Named(value) => {
            let [lo, hi] = (value as u16).to_le_bytes();
            [0, lo, hi, 0]
        }
        Color::Indexed(n) => [1, n, 0, 0],
        Color::Spec(c) => [2, c.r, c.g, c.b],
    }
}
/// Reads the rows this process wrote itself, so malformed data is a bug, not an input.
struct Bytes<'a>(&'a [u8]);
impl<'a> Bytes<'a> {
    fn take(&mut self, count: usize) -> &'a [u8] {
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        head
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn color(&mut self) -> Color {
        match *self.take(4) {
            [0, lo, hi, _] => {
                Color::Named(NamedColor::from_index(u16::from_le_bytes([lo, hi])).unwrap())
            }
            [1, index, ..] => Color::Indexed(index),
            [2, r, g, b] => Color::Spec(Rgb { r, g, b }),
            _ => unreachable!(),
        }
    }
}
// Bit 15 is not a cell flag: the run holds wide characters whose spacer cells the
// reader derives, so wide text needs neither spacer text nor one run per cell.
const WIDE_PAIRS: u16 = 1 << 15;

fn write_row(bytes: &mut Vec<u8>, row: &Row) {
    let used = row.used();
    let extras = row.extras();
    let stored = row.cells[used..]
        .iter()
        .rposition(|cell| *cell != Cell::default())
        .map_or(used, |column| used + column + 1);
    bytes.extend_from_slice(&(row.cells.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(used as u16).to_le_bytes());
    bytes.extend_from_slice(&(stored as u16).to_le_bytes());
    bytes.push(u8::from(row.wrapped));
    bytes.extend_from_slice(&(extras.len() as u16).to_le_bytes());
    if stored == 0 && extras.is_empty() {
        return;
    }
    // Length, text length and run count are filled in once the row is written.
    let header = bytes.len();
    bytes.extend_from_slice(&[0; 10]);
    bytes.reserve(stored);
    let cells = &row.cells[..stored];
    let pair = |column: usize| {
        cells[column].flags.contains(Flags::WIDE_CHAR)
            && cells.get(column + 1) == Some(&cells[column].wide_spacer())
    };
    let mut column = 0;
    while column < stored {
        let cell = &cells[column];
        if cell.c.is_ascii() && !cell.flags.contains(Flags::WIDE_CHAR) {
            let start = column;
            column += 1;
            while column < stored
                && cells[column].c.is_ascii()
                && !cells[column].flags.contains(Flags::WIDE_CHAR)
            {
                column += 1;
            }
            bytes.extend(cells[start..column].iter().map(|cell| cell.c as u8));
        } else {
            bytes.extend_from_slice(cell.c.encode_utf8(&mut [0; 4]).as_bytes());
            column += 1 + usize::from(pair(column));
        }
    }
    let text_bytes = bytes.len() - header - 10;
    let style = |column: usize| {
        let cell = &cells[column];
        (cell.fg, cell.bg, cell.flags, pair(column))
    };
    let default = Cell::default();
    let default = (default.fg, default.bg, default.flags, false);
    let (mut runs, mut column) = (0u16, 0);
    while column < stored {
        let (start, key) = (column, style(column));
        column += 1 + usize::from(key.3);
        while column < stored && style(column) == key {
            column += 1 + usize::from(key.3);
        }
        if key != default {
            bytes.extend_from_slice(&(start as u16).to_le_bytes());
            bytes.extend_from_slice(&(column as u16).to_le_bytes());
            bytes.extend_from_slice(&color_bytes(key.0));
            bytes.extend_from_slice(&color_bytes(key.1));
            let pairs = if key.3 { WIDE_PAIRS } else { 0 };
            bytes.extend_from_slice(&(key.2.bits() | pairs).to_le_bytes());
            runs += 1;
        }
    }
    for (column, extra) in extras {
        bytes.extend_from_slice(&(*column as u16).to_le_bytes());
        bytes.extend_from_slice(&(extra.zerowidth.len() as u32).to_le_bytes());
        for c in &extra.zerowidth {
            bytes.extend_from_slice(&(*c as u32).to_le_bytes());
        }
        bytes.push(u8::from(extra.underline_color.is_some()));
        if let Some(color) = extra.underline_color {
            bytes.extend_from_slice(&color_bytes(color));
        }
    }
    let length = (bytes.len() - header - 4) as u32;
    bytes[header..header + 4].copy_from_slice(&length.to_le_bytes());
    bytes[header + 4..header + 8].copy_from_slice(&(text_bytes as u32).to_le_bytes());
    bytes[header + 8..header + 10].copy_from_slice(&runs.to_le_bytes());
}
fn skip_rows(file: &mut BufReader<File>, count: u64) -> io::Result<()> {
    for _ in 0..count {
        let header = read_array::<9>(file)?;
        // Rows without cells or extras are only a header.
        if header[4..6] != [0; 2] || header[7..9] != [0; 2] {
            let length = u32::from_le_bytes(read_array(file)?);
            file.seek_relative(i64::from(length))?;
        }
    }
    Ok(())
}
fn read_array<const N: usize>(file: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn read_row(file: &mut impl BufRead, mut cells: Vec<Cell>) -> io::Result<Row> {
    let header = read_array::<9>(file)?;
    let field = |at: usize| usize::from(u16::from_le_bytes([header[at], header[at + 1]]));
    let (columns, used, stored, count) = (field(0), field(2), field(4), field(7));
    cells.clear();
    cells.resize(columns, Cell::default());
    let mut extras = Vec::new();
    if stored != 0 || count != 0 {
        let length = u32::from_le_bytes(read_array(file)?) as usize;
        extras = if file.fill_buf()?.len() >= length {
            let extras = decode(&file.fill_buf()?[..length], &mut cells, stored, count);
            file.consume(length);
            extras
        } else {
            let mut bytes = Vec::new();
            file.take(length as u64).read_to_end(&mut bytes)?;
            decode(&bytes, &mut cells, stored, count)
        };
    }
    Ok(Row::from_parts(cells, header[6] != 0, used, extras))
}

/// Fills the first `stored` cells from a row body and returns its extras.
fn decode(body: &[u8], cells: &mut [Cell], stored: usize, count: usize) -> Vec<(usize, Extra)> {
    let mut bytes = Bytes(body);
    let text_bytes = bytes.u32() as usize;
    let runs = bytes.u16();
    let mut chars = std::str::from_utf8(bytes.take(text_bytes)).unwrap().chars();
    let mut next = || chars.next().unwrap();
    let mut column = 0;
    for _ in 0..runs {
        let (start, end) = (usize::from(bytes.u16()), usize::from(bytes.u16()));
        let (fg, bg, bits) = (bytes.color(), bytes.color(), bytes.u16());
        let flags = Flags::from_bits(bits & !WIDE_PAIRS);
        for cell in &mut cells[column..start] {
            cell.c = next();
        }
        let pairs = bits & WIDE_PAIRS != 0;
        for chunk in cells[start..end].chunks_exact_mut(1 + usize::from(pairs)) {
            chunk[0] = Cell {
                c: next(),
                fg,
                bg,
                flags,
            };
            if pairs {
                chunk[1] = chunk[0].wide_spacer();
            }
        }
        column = end;
    }
    for cell in &mut cells[column..stored] {
        cell.c = next();
    }
    (0..count)
        .map(|_| {
            let column = usize::from(bytes.u16());
            let zerowidth = (0..bytes.u32())
                .map(|_| char::from_u32(bytes.u32()).unwrap())
                .collect();
            let underline_color = (bytes.take(1)[0] != 0).then(|| bytes.color());
            (
                column,
                Extra {
                    zerowidth,
                    underline_color,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn budget(capacity: u64) -> Arc<HistoryBudget> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        HistoryBudget::new(
            root.join(format!(
                "history-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            capacity,
        )
        .unwrap()
    }
    // Dense styles keep the fixtures independent of plain-text compression.
    fn row(id: u64) -> Row {
        let value = format!("{id:06} {}", "x".repeat(65));
        let cells = value
            .chars()
            .enumerate()
            .map(|(column, c)| Cell {
                c,
                fg: Color::Indexed(column as u8),
                ..Cell::default()
            })
            .collect::<Vec<_>>();
        let used = cells.len();
        Row::from_parts(cells, false, used, Vec::new())
    }
    fn rows(ids: std::ops::Range<u64>) -> Vec<Row> {
        ids.map(row).collect()
    }
    fn blank(wrapped: bool) -> Row {
        Row::from_parts(vec![Cell::default(); 80], wrapped, 0, Vec::new())
    }
    fn wait<T>(history: &mut History, mut wanted: impl FnMut(Reply) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for reply in history.poll() {
                if let Reply::Error(error) = &reply {
                    panic!("history writer failed: {error}");
                }
                if let Some(found) = wanted(reply) {
                    return found;
                }
            }
            assert!(Instant::now() < deadline, "history writer stalled");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn page(history: &mut History, start: u64, count: usize) -> (u64, Vec<Row>) {
        history.page(1, start, count);
        wait(history, |reply| match reply {
            Reply::Page { start, rows, .. } => Some((start, rows)),
            _ => None,
        })
    }
    /// The oldest retained row as announced once every earlier request has run.
    fn first(history: &mut History) -> u64 {
        history.page(1, 0, 0);
        let mut first = 0;
        wait(history, |reply| match reply {
            Reply::First(announced) => {
                first = announced;
                None
            }
            Reply::Page { .. } => Some(first),
            _ => None,
        })
    }
    fn search(history: &History, request: HistorySearch) -> SearchResult {
        let (sender, receiver) = mpsc::channel();
        history.search(request, sender);
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    }
    fn search_request(query: &str, hot_start: u64, hot: Vec<Row>) -> HistorySearch {
        HistorySearch {
            query: query.into(),
            hot_start,
            hot,
            columns: 80,
            alternate: false,
            cancelled: Default::default(),
        }
    }
    fn copy(history: &History, request: HistoryCopyRequest) -> (String, bool) {
        let (sender, receiver) = mpsc::channel();
        history.copy(request, sender);
        let mut copy = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut text = String::new();
        copy.file.read_to_string(&mut text).unwrap();
        (text, copy.incomplete)
    }
    fn copy_request(
        start: (u64, usize),
        end: (u64, usize),
        hot: (u64, Vec<Row>),
        kind: Option<SelectionType>,
    ) -> HistoryCopyRequest {
        HistoryCopyRequest {
            start,
            end,
            hot_start: hot.0,
            hot: hot.1,
            kind,
            incomplete: false,
            in_memory: false,
        }
    }
    fn read(history: &History, request: HistoryRead) -> HistoryPage {
        let (sender, receiver) = mpsc::channel();
        history.read(request, sender);
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    }
    fn read_request(end: u64, hot: (u64, Vec<Row>), query: Option<&str>) -> HistoryRead {
        HistoryRead {
            start: (0, 0),
            end: (end, 71),
            hot_start: hot.0,
            hot: hot.1,
            cursor: HistoryCursor {
                epoch: 0,
                row: 0,
                offset: 0,
            },
            query: query.map(str::to_owned),
            incomplete: false,
            command: None,
            cancelled: Default::default(),
        }
    }

    #[test]
    fn decoding_restores_every_cell_with_reused_storage_and_any_read_buffer_size() {
        let red = Color::Named(NamedColor::Red);
        let wide = |c| Cell {
            c,
            fg: red,
            flags: Flags::WIDE_CHAR,
            ..Cell::default()
        };
        // Spacers that differ from the derived one, and wide cells without one, stay exact.
        let mut cells = vec![Cell::default(); 12];
        cells[1] = wide('中');
        cells[2] = cells[1].wide_spacer();
        cells[3] = wide('文');
        cells[4] = cells[3].wide_spacer();
        cells[5] = wide('字');
        cells[6] = Cell {
            bg: Color::Named(NamedColor::Blue),
            ..cells[5].wide_spacer()
        };
        cells[9] = wide('尾');
        let mut styled = vec![Cell::default(); 9];
        styled[2].fg = Color::Spec(Rgb { r: 1, g: 2, b: 3 });
        styled[2].flags = Flags::BOLD | Flags::HIDDEN;
        styled[3..6].fill(Cell {
            c: 'a',
            fg: Color::Indexed(5),
            bg: Color::Indexed(6),
            flags: Flags::ITALIC,
        });
        styled[8].bg = Color::Indexed(17);
        let mut rows = [
            Row::from_parts(
                vec![
                    Cell {
                        c: '界',
                        ..Cell::default()
                    };
                    1200
                ],
                false,
                1200,
                Vec::new(),
            ),
            Row::from_parts(vec![Cell::default(); 100], false, 0, Vec::new()),
            Row::from_parts(vec![Cell::default(); 3], false, 0, Vec::new()),
            Row::from_parts(styled, false, 0, Vec::new()),
            Row::from_parts(cells, false, 10, Vec::new()),
        ];
        rows[0].extras_mut().push((
            0,
            Extra {
                zerowidth: vec!['\u{301}'].into(),
                underline_color: Some(Color::Indexed(17)),
            },
        ));
        rows[0].extras_mut().extend([
            (
                1,
                Extra {
                    zerowidth: vec!['\u{301}', '\u{308}', '\u{327}', '\u{20dd}', '\u{20de}'].into(),
                    underline_color: None,
                },
            ),
            (
                2,
                Extra {
                    zerowidth: Vec::new().into(),
                    underline_color: Some(Color::Indexed(42)),
                },
            ),
        ]);
        let mut bytes = Vec::new();
        for row in &rows {
            write_row(&mut bytes, row);
        }
        let fd = unsafe { libc::memfd_create(c"history-skip".as_ptr(), libc::MFD_CLOEXEC) };
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes).unwrap();
        let same = |decoded: &Row, expected: &Row| {
            assert_eq!(decoded.cells, expected.cells);
            assert_eq!(decoded.used(), expected.used());
            assert_eq!(decoded.wrapped, expected.wrapped);
            assert_eq!(decoded.extras(), expected.extras());
        };
        for capacity in [7, 31, 257, 4096] {
            let mut reader = BufReader::with_capacity(capacity, bytes.as_slice());
            let mut cells = Vec::new();
            for expected in &rows {
                let decoded = read_row(&mut reader, cells).unwrap();
                same(&decoded, expected);
                cells = decoded.cells;
            }
            assert!(reader.fill_buf().unwrap().is_empty());
            for (skip, expected) in rows.iter().enumerate() {
                file.rewind().unwrap();
                let mut reader = BufReader::with_capacity(capacity, file.try_clone().unwrap());
                skip_rows(&mut reader, skip as u64).unwrap();
                same(&read_row(&mut reader, Vec::new()).unwrap(), expected);
            }
        }
    }

    #[test]
    fn search_and_read_find_text_across_segments_rows_and_the_cold_hot_boundary() {
        let mut history = History::new(budget(8 << 20));
        history.append(0, rows(0..192));
        // Every batch of 32 rows fills a segment; row 190 onwards is hot.
        let hot = (190, rows(190..192));
        for id in [31, 32, 189, 190, 191] {
            let query = format!("{id:06} x");
            let result = search(&history, search_request(&query, hot.0, hot.1.clone()));
            let found = result.matches.iter().map(|m| (m.start_row, m.end_row));
            assert_eq!(found.collect::<Vec<_>>(), [(id, id)], "{query}");
        }
        // A read query may span rows, but a search never joins hard lines.
        for id in [31, 189] {
            let query = format!("{}\n{:06}", "x".repeat(65), id + 1);
            let page = read(&history, read_request(191, hot.clone(), Some(&query)));
            assert_eq!(page.matched, Some(true));
            assert!(page.text.contains(&query));
            assert!(!page.incomplete);
        }
        let all = read(&history, read_request(191, hot, None));
        assert_eq!(all.text.lines().count(), 192);
        assert!(!all.has_more);
    }

    #[test]
    fn copied_unicode_and_combining_marks_survive_long_rows_and_prompt_filtering() {
        let mut core = crate::core::Core::new(
            crate::core::WindowSize {
                num_cols: 1200,
                num_lines: 2,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = crate::vt::Parser::new();
        parser.advance(&mut core, b"> ");
        let text = "中😀x\u{301}".repeat(200);
        parser.advance(&mut core, text.as_bytes());
        let mut row = core.row(0).clone();
        row.cells[0].flags.insert(Flags::PROMPT);
        row.cells[1].flags.insert(Flags::PROMPT);
        for skip_prompt in [false, true] {
            let mut copied = b"prefix".to_vec();
            write_chars(&mut copied, row.text(0, row.used(), skip_prompt)).unwrap();
            let expected = format!("prefix{}{}", if skip_prompt { "" } else { "> " }, text);
            assert_eq!(String::from_utf8(copied).unwrap(), expected);
        }
    }

    #[test]
    fn blocks_not_bytes_are_charged_so_empty_line_floods_stay_within_the_budget() {
        let budget = budget(64 * 1024);
        let mut history = History::new(budget.clone());
        for batch in 0..625 {
            history.append(batch * 32, vec![blank(false); 32]);
        }
        assert!(first(&mut history) > 0);
        let allocated: u64 = fs::read_dir(&budget.directory)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().blocks() * 512)
            .sum();
        assert!(allocated <= 64 * 1024, "{allocated}");
    }

    #[test]
    fn evicted_and_cleared_archives_release_open_files() {
        for evicted in [false, true] {
            let budget = budget(if evicted { 1 } else { 1 << 20 });
            let mut history = History::new(budget.clone());
            history.append(0, rows(0..BATCH as u64));
            if evicted {
                first(&mut history);
            } else {
                history.clear();
                wait(&mut history, |reply| {
                    matches!(reply, Reply::Cleared).then_some(())
                });
            }
            let directory = fs::canonicalize(&budget.directory).unwrap();
            for entry in fs::read_dir("/proc/self/fd").unwrap() {
                let path = match fs::read_link(entry.unwrap().path()) {
                    Ok(path) => path,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => panic!("{error}"),
                };
                assert!(!path.starts_with(&directory), "retained {path:?}");
            }
        }
    }

    #[test]
    fn search_preserves_wrapped_empty_rows_and_breaks_at_hard_empty_rows() {
        let mut core = crate::core::Core::new(
            crate::core::WindowSize {
                num_cols: 80,
                num_lines: 2,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(&mut core, "ab\r\n界a\u{301}d".as_bytes());
        let mut first = core.row(0).clone();
        first.wrapped = true;
        for hard_break in [false, true] {
            let mut rows = vec![first.clone()];
            rows.extend(std::iter::repeat_n(blank(true), 1000));
            rows[500].wrapped = !hard_break;
            rows.push(core.row(1).clone());
            let mut history = History::new(budget(8 << 20));
            history.append(0, rows.clone());
            for hot_start in [1001, 1002] {
                let hot = rows[hot_start..].to_vec();
                let result = search(
                    &history,
                    search_request("b界a\u{301}", hot_start as u64, hot),
                );
                assert!(!result.truncated);
                if hard_break {
                    assert!(result.matches.is_empty());
                } else {
                    assert_eq!(
                        result.matches.as_ref(),
                        vec![crate::search::SearchMatch {
                            start_row: 0,
                            end_row: 1001,
                            start_column: 1,
                            end_column: 3,
                        }]
                    );
                }
            }
        }
    }

    #[test]
    fn cold_selection_expands_distinct_wrapped_rows_across_hot_history() {
        use crate::core::{Core, WindowSize};
        use crate::vt::Parser;

        let mut expected = String::new();
        let rows = (0..320)
            .map(|id| {
                let text = format!(
                    "{id:04}界a\u{301}{}",
                    char::from(b'a' + (id % 26) as u8).to_string().repeat(10)
                );
                expected.push_str(&text);
                let mut core = Core::new(
                    WindowSize {
                        num_cols: 17,
                        num_lines: 1,
                        cell_width: 9,
                        cell_height: 19,
                    },
                    0xcccccc,
                    0x202020,
                );
                Parser::new().advance(
                    &mut core,
                    format!("\x1b[38;2;{};32;64m{text}", id % 256).as_bytes(),
                );
                let mut row = core.row(0).clone();
                row.wrapped = id < 319;
                row
            })
            .collect::<Vec<_>>();
        let mut history = History::new(budget(8 << 20));
        history.append(0, rows[..300].to_vec());
        for kind in [SelectionType::Lines, SelectionType::Semantic] {
            let request = copy_request((165, 5), (165, 6), (300, rows[300..].to_vec()), Some(kind));
            assert_eq!(copy(&history, request), (expected.clone(), false));
        }
    }

    #[test]
    fn paging_and_copy_preserve_mixed_rows_after_long_empty_prefixes() {
        let mut history = History::new(budget(8 << 20));
        let mut rows = vec![blank(false); 2000];
        rows[910] = row(910);
        rows[1000] = row(1000);
        rows[1000].extras_mut().push((
            0,
            Extra {
                zerowidth: vec!['\u{301}'].into(),
                underline_color: Some(Color::Indexed(17)),
            },
        ));
        rows[1200].wrapped = true;
        history.append(0, rows.clone());
        for start in [900, 1000, 1184, 1900] {
            let (actual, page) = page(&mut history, start, 32);
            assert_eq!(actual, start);
            assert_eq!(page.len(), 32);
            for (actual, expected) in page.iter().zip(&rows[start as usize..]) {
                assert_eq!(actual.cells, expected.cells);
                assert_eq!(actual.used(), expected.used());
                assert_eq!(actual.wrapped, expected.wrapped);
                assert_eq!(actual.extras(), expected.extras());
            }
        }
        for (start, end, text) in [(900, 910, "000910"), (990, 1000, "0\u{301}01000")] {
            let request = copy_request((start, 0), (end, 6), (2000, Vec::new()), None);
            assert_eq!(copy(&history, request), ("\n".repeat(10) + text, false));
        }
    }

    #[test]
    fn eviction_removes_the_oldest_segments_of_any_pane_and_reads_report_the_loss() {
        let budget = budget(256 * 1024);
        let mut first = History::new(budget.clone());
        let mut other = History::new(budget);
        first.append(0, rows(0..192));
        other.append(0, rows(0..128));
        let evicted = self::first(&mut first);
        assert!(evicted > 0);
        let (actual, page) = page(&mut first, 0, 192);
        assert_eq!(actual, evicted);
        for (offset, row) in page.iter().enumerate() {
            let prefix: String = row.cells.iter().take(6).map(|cell| cell.c).collect();
            assert_eq!(prefix.parse::<u64>().unwrap(), actual + offset as u64);
        }
        let hot = (192, Vec::new());
        assert!(read(&first, read_request(191, hot.clone(), None)).incomplete);
        let (text, incomplete) = copy(&first, copy_request((0, 0), (191, 71), hot, None));
        assert!(incomplete);
        assert!(text.starts_with(&format!("{evicted:06} ")));
    }

    fn terminal(
        script: &str,
        columns: u16,
        budget: Arc<HistoryBudget>,
    ) -> crate::terminal::Terminal {
        let size = crate::core::WindowSize {
            num_cols: columns,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let shell = crate::terminal::Shell::new("python3".into(), vec!["-c".into(), script.into()]);
        crate::terminal::Terminal::new_with_history(Some(shell), None, size, 0xffffff, 0, budget)
            .unwrap()
    }

    #[test]
    fn child_exit_drains_a_large_unterminated_sync_frame_through_disk_backpressure() {
        let script = "import os; os.write(1,b'\\x1b[?2026h'+b'x\\r\\n'*20000+b'LAST')";
        let mut terminal = terminal(script, 8, budget(16 << 20));
        while !matches!(
            crate::terminal::tests::next_event(&mut terminal, Duration::from_secs(5)).unwrap(),
            crate::terminal::Event::Exit(_)
        ) {}
        let core = &terminal.core;
        assert_eq!(core.history_size(), 19998);
        assert_eq!(core.row(2).cells[0].c, 'L');
    }

    #[test]
    fn failed_history_write_keeps_pty_input_and_output_running() {
        use crate::terminal::Event;

        let budget = budget(256 * 1024);
        let script = "import os; input(); os.write(1, b'x\\r\\n' * 12000); input(); os.write(1, b'\\r\\nALIVE'); input()";
        let mut terminal = terminal(script, 8, budget.clone());
        fs::remove_dir(&budget.directory).unwrap();
        terminal.write(b"start\n").unwrap();
        loop {
            match crate::terminal::tests::next_event(&mut terminal, Duration::from_secs(5)).unwrap()
            {
                Event::HistoryError(error) => {
                    assert!(error.to_string().contains("disk history disabled"));
                    break;
                }
                Event::Exit(status) => panic!("history failure ended child: {status}"),
                _ => (),
            }
        }
        terminal.write(b"continue\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !terminal.core.row(2).cells[..5]
            .iter()
            .map(|cell| cell.c)
            .eq("ALIVE".chars())
        {
            assert!(
                Instant::now() < deadline,
                "PTY output stopped after history failure"
            );
            crate::terminal::tests::drive(&mut terminal);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(terminal.core.history_size() <= HOT_ROWS);
        let core = &mut terminal.core;
        let point = |column| crate::core::Point { line: 2, column };
        core.selection_start(point(0), crate::core::Side::Left, SelectionType::Simple);
        core.selection_update(point(4), crate::core::Side::Right);
        let mut copy = core
            .copy_selection()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let mut text = String::new();
        copy.file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "ALIVE");
        let page = core
            .read_history(None, None, None)
            .0
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(page.text.contains("ALIVE") && page.incomplete);
    }
}
