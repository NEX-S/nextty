//! Bounded, workspace-owned disk history. Only this worker performs archive I/O.
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use crate::core::{Cell, Extra, Flags, Row, SelectionType, object_selection};
use crate::eventfd::{consume_notification, notify};
use crate::vt::{Color, NamedColor, Rgb};

pub const HOT_ROWS: usize = 512;
pub(crate) const BATCH: usize = 32;
const SEGMENT_BYTES: u64 = 64 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);

pub struct HistoryBudget {
    directory: PathBuf,
    capacity: u64,
    files: Mutex<BudgetFiles>,
    generation: AtomicU64,
    notifications: Mutex<Vec<std::sync::Weak<OwnedFd>>>,
}
struct BudgetFiles {
    // Registered entries and segments share their Arc, so identity avoids path scans.
    files: VecDeque<(Arc<Path>, u64, u64)>,
    used: u64,
}
impl HistoryBudget {
    pub fn new(directory: PathBuf, capacity: u64) -> io::Result<Arc<Self>> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "history capacity must be nonzero",
            ));
        }
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Arc::new(Self {
            directory,
            capacity,
            files: Mutex::new(BudgetFiles {
                files: VecDeque::new(),
                used: 0,
            }),
            generation: AtomicU64::new(0),
            notifications: Mutex::new(Vec::new()),
        }))
    }
    fn record_file(
        &self,
        path: &Arc<Path>,
        create: bool,
        bytes: u64,
        order: u64,
    ) -> io::Result<bool> {
        let mut files = self
            .files
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        if create {
            let end = if files.files.back().is_some_and(|(_, _, age)| *age > order) {
                files.files.partition_point(|(_, _, age)| *age <= order)
            } else {
                files.files.len()
            };
            files.files.insert(end, (path.clone(), bytes, order));
            files.used += bytes;
        } else {
            let start = files.files.partition_point(|(_, _, age)| *age < order);
            // Another pane can evict this segment while its write is in progress.
            let Some((_, previous, _)) = files
                .files
                .range_mut(start..)
                .take_while(|(_, _, age)| *age == order)
                .find(|(existing, _, _)| Arc::ptr_eq(existing, path))
            else {
                return Ok(false);
            };
            let previous = std::mem::replace(previous, bytes);
            files.used = files.used - previous + bytes;
        }
        let mut evicted = smallvec::SmallVec::<[Arc<Path>; 4]>::new();
        let mut retained = true;
        while files.used > self.capacity {
            let (evicted_path, bytes, _) = files.files.pop_front().unwrap();
            retained &= !Arc::ptr_eq(&evicted_path, path);
            files.used -= bytes;
            evicted.push(evicted_path);
        }
        drop(files);
        if !evicted.is_empty() {
            for path in evicted {
                remove_segment(&path)?;
            }
            // Readers may trust a generation until every evicted path is unlinked.
            self.generation.fetch_add(1, Ordering::Release);
            let mut notifications = self
                .notifications
                .lock()
                .map_err(|e| io::Error::other(e.to_string()))?;
            notifications.retain(|weak| weak.strong_count() != 0);
            for fd in notifications.iter().filter_map(std::sync::Weak::upgrade) {
                notify(fd.as_raw_fd())?;
            }
        }
        Ok(retained)
    }
    fn unregister(&self, segments: &[Segment]) -> io::Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        let mut files = self
            .files
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut removed = 0;
        if let [segment] = segments {
            let first = files
                .files
                .partition_point(|(_, _, order)| *order < segment.order);
            if let Some(index) = files
                .files
                .range(first..)
                .take_while(|(_, _, order)| *order == segment.order)
                .position(|(path, _, _)| Arc::ptr_eq(path, &segment.path))
            {
                let (_, bytes, _) = files.files.remove(first + index).unwrap();
                removed = bytes;
            }
        } else {
            // Batch clears avoid shifting the file table once per removed segment.
            let mut paths: Vec<_> = segments
                .iter()
                .map(|segment| Arc::as_ptr(&segment.path).cast::<()>())
                .collect();
            paths.sort_unstable();
            files.files.retain(|(path, bytes, _)| {
                if paths.binary_search(&Arc::as_ptr(path).cast::<()>()).is_ok() {
                    removed += bytes;
                    false
                } else {
                    true
                }
            });
        }
        files.used -= removed;
        drop(files);
        for segment in segments {
            remove_segment(&segment.path)?;
        }
        Ok(())
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

#[derive(Clone)]
pub struct HistoryConfig {
    pub budget: Arc<HistoryBudget>,
    pub hot_rows: usize,
}
impl HistoryConfig {
    pub fn new(budget: Arc<HistoryBudget>) -> Self {
        Self {
            budget,
            hot_rows: HOT_ROWS,
        }
    }
}

pub struct HistoryCopy {
    pub file: File,
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    pub epoch: u64,
    pub row: u64,
    pub offset: u64,
}

#[derive(Debug)]
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

