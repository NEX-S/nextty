//! Bounded, workspace-owned disk history. Only this worker performs archive I/O.
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use crate::core::{Cell, Extra, Flags, Row, SelectionType, object_selection, semantic_group};
use crate::eventfd::{consume_notification, notify};
use crate::vt::{Color, NamedColor, Rgb};

pub const HOT_ROWS: usize = 512;
const BATCH: usize = 32;
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
    open: Option<(PathBuf, File)>,
    files: VecDeque<(PathBuf, u64, u64)>,
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
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Arc::new(Self {
            directory,
            capacity,
            files: Mutex::new(BudgetFiles {
                open: None,
                files: VecDeque::new(),
                used: 0,
            }),
            generation: AtomicU64::new(0),
            notifications: Mutex::new(Vec::new()),
        }))
    }
    fn append_file(
        &self,
        path: &PathBuf,
        create: bool,
        data: &[u8],
        order: u64,
    ) -> io::Result<u64> {
        // Serialization with eviction prevents another pane from unlinking a segment
        // between opening it and publishing the appended rows.
        let mut files = self
            .files
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let previous = if create || files.open.as_ref().is_none_or(|(open, _)| open != path) {
            let file = if create {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?
            } else {
                OpenOptions::new().append(true).open(path)?
            };
            files.open.replace((path.clone(), file))
        } else {
            None
        };
        let file = &mut files.open.as_mut().unwrap().1;
        file.write_all(data)?;
        let metadata = file.metadata()?;
        if metadata.nlink() == 0 {
            files.open = None;
            return Err(io::ErrorKind::NotFound.into());
        }
        let bytes = (metadata.blocks() * 512).max(4096);
        let end = if files.files.back().is_some_and(|(_, _, age)| *age > order) {
            files.files.partition_point(|(_, _, age)| *age <= order)
        } else {
            files.files.len()
        };
        if !create
            && let Some(index) = files
                .files
                .range(..end)
                .rposition(|(existing, _, _)| existing == path)
        {
            let previous = files.files[index].1;
            files.files[index].1 = bytes;
            files.used = files.used - previous + bytes;
        } else {
            files.files.insert(end, (path.clone(), bytes, order));
            files.used += bytes;
        }
        let mut evicted = false;
        while files.used > self.capacity {
            let (path, bytes, _) = files.files.pop_front().unwrap();
            if files.open.as_ref().is_some_and(|(open, _)| open == &path) {
                files.open = None;
            }
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
            files.used -= bytes;
            evicted = true;
        }
        // Replaced handles can close without serializing other archive writers.
        drop(files);
        drop(previous);
        if evicted {
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
        Ok(metadata.len())
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
                .position(|(path, _, _)| path == &segment.path)
            {
                let (_, bytes, _) = files.files.remove(first + index).unwrap();
                removed = bytes;
            }
        } else {
            // Batch clears avoid shifting the file table once per removed segment.
            let paths: HashSet<_> = segments.iter().map(|segment| &segment.path).collect();
            files.files.retain(|(path, bytes, _)| {
                if paths.contains(path) {
                    removed += bytes;
                    false
                } else {
                    true
                }
            });
        }
        files.used -= removed;
        for segment in segments {
            if files
                .open
                .as_ref()
                .is_some_and(|(open, _)| open == &segment.path)
            {
                files.open = None;
            }
            match fs::remove_file(&segment.path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
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
    pub next: Option<HistoryCursor>,
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
    segments: Vec<(PathBuf, u64, u64)>,
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
    read: HistoryRead,
    segments: Vec<(PathBuf, u64, u64)>,
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
                loop {
                    let (lock, changed) = &*worker;
                    let mut state = lock.lock().unwrap();
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
                        SearchWork::Find(job) => {
                            let result = search_archive(&job);
                            if !job.cancelled.load(Ordering::Relaxed) {
                                let _ = job.sender.send(result);
                                if let Err(error) = notify(job.wake.as_raw_fd()) {
                                    eprintln!("notify pane search: {error}");
                                }
                            }
                        }
                        SearchWork::Read(job) => {
                            let result = read_archive(&job.read, &job.segments);
                            if !job.read.cancelled.load(Ordering::Relaxed) {
                                let _ = job.sender.send(result);
                                if let Err(error) = notify(job.wake.as_raw_fd()) {
                                    eprintln!("notify history read: {error}");
                                }
                            }
                        }
                    }
                    lock.lock().unwrap().active = None;
                }
            })?;
        Ok(Self(shared))
    }
    fn submit(&self, job: SearchWork) {
        let (lock, changed) = &*self.0;
        let mut state = lock.lock().unwrap();
        state
            .pending
            .retain(|job| !job.cancelled().load(Ordering::Relaxed));
        if !job.cancelled().load(Ordering::Relaxed) {
            state.pending.push_back(job);
            changed.notify_one();
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
        state.pending.clear();
        changed.notify_one();
    }
}
fn search_archive(job: &SearchJob) -> io::Result<crate::search::SearchResult> {
    let mut matcher = crate::search::Matcher::new(&job.query);
    let mut cells = Vec::new();
    for (path, start, end) in &job.segments {
        if job.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let mut reader = match File::open(path) {
            Ok(file) => BufReader::new(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                matcher.gap();
                continue;
            }
            Err(error) => return Err(error),
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
                    (count + 1, line_break || header[6] == 0)
                });
            if empty != 0 {
                if line_break {
                    matcher.line_break();
                }
                reader.consume(empty * 9);
                id += empty as u64;
                continue;
            }
            let row = read_row(&mut reader, cells)?;
            matcher.row(id, &row);
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
        end: u64,
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
    Read {
        read: HistoryRead,
        sender: mpsc::Sender<io::Result<HistoryPage>>,
    },
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
    Search(SearchJob),
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
        config
            .budget
            .notifications
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?
            .push(Arc::downgrade(&notification));
        let budget = config.budget.clone();
        let generation = budget.generation.load(Ordering::Acquire);
        let wake = notification.clone();
        let (sender, requests) = mpsc::sync_channel(8);
        let (responses, replies) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("history-io".into())
            .spawn(move || {
                let mut archive = Archive {
                    budget: config.budget,
                    segments: VecDeque::new(),
                    end: 0,
                    order_hint: None,
                    encoded: Vec::new(),
                };
                let mut search_worker: Option<SearchWorker> = None;
                while let Ok(request) = requests.recv() {
                    let result = match request {
                        Request::Read { read, sender } => {
                            let setup = archive.first().and_then(|_| {
                                if search_worker.is_none() {
                                    search_worker = Some(SearchWorker::new()?);
                                }
                                Ok(())
                            });
                            match setup {
                                Ok(()) => {
                                    let segments = archive
                                        .segments
                                        .iter()
                                        .filter(|segment| {
                                            segment.end > read.cursor.row
                                                && segment.start <= read.end.0
                                        })
                                        .map(|segment| {
                                            (segment.path.clone(), segment.start, segment.end)
                                        })
                                        .collect();
                                    search_worker.as_ref().unwrap().submit(SearchWork::Read(
                                        ReadJob {
                                            read,
                                            segments,
                                            sender,
                                            wake: Arc::clone(&wake),
                                        },
                                    ));
                                }
                                Err(error) => {
                                    let _ = sender.send(Err(error));
                                }
                            }
                            None
                        }
                        Request::Append { start, rows } => Some(
                            archive
                                .append(start, &rows)
                                .and_then(|()| archive.first())
                                .map(|first| Reply::Stored {
                                    first,
                                    end: archive.end,
                                }),
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
                        Request::Search(mut job) => {
                            if !job.cancelled.load(Ordering::Relaxed) {
                                if !job.alternate {
                                    job.segments = archive
                                        .segments
                                        .iter()
                                        .filter(|segment| segment.start < job.hot_start)
                                        .map(|segment| {
                                            (segment.path.clone(), segment.start, segment.end)
                                        })
                                        .collect();
                                }
                                if search_worker.is_none() {
                                    match SearchWorker::new() {
                                        Ok(worker) => search_worker = Some(worker),
                                        Err(error) => {
                                            let _ = job.sender.send(Err(error));
                                            let _ = notify(job.wake.as_raw_fd());
                                            continue;
                                        }
                                    }
                                }
                                search_worker
                                    .as_ref()
                                    .unwrap()
                                    .submit(SearchWork::Find(job));
                            }
                            None
                        }
                        Request::Clear => Some(archive.clear().map(|()| Reply::Cleared)),
                        Request::Status => Some(archive.first().map(|first| Reply::Stored {
                            first,
                            end: archive.end,
                        })),
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
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "history queue is full")
                }
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
        self.send(Request::Search(SearchJob {
            segments: Vec::new(),
            hot_start,
            hot,
            query,
            columns,
            alternate,
            cancelled,
            wake,
            sender,
        }))?;
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
        self.send(Request::Read { read, sender })?;
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
struct ReadText<'a> {
    read: &'a HistoryRead,
    cursor: HistoryCursor,
    text: String,
    carry: String,
    found: bool,
}
impl ReadText<'_> {
    fn append(&mut self, text: &str) -> usize {
        let mut count = (READ_LIMIT - self.text.len()).min(text.len());
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        self.text.push_str(&text[..count]);
        count
    }
    fn row(&mut self, id: u64, row: &Row) -> io::Result<bool> {
        if self.read.cancelled.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut bytes = Vec::new();
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
        write_text(&mut bytes, row, from, to, false)?;
        if id != self.read.end.0 && !row.wrapped {
            bytes.push(b'\n');
        }
        let rendered = String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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
        self.cursor.row = id;
        self.cursor.offset = offset as u64;
        let consumed = if self.found {
            self.append(input)
        } else {
            let prefix = self.carry.len();
            self.carry.push_str(input);
            let query = self.read.query.as_deref().unwrap();
            if let Some(found) = self.carry.find(query) {
                self.found = true;
                let matched = self.carry[found..].to_owned();
                self.carry.clear();
                (found.saturating_sub(prefix)
                    + self
                        .append(&matched)
                        .saturating_sub(prefix.saturating_sub(found)))
                .min(input.len())
            } else {
                let mut keep = self
                    .carry
                    .len()
                    .saturating_sub(query.len().saturating_sub(1));
                while !self.carry.is_char_boundary(keep) {
                    keep -= 1;
                }
                self.carry.drain(..keep);
                input.len()
            }
        };
        self.cursor.offset += consumed as u64;
        if consumed == input.len() {
            self.cursor.row = id + 1;
            self.cursor.offset = 0;
        }
        Ok(consumed < input.len() || self.text.len() == READ_LIMIT)
    }
}

fn read_archive(read: &HistoryRead, segments: &[(PathBuf, u64, u64)]) -> io::Result<HistoryPage> {
    let first = segments
        .first()
        .map_or(read.hot_start, |(_, start, _)| *start)
        .min(read.hot_start);
    let mut incomplete = read.incomplete || read.cursor.row < first;
    let cursor = if read.cursor.row < first {
        HistoryCursor {
            row: first,
            offset: 0,
            ..read.cursor
        }
    } else {
        read.cursor
    };
    let mut output = ReadText {
        read,
        cursor,
        text: String::new(),
        carry: String::new(),
        found: read.query.is_none(),
    };
    let mut cells = Vec::new();
    let mut full = false;
    for (path, start, end) in segments {
        if read.cancelled.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if *start >= read.hot_start || *start > read.end.0 {
            break;
        }
        if *start > output.cursor.row {
            incomplete = true;
            output.cursor.row = *start;
            output.cursor.offset = 0;
            output.carry.clear();
        }
        let mut file = match File::open(path) {
            Ok(file) => BufReader::new(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                incomplete = true;
                output.cursor.row = *end;
                output.cursor.offset = 0;
                output.carry.clear();
                continue;
            }
            Err(error) => return Err(error),
        };
        let from = output.cursor.row.max(*start);
        cells = skip_rows(&mut file, from - *start, cells)?;
        for id in from..(*end).min(read.hot_start) {
            if id > read.end.0 {
                break;
            }
            let row = read_row(&mut file, cells)?;
            full = output.row(id, &row)?;
            cells = row.cells;
            if full {
                break;
            }
        }
        if full {
            break;
        }
    }
    if !full && output.cursor.row < read.hot_start && output.cursor.row <= read.end.0 {
        incomplete = true;
        output.cursor.row = read.hot_start;
        output.cursor.offset = 0;
        output.carry.clear();
    }
    if !full {
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
            if output.row(id, row)? {
                break;
            }
        }
    }
    Ok(HistoryPage {
        text: output.text,
        next: (output.cursor.row <= read.end.0).then_some(output.cursor),
        incomplete,
        command: read.command.clone(),
    })
}

struct Segment {
    path: PathBuf,
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
}
impl Archive {
    fn first(&mut self) -> io::Result<u64> {
        while let Some(segment) = self.segments.front() {
            if segment.path.try_exists()? {
                break;
            }
            self.segments.pop_front();
        }
        Ok(self
            .segments
            .front()
            .map_or(self.end, |segment| segment.start))
    }
    fn append(&mut self, start: u64, rows: &[Row]) -> io::Result<()> {
        if start != self.end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "noncontiguous disk history",
            ));
        }
        for chunk in rows.chunks(BATCH) {
            self.encoded.clear();
            for row in chunk {
                write_row(&mut self.encoded, row)?;
            }
            let reuse = self
                .segments
                .back()
                .is_some_and(|segment| segment.bytes + self.encoded.len() as u64 <= SEGMENT_BYTES);
            let bytes = if reuse {
                match self.budget.append_file(
                    &self.segments.back().unwrap().path,
                    false,
                    &self.encoded,
                    self.segments.back().unwrap().order,
                ) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            if let Some(bytes) = bytes {
                let segment = self.segments.back_mut().unwrap();
                segment.bytes = bytes;
                segment.end += chunk.len() as u64;
            } else {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = self.budget.directory.join(format!("{id}.rows"));
                let order = self.order_hint.unwrap_or(id);
                let bytes = self.budget.append_file(&path, true, &self.encoded, order)?;
                self.segments.push_back(Segment {
                    path,
                    start: self.end,
                    end: self.end + chunk.len() as u64,
                    bytes,
                    order,
                });
            }
            self.end += chunk.len() as u64;
        }
        Ok(())
    }
    fn page(&mut self, start: u64, count: usize) -> io::Result<(u64, Vec<Row>)> {
        let first = self.first()?;
        let end = start.saturating_add(count as u64);
        let mut actual = start.max(first).min(end);
        let mut rows = Vec::with_capacity(count);
        let mut cells = Vec::new();
        let begin = self
            .segments
            .partition_point(|segment| segment.end <= actual);
        for segment in self.segments.range(begin..) {
            if segment.start >= end {
                break;
            }
            let mut file = match File::open(&segment.path) {
                Ok(file) => BufReader::new(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // Another pane can evict a not-yet-open segment. Keep only a
                    // contiguous suffix and retain its real row ID.
                    actual = segment.end.min(end);
                    rows.clear();
                    continue;
                }
                Err(error) => return Err(error),
            };
            let start = actual.max(segment.start);
            cells = skip_rows(&mut file, start - segment.start, cells)?;
            for _ in start..segment.end.min(end) {
                rows.push(read_row(&mut file, cells)?);
                cells = Vec::new();
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
    fn write_copy(
        &mut self,
        file: &mut impl Write,
        start: (u64, usize),
        end: (u64, usize),
        hot: (u64, &[Row]),
        mut incomplete: bool,
        skip_prompt: bool,
    ) -> io::Result<bool> {
        let (hot_start, hot) = hot;
        let first = self.first()?.min(hot_start);
        incomplete |= start.0 < first;
        let mut write = |id: u64, row: &Row| -> io::Result<()> {
            let mut from = if id == start.0 { start.1 } else { 0 };
            if from > 0
                && row
                    .cells
                    .get(from)
                    .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            {
                from -= 1;
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
            let mut reader = match File::open(&segment.path) {
                Ok(file) => BufReader::new(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    incomplete = true;
                    next = segment.end.min(hot_start);
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Keep the segment open while copying it. Repeated small pages would
            // decode the same prefix for every batch, especially for empty rows.
            let row_start = next.max(segment.start).min(hot_start);
            cells = skip_rows(&mut reader, row_start - segment.start, cells)?;
            for id in row_start..segment.end.min(hot_start) {
                if id > end.0 {
                    break;
                }
                let row = read_row(&mut reader, cells)?;
                write(id, &row)?;
                next = id + 1;
                cells = row.cells;
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
            write(next, row)?;
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
        let mut source_row = |id: u64| -> io::Result<Option<Cow<'_, Row>>> {
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
                let file = match File::open(&segment.path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error),
                };
                input = Some((
                    segment.start,
                    segment.end,
                    VecDeque::new(),
                    BufReader::new(file),
                ));
            }
            let (next, _, rows, file) = input.as_mut().unwrap();
            while *next <= id {
                let mut cells = if rows.len() == BATCH {
                    rows.pop_front().unwrap().cells
                } else {
                    Vec::new()
                };
                // Retain preceding rows for backward selection expansion.
                let skip = (id - *next).saturating_sub((BATCH - 1) as u64);
                if skip != 0 {
                    cells = skip_rows(file, skip, cells)?;
                    *next += skip;
                }
                rows.push_back(read_row(file, cells)?);
                *next += 1;
            }
            Ok(rows.back().cloned().map(Cow::Owned))
        };
        if kind == SelectionType::Object {
            if let Some([first, _]) = object_selection((start.0 as i64, start.1), |id| {
                if id < first as i64 {
                    Ok(None)
                } else {
                    source_row(id as u64)
                }
            })? {
                *start = (first.0 as u64, first.1);
            }
            if let Some([_, last]) =
                object_selection((end.0 as i64, end.1.saturating_sub(1)), |id| {
                    if id < first as i64 {
                        Ok(None)
                    } else {
                        source_row(id as u64)
                    }
                })?
            {
                let row = source_row(last.0 as u64)?.unwrap();
                *end = (
                    last.0 as u64,
                    last.1
                        + if row.cells[last.1].flags.contains(Flags::WIDE_CHAR) {
                            2
                        } else {
                            1
                        },
                );
            }
        } else if matches!(kind, SelectionType::Lines) {
            while start.0 > first && source_row(start.0 - 1)?.is_some_and(|row| row.wrapped) {
                start.0 -= 1;
            }
            while source_row(end.0)?.is_some_and(|row| row.wrapped)
                && end.0 + 1 < hot_start + hot.len() as u64
            {
                end.0 += 1;
            }
            start.1 = 0;
            end.1 = source_row(end.0)?.map_or(0, |row| row.cells.len());
        } else if matches!(kind, SelectionType::Semantic) {
            if let Some(mut row) = source_row(start.0)? {
                if start.1 > 0
                    && row
                        .cells
                        .get(start.1)
                        .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                {
                    start.1 -= 1;
                }
                let class = semantic_group(row.cells[start.1.min(row.cells.len() - 1)].c);
                loop {
                    let mut previous = *start;
                    if previous.1 > 0 {
                        previous.1 -= 1;
                    } else if previous.0 > first {
                        let Some(previous_row) = source_row(previous.0 - 1)? else {
                            break;
                        };
                        if !previous_row.wrapped {
                            break;
                        }
                        previous.0 -= 1;
                        previous.1 = previous_row.used().saturating_sub(1);
                        row = previous_row;
                    } else {
                        break;
                    }
                    if previous.1 > 0
                        && row.cells[previous.1]
                            .flags
                            .contains(Flags::WIDE_CHAR_SPACER)
                    {
                        previous.1 -= 1;
                    }
                    if semantic_group(row.cells[previous.1].c) != class {
                        break;
                    }
                    *start = previous;
                }
            }
            // The core supplies an inclusive semantic end converted to a half-open endpoint.
            if end.1 > 0 {
                end.1 -= 1;
            }
            if let Some(mut row) = source_row(end.0)? {
                end.1 = end.1.min(row.cells.len() - 1);
                if end.1 > 0 && row.cells[end.1].flags.contains(Flags::WIDE_CHAR_SPACER) {
                    end.1 -= 1;
                }
                let class = semantic_group(row.cells[end.1].c);
                loop {
                    let mut next = *end;
                    next.1 += if row.cells[end.1].flags.contains(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    };
                    if row.wrapped && next.1 >= row.used() {
                        let Some(next_row) = source_row(next.0 + 1)? else {
                            break;
                        };
                        if semantic_group(next_row.cells[0].c) != class {
                            break;
                        }
                        next.0 += 1;
                        next.1 = 0;
                        row = next_row;
                    } else {
                        if next.1 >= row.cells.len() || semantic_group(row.cells[next.1].c) != class
                        {
                            break;
                        }
                    }
                    *end = next;
                }
                end.1 += if row.cells[end.1].flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
            }
        }
        Ok(())
    }
    fn reflow(&mut self, mut request: Reflow) -> io::Result<Reply> {
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
                output.count = segment.end;
                output.archive.end = segment.end;
                output.archive.segments.push_back(segment);
            }
            while let Some(&(row, column, index)) = anchors.get(anchor)
                && row < output.count
            {
                output.points[index] = Some((row, column));
                anchor += 1;
            }
            if (first..output.count).contains(&request.view_top) {
                output.view_top = Some(request.view_top);
            }
        }
        let mut cells = Vec::new();
        while let Some(segment) = self.segments.pop_front() {
            let mut file = match File::open(&segment.path) {
                Ok(file) => BufReader::new(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            output.archive.order_hint = Some(segment.order);
            for id in segment.start..segment.end {
                let row = read_row(&mut file, cells)?;
                cells = output.row(id, row, &anchors, &mut anchor, &request)?;
            }
            drop(file);
            self.budget.unregister(std::slice::from_ref(&segment))?;
        }
        output.archive.order_hint = Some(NEXT.fetch_add(1, Ordering::Relaxed));
        let hot_len = request.hot.len();
        for (index, row) in std::mem::take(&mut request.hot).into_iter().enumerate() {
            output.row(
                request.hot_start + index as u64,
                row,
                &anchors,
                &mut anchor,
                &request,
            )?;
        }
        if output.column != 0 || output.last_wrapped {
            output.emit(false)?;
        }
        let cursor = output.points.first().copied().flatten().unwrap_or((0, 0)).0;
        if request.old_columns != request.columns {
            while output.tail.len() > request.lines
                && output.tail.back().is_some_and(|row| row.used() == 0)
                && output.count - 1 > cursor
            {
                output.tail.pop_back();
                output.count -= 1;
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
                output.count -= 1;
            }
        }
        let screen_start = output
            .count
            .saturating_sub(request.lines as u64)
            .min(cursor);
        while output.count > screen_start + request.lines as u64 {
            output.tail.pop_back();
            output.count -= 1;
        }
        while output.tail.len() < request.lines {
            output
                .tail
                .push_back(Row::new(request.columns, Cell::default()));
            output.count += 1;
        }
        output.flush()?;
        let hot_start = output.count - output.tail.len() as u64;
        let first = output.archive.first()?;
        let view_top = output.view_top.unwrap_or(first).max(first);
        output.archive.order_hint = None;
        *self = output.archive;
        Ok(Reply::Reflow {
            rows: output.tail,
            first,
            hot_start,
            points: output.points,
            view_top,
        })
    }
    fn clear(&mut self) -> io::Result<()> {
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
    row: Row,
    column: usize,
    count: u64,
    points: Vec<Option<(u64, usize)>>,
    view_top: Option<u64>,
    keep: usize,
    last_wrapped: bool,
}
impl ReflowOutput {
    fn new(budget: Arc<HistoryBudget>, request: &Reflow) -> Self {
        Self {
            archive: Archive {
                budget,
                segments: VecDeque::new(),
                end: 0,
                order_hint: None,
                encoded: Vec::new(),
            },
            tail: VecDeque::new(),
            pending: Vec::new(),
            row: Row::new(request.columns, Cell::default()),
            column: 0,
            count: 0,
            points: vec![None; request.points.len()],
            view_top: None,
            keep: request.keep_rows.max(request.lines),
            last_wrapped: false,
        }
    }
    fn emit(&mut self, wrapped: bool) -> io::Result<()> {
        let columns = self.row.cells.len();
        let mut row = std::mem::replace(&mut self.row, Row::new(columns, Cell::default()));
        row.wrapped = wrapped;
        self.column = 0;
        self.push_row(row)
    }
    fn push_row(&mut self, row: Row) -> io::Result<()> {
        self.tail.push_back(row);
        self.count += 1;
        while self.tail.len() > self.keep {
            self.pending.push(self.tail.pop_front().unwrap());
            if self.pending.len() == BATCH {
                self.flush()?;
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.archive.append(self.archive.end, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
    fn row(
        &mut self,
        id: u64,
        row: Row,
        anchors: &[(u64, usize, usize)],
        anchor: &mut usize,
        request: &Reflow,
    ) -> io::Result<Vec<Cell>> {
        if id == request.view_top {
            self.view_top = Some(self.count);
        }
        while anchors.get(*anchor).is_some_and(|(line, _, _)| *line < id) {
            *anchor += 1;
        }
        let begin = *anchor;
        while anchors.get(*anchor).is_some_and(|(line, _, _)| *line == id) {
            *anchor += 1;
        }
        let anchors = &anchors[begin..*anchor];
        if request.old_columns == request.columns {
            for (_, column, index) in anchors {
                self.points[*index] = Some((self.count, *column));
            }
            return self.push_row(row).map(|_| Vec::new());
        }
        let mut used = row.used();
        if !row.wrapped {
            for (_, column, index) in anchors {
                used = used.max(if *index < 2 && *column == request.old_columns {
                    *column
                } else {
                    column + 1
                });
            }
        }
        used = used.min(request.old_columns);
        let mut extras = row
            .extras
            .map(Arc::unwrap_or_clone)
            .unwrap_or_default()
            .into_iter()
            .peekable();
        for source in 0..used {
            let cell = row.cells[source];
            if (cell.flags.contains(Flags::WIDE_CHAR)
                && self.column + 1 == request.columns
                && request.columns > 1)
                || self.column == request.columns
            {
                self.emit(true)?;
            }
            for (_, column, index) in anchors {
                if (*column).min(if row.wrapped { row.used } else { used }) == source {
                    self.points[*index] = Some((self.count, self.column));
                }
            }
            let extra = if extras.peek().is_some_and(|(column, _)| *column == source) {
                Some(extras.next().unwrap().1)
            } else {
                None
            };
            if request.columns == 1 && cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            self.row.cells[self.column] = cell;
            if request.columns == 1 {
                self.row.cells[self.column].flags.remove(Flags::WIDE_CHAR);
            }
            if source < row.used {
                self.row.used = self.column + 1;
            }
            if let Some(extra) = extra {
                self.row.extras_mut().push((self.column, extra));
            }
            self.column += 1;
        }
        for (_, column, index) in anchors {
            if (*column).min(if row.wrapped { row.used } else { used }) == used {
                self.points[*index] = Some((self.count, self.column));
            }
        }
        self.last_wrapped = row.wrapped;
        if !row.wrapped {
            self.emit(false)?;
        }
        Ok(row.cells)
    }
}
use std::os::unix::fs::OpenOptionsExt;
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
    let mut push = |ch: char| -> io::Result<()> {
        if length + 4 > bytes.len() {
            file.write_all(&bytes[..length])?;
            length = 0;
        }
        length += ch.encode_utf8(&mut bytes[length..]).len();
        Ok(())
    };
    for (_, cell, extra) in row.cells_with_extras(start..end) {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            || (skip_prompt && cell.flags.contains(Flags::PROMPT))
        {
            continue;
        }
        push(cell.c)?;
        if let Some(extra) = extra {
            for &ch in &extra.zerowidth {
                push(ch)?;
            }
        }
    }
    file.write_all(&bytes[..length])
}
fn color_bytes(color: Color) -> [u8; 4] {
    match color {
        Color::Named(value) => {
            let n = (value as u16).to_le_bytes();
            [0, n[0], n[1], 0]
        }
        Color::Indexed(n) => [1, n, 0, 0],
        Color::Spec(c) => [2, c.r, c.g, c.b],
    }
}
fn read_color(bytes: [u8; 4]) -> io::Result<Color> {
    match bytes[0] {
        0 => {
            let n = u16::from_le_bytes([bytes[1], bytes[2]]);
            NamedColor::from_index(n)
                .map(Color::Named)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid history color"))
        }
        1 => Ok(Color::Indexed(bytes[1])),
        2 => Ok(Color::Spec(Rgb {
            r: bytes[1],
            g: bytes[2],
            b: bytes[3],
        })),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid history color",
        )),
    }
}
fn write_row(file: &mut impl Write, row: &Row) -> io::Result<()> {
    let stored = if row.used() == row.cells.len() {
        row.used()
    } else {
        row.non_default_len().max(row.used())
    };
    file.write_all(&(row.cells.len() as u16).to_le_bytes())?;
    file.write_all(&(row.used() as u16).to_le_bytes())?;
    file.write_all(&(stored as u16).to_le_bytes())?;
    file.write_all(&[u8::from(row.wrapped)])?;
    file.write_all(&(row.extras().len() as u16).to_le_bytes())?;
    for cells in row.cells[..stored].chunks(64) {
        let mut bytes = [0; 14 * 64];
        for (cell, bytes) in cells.iter().zip(bytes.chunks_exact_mut(14)) {
            bytes[..4].copy_from_slice(&(cell.c as u32).to_le_bytes());
            bytes[4..8].copy_from_slice(&color_bytes(cell.fg));
            bytes[8..12].copy_from_slice(&color_bytes(cell.bg));
            bytes[12..].copy_from_slice(&cell.flags.bits().to_le_bytes());
        }
        file.write_all(&bytes[..cells.len() * 14])?;
    }
    for (column, extra) in row.extras() {
        file.write_all(&(*column as u16).to_le_bytes())?;
        file.write_all(&(extra.zerowidth.len() as u32).to_le_bytes())?;
        for c in &extra.zerowidth {
            file.write_all(&(*c as u32).to_le_bytes())?;
        }
        file.write_all(&[u8::from(extra.underline_color.is_some())])?;
        if let Some(color) = extra.underline_color {
            file.write_all(&color_bytes(color))?;
        }
    }
    Ok(())
}
fn empty_row_headers(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Empty rows have only a header; avoid materializing their cells.
    bytes.chunks_exact(9).take_while(|header| {
        header[2..6] == [0; 4] && header[7..] == [0; 2] && header[..2] != [0, 0]
    })
}
fn skip_rows(
    file: &mut BufReader<File>,
    mut count: u64,
    mut cells: Vec<Cell>,
) -> io::Result<Vec<Cell>> {
    while count != 0 {
        let empty = empty_row_headers(file.fill_buf()?)
            .take(count as usize)
            .count();
        if empty == 0 {
            cells = read_row(file, cells)?.cells;
            count -= 1;
        } else {
            file.consume(empty * 9);
            count -= empty as u64;
        }
    }
    Ok(cells)
}
fn read_array<const N: usize>(file: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn read_row(file: &mut impl Read, mut cells: Vec<Cell>) -> io::Result<Row> {
    let header = read_array::<9>(file)?;
    let columns = u16::from_le_bytes([header[0], header[1]]) as usize;
    let used = u16::from_le_bytes([header[2], header[3]]) as usize;
    let stored = u16::from_le_bytes([header[4], header[5]]) as usize;
    let wrapped = header[6] != 0;
    let count = u16::from_le_bytes([header[7], header[8]]) as usize;
    if columns == 0 || used > stored || stored > columns {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid history row",
        ));
    }
    cells.truncate(stored);
    cells.resize(columns, Cell::default());
    for cells in cells[..stored].chunks_mut(64) {
        let mut bytes = [0; 14 * 64];
        file.read_exact(&mut bytes[..cells.len() * 14])?;
        for (cell, bytes) in cells.iter_mut().zip(bytes.chunks_exact(14)) {
            cell.c = char::from_u32(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid history character")
                })?;
            cell.fg = read_color(bytes[4..8].try_into().unwrap())?;
            cell.bg = read_color(bytes[8..12].try_into().unwrap())?;
            cell.flags = Flags::from_bits(u16::from_le_bytes(bytes[12..].try_into().unwrap()))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid history flags")
                })?;
        }
    }
    let mut extras = Vec::with_capacity(count);
    for _ in 0..count {
        let column = u16::from_le_bytes(read_array(file)?) as usize;
        let count = u32::from_le_bytes(read_array(file)?) as usize;
        let mut zerowidth = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            zerowidth.push(
                char::from_u32(u32::from_le_bytes(read_array(file)?)).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid history combining character",
                    )
                })?,
            );
        }
        let underline_color = if read_array::<1>(file)?[0] == 0 {
            None
        } else {
            Some(read_color(read_array(file)?)?)
        };
        extras.push((
            column,
            Extra {
                zerowidth,
                underline_color,
            },
        ));
    }
    Row::from_parts(cells, wrapped, used, extras)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid history row"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_keeps_unicode_matches_across_the_cold_hot_boundary() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut archive = Archive {
            budget: HistoryBudget::new(directory.clone(), 1024 * 1024).unwrap(),
            segments: VecDeque::new(),
            end: 0,
            order_hint: None,
            encoded: Vec::new(),
        };
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
            result.matches,
            vec![crate::search::SearchMatch {
                start: (0, 2),
                end: (1, 1)
            }]
        );
        assert!(!result.truncated);
        drop(worker);
        archive.clear().unwrap();
        drop(archive);
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
        styled[8].bg = Color::Indexed(17);
        let rows = [
            Row::from_parts(
                vec![
                    Cell {
                        c: '界',
                        ..Cell::default()
                    };
                    100
                ],
                false,
                100,
                Vec::new(),
            )
            .unwrap(),
            Row::from_parts(vec![Cell::default(); 100], false, 0, Vec::new()).unwrap(),
            Row::from_parts(vec![Cell::default(); 3], false, 0, Vec::new()).unwrap(),
            Row::from_parts(styled, false, 0, Vec::new()).unwrap(),
        ];
        let mut bytes = Vec::new();
        for row in &rows {
            write_row(&mut bytes, row).unwrap();
        }
        let mut reader = bytes.as_slice();
        let mut cells = Vec::new();
        for expected in rows {
            let decoded = read_row(&mut reader, cells).unwrap();
            assert_eq!(decoded.cells, expected.cells);
            assert_eq!(decoded.used(), expected.used());
            assert_eq!(decoded.wrapped, expected.wrapped);
            cells = decoded.cells;
        }
        assert!(reader.is_empty());
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
        let mut archive = Archive {
            budget: budget.clone(),
            segments: VecDeque::new(),
            end: 0,
            order_hint: None,
            encoded: Vec::new(),
        };
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
    fn archive(budget: Arc<HistoryBudget>) -> Archive {
        Archive {
            budget,
            segments: VecDeque::new(),
            end: 0,
            order_hint: None,
            encoded: Vec::new(),
        }
    }
    fn row(id: u64) -> Row {
        let value = format!("{id:06} {}", "x".repeat(65));
        let cells = value
            .chars()
            .map(|c| Cell {
                c,
                ..Cell::default()
            })
            .collect::<Vec<_>>();
        let used = cells.len();
        Row::from_parts(cells, false, used, Vec::new()).unwrap()
    }

    #[test]
    fn removed_cached_segment_is_replaced_before_publishing_more_rows() {
        let mut archive = archive(budget());
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
            let mut archive = archive(budget.clone());
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
                let event = terminal
                    .events
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                match event {
                    Event::Error(error) => panic!("{error}"),
                    Event::Exit(_) if exited => break,
                    Event::Wakeup => {
                        terminal.acknowledge_wakeup();
                        let core = terminal.core.lock().unwrap();
                        if !exited && core.row(2).cells[0].c == 'R' {
                            break;
                        }
                    }
                    _ => (),
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
    fn synchronized_ris_keeps_the_replay_guard_through_disk_backpressure() {
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
        // Pause archive writes until the replay has filled its bounded queue.
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
            if !committed && parser.has_queued() {
                assert!(core.mode().contains(Mode::SYNC_UPDATE));
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
                    Event::HistoryCleared => {
                        assert!(committed || core.mode().contains(Mode::SYNC_UPDATE));
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
                    backpressured = true;
                    paused.take();
                }
                std::thread::yield_now();
            }
            assert!(Instant::now() < deadline, "RIS replay stalled");
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

        // The preceding replay can leave a full history spool. Pump its replies
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
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .saturating_add(1) as i32;
                    assert_eq!(unsafe { libc::poll(&mut fd, 1, remaining) }, 1);
                    assert_ne!(fd.revents & libc::POLLIN, 0);
                }
                assert!(Instant::now() < deadline, "next synchronized frame stalled");
            }
        };
        parser.advance_queued(&mut core, b"\x1b[?2026h\rNEXT");
        drain_queued(&mut core, &mut parser);
        assert_eq!(core.row(2).cells[0].c, 'D');
        parser.advance_queued(&mut core, b"\x1b[?2026l");
        drain_queued(&mut core, &mut parser);
        assert_eq!(
            core.row(2).cells[..4]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "NEXT"
        );
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
                        result.matches,
                        vec![crate::search::SearchMatch {
                            start: (0, 1),
                            end: (1001, 3),
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
        let mut archive = archive(budget());
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
        let mut archive = archive(budget());
        let blank = Row::from_parts(vec![Cell::default(); 80], false, 0, Vec::new()).unwrap();
        let mut rows = vec![blank; 2000];
        rows[910] = row(910);
        rows[1000] = row(1000);
        rows[1000].extras_mut().push((
            0,
            Extra {
                zerowidth: vec!['\u{301}'],
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
        let mut archive = archive(budget());
        let rows = (0..4).map(row).collect::<Vec<_>>();
        archive.append(0, &rows).unwrap();
        let mut prefix = Vec::new();
        for row in &rows[..2] {
            write_row(&mut prefix, row).unwrap();
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
        let mut first = archive(budget.clone());
        let mut second = archive(budget.clone());
        first.order_hint = Some(7);
        second.order_hint = Some(7);
        let row = Row::from_parts(
            vec![
                Cell {
                    c: 'x',
                    ..Cell::default()
                };
                100
            ],
            false,
            100,
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
        let mut archive = archive(budget());
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
        let mut first = archive(budget.clone());
        let mut other = archive(budget);
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
        let mut first = archive(budget.clone());
        let rows = (0..192).map(row).collect::<Vec<_>>();
        first.append(0, &rows).unwrap();
        assert_eq!(first.first().unwrap(), 0);
        let barrier = Arc::new(Barrier::new(2));
        let evicted = Arc::new(AtomicBool::new(false));
        let flood_barrier = barrier.clone();
        let flood_evicted = evicted.clone();
        let thread = std::thread::spawn(move || {
            let mut other = archive(budget);
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