struct SearchJob {
    segments: Vec<(Arc<Path>, u64, u64)>,
    hot_start: u64,
    hot: Vec<Row>,
    query: String,
    columns: usize,
    alternate: bool,
    cancelled: Arc<AtomicBool>,
    wake: OwnedFd,
    sender: mpsc::Sender<io::Result<crate::search::SearchResult>>,
}
struct ReadJob {
    read: Box<HistoryRead>,
    segments: Vec<(Arc<Path>, u64, u64)>,
    sender: mpsc::Sender<io::Result<HistoryPage>>,
    wake: Arc<OwnedFd>,
}
enum SearchWork {
    Find(SearchJob),
    Read(ReadJob),
}
impl SearchWork {
    fn cancelled(&self) -> &Arc<AtomicBool> {
        match self {
            Self::Find(job) => &job.cancelled,
            Self::Read(job) => &job.read.cancelled,
        }
    }
}
#[derive(Default)]
struct SearchState {
    pending: VecDeque<SearchWork>,
    active: Option<Arc<AtomicBool>>,
    closed: bool,
}
struct SearchWorker(Arc<(Mutex<SearchState>, Condvar)>);
impl SearchWorker {
    fn new() -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(SearchState::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("history-search".into())
            .spawn(move || {
                let (lock, changed) = &*worker;
                let mut state = lock.lock().unwrap();
                loop {
                    while state.pending.is_empty() && !state.closed {
                        state = changed.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    let job = state.pending.pop_front().unwrap();
                    state.active = Some(Arc::clone(job.cancelled()));
                    drop(state);
                    match job {
                        job if job.cancelled().load(Ordering::Relaxed) => drop(job),
                        SearchWork::Find(job) => {
                            let result = search_archive(&job);
                            if !job.cancelled.load(Ordering::Relaxed)
                                && job.sender.send(result).is_ok()
                                && let Err(error) = notify(job.wake.as_raw_fd())
                            {
                                eprintln!("notify pane search: {error}");
                            }
                        }
                        SearchWork::Read(job) => {
                            let result = read_archive(&job.read, &job.segments);
                            if !job.read.cancelled.load(Ordering::Relaxed)
                                && job.sender.send(result).is_ok()
                                && let Err(error) = notify(job.wake.as_raw_fd())
                            {
                                eprintln!("notify history read: {error}");
                            }
                        }
                    }
                    state = lock.lock().unwrap();
                    state.active = None;
                }
            })?;
        Ok(Self(shared))
    }
    fn submit(&self, job: SearchWork) {
        let (lock, changed) = &*self.0;
        let mut state = lock.lock().unwrap();
        let wake = state.active.is_none() && state.pending.is_empty();
        state
            .pending
            .retain(|job| !job.cancelled().load(Ordering::Relaxed));
        if !job.cancelled().load(Ordering::Relaxed) {
            state.pending.push_back(job);
            drop(state);
            if wake {
                changed.notify_one();
            }
        }
    }
}
impl Drop for SearchWorker {
    fn drop(&mut self) {
        let (lock, changed) = &*self.0;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        if let Some(cancelled) = &state.active {
            cancelled.store(true, Ordering::Relaxed);
        }
        let pending = std::mem::take(&mut state.pending);
        drop(state);
        changed.notify_one();
        drop(pending);
    }
}
fn search_archive(job: &SearchJob) -> io::Result<crate::search::SearchResult> {
    let mut matcher = crate::search::Matcher::new(&job.query);
    let mut cells = Vec::new();
    let finder = memchr::memmem::Finder::new(job.query.as_bytes());
    let mut wrapped = false;
    for (path, start, end) in &job.segments {
        if job.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let Some(mut reader) = open_segment(path)? else {
            matcher.gap();
            wrapped = false;
            continue;
        };
        let mut id = *start;
        let end = (*end).min(job.hot_start);
        while id < end {
            if job.cancelled.load(Ordering::Relaxed) {
                return Ok(matcher.finish(job.columns, job.alternate));
            }
            let (empty, line_break) = empty_row_headers(reader.buffer())
                .take((end - id) as usize)
                .fold((0, false), |(count, line_break), header| {
                    (count + 1, line_break | (header[6] == 0))
                });
            if empty != 0 {
                if line_break {
                    matcher.line_break();
                    wrapped = false;
                }
                reader.consume(empty * 9);
                id += empty as u64;
                continue;
            }
            if !wrapped
                && let Some((length, text)) = plain_row_text(reader.fill_buf()?)
                && finder.find(text).is_none()
            {
                reader.consume(length);
                matcher.line_break();
                id += 1;
                continue;
            }
            let row = read_row::<false>(&mut reader, cells)?;
            matcher.row(id, &row);
            wrapped = row.wrapped;
            cells = row.cells;
            id += 1;
        }
    }
    for (offset, row) in job.hot.iter().enumerate() {
        if job.cancelled.load(Ordering::Relaxed) {
            break;
        }
        matcher.row(job.hot_start + offset as u64, row);
    }
    Ok(matcher.finish(job.columns, job.alternate))
}

pub(crate) enum Reply {
    Stored {
        first: u64,
    },
    Cleared,
    Page {
        epoch: u64,
        start: u64,
        first: u64,
        rows: Vec<Row>,
    },
    Reflow {
        rows: VecDeque<Row>,
        first: u64,
        hot_start: u64,
        points: Vec<Option<(u64, usize)>>,
        view_top: u64,
    },
    Error(io::Error),
}
enum Request {
    Append {
        start: u64,
        rows: Vec<Row>,
    },
    Page {
        epoch: u64,
        start: u64,
        count: usize,
    },
    Copy {
        start: (u64, usize),
        end: (u64, usize),
        hot_start: u64,
        hot: Vec<Row>,
        kind: Option<SelectionType>,
        incomplete: bool,
        sender: mpsc::Sender<io::Result<HistoryCopy>>,
    },
    Search(SearchWork),
    Clear,
    Status,
    Reflow(Reflow),
}
pub(crate) struct Reflow {
    pub hot_start: u64,
    pub hot: Vec<Row>,
    pub points: Vec<(u64, usize)>,
    pub old_columns: usize,
    pub old_lines: usize,
    pub columns: usize,
    pub lines: usize,
    pub view_top: u64,
    pub keep_rows: usize,
}
pub(crate) struct History {
    sender: Option<mpsc::SyncSender<Request>>,
    replies: mpsc::Receiver<Reply>,
    notification: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
    pub hot_rows: usize,
    budget: Arc<HistoryBudget>,
    generation: u64,
}
impl History {
    pub fn new(config: HistoryConfig) -> io::Result<Self> {
        let notification = Arc::new(crate::eventfd::new()?);
        {
            let mut notifications = config
                .budget
                .notifications
                .lock()
                .map_err(|e| io::Error::other(e.to_string()))?;
            // Quiet panes can close without ever triggering budget eviction.
            notifications.retain(|weak| weak.strong_count() != 0);
            notifications.push(Arc::downgrade(&notification));
        }
        let budget = config.budget.clone();
        let generation = budget.generation.load(Ordering::Acquire);
        let wake = notification.clone();
        let (sender, requests) = mpsc::sync_channel(8);
        let (responses, replies) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("history-io".into())
            .spawn(move || {
                let mut archive = Archive::new(config.budget);
                let mut search_worker: Option<SearchWorker> = None;
                while let Ok(request) = requests.recv() {
                    let result = match request {
                        Request::Append { start, rows } => Some(
                            archive
                                .append(start, &rows)
                                .and_then(|()| archive.first())
                                .map(|first| Reply::Stored { first }),
                        ),
                        Request::Page {
                            epoch,
                            start,
                            count,
                        } => Some(archive.page(start, count).and_then(|(start, rows)| {
                            archive.first().map(|first| Reply::Page {
                                epoch,
                                start,
                                first,
                                rows,
                            })
                        })),
                        Request::Copy {
                            start,
                            end,
                            hot_start,
                            hot,
                            kind,
                            incomplete,
                            sender,
                        } => {
                            let _ = sender
                                .send(archive.copy(start, end, hot_start, &hot, kind, incomplete));
                            None
                        }
                        Request::Search(mut work) => {
                            if !work.cancelled().load(Ordering::Relaxed) {
                                let setup = (|| {
                                    let (segments, range) = match &mut work {
                                        SearchWork::Read(job) => {
                                            archive.first()?;
                                            let begin =
                                                archive.segments.partition_point(|segment| {
                                                    segment.end <= job.read.cursor.row
                                                });
                                            let end = archive.segments.partition_point(|segment| {
                                                segment.start <= job.read.end.0
                                            });
                                            (&mut job.segments, begin..end)
                                        }
                                        SearchWork::Find(job) => {
                                            let end = if job.alternate {
                                                0
                                            } else {
                                                archive.segments.partition_point(|segment| {
                                                    segment.start < job.hot_start
                                                })
                                            };
                                            (&mut job.segments, 0..end)
                                        }
                                    };
                                    *segments = archive
                                        .segments
                                        .range(range)
                                        .map(|segment| {
                                            (segment.path.clone(), segment.start, segment.end)
                                        })
                                        .collect();
                                    if search_worker.is_none() {
                                        search_worker = Some(SearchWorker::new()?);
                                    }
                                    Ok(())
                                })();
                                match setup {
                                    Ok(()) => search_worker.as_ref().unwrap().submit(work),
                                    Err(error) => match work {
                                        SearchWork::Read(job) => {
                                            let _ = job.sender.send(Err(error));
                                        }
                                        SearchWork::Find(job) => {
                                            let _ = job.sender.send(Err(error));
                                            if let Err(error) = notify(job.wake.as_raw_fd()) {
                                                eprintln!("notify pane search: {error}");
                                            }
                                            continue;
                                        }
                                    },
                                }
                            }
                            None
                        }
                        Request::Clear => Some(archive.clear().map(|()| Reply::Cleared)),
                        Request::Status => {
                            Some(archive.first().map(|first| Reply::Stored { first }))
                        }
                        Request::Reflow(request) => Some(archive.reflow(request)),
                    };
                    if let Some(result) = result
                        && responses.send(result.unwrap_or_else(Reply::Error)).is_err()
                    {
                        break;
                    }
                    if let Err(error) = notify(wake.as_raw_fd()) {
                        eprintln!("notify history consumer: {error}");
                        break;
                    }
                }
                if let Err(e) = archive.clear() {
                    eprintln!("clean pane history: {e}");
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            replies,
            notification,
            thread: Some(thread),
            hot_rows: config.hot_rows.max(1),
            budget,
            generation,
        })
    }
    pub fn event_fd(&self) -> RawFd {
        self.notification.as_raw_fd()
    }
    pub(crate) fn shutdown(mut self) {
        self.sender.take();
        self.thread.take();
    }
    pub fn check_budget(&mut self) {
        let generation = self.budget.generation.load(Ordering::Acquire);
        if generation != self.generation
            && self
                .sender
                .as_ref()
                .unwrap()
                .try_send(Request::Status)
                .is_ok()
        {
            self.generation = generation;
        }
    }
    pub fn drain(&self) -> io::Result<bool> {
        consume_notification(self.event_fd())
    }
    pub fn poll(&self) -> impl Iterator<Item = Reply> + '_ {
        self.replies.try_iter()
    }
    pub fn append(&self, start: u64, rows: Vec<Row>) -> Result<(), Vec<Row>> {
        match self
            .sender
            .as_ref()
            .unwrap()
            .try_send(Request::Append { start, rows })
        {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(Request::Append { rows, .. }))
            | Err(mpsc::TrySendError::Disconnected(Request::Append { rows, .. })) => Err(rows),
            _ => unreachable!(),
        }
    }
    pub fn page(&self, epoch: u64, start: u64, count: usize) -> bool {
        self.sender
            .as_ref()
            .unwrap()
            .try_send(Request::Page {
                epoch,
                start,
                count,
            })
            .is_ok()
    }
    fn send(&self, request: Request) -> io::Result<()> {
        self.sender
            .as_ref()
            .unwrap()
            .try_send(request)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock.into(),
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "history worker stopped")
                }
            })
    }
    pub fn clear(&self) -> io::Result<()> {
        self.send(Request::Clear)
    }
    pub fn reflow(&self, request: Reflow) -> Result<(), Reflow> {
        match self
            .sender
            .as_ref()
            .unwrap()
            .try_send(Request::Reflow(request))
        {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(Request::Reflow(request)))
            | Err(mpsc::TrySendError::Disconnected(Request::Reflow(request))) => Err(request),
            _ => unreachable!(),
        }
    }
    pub fn search(
        &self,
        query: String,
        hot: (u64, Vec<Row>),
        geometry: (usize, bool),
        cancelled: Arc<AtomicBool>,
        wake: OwnedFd,
    ) -> io::Result<mpsc::Receiver<io::Result<crate::search::SearchResult>>> {
        let (hot_start, hot) = hot;
        let (columns, alternate) = geometry;
        let (sender, receiver) = mpsc::channel();
        self.send(Request::Search(SearchWork::Find(SearchJob {
            segments: Vec::new(),
            hot_start,
            hot,
            query,
            columns,
            alternate,
            cancelled,
            wake,
            sender,
        })))?;
        Ok(receiver)
    }
    pub fn copy(
        &self,
        start: (u64, usize),
        end: (u64, usize),
        hot_start: u64,
        hot: Vec<Row>,
        kind: Option<SelectionType>,
        incomplete: bool,
    ) -> io::Result<mpsc::Receiver<io::Result<HistoryCopy>>> {
        let (sender, receiver) = mpsc::channel();
        self.send(Request::Copy {
            start,
            end,
            hot_start,
            hot,
            kind,
            incomplete,
            sender,
        })?;
        Ok(receiver)
    }

    pub fn read(&self, read: HistoryRead) -> io::Result<mpsc::Receiver<io::Result<HistoryPage>>> {
        let (sender, receiver) = mpsc::channel();
        self.send(Request::Search(SearchWork::Read(ReadJob {
            read: Box::new(read),
            segments: Vec::new(),
            sender,
            wake: Arc::clone(&self.notification),
        })))?;
        Ok(receiver)
    }
}
impl Drop for History {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("history worker panicked");
        }
    }
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
        if self.read.cancelled.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        rendered.clear();
        let from = if id == self.read.start.0 {
            self.read.start.1
        } else {
            0
        };
        let to = if id == self.read.end.0 {
            self.read.end.1.min(row.used())
        } else {
            row.used()
        };
        if from < to {
            rendered.extend(row.characters(from..to).map(|(_, ch)| ch));
        }
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

fn read_archive(read: &HistoryRead, segments: &[(Arc<Path>, u64, u64)]) -> io::Result<HistoryPage> {
    let first = segments
        .first()
        .map_or(read.hot_start, |(_, start, _)| *start)
        .min(read.hot_start);
    let mut incomplete = read.incomplete || read.cursor.row < first;
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
    if read.cursor.row < first {
        output.skip_to(first);
    }
    let mut cells = Vec::new();
    let mut rendered = String::new();
    let mut full = false;
    for (path, start, end) in segments {
        if read.cancelled.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if !output.has_more || *start >= read.hot_start || *start > read.end.0 {
            break;
        }
        if *end <= output.cursor.row {
            continue;
        }
        if *start > output.cursor.row {
            incomplete = true;
            output.skip_to(*start);
            if output.found {
                full = true;
                break;
            }
        }
        let Some(mut file) = open_segment(path)? else {
            incomplete = true;
            output.skip_to(*end);
            if output.found {
                full = true;
                break;
            }
            continue;
        };
        let from = output.cursor.row.max(*start);
        skip_rows(&mut file, from - *start)?;
        for id in from..(*end).min(read.hot_start) {
            if id > read.end.0 {
                break;
            }
            if id > read.start.0
                && id < read.end.0
                && let Some((length, text)) = plain_row_text(file.fill_buf()?)
                && !text.is_empty()
            {
                if read.cancelled.load(Ordering::Relaxed) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                std::str::from_utf8(text).unwrap().clone_into(&mut rendered);
                rendered.push('\n');
                full = output.text(id, &rendered)?;
                file.consume(length);
            } else {
                let row = read_row::<false>(&mut file, cells)?;
                full = output.row(id, &row, &mut rendered)?;
                cells = row.cells;
            }
            if full {
                break;
            }
        }
        if full {
            break;
        }
    }
    if !full && output.has_more && output.cursor.row < read.hot_start {
        incomplete = true;
        output.skip_to(read.hot_start);
        full = output.found;
    }
    if !full && output.has_more {
        for (index, row) in read
            .hot
            .iter()
            .enumerate()
            .skip(output.cursor.row.saturating_sub(read.hot_start) as usize)
        {
            let id = read.hot_start + index as u64;
            if id > read.end.0 {
                break;
            }
            if output.row(id, row, &mut rendered)? {
                break;
            }
        }
    }
    Ok(HistoryPage {
        text: output.text,
        cursor: output.cursor,
        has_more: output.has_more,
        matched: read.query.as_ref().map(|_| output.found),
        incomplete,
        command: read.command,
    })
}

struct Segment {
    path: Arc<Path>,
    start: u64,
    end: u64,
    bytes: u64,
    order: u64,
}
struct Archive {
    budget: Arc<HistoryBudget>,
    segments: VecDeque<Segment>,
    end: u64,
    order_hint: Option<u64>,
    encoded: Vec<u8>,
    current: Option<File>,
    generation: u64,
}
impl Archive {
    fn new(budget: Arc<HistoryBudget>) -> Self {
        Self {
            generation: budget.generation.load(Ordering::Acquire),
            budget,
            segments: VecDeque::new(),
            end: 0,
            order_hint: None,
            encoded: Vec::new(),
            current: None,
        }
    }
    fn first(&mut self) -> io::Result<u64> {
        let generation = self.budget.generation.load(Ordering::Acquire);
        if generation != self.generation {
            while let Some(segment) = self.segments.front()
                && !segment.path.try_exists()?
            {
                self.segments.pop_front();
            }
            if self.segments.is_empty() {
                self.current = None;
            }
            self.generation = generation;
        }
        Ok(self
            .segments
            .front()
            .map_or(self.end, |segment| segment.start))
    }
    fn append_file(
        &mut self,
        path: &Arc<Path>,
        create: bool,
        order: u64,
    ) -> io::Result<Option<u64>> {
        if create {
            self.current = Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?,
            );
        }
        let file = self.current.as_mut().unwrap();
        file.write_all(&self.encoded)?;
        let metadata = file.metadata()?;
        if metadata.nlink() == 0 {
            self.current = None;
            return Ok(None);
        }
        let retained =
            self.budget
                .record_file(path, create, (metadata.blocks() * 512).max(4096), order)?;
        if !retained {
            self.current = None;
        }
        Ok((create || retained).then_some(metadata.len()))
    }
    fn append(&mut self, start: u64, rows: &[Row]) -> io::Result<()> {
        if start != self.end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "noncontiguous disk history",
            ));
        }
        for chunk in rows.chunks(BATCH) {
            let end = self.end + chunk.len() as u64;
            self.encoded.clear();
            for row in chunk {
                write_row(&mut self.encoded, row);
            }
            let reuse = self.current.is_some()
                && self.segments.back().is_some_and(|segment| {
                    segment.bytes + self.encoded.len() as u64 <= SEGMENT_BYTES
                        && self.order_hint.is_none_or(|order| order == segment.order)
                });
            let bytes = if reuse {
                let segment = self.segments.back().unwrap();
                let path = segment.path.clone();
                let order = segment.order;
                self.append_file(&path, false, order)?
            } else {
                None
            };
            if let Some(bytes) = bytes {
                let segment = self.segments.back_mut().unwrap();
                segment.bytes = bytes;
                segment.end = end;
            } else {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let path: Arc<Path> = self.budget.directory.join(format!("{id}.rows")).into();
                let order = self.order_hint.unwrap_or(id);
                let bytes = self.append_file(&path, true, order)?.unwrap();
                self.segments.push_back(Segment {
                    path,
                    start: self.end,
                    end,
                    bytes,
                    order,
                });
            }
            self.end = end;
        }
        Ok(())
    }
    fn page(&mut self, start: u64, count: usize) -> io::Result<(u64, Vec<Row>)> {
        let first = self.first()?;
        let end = start.saturating_add(count as u64);
        let mut actual = start.max(first).min(end);
        let mut rows = Vec::with_capacity(count);
        let begin = self
            .segments
            .partition_point(|segment| segment.end <= actual);
        for segment in self.segments.range(begin..) {
            if segment.start >= end {
                break;
            }
            let Some(mut file) = open_segment(&segment.path)? else {
                // Keep only a contiguous suffix and retain its real row ID.
                actual = segment.end.min(end);
                rows.clear();
                continue;
            };
            let start = actual.max(segment.start);
            skip_rows(&mut file, start - segment.start)?;
            for _ in start..segment.end.min(end) {
                rows.push(read_row::<true>(&mut file, Vec::new())?);
            }
        }
        Ok((actual, rows))
    }
    fn copy(
        &mut self,
        mut start: (u64, usize),
        mut end: (u64, usize),
        hot_start: u64,
        hot: &[Row],
        kind: Option<SelectionType>,
        incomplete: bool,
    ) -> io::Result<HistoryCopy> {
        if let Some(kind) = kind
            && kind != SelectionType::Simple
        {
            let first = self.first()?.min(hot_start);
            self.expand_selection(&mut start, &mut end, first, hot_start, hot, kind)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .mode(0o600)
            .open(&self.budget.directory)?;
        let mut file = BufWriter::new(file);
        let incomplete = self.write_copy(
            &mut file,
            start,
            end,
            (hot_start, hot),
            incomplete,
            kind.is_some(),
        )?;
        let mut file = file.into_inner()?;
        file.rewind()?;
        Ok(HistoryCopy { file, incomplete })
    }
    fn write_copy<W: Write>(
        &mut self,
        file: &mut W,
        start: (u64, usize),
        end: (u64, usize),
        hot: (u64, &[Row]),
        mut incomplete: bool,
        skip_prompt: bool,
    ) -> io::Result<bool> {
        let (hot_start, hot) = hot;
        let first = self.first()?.min(hot_start);
        incomplete |= start.0 < first;
        let write = |file: &mut W, id: u64, row: &Row| -> io::Result<()> {
            let mut from = if id == start.0 { start.1 } else { 0 };
            if from < row.cells.len() {
                from = row.leading_column(from);
            }
            let to = if id == end.0 { end.1 } else { row.used() }.min(row.used());
            write_text(file, row, from, to, skip_prompt)?;
            if id != end.0 && !row.wrapped {
                file.write_all(b"\n")?;
            }
            Ok(())
        };
        let mut next = start.0.max(first);
        let begin = self.segments.partition_point(|segment| segment.end <= next);
        let mut cells = Vec::new();
        for segment in self.segments.range(begin..) {
            if segment.start >= hot_start || segment.start > end.0 {
                break;
            }
            let Some(mut reader) = open_segment(&segment.path)? else {
                incomplete = true;
                next = segment.end.min(hot_start);
                continue;
            };
            // Keep the segment open while copying it. Repeated small pages would
            // decode the same prefix for every batch, especially for empty rows.
            let row_start = next.max(segment.start).min(hot_start);
            skip_rows(&mut reader, row_start - segment.start)?;
            for id in row_start..segment.end.min(hot_start) {
                if id > end.0 {
                    break;
                }
                if id > start.0
                    && id < end.0
                    && let Some((length, text)) = plain_row_text(reader.fill_buf()?)
                    && !text.is_empty()
                {
                    file.write_all(text)?;
                    file.write_all(b"\n")?;
                    reader.consume(length);
                } else if let Some(line_break) = {
                    empty_row_headers(reader.fill_buf()?)
                        .next()
                        .map(|header| header[6] == 0)
                } {
                    if line_break && id != end.0 {
                        file.write_all(b"\n")?;
                    }
                    reader.consume(9);
                } else {
                    let row = read_row::<false>(&mut reader, cells)?;
                    write(file, id, &row)?;
                    cells = row.cells;
                }
                next = id + 1;
            }
        }
        if next < hot_start && next <= end.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing disk history without an evicted range",
            ));
        }
        for row in hot.iter().skip(next.saturating_sub(hot_start) as usize) {
            if next > end.0 {
                break;
            }
            write(file, next, row)?;
            next += 1;
        }
        incomplete |= next <= end.0;
        Ok(incomplete)
    }
    fn expand_selection(
        &mut self,
        start: &mut (u64, usize),
        end: &mut (u64, usize),
        first: u64,
        hot_start: u64,
        hot: &[Row],
        kind: SelectionType,
    ) -> io::Result<()> {
        let mut input: Option<(u64, u64, VecDeque<Row>, BufReader<File>)> = None;
        let mut source_row = |id: i64| -> io::Result<Option<Cow<'_, Row>>> {
            if id < first as i64 {
                return Ok(None);
            }
            let id = id as u64;
            if id >= hot_start {
                return Ok(hot.get((id - hot_start) as usize).map(Cow::Borrowed));
            }
            if let Some((next, _, rows, _)) = &input
                && id < *next
                && id >= *next - rows.len() as u64
            {
                return Ok(rows
                    .get((id - (*next - rows.len() as u64)) as usize)
                    .cloned()
                    .map(Cow::Owned));
            }
            if input
                .as_ref()
                .is_none_or(|(next, end, _, _)| id < *next || id >= *end)
            {
                let index = self.segments.partition_point(|segment| segment.end <= id);
                let Some(segment) = self
                    .segments
                    .get(index)
                    .filter(|segment| segment.start <= id)
                else {
                    return Ok(None);
                };
                let Some(file) = open_segment(&segment.path)? else {
                    return Ok(None);
                };
                input = Some((segment.start, segment.end, VecDeque::new(), file));
            }
            let (next, _, rows, file) = input.as_mut().unwrap();
            while *next <= id {
                let cells = if rows.len() == BATCH {
                    rows.pop_front().unwrap().cells
                } else {
                    Vec::new()
                };
                // Retain preceding rows for backward selection expansion.
                let skip = (id - *next).saturating_sub((BATCH - 1) as u64);
                if skip != 0 {
                    skip_rows(file, skip)?;
                    *next += skip;
                }
                rows.push_back(read_row::<false>(file, cells)?);
                *next += 1;
            }
            Ok(rows.back().cloned().map(Cow::Owned))
        };
        if kind == SelectionType::Object {
            if let Some([first, _]) = object_selection((start.0 as i64, start.1), &mut source_row)?
            {
                *start = (first.0 as u64, first.1);
            }
            if let Some([_, last]) =
                object_selection((end.0 as i64, end.1.saturating_sub(1)), &mut source_row)?
            {
                let row = source_row(last.0)?.unwrap();
                *end = (last.0 as u64, last.1 + row.cells[last.1].width());
            }
        } else {
            let mut first_point = (start.0 as i64, start.1);
            let mut last_point = (end.0 as i64, end.1.saturating_sub(1));
            crate::core::expand_selection(
                kind,
                &mut first_point,
                &mut last_point,
                &mut source_row,
            )?;
            *start = (first_point.0 as u64, first_point.1);
            let row = source_row(last_point.0)?;
            *end = (
                last_point.0 as u64,
                last_point.1
                    + row
                        .as_ref()
                        .map_or(1, |row| row.cells[last_point.1].width()),
            );
        }
        Ok(())
    }
    fn reflow(&mut self, request: Reflow) -> io::Result<Reply> {
        let first = self.first()?;
        let mut output = ReflowOutput::new(self.budget.clone(), &request);
        let mut anchors: Vec<_> = request
            .points
            .iter()
            .copied()
            .enumerate()
            .map(|(index, (row, column))| (row, column, index))
            .collect();
        anchors.sort_unstable();
        let mut anchor = 0;
        while anchors.get(anchor).is_some_and(|(row, _, _)| *row < first) {
            anchor += 1;
        }
        if request.old_columns == request.columns {
            let needed = output.keep.saturating_sub(request.hot.len());
            let retain_before = request.hot_start.saturating_sub(needed as u64);
            while self
                .segments
                .front()
                .is_some_and(|segment| segment.end <= retain_before)
            {
                let segment = self.segments.pop_front().unwrap();
                output.flow.count = segment.end;
                output.archive.end = segment.end;
                output.archive.segments.push_back(segment);
            }
            while let Some(&(row, column, index)) = anchors.get(anchor)
                && row < output.flow.count
            {
                output.flow.points[index] = Some((row, column));
                anchor += 1;
            }
            if (first..output.flow.count).contains(&request.view_top) {
                output.flow.view_top = Some(request.view_top);
            }
        }
        let mut cells = Vec::new();
        while let Some(segment) = self.segments.pop_front() {
            let Some(mut file) = open_segment(&segment.path)? else {
                continue;
            };
            output.archive.order_hint = Some(segment.order);
            for id in segment.start..segment.end {
                let row = read_row::<true>(&mut file, cells)?;
                cells = output.row(id, row, &anchors, &mut anchor)?;
            }
            drop(file);
            self.budget.unregister(std::slice::from_ref(&segment))?;
        }
        output.archive.order_hint = Some(NEXT.fetch_add(1, Ordering::Relaxed));
        let hot_len = request.hot.len();
        for (index, row) in request.hot.into_iter().enumerate() {
            output.row(request.hot_start + index as u64, row, &anchors, &mut anchor)?;
        }
        if let Some(row) = output.flow.finish() {
            ReflowOutput::retain_row(
                row,
                &mut output.tail,
                &mut output.pending,
                &mut output.archive,
                output.keep,
            )?;
        }
        let cursor = output
            .flow
            .points
            .first()
            .copied()
            .flatten()
            .unwrap_or((0, 0))
            .0;
        if request.old_columns != request.columns {
            while output.tail.len() > request.lines
                && output.tail.back().is_some_and(|row| row.used() == 0)
                && output.flow.count - 1 > cursor
            {
                output.tail.pop_back();
                output.flow.count -= 1;
            }
        } else {
            let removed = request.old_lines.saturating_sub(request.lines).min(
                request.old_lines
                    - 1
                    - (request.points[0].0.saturating_sub(
                        request.hot_start + hot_len as u64 - request.old_lines as u64,
                    ) as usize),
            );
            for _ in 0..removed {
                output.tail.pop_back();
                output.flow.count -= 1;
            }
        }
        let screen_start = output
            .flow
            .count
            .saturating_sub(request.lines as u64)
            .min(cursor);
        while output.flow.count > screen_start + request.lines as u64 {
            output.tail.pop_back();
            output.flow.count -= 1;
        }
        while output.tail.len() < request.lines {
            output
                .tail
                .push_back(Row::new(request.columns, Cell::default()));
            output.flow.count += 1;
        }
        output.flush()?;
        let hot_start = output.flow.count - output.tail.len() as u64;
        let first = output.archive.first()?;
        let view_top = output.flow.view_top.unwrap_or(first).max(first);
        output.archive.order_hint = None;
        *self = output.archive;
        Ok(Reply::Reflow {
            rows: output.tail,
            first,
            hot_start,
            points: output.flow.points,
            view_top,
        })
    }
    fn clear(&mut self) -> io::Result<()> {
        self.current = None;
        let result = self.budget.unregister(self.segments.make_contiguous());
        self.segments.clear();
        result?;
        self.end = 0;
        Ok(())
    }
}
struct ReflowOutput {
    archive: Archive,
    tail: VecDeque<Row>,
    pending: Vec<Row>,
    flow: crate::core::RowReflow,
    keep: usize,
}
impl ReflowOutput {
    fn new(budget: Arc<HistoryBudget>, request: &Reflow) -> Self {
        Self {
            archive: Archive::new(budget),
            tail: VecDeque::new(),
            pending: Vec::new(),
            flow: crate::core::RowReflow::new(
                request.old_columns,
                request.columns,
                request.points.len(),
                request.view_top,
            ),
            keep: request.keep_rows.max(request.lines),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.archive.append(self.archive.end, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
    fn retain_row(
        row: Row,
        tail: &mut VecDeque<Row>,
        pending: &mut Vec<Row>,
        archive: &mut Archive,
        keep: usize,
    ) -> io::Result<()> {
        if tail.len() == keep {
            pending.push(tail.pop_front().unwrap());
        }
        tail.push_back(row);
        if pending.len() == BATCH {
            archive.append(archive.end, pending)?;
            pending.clear();
        }
        Ok(())
    }
    fn row(
        &mut self,
        id: u64,
        row: Row,
        anchors: &[(u64, usize, usize)],
        anchor: &mut usize,
    ) -> io::Result<Vec<Cell>> {
        self.flow.row(id, row, anchors, anchor, &mut |row| {
            Self::retain_row(
                row,
                &mut self.tail,
                &mut self.pending,
                &mut self.archive,
                self.keep,
            )
        })
    }
}

/// `None` means another pane's append evicted the segment.
fn open_segment(path: &Path) -> io::Result<Option<BufReader<File>>> {
    match File::open(path) {
        Ok(file) => Ok(Some(BufReader::new(file))),
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
fn write_text(
    file: &mut impl Write,
    row: &Row,
    start: usize,
    end: usize,
    skip_prompt: bool,
) -> io::Result<()> {
    if start >= end {
        return Ok(());
    }
    let mut bytes = [0; 512];
    let mut length = 0;
    for (column, ch) in row.characters(start..end) {
        if skip_prompt && row.cells[column].flags.contains(Flags::PROMPT) {
            continue;
        }
        if length + 4 > bytes.len() {
            file.write_all(&bytes[..length])?;
            length = 0;
        }
        length += ch.encode_utf8(&mut bytes[length..]).len();
    }
    file.write_all(&bytes[..length])
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
fn read_color(bytes: [u8; 4]) -> io::Result<Color> {
    match bytes {
        [0, lo, hi, _] => NamedColor::from_index(u16::from_le_bytes([lo, hi])).map(Color::Named),
        [1, index, _, _] => Some(Color::Indexed(index)),
        [2, r, g, b] => Some(Color::Spec(Rgb { r, g, b })),
        _ => None,
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid history color"))
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
fn plain_row_text(bytes: &[u8]) -> Option<(usize, &[u8])> {
    let header = bytes.get(..19)?;
    let columns = u16::from_le_bytes(header[..2].try_into().unwrap()) as usize;
    let used = u16::from_le_bytes(header[2..4].try_into().unwrap()) as usize;
    let stored = u16::from_le_bytes(header[4..6].try_into().unwrap()) as usize;
    let length = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;
    let text_bytes = u32::from_le_bytes(header[13..17].try_into().unwrap()) as usize;
    if columns == 0
        || used != stored
        || stored > columns
        || header[6..9] != [0; 3]
        || header[17..19] != [0; 2]
        || length != 6 + text_bytes
    {
        return None;
    }
    let text = bytes.get(19..13 + length)?;
    let valid = if text_bytes == stored {
        text.is_ascii()
    } else {
        std::str::from_utf8(text).is_ok_and(|text| text.chars().count() == stored)
    };
    valid.then_some((13 + length, text))
}
fn empty_row_headers(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Empty rows have only a header; avoid materializing their cells.
    bytes.chunks_exact(9).take_while(|header| {
        header[2..6] == [0; 4] && header[7..] == [0; 2] && header[..2] != [0, 0]
    })
}
fn skip_rows(file: &mut BufReader<File>, mut count: u64) -> io::Result<()> {
    while count != 0 {
        let buffer = file.fill_buf()?;
        let empty = empty_row_headers(buffer).take(count as usize).count();
        if empty == 0 {
            let mut consumed = 0;
            let mut skipped = 0;
            while skipped < count {
                let remaining = &buffer[consumed..];
                let Some(header) = remaining.get(..13) else {
                    break;
                };
                if header[4..6] == [0; 2] && header[7..9] == [0; 2] {
                    break;
                }
                let length = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;
                if length > remaining.len() - 13 {
                    break;
                }
                consumed += 13 + length;
                skipped += 1;
            }
            if skipped != 0 {
                file.consume(consumed);
                count -= skipped;
                continue;
            }
            let header = read_array::<9>(file)?;
            if header[4..6] != [0; 2] || header[7..9] != [0; 2] {
                let length = u32::from_le_bytes(read_array(file)?);
                file.seek_relative(i64::from(length))?;
            }
            count -= 1;
        } else {
            file.consume(empty * 9);
            count -= empty as u64;
        }
    }
    Ok(())
}
fn read_array<const N: usize>(file: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn read_row<const COLORS: bool>(file: &mut impl BufRead, mut cells: Vec<Cell>) -> io::Result<Row> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let header = read_array::<9>(file)?;
    let columns = u16::from_le_bytes([header[0], header[1]]) as usize;
    let used = u16::from_le_bytes([header[2], header[3]]) as usize;
    let stored = u16::from_le_bytes([header[4], header[5]]) as usize;
    let wrapped = header[6] != 0;
    let count = u16::from_le_bytes([header[7], header[8]]) as usize;
    if columns == 0 || used > stored || stored > columns {
        return Err(invalid("invalid history row"));
    }
    cells.clear();
    cells.resize(columns, Cell::default());
    if stored == 0 && count == 0 {
        return Ok(Row::from_parts(cells, wrapped, used, Vec::new()).unwrap());
    }
    let length = u32::from_le_bytes(read_array(file)?) as usize;
    let decode = |mut file: &[u8]| {
        let text_bytes = u32::from_le_bytes(read_array(&mut file)?) as usize;
        let runs = u16::from_le_bytes(read_array(&mut file)?) as usize;
        let text = file.get(..text_bytes).ok_or(io::ErrorKind::UnexpectedEof)?;
        file = &file[text_bytes..];
        let text = std::str::from_utf8(text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut chars = text.chars();
        let mut next = || chars.next().ok_or_else(|| invalid("short history text"));
        let mut column = 0;
        for _ in 0..runs {
            let run = read_array::<14>(&mut file)?;
            let start = u16::from_le_bytes(run[..2].try_into().unwrap()) as usize;
            let end = u16::from_le_bytes(run[2..4].try_into().unwrap()) as usize;
            let bits = u16::from_le_bytes(run[12..14].try_into().unwrap());
            let pairs = bits & WIDE_PAIRS != 0;
            let flags = Flags::from_bits(bits & !WIDE_PAIRS)
                .ok_or_else(|| invalid("invalid history flags"))?;
            if start < column
                || start > end
                || end > stored
                || pairs && !(end - start).is_multiple_of(2)
            {
                return Err(invalid("invalid history style range"));
            }
            for cell in &mut cells[column..start] {
                cell.c = next()?;
            }
            let (fg, bg) = if COLORS {
                (
                    read_color(run[4..8].try_into().unwrap())?,
                    read_color(run[8..12].try_into().unwrap())?,
                )
            } else {
                (Cell::default().fg, Cell::default().bg)
            };
            if pairs {
                for pair in cells[start..end].chunks_exact_mut(2) {
                    pair[0] = Cell {
                        c: next()?,
                        fg,
                        bg,
                        flags,
                    };
                    pair[1] = pair[0].wide_spacer();
                }
            } else {
                for cell in &mut cells[start..end] {
                    *cell = Cell {
                        c: next()?,
                        fg,
                        bg,
                        flags,
                    };
                }
            }
            column = end;
        }
        for cell in &mut cells[column..stored] {
            cell.c = next()?;
        }
        if chars.next().is_some() {
            return Err(invalid("long history text"));
        }
        let mut extras = Vec::with_capacity(count);
        for _ in 0..count {
            let header = read_array::<6>(&mut file)?;
            let column = u16::from_le_bytes(header[..2].try_into().unwrap()) as usize;
            let count = u32::from_le_bytes(header[2..].try_into().unwrap()) as usize;
            let mut zerowidth = smallvec::SmallVec::with_capacity(count.min(1024));
            for _ in 0..count {
                zerowidth.push(
                    char::from_u32(u32::from_le_bytes(read_array(&mut file)?))
                        .ok_or_else(|| invalid("invalid history combining character"))?,
                );
            }
            let underline_color = if read_array::<1>(&mut file)?[0] == 0 {
                None
            } else {
                Some(read_color(read_array(&mut file)?)?)
            };
            extras.push((
                column,
                Extra {
                    zerowidth,
                    underline_color,
                },
            ));
        }
        if !file.is_empty() {
            return Err(invalid("invalid history row length"));
        }
        Row::from_parts(cells, wrapped, used, extras).ok_or_else(|| invalid("invalid history row"))
    };
    if file.fill_buf()?.len() >= length {
        let row = decode(&file.fill_buf()?[..length])?;
        file.consume(length);
        Ok(row)
    } else {
        let mut bytes = smallvec::SmallVec::<[u8; 1024]>::with_capacity(length.min(8192));
        let mut file = file.take(length as u64);
        while bytes.len() < length {
            let buffer = file.fill_buf()?;
            if buffer.is_empty() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            bytes.extend_from_slice(buffer);
            let count = buffer.len();
            file.consume(count);
        }
        decode(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn wide_character_runs_restore_every_cell() {
        let red = Color::Named(NamedColor::Red);
        let wide = |c| Cell {
            c,
            fg: red,
            flags: Flags::WIDE_CHAR,
            ..Cell::default()
        };
        let mut cells = vec![Cell::default(); 12];
        cells[0].c = 'a';
        cells[1] = wide('中');
        cells[2] = cells[1].wide_spacer();
        cells[3] = wide('文');
        cells[4] = cells[3].wide_spacer();
        cells[5] = wide('字');
        // Spacers that differ from the derived one, and wide cells without one, stay exact.
        cells[6] = Cell {
            bg: Color::Named(NamedColor::Blue),
            ..cells[5].wide_spacer()
        };
        cells[7].c = 'b';
        cells[9] = wide('尾');
        let row = Row::from_parts(cells, false, 10, Vec::new()).unwrap();
        let mut bytes = Vec::new();
        write_row(&mut bytes, &row);
        let decoded = read_row::<true>(&mut bytes.as_slice(), Vec::new()).unwrap();
        assert_eq!(decoded.cells, row.cells);
        assert_eq!(decoded.used(), row.used());
    }
    #[test]
    fn search_keeps_unicode_matches_across_the_cold_hot_boundary() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut archive = Archive::new(HistoryBudget::new(directory.clone(), 1024 * 1024).unwrap());
        let mut core = crate::core::Core::new(
            crate::core::WindowSize {
                num_cols: 4,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(&mut core, "中a\u{301}bc".as_bytes());
        archive.append(0, &[core.row(0).clone()]).unwrap();
        let (sender, receiver) = mpsc::channel();
        let job = SearchJob {
            segments: archive
                .segments
                .iter()
                .map(|s| (s.path.clone(), s.start, s.end))
                .collect(),
            hot_start: 1,
            hot: vec![core.row(1).clone()],
            query: "a\u{301}bc".into(),
            columns: 4,
            alternate: false,
            cancelled: Default::default(),
            wake: crate::eventfd::new().unwrap(),
            sender,
        };
        let worker = SearchWorker::new().unwrap();
        worker.submit(SearchWork::Find(job));
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            result.matches.as_ref(),
            vec![crate::search::SearchMatch {
                start_row: 0,
                end_row: 1,
                start_column: 2,
                end_column: 1,
            }]
        );
        assert!(!result.truncated);
        drop(worker);
        archive.clear().unwrap();
        drop(archive);
    }
    #[test]
    fn text_query_matches_across_segments_hard_lines_and_the_cold_hot_boundary() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "read-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let budget = HistoryBudget::new(directory, 1024 * 1024).unwrap();
        let mut core = crate::core::Core::new(
            crate::core::WindowSize {
                num_cols: 8,
                num_lines: 8,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(
            &mut core,
            "before\r\nabcde中Xa\u{301}bc\r\nlast\r\nafter\r\ntail".as_bytes(),
        );
        let segments = [(0, 2), (2, 3)]
            .into_iter()
            .map(|(start, end)| {
                let path: Arc<Path> = budget.directory.join(format!("{start}.rows")).into();
                let mut bytes = Vec::new();
                for id in start..end {
                    write_row(&mut bytes, core.row(id));
                }
                std::fs::write(&path, bytes).unwrap();
                (path, start as u64, end as u64)
            })
            .collect::<Vec<_>>();
        let read = HistoryRead {
            start: (0, 0),
            end: (5, core.row(5).used()),
            hot_start: 3,
            hot: (3..=5).map(|id| core.row(id).clone()).collect(),
            cursor: HistoryCursor {
                epoch: 3,
                row: 0,
                offset: 0,
            },
            query: Some("中Xa\u{301}bc\nlast".into()),
            incomplete: false,
            command: None,
            cancelled: Default::default(),
        };
        let page = read_archive(&read, &segments).unwrap();
        assert_eq!(page.text, "before\nabcde中Xa\u{301}bc\nlast\nafter\n");
        assert_eq!(page.matched, Some(true));
        assert!(page.has_more);
        assert!(!page.incomplete);
        assert_eq!(
            page.cursor,
            HistoryCursor {
                epoch: 3,
                row: 5,
                offset: 0
            }
        );

        fs::remove_file(&segments[0].0).unwrap();
        let missing = HistoryRead {
            end: (0, core.row(0).used()),
            hot: Vec::new(),
            query: None,
            ..read
        };
        let page = read_archive(&missing, &segments).unwrap();
        assert_eq!(page.text, "");
        assert_eq!(page.cursor.row, 0);
        assert_eq!(page.cursor.offset, 0);
        assert!(!page.has_more);
        assert!(page.incomplete);
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
            write_text(&mut copied, &row, 0, row.used(), skip_prompt).unwrap();
            let expected = format!("prefix{}{}", if skip_prompt { "" } else { "> " }, text);
            assert_eq!(String::from_utf8(copied).unwrap(), expected);
        }
    }

    #[test]
    fn decoding_sparse_rows_with_reused_storage_preserves_their_full_cells() {
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
            )
            .unwrap(),
            Row::from_parts(vec![Cell::default(); 100], false, 0, Vec::new()).unwrap(),
            Row::from_parts(vec![Cell::default(); 3], false, 0, Vec::new()).unwrap(),
            Row::from_parts(styled, false, 0, Vec::new()).unwrap(),
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
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes).unwrap();
        for capacity in [7, 31, 257, 4096] {
            let mut reader = BufReader::with_capacity(capacity, bytes.as_slice());
            let mut cells = Vec::new();
            for expected in &rows {
                let decoded = read_row::<true>(&mut reader, cells).unwrap();
                assert_eq!(decoded.cells, expected.cells);
                assert_eq!(decoded.used(), expected.used());
                assert_eq!(decoded.wrapped, expected.wrapped);
                assert_eq!(decoded.extras(), expected.extras());
                cells = decoded.cells;
            }
            assert!(reader.fill_buf().unwrap().is_empty());
            for (skip, expected) in rows.iter().enumerate() {
                file.rewind().unwrap();
                let mut reader = BufReader::with_capacity(capacity, file.try_clone().unwrap());
                skip_rows(&mut reader, skip as u64).unwrap();
                let decoded = read_row::<true>(&mut reader, Vec::new()).unwrap();
                assert_eq!(decoded.cells, expected.cells);
                assert_eq!(decoded.used(), expected.used());
                assert_eq!(decoded.wrapped, expected.wrapped);
                assert_eq!(decoded.extras(), expected.extras());
            }
        }
    }
    #[test]
    fn empty_line_flood_uses_filled_segments_and_charges_filesystem_blocks() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "empty-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let budget = HistoryBudget::new(directory.clone(), 1024 * 1024).unwrap();
        let mut archive = Archive::new(budget.clone());
        let rows =
            vec![Row::from_parts(vec![Cell::default(); 80], false, 0, Vec::new()).unwrap(); 32];
        for _ in 0..625 {
            archive.append(archive.end, &rows).unwrap();
        }
        assert_eq!(archive.end, 20_000);
        assert_eq!(archive.segments.len(), 3);
        let actual: u64 = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().blocks() * 512)
            .sum();
        let files = budget.files.lock().unwrap();
        assert_eq!(files.used, actual);
        assert!(actual <= 208 * 1024);
        assert_eq!(files.files.len(), 3);
        drop(files);
        archive.clear().unwrap();
        drop(archive);
        drop(budget);
        assert!(!directory.exists());
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    };
    fn budget() -> Arc<HistoryBudget> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        HistoryBudget::new(
            root.join(format!(
                "concurrent-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            256 * 1024,
        )
        .unwrap()
    }
    fn row(id: u64) -> Row {
        let value = format!("{id:06} {}", "x".repeat(65));
        // Dense styles keep eviction fixtures independent of plain-text compression.
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
        Row::from_parts(cells, false, used, Vec::new()).unwrap()
    }

    #[test]
    fn closed_histories_do_not_accumulate_notification_registrations() {
        let budget = budget();
        let live = History::new(HistoryConfig::new(budget.clone())).unwrap();
        for _ in 0..32 {
            drop(History::new(HistoryConfig::new(budget.clone())).unwrap());
        }
        let notifications = budget.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 2);
        assert!(notifications[0].ptr_eq(&Arc::downgrade(&live.notification)));
        assert!(notifications[1].upgrade().is_none());
    }

    #[test]
    fn failed_history_write_keeps_pty_input_and_output_running() {
        use crate::core::WindowSize;
        use crate::terminal::{Event, Shell, Terminal};
        use std::time::{Duration, Instant};

        let budget = budget();
        let terminal = Terminal::new_with_history(
            Some(Shell::new(
                "python3".into(),
                vec![
                    "-c".into(),
                    "import os; input(); os.write(1, b'x\\r\\n' * 12000); input(); os.write(1, b'\\r\\nALIVE'); input()".into(),
                ],
            )),
            None,
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
            HistoryConfig {
                budget: budget.clone(),
                hot_rows: 8,
            },
        )
        .unwrap();
        fs::remove_dir(&budget.directory).unwrap();
        terminal.write(b"start\n".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match terminal
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::HistoryError(error) => {
                    assert!(error.to_string().contains("disk history disabled"));
                    break;
                }
                Event::Exit(status) => panic!("history failure ended child: {status}"),
                _ => (),
            }
        }
        terminal.write(b"continue\n".to_vec()).unwrap();
        loop {
            let core = terminal.core.lock().unwrap();
            if core.row(2).cells[..5]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                == "ALIVE"
            {
                assert!(core.history_read_ready());
                assert!(core.history_size() <= 10_000);
                assert_eq!(core.history_event_fd(), None);
                break;
            }
            drop(core);
            assert!(
                Instant::now() < deadline,
                "PTY output stopped after history failure"
            );
            for event in terminal.events.try_iter() {
                if let Event::Exit(status) = event {
                    panic!("history failure ended child: {status}");
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn disk_reflow_preserves_custom_tabs_like_memory_resize() {
        use crate::core::{Core, WindowSize};
        use crate::vt::Parser;
        use std::time::{Duration, Instant};

        let mut size = WindowSize {
            num_cols: 24,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let mut memory = Core::new(size, 0xffffff, 0);
        let mut disk = Core::new(size, 0xffffff, 0);
        disk.enable_history(HistoryConfig {
            budget: budget(),
            hot_rows: 8,
        })
        .unwrap();
        let mut memory_parser = Parser::new();
        let mut disk_parser = Parser::new();
        let custom = b"\x1b[3g\x1b[1;4H\x1bH\x1b[H";
        memory_parser.advance(&mut memory, custom);
        disk_parser.advance(&mut disk, custom);
        for (columns, lines) in [(32, 3), (12, 3), (24, 3), (24, 4)] {
            size.num_cols = columns;
            size.num_lines = lines;
            memory.resize(size);
            disk.resize(size);
            let deadline = Instant::now() + Duration::from_secs(5);
            while disk.resize_pending() {
                disk.poll_history(true).unwrap();
                assert!(Instant::now() < deadline, "disk resize stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
            for column in 1..columns {
                let input = format!("\x1b[1;{column}H\t");
                memory_parser.advance(&mut memory, input.as_bytes());
                disk_parser.advance(&mut disk, input.as_bytes());
                assert_eq!(disk.cursor().0, memory.cursor().0);
            }
        }
    }

    #[test]
    fn removed_cached_segment_is_replaced_before_publishing_more_rows() {
        let mut archive = Archive::new(budget());
        archive.append(0, &[row(0)]).unwrap();
        fs::remove_file(&archive.segments.back().unwrap().path).unwrap();
        archive.append(1, &[row(1)]).unwrap();
        let (start, rows) = archive.page(0, 2).unwrap();
        assert_eq!(start, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells, row(1).cells);
    }

    #[test]
    fn evicted_and_cleared_archives_release_open_files() {
        for evicted in [false, true] {
            let mut budget = budget();
            if evicted {
                Arc::get_mut(&mut budget).unwrap().capacity = 1;
            }
            let mut archive = Archive::new(budget.clone());
            archive
                .append(0, &(0..BATCH as u64).map(row).collect::<Vec<_>>())
                .unwrap();
            if !evicted {
                archive.clear().unwrap();
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
    fn exited_child_waits_for_blocked_history_without_spinning_and_drains_output() {
        use crate::core::WindowSize;
        use crate::terminal::{Event, Shell, Terminal};
        use std::time::{Duration, Instant};

        let budget = budget();
        let pid_file = budget.directory.join("exited-child-pid");
        // Release the archive lock before Terminal joins its workers on assertion failure.
        let terminal;
        let paused = budget.files.lock().unwrap();
        terminal = Terminal::new_with_history(
            Some(Shell::new(
                "python3".into(),
                vec![
                    "-c".into(),
                    r"import os, sys
from pathlib import Path
path = Path(sys.argv[1])
pending = path.with_suffix('.tmp')
pending.write_text(str(os.getpid()))
pending.replace(path)
os.write(1, b'x\r\n' * 1200 + b'LAST')
"
                    .into(),
                    pid_file.to_str().unwrap().into(),
                ],
            )),
            None,
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
            HistoryConfig {
                budget: budget.clone(),
                hot_rows: 8,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let blocked = !terminal.core.lock().unwrap().history_read_ready();
            let exited = fs::read_to_string(&pid_file).ok().is_some_and(|pid| {
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .unwrap()
                    .split_once(") ")
                    .unwrap()
                    .1
                    .starts_with('Z')
            });
            if blocked && exited {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child did not exit with blocked history"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let cpu_time = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(terminal.io_cpu_clock(), &mut time) },
                0
            );
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        };
        let start = cpu_time();
        std::thread::sleep(Duration::from_millis(200));
        let elapsed = cpu_time() - start;
        drop(paused);
        loop {
            match terminal
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Exit(_) => break,
                Event::Error(error) => panic!("{error}"),
                _ => (),
            }
        }
        let core = terminal.core.lock().unwrap();
        assert_eq!(core.history_size(), 1198);
        assert_eq!(
            core.row(2).cells[..4]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "LAST"
        );
        drop(core);
        drop(terminal);
        assert!(
            elapsed < Duration::from_millis(20),
            "blocked terminal consumed {elapsed:?} CPU in 200 ms"
        );
    }

    #[test]
    fn terminal_shutdown_leaves_busy_disk_cleanup_on_the_io_thread_even_after_eof() {
        use crate::core::WindowSize;
        use crate::terminal::{Event, Shell, Terminal};
        use std::time::{Duration, Instant};

        for exited in [false, true] {
            let budget = budget();
            let directory = budget.directory.clone();
            let pid_file = directory.join("child-pid");
            let paused = budget.files.lock().unwrap();
            let script = format!(
                "import os,signal,sys,time; signal.signal(signal.SIGHUP,signal.SIG_IGN); open(sys.argv[1],'w').write(str(os.getpid())); os.write(1,b'x\\r\\n'*64+b'READY'); {}",
                if exited { "" } else { "time.sleep(30)" },
            );
            let terminal = Terminal::new_with_history(
                Some(Shell::new(
                    "python3".into(),
                    vec!["-c".into(), script, pid_file.to_str().unwrap().into()],
                )),
                None,
                WindowSize {
                    num_cols: 8,
                    num_lines: 3,
                    cell_width: 0,
                    cell_height: 0,
                },
                0xffffff,
                0,
                HistoryConfig {
                    budget: budget.clone(),
                    hot_rows: 8,
                },
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut fd = libc::pollfd {
                    fd: terminal.event_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let timeout =
                    crate::poll::timeout_millis(deadline.saturating_duration_since(Instant::now()));
                assert!(unsafe { libc::poll(&mut fd, 1, timeout) } > 0);
                terminal.drain_notification().unwrap();
                terminal.acknowledge_wakeup();
                let mut finished = false;
                for event in terminal.events.try_iter() {
                    match event {
                        Event::Error(error) => panic!("{error}"),
                        Event::Exit(_) if exited => finished = true,
                        _ => (),
                    }
                }
                if finished || !exited && terminal.core.lock().unwrap().row(2).cells[0].c == 'R' {
                    break;
                }
            }
            let pid = fs::read_to_string(pid_file).unwrap();
            let (sender, receiver) = mpsc::channel();
            let caller = std::thread::spawn(move || sender.send(terminal.shutdown()).unwrap());
            let shutdown = receiver.recv_timeout(Duration::from_secs(1));
            drop(paused);
            caller.join().unwrap();
            shutdown
                .expect("shutdown waited for the disk worker")
                .join()
                .unwrap();
            assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
            drop(budget);
            assert!(!directory.exists());
        }
    }

    #[test]
    fn copy_retry_after_backpressure_clear_and_resize_progresses_on_notifications() {
        use crate::core::{Core, Point, SelectionType, Side, WindowSize};
        use crate::vt::{ClearMode, Parser};

        let budget = budget();
        let mut core = Core::new(
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        core.enable_history(HistoryConfig {
            budget: budget.clone(),
            hot_rows: 8,
        })
        .unwrap();
        let mut parser = Parser::new();
        let paused = budget.files.lock().unwrap();
        parser.advance_queued(&mut core, "x\r\n".repeat(10_000).as_bytes());
        assert!(!core.history_read_ready());
        core.resize(WindowSize {
            num_cols: 6,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        });
        core.clear_screen(ClearMode::Saved);
        assert!(
            matches!(core.copy_selection(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(paused);
        let fd = core.history_event_fd().unwrap();
        let wait = || {
            let mut fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 1000) }, 1);
            assert_ne!(fd.revents & libc::POLLIN, 0);
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            core.poll_history(true).unwrap();
            while core.history_read_ready() && parser.has_queued() {
                parser.advance_queued(&mut core, &[]);
            }
            if !parser.has_queued() {
                core.selection_start(
                    Point { line: 2, column: 0 },
                    Side::Left,
                    SelectionType::Simple,
                );
                core.selection_update(Point { line: 2, column: 0 }, Side::Right);
                match core.copy_selection() {
                    Ok(receiver) => {
                        let copy = receiver
                            .recv_timeout(std::time::Duration::from_secs(1))
                            .unwrap()
                            .unwrap();
                        assert!(!copy.incomplete);
                        wait();
                        assert!(core.poll_history(true).unwrap());
                        break;
                    }
                    Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "history copy retry stalled"
            );
            wait();
        }
        assert_eq!(core.columns(), 6);
    }

    #[test]
    fn synchronized_ris_parses_in_order_with_bounded_disk_backpressure() {
        use crate::core::{Core, Event, Mode, WindowSize};
        use crate::vt::Parser;
        use std::time::{Duration, Instant};

        let budget = budget();
        let mut core = Core::new(
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        core.enable_history(HistoryConfig {
            budget: budget.clone(),
            hot_rows: 8,
        })
        .unwrap();
        let mut parser = Parser::new();
        // Pause archive writes until parsing fills its bounded queue.
        let mut paused = Some(budget.files.lock().unwrap());
        let input = format!(
            "\x1b[?2026h{}\x1b[3J\x1bc\x1b[?2026$p{}DONE\x1b[?2026l",
            "before\r\n".repeat(2000),
            "after\r\n".repeat(1000),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut committed = parser.advance_queued(&mut core, input.as_bytes());
        let mut backpressured = false;
        let mut reset = false;
        let mut replies = Vec::new();
        loop {
            if core.mode().contains(Mode::SYNC_UPDATE) {
                assert!(!committed);
            }
            for event in core.take_events() {
                match event {
                    Event::Title(None) => {
                        assert!(
                            committed,
                            "RIS must finish its batch before publishing the reset"
                        );
                        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
                        reset = true;
                    }
                    Event::Write(bytes) => replies.extend(bytes),
                    _ => (),
                }
            }
            if !parser.has_queued() {
                break;
            }
            core.poll_history(true).unwrap();
            if core.history_read_ready() {
                committed = parser.advance_queued(&mut core, &[]);
            } else {
                if paused.is_some() {
                    assert!(core.mode().contains(Mode::SYNC_UPDATE));
                    assert!(!reset);
                    assert!(core.history_size() < 1024);
                    backpressured = true;
                    paused.take();
                }
                std::thread::yield_now();
            }
            assert!(Instant::now() < deadline, "RIS parsing stalled");
        }
        assert!(backpressured);
        assert!(reset);
        assert_eq!(replies, b"\x1b[?2026;2$y");
        assert!(parser.deadline().is_none());
        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
        assert_eq!(core.history_size(), 998);
        assert_eq!(
            core.row(2).cells[..4]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "DONE"
        );

        // The preceding batch can leave a full history spool. Pump its replies
        // before the next frame, as the PTY loop does even without new output.
        let drain_queued = |core: &mut Core, parser: &mut Parser| {
            while parser.has_queued() {
                core.poll_history(true).unwrap();
                if core.history_read_ready() {
                    parser.advance_queued(core, &[]);
                } else {
                    let mut fd = libc::pollfd {
                        fd: core.history_event_fd().unwrap(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let remaining = crate::poll::timeout_millis(
                        deadline.saturating_duration_since(Instant::now()),
                    );
                    assert_eq!(unsafe { libc::poll(&mut fd, 1, remaining) }, 1);
                    assert_ne!(fd.revents & libc::POLLIN, 0);
                }
                assert!(Instant::now() < deadline, "next synchronized frame stalled");
            }
        };
        parser.advance_queued(&mut core, "\x1b[?2026h\r中NEXT".as_bytes());
        drain_queued(&mut core, &mut parser);
        assert_eq!(core.row(2).cells[0].c, '中');
        assert!(core.row(2).cells[1].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert!(core.mode().contains(Mode::SYNC_UPDATE));
        assert!(parser.deadline().is_some());
        parser.advance_queued(&mut core, b"\x1b[?2026l");
        drain_queued(&mut core, &mut parser);
        assert_eq!(
            core.row(2).cells[2..6]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "NEXT"
        );
        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
        assert!(parser.deadline().is_none());
    }

    #[test]
    fn search_preserves_wrapped_empty_rows_and_breaks_at_hard_empty_rows() {
        use crate::core::{Core, WindowSize};
        let mut core = Core::new(
            WindowSize {
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
        let blank = Row::from_parts(vec![Cell::default(); 80], true, 0, Vec::new()).unwrap();
        for hard_break in [false, true] {
            let mut rows = vec![first.clone()];
            rows.extend(std::iter::repeat_n(blank.clone(), 1000));
            rows[500].wrapped = !hard_break;
            rows.push(core.row(1).clone());
            let history = History::new(HistoryConfig::new(budget())).unwrap();
            history.append(0, rows.clone()).unwrap();
            for hot_start in [1001, 1002] {
                let result = history
                    .search(
                        "b界a\u{301}".into(),
                        (hot_start as u64, rows[hot_start..].to_vec()),
                        (80, false),
                        Default::default(),
                        crate::eventfd::new().unwrap(),
                    )
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
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
        let mut archive = Archive::new(budget());
        archive.append(0, &rows[..300]).unwrap();
        for kind in [
            SelectionType::Lines,
            SelectionType::Semantic,
            SelectionType::Object,
        ] {
            let mut copy = archive
                .copy((165, 5), (165, 6), 300, &rows[300..], Some(kind), false)
                .unwrap();
            assert!(!copy.incomplete);
            let mut text = String::new();
            copy.file.read_to_string(&mut text).unwrap();
            assert_eq!(text, expected);
        }
        archive.clear().unwrap();
    }

    #[test]
    fn paging_and_copy_preserve_mixed_rows_after_long_empty_prefixes() {
        let mut archive = Archive::new(budget());
        let blank = Row::from_parts(vec![Cell::default(); 80], false, 0, Vec::new()).unwrap();
        let mut rows = vec![blank; 2000];
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
        archive.append(0, &rows).unwrap();
        for start in [900, 1000, 1184, 1900] {
            let (actual, page) = archive.page(start, 32).unwrap();
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
            let mut copied = Vec::new();
            assert!(
                !archive
                    .write_copy(&mut copied, (start, 0), (end, 6), (2000, &[]), false, false)
                    .unwrap()
            );
            assert_eq!(copied, ("\n".repeat(10) + text).as_bytes());
        }
    }

    #[test]
    fn paging_and_copy_only_require_the_requested_prefix_to_be_readable() {
        let mut archive = Archive::new(budget());
        let rows = (0..4).map(row).collect::<Vec<_>>();
        archive.append(0, &rows).unwrap();
        let mut prefix = Vec::new();
        for row in &rows[..2] {
            write_row(&mut prefix, row);
        }
        OpenOptions::new()
            .write(true)
            .open(&archive.segments[0].path)
            .unwrap()
            .set_len(prefix.len() as u64)
            .unwrap();
        let (start, page) = archive.page(1, 1).unwrap();
        assert_eq!(start, 1);
        assert_eq!(page[0].cells, rows[1].cells);
        for (kind, expected) in [
            (SelectionType::Lines, "000001 ".to_owned() + &"x".repeat(65)),
            (SelectionType::Semantic, "x".repeat(65)),
        ] {
            let mut copy = archive
                .copy((1, 7), (1, 8), 4, &[], Some(kind), false)
                .unwrap();
            assert!(!copy.incomplete);
            let mut text = String::new();
            copy.file.read_to_string(&mut text).unwrap();
            assert_eq!(text, expected);
        }
        let mut copy = Vec::new();
        assert!(
            !archive
                .write_copy(
                    &mut copy,
                    (0, 0),
                    (1, rows[1].used()),
                    (4, &[]),
                    false,
                    false
                )
                .unwrap()
        );
        let expected = rows[..2]
            .iter()
            .map(|row| row.cells.iter().map(|cell| cell.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(String::from_utf8(copy).unwrap(), expected);
        let mut hot_copy = Vec::new();
        assert!(
            !archive
                .write_copy(
                    &mut hot_copy,
                    (3, 0),
                    (3, rows[3].used()),
                    (2, &rows[2..]),
                    false,
                    false,
                )
                .unwrap()
        );
        let expected: String = rows[3].cells.iter().map(|cell| cell.c).collect();
        assert_eq!(hot_copy, expected.as_bytes());
        assert_eq!(
            archive.page(2, 1).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            archive
                .write_copy(&mut Vec::new(), (0, 0), (2, 1), (4, &[]), false, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        archive.clear().unwrap();
    }
    #[test]
    fn unregister_distinguishes_segments_with_the_same_order() {
        let budget = budget();
        let mut first = Archive::new(budget.clone());
        let mut second = Archive::new(budget.clone());
        first.order_hint = Some(7);
        second.order_hint = Some(7);
        let row = Row::from_parts(
            vec![
                Cell {
                    c: 'x',
                    ..Cell::default()
                };
                1200
            ],
            false,
            1200,
            Vec::new(),
        )
        .unwrap();
        let rows = vec![row; BATCH];
        first.append(0, &rows).unwrap();
        second.append(0, &rows).unwrap();
        first.append(BATCH as u64, &rows).unwrap();
        let target = first.segments.pop_back().unwrap();
        assert_eq!(first.segments.len(), 1);
        budget.unregister(std::slice::from_ref(&target)).unwrap();
        assert!(!target.path.exists());
        let used = {
            let files = budget.files.lock().unwrap();
            assert_eq!(files.files.len(), 2);
            assert!(
                files
                    .files
                    .iter()
                    .any(|(path, _, _)| path == &first.segments[0].path)
            );
            assert!(
                files
                    .files
                    .iter()
                    .any(|(path, _, _)| path == &second.segments[0].path)
            );
            files.used
        };
        budget.unregister(std::slice::from_ref(&target)).unwrap();
        assert_eq!(budget.files.lock().unwrap().used, used);
        first.clear().unwrap();
        second.clear().unwrap();
        assert_eq!(budget.files.lock().unwrap().used, 0);
    }

    #[test]
    fn height_reflow_preserves_cold_files_and_recovers_the_required_tail() {
        let mut archive = Archive::new(budget());
        archive
            .append(0, &(0..320).map(row).collect::<Vec<_>>())
            .unwrap();
        let first = archive.first().unwrap();
        assert!(first > 0);
        let files = archive
            .segments
            .iter()
            .map(|segment| {
                let metadata = fs::metadata(&segment.path).unwrap();
                (
                    segment.path.clone(),
                    segment.end,
                    fs::read(&segment.path).unwrap(),
                    metadata.modified().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let Reply::Reflow {
            rows,
            hot_start,
            points,
            view_top,
            first: after_first,
        } = archive
            .reflow(Reflow {
                hot_start: 320,
                hot: (320..328).map(row).collect(),
                points: vec![(324, 3), (first, 4), (0, 1)],
                old_columns: 72,
                old_lines: 4,
                columns: 72,
                lines: 2,
                view_top: first,
                keep_rows: 8,
            })
            .unwrap()
        else {
            panic!("expected reflow reply")
        };
        assert_eq!(after_first, first);
        assert_eq!(hot_start, 320);
        assert_eq!(rows.len(), 6);
        assert_eq!(points, vec![Some((324, 3)), Some((first, 4)), None]);
        assert_eq!(view_top, first);
        for (path, _, bytes, modified) in &files {
            assert_eq!(fs::read(path).unwrap(), *bytes);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), *modified);
        }
        let Reply::Reflow {
            rows,
            hot_start,
            points,
            view_top,
            first: after_first,
        } = archive
            .reflow(Reflow {
                hot_start,
                hot: rows.into(),
                points: vec![(324, 3), (first, 4), (0, 1)],
                old_columns: 72,
                old_lines: 2,
                columns: 72,
                lines: 80,
                view_top,
                keep_rows: 80,
            })
            .unwrap()
        else {
            panic!("expected reflow reply")
        };
        assert_eq!(after_first, first);
        assert_eq!(hot_start, 246);
        assert_eq!(rows.len(), 80);
        assert_eq!(points, vec![Some((324, 3)), Some((first, 4)), None]);
        assert_eq!(view_top, first);
        for (index, actual) in rows.iter().enumerate() {
            assert_eq!(actual.cells, row(hot_start + index as u64).cells);
        }
        let mut preserved = 0;
        for (path, end, bytes, modified) in files {
            if end <= hot_start {
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
                preserved += 1;
            }
        }
        assert!(preserved > 0);
        archive.clear().unwrap();
    }
    #[test]
    fn pages_return_the_actual_start_after_another_pane_evicts_the_old_prefix() {
        let budget = budget();
        let mut first = Archive::new(budget.clone());
        let mut other = Archive::new(budget);
        let rows = (0..192).map(row).collect::<Vec<_>>();
        first.append(0, &rows).unwrap();
        other
            .append(0, &(0..128).map(row).collect::<Vec<_>>())
            .unwrap();
        let (actual, rows) = first.page(0, 192).unwrap();
        assert!(actual > 0);
        assert!(!rows.is_empty());
        for (offset, row) in rows.iter().enumerate() {
            let prefix = row
                .cells
                .iter()
                .take(6)
                .map(|cell| cell.c)
                .collect::<String>();
            assert_eq!(prefix.parse::<u64>().unwrap(), actual + offset as u64);
        }
        first.clear().unwrap();
        other.clear().unwrap();
    }
    struct PausedWriter {
        bytes: Vec<u8>,
        barrier: Arc<Barrier>,
        evicted: Arc<AtomicBool>,
        paused: bool,
    }
    impl Write for PausedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.paused {
                self.paused = true;
                self.barrier.wait();
                while !self.evicted.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn concurrent_eviction_marks_copy_incomplete_and_keeps_the_hot_suffix_in_order() {
        let budget = budget();
        let mut first = Archive::new(budget.clone());
        let rows = (0..192).map(row).collect::<Vec<_>>();
        first.append(0, &rows).unwrap();
        assert_eq!(first.first().unwrap(), 0);
        let barrier = Arc::new(Barrier::new(2));
        let evicted = Arc::new(AtomicBool::new(false));
        let flood_barrier = barrier.clone();
        let flood_evicted = evicted.clone();
        let thread = std::thread::spawn(move || {
            let mut other = Archive::new(budget);
            flood_barrier.wait();
            let batch = (0..192).map(row).collect::<Vec<_>>();
            for _ in 0..4 {
                other.append(other.end, &batch).unwrap();
            }
            flood_evicted.store(true, Ordering::Release);
            other
        });
        let mut writer = PausedWriter {
            bytes: Vec::new(),
            barrier,
            evicted,
            paused: false,
        };
        let hot = (192..196).map(row).collect::<Vec<_>>();
        let incomplete = first
            .write_copy(
                &mut writer,
                (0, 0),
                (195, hot[3].used()),
                (192, &hot),
                false,
                false,
            )
            .unwrap();
        assert!(incomplete);
        let values = String::from_utf8(writer.bytes)
            .unwrap()
            .lines()
            .map(|line| line[..6].parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(&values[values.len() - 4..], &[192, 193, 194, 195]);
        assert!(values.len() < 196);
        let mut other = thread.join().unwrap();
        first.clear().unwrap();
        other.clear().unwrap();
    }
}
