//! Shared asynchronous file navigation for terminal front ends.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions, ReadDir};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const SEARCH_LIMIT: usize = 200;
pub const PREVIEW_BYTE_LIMIT: usize = 256 * 1024;
pub const PREVIEW_LINE_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileFocus {
    List,
    Preview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

impl FileEntry {
    pub fn display_name(&self) -> Cow<'_, str> {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
    }

    pub fn display_path(&self, directory: &Path) -> Cow<'_, str> {
        self.path
            .strip_prefix(directory)
            .unwrap_or(&self.path)
            .to_string_lossy()
    }
}

pub struct FileNav {
    wakeup: Arc<OwnedFd>,
    directory: PathBuf,
    expanded: BTreeSet<PathBuf>,
    query: Option<String>,
    scan: Scan,
    selection: usize,
    generation: Arc<AtomicU64>,
    scan_pending: bool,
    worker: Worker<ScanRequest, (u64, Scan, bool)>,
    restore_selection: Option<PathBuf>,
    directory_selection: BTreeMap<PathBuf, PathBuf>,
    focus: FileFocus,
    preview_path: Option<PathBuf>,
    preview: Preview,
    preview_scroll: usize,
    preview_generation: Arc<AtomicU64>,
    preview_pending: bool,
    preview_worker: Worker<(u64, PathBuf, Preview), (u64, Preview)>,
}

#[derive(Default)]
struct Preview {
    lines: Box<[String]>,
    error: Option<io::Error>,
    truncated: bool,
}

struct ScanRequest {
    generation: u64,
    directory: PathBuf,
    query: Option<String>,
    expanded: BTreeSet<PathBuf>,
    previous: (Scan, Preview),
}

struct Work<Request, Output> {
    pending: Option<Request>,
    result: Option<Output>,
    closed: bool,
}

struct Worker<Request, Output> {
    shared: Arc<(Mutex<Work<Request, Output>>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl<Request: Send + 'static, Output: Send + 'static> Worker<Request, Output> {
    fn new(
        name: &str,
        wakeup: Arc<OwnedFd>,
        run: impl Fn(Request, &dyn Fn(Output)) + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new((
            Mutex::new(Work {
                pending: None,
                result: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let worker = Arc::clone(&shared);
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            let (lock, changed) = &*worker;
            let _exit = WakeOnDrop(|| {
                lock.lock().unwrap().closed = true;
                notify(&wakeup);
            });
            let publish = |result| {
                let previous = lock.lock().unwrap().result.replace(result);
                drop(previous);
                notify(&wakeup);
            };
            loop {
                let mut state = lock.lock().unwrap();
                while state.pending.is_none() && !state.closed {
                    state = changed.wait(state).unwrap();
                }
                if state.closed {
                    return;
                }
                let request = state.pending.take().unwrap();
                drop(state);
                run(request, &publish);
            }
        })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    fn submit(&self, request: Request) {
        let (lock, changed) = &*self.shared;
        let previous = lock.lock().unwrap().pending.replace(request);
        if previous.is_none() {
            changed.notify_one();
        }
        drop(previous);
    }

    fn take(&mut self) -> Option<Output> {
        let mut state = self.shared.0.lock().unwrap();
        if !state.closed {
            return state.result.take();
        }
        drop(state);
        self.handle
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        unreachable!("file worker exited while its navigator is alive");
    }
}

impl<Request, Output> Drop for Worker<Request, Output> {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        changed.notify_one();
    }
}

struct WakeOnDrop<Finish: Fn()>(Finish);

impl<Finish: Fn()> Drop for WakeOnDrop<Finish> {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn notify(wakeup: &OwnedFd) {
    if let Err(error) = crate::eventfd::notify(wakeup.as_raw_fd()) {
        eprintln!("notify file navigation: {error}");
    }
}

#[derive(Default)]
struct Scan {
    entries: Vec<FileEntry>,
    error: Option<io::Error>,
    truncated: bool,
}

impl FileNav {
    /// Starts loading a directory without waiting for filesystem traversal.
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        let directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        let wakeup = Arc::new(crate::eventfd::new()?);
        let generation = Arc::new(AtomicU64::new(0));
        let preview_generation = Arc::new(AtomicU64::new(0));
        let cancellation = Arc::clone(&generation);
        let worker = Worker::new(
            "nextty-files",
            Arc::clone(&wakeup),
            move |request: ScanRequest, publish| {
                drop(request.previous);
                let cancelled = || cancellation.load(Ordering::Relaxed) != request.generation;
                let result = scan(
                    &request.directory,
                    request.query.as_deref(),
                    &request.expanded,
                    &cancelled,
                    &|scan| {
                        publish((request.generation, scan, false));
                    },
                );
                publish((request.generation, result, true));
            },
        )?;
        let cancellation = Arc::clone(&preview_generation);
        let preview_worker = Worker::new(
            "nextty-preview",
            Arc::clone(&wakeup),
            move |(generation, path, previous): (u64, PathBuf, Preview), publish| {
                drop(previous);
                let cancelled = || cancellation.load(Ordering::Relaxed) != generation;
                let result = read_preview(&path, &cancelled).unwrap_or_else(|error| Preview {
                    error: Some(error),
                    ..Preview::default()
                });
                publish((generation, result));
            },
        )?;
        let mut nav = Self {
            wakeup,
            directory,
            expanded: BTreeSet::new(),
            query: None,
            scan: Scan::default(),
            selection: 0,
            generation,
            scan_pending: false,
            worker,
            restore_selection: None,
            directory_selection: BTreeMap::new(),
            focus: FileFocus::List,
            preview_path: None,
            preview: Preview::default(),
            preview_scroll: 0,
            preview_generation,
            preview_pending: false,
            preview_worker,
        };
        nav.request_scan();
        Ok(nav)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn fd(&self) -> RawFd {
        self.wakeup.as_raw_fd()
    }

    pub fn drain_notification(&self) -> io::Result<()> {
        crate::eventfd::consume_notification(self.fd()).map(|_| ())
    }

    pub fn query(&self) -> &str {
        self.query.as_deref().unwrap_or("")
    }

    pub fn is_search(&self) -> bool {
        self.query.is_some()
    }

    pub fn entries(&self) -> &[FileEntry] {
        &self.scan.entries
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    pub fn selected(&self) -> Option<&FileEntry> {
        self.scan.entries.get(self.selection)
    }

    /// Whether a directory load, recursive search or preview is still running.
    pub fn searching(&self) -> bool {
        self.scan_pending || self.preview_loading()
    }

    pub fn focus(&self) -> FileFocus {
        self.focus
    }

    pub fn focus_preview(&mut self) -> bool {
        if self.selected().is_some_and(|entry| !entry.is_dir) {
            self.focus = FileFocus::Preview;
            self.update_preview();
            true
        } else {
            false
        }
    }

    pub fn focus_list(&mut self) {
        self.focus = FileFocus::List;
        self.update_preview();
    }

    pub fn preview_lines(&self) -> &[String] {
        &self.preview.lines
    }

    pub fn preview_scroll(&self) -> usize {
        self.preview_scroll
    }

    pub fn scroll_preview(&mut self, delta: isize, visible_rows: usize) {
        self.preview_scroll = self
            .preview_scroll
            .saturating_add_signed(delta)
            .min(self.preview.lines.len().saturating_sub(visible_rows.max(1)));
    }

    pub fn preview_loading(&self) -> bool {
        self.preview_pending
    }

    pub fn preview_truncated(&self) -> bool {
        self.preview.truncated
    }

    pub fn preview_error(&self) -> Option<&io::Error> {
        self.preview.error.as_ref()
    }

    /// Search retains the best SEARCH_LIMIT matches and still scans for better ones.
    pub fn truncated(&self) -> bool {
        self.scan.truncated
    }

    /// Partial results remain usable when an inaccessible descendant is encountered.
    pub fn error(&self) -> Option<&io::Error> {
        self.scan.error.as_ref()
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selection = self
            .selection
            .saturating_add_signed(delta)
            .min(self.scan.entries.len().saturating_sub(1));
        self.restore_selection = None;
        self.update_preview();
    }

    pub fn start_search(&mut self) {
        self.set_query(String::new());
    }

    pub fn set_query(&mut self, query: String) {
        if self.query.as_ref() != Some(&query) {
            if self.query.is_none() {
                self.remember_selection();
            }
            self.query = Some(query);
            self.restore_selection = None;
            self.request_scan();
        }
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.focus_list();
        match &mut self.query {
            Some(query) if !text.is_empty() => {
                query.push_str(text);
                self.restore_selection = None;
                self.request_scan();
            }
            None => self.set_query(text.to_owned()),
            _ => (),
        }
    }

    pub(crate) fn backspace_query(&mut self) {
        if self.query.as_mut().unwrap().pop().is_some() {
            self.restore_selection = None;
            self.request_scan();
        }
    }

    pub fn stop_search(&mut self) {
        if self.query.take().is_some() {
            self.restore_selection = self.directory_selection.get(&self.directory).cloned();
            self.request_scan();
        }
    }

    pub fn refresh(&mut self) {
        self.restore_selection = self.selected().map(|entry| entry.path.clone());
        self.request_scan();
    }

    pub fn set_directory(&mut self, directory: PathBuf) -> io::Result<()> {
        self.remember_selection();
        self.directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        self.query = None;
        self.expanded.clear();
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
        Ok(())
    }

    pub fn parent(&mut self) -> bool {
        if let Some(entry) = self.scan.entries.get(self.selection) {
            let path = &entry.path;
            if self.expanded.remove(path) {
                self.restore_selection = Some(path.clone());
                self.request_scan();
                return true;
            }
            if let Some(parent) = path.parent().filter(|parent| *parent != self.directory)
                && let Some(index) = self
                    .scan
                    .entries
                    .iter()
                    .position(|entry| entry.path == parent)
            {
                self.selection = index;
                return true;
            }
        }
        let Some(parent) = self.directory.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.restore_selection = Some(std::mem::replace(&mut self.directory, parent));
        self.expanded.clear();
        self.query = None;
        self.request_scan();
        true
    }

    pub fn enter_selected(&mut self) -> bool {
        let Some(entry) = self.selected().filter(|entry| entry.is_dir) else {
            return false;
        };
        let path = &entry.path;
        if self.expanded.contains(path) && !self.is_search() {
            if self
                .scan
                .entries
                .get(self.selection + 1)
                .is_some_and(|entry| entry.path.parent() == Some(path.as_path()))
            {
                self.move_selection(1);
            }
            return true;
        }
        let path = path.clone();
        for directory in path
            .ancestors()
            .take_while(|directory| *directory != self.directory)
        {
            self.expanded.insert(directory.to_path_buf());
        }
        self.query = None;
        self.restore_selection = Some(path);
        self.request_scan();
        true
    }

    pub fn entry_label<'a>(&self, entry: &'a FileEntry) -> Cow<'a, str> {
        if self.is_search() {
            return entry.display_path(&self.directory);
        }
        let depth = entry
            .path
            .strip_prefix(&self.directory)
            .unwrap()
            .components()
            .count()
            .saturating_sub(1);
        let marker = if entry.is_dir {
            if self.expanded.contains(&entry.path) {
                ""
            } else {
                ""
            }
        } else {
            " "
        };
        Cow::Owned(format!(
            "{:indent$}{marker} {}",
            "",
            entry.display_name(),
            indent = depth * 2,
        ))
    }

    /// Receives the latest available results without waiting for filesystem work.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some((generation, scan, complete)) = self.worker.take()
            && generation == self.generation.load(Ordering::Relaxed)
        {
            self.publish_scan(scan);
            if complete {
                self.restore_selection = None;
                self.scan_pending = false;
            }
            changed = true;
        }
        self.update_preview();
        if let Some((generation, preview)) = self.preview_worker.take()
            && generation == self.preview_generation.load(Ordering::Relaxed)
        {
            self.preview = preview;
            self.preview_pending = false;
            changed = true;
        }
        changed
    }

    fn remember_selection(&mut self) {
        if self.is_search() {
            return;
        }
        if let Some(entry) = self.selected() {
            self.directory_selection
                .insert(self.directory.clone(), entry.path.clone());
        }
    }

    fn publish_scan(&mut self, scan: Scan) {
        let selected = self
            .restore_selection
            .as_deref()
            .or_else(|| self.selected().map(|entry| entry.path.as_path()));
        self.selection = selected
            .and_then(|path| scan.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selection.min(scan.entries.len().saturating_sub(1)));
        self.scan = scan;
    }

    fn update_preview(&mut self) {
        let path = (self.focus == FileFocus::Preview)
            .then(|| self.selected().map(|entry| &entry.path))
            .flatten();
        if self.preview_path.as_ref() == path {
            return;
        }
        self.preview_path = path.cloned();
        let previous = std::mem::take(&mut self.preview);
        self.preview_scroll = 0;
        let generation = self.preview_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.preview_pending = self.preview_path.is_some();
        if let Some(path) = self.preview_path.clone() {
            self.preview_worker.submit((generation, path, previous));
        }
    }

    fn request_scan(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = (
            std::mem::take(&mut self.scan),
            std::mem::take(&mut self.preview),
        );
        self.selection = 0;
        self.focus = FileFocus::List;
        self.update_preview();
        self.scan_pending = true;
        self.worker.submit(ScanRequest {
            generation,
            directory: self.directory.clone(),
            query: self.query.clone(),
            expanded: self.expanded.clone(),
            previous,
        });
    }
}

impl Drop for FileNav {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.preview_generation.fetch_add(1, Ordering::Relaxed);
        // A remote filesystem syscall may not return promptly. Cancellation owns
        // the workers' remaining lifetime without making the frontend wait for it.
    }
}

fn read_preview(path: &Path, cancelled: &impl Fn() -> bool) -> io::Result<Preview> {
    if cancelled() {
        return Ok(Preview::default());
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Ok(Preview {
            lines: Box::new(["Special file; preview unavailable".into()]),
            ..Preview::default()
        });
    }
    // NONBLOCK closes the metadata/open race with a file replaced by a FIFO.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Preview {
            lines: Box::new(["Special file; preview unavailable".into()]),
            ..Preview::default()
        });
    }
    let mut bytes =
        Vec::with_capacity(metadata.len().min((PREVIEW_BYTE_LIMIT + 1) as u64) as usize);
    while bytes.len() <= PREVIEW_BYTE_LIMIT {
        if cancelled() {
            return Ok(Preview::default());
        }
        let remaining = (PREVIEW_BYTE_LIMIT + 1 - bytes.len()).min(8192);
        let count = file
            .by_ref()
            .take(remaining as u64)
            .read_to_end(&mut bytes)?;
        if count < remaining {
            break;
        }
    }
    let mut truncated = bytes.len() > PREVIEW_BYTE_LIMIT;
    bytes.truncate(PREVIEW_BYTE_LIMIT);
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => Some(text),
        Err(error) if truncated && error.error_len().is_none() => {
            Some(std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap())
        }
        _ => None,
    };
    let Some(text) = text.filter(|text| memchr::memchr(0, text.as_bytes()).is_none()) else {
        return Ok(Preview {
            lines: Box::new(["Binary or non-UTF-8 file; preview unavailable".into()]),
            ..Preview::default()
        });
    };
    let mut lines = Vec::new();
    let mut ends = memchr::memchr_iter(b'\n', text.as_bytes());
    let mut offset = 0;
    while offset < text.len() {
        let end = ends.next().unwrap_or(text.len());
        let line = &text[offset..end];
        offset = end + 1;
        if lines.len() == PREVIEW_LINE_LIMIT {
            truncated = true;
            break;
        }
        lines.push(preview_text(line.strip_suffix('\r').unwrap_or(line)));
    }
    Ok(Preview {
        lines: lines.into_boxed_slice(),
        truncated,
        error: None,
    })
}

fn preview_text(text: &str) -> String {
    let Some(first) = text.bytes().position(|byte| !(b' '..=b'~').contains(&byte)) else {
        return text.to_owned();
    };
    let mut result = String::with_capacity(text.len());
    let mut start = 0;
    // The printable ASCII prefix makes first a UTF-8 boundary.
    for (index, ch) in text[first..]
        .char_indices()
        .filter(|(_, ch)| ch.is_control())
    {
        let index = first + index;
        result.push_str(&text[start..index]);
        match ch {
            '\t' => result.push_str("    "),
            ch => {
                // C0 and C1 controls fit in two hexadecimal digits.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let value = ch as usize;
                result.push_str("\\u{");
                if value >= 16 {
                    result.push(char::from(HEX[value >> 4]));
                }
                result.push(char::from(HEX[value & 15]));
                result.push('}');
            }
        }
        start = index + ch.len_utf8();
    }
    result.push_str(&text[start..]);
    result
}

fn scan(
    directory: &Path,
    query: Option<&str>,
    expanded: &BTreeSet<PathBuf>,
    cancelled: &impl Fn() -> bool,
    publish: &impl Fn(Scan),
) -> Scan {
    if cancelled() {
        return Scan::default();
    }
    let root = match fs::read_dir(directory) {
        Ok(root) => root,
        Err(error) => {
            return Scan {
                error: Some(error),
                ..Scan::default()
            };
        }
    };
    match query {
        None if expanded.is_empty() => list_directory(root, cancelled),
        None => {
            let mut result = list_directory(root, cancelled);
            let entries = std::mem::take(&mut result.entries);
            for entry in entries {
                let children = if entry.is_dir && expanded.contains(&entry.path) {
                    Some(scan(&entry.path, None, expanded, cancelled, publish))
                } else {
                    None
                };
                result.entries.push(entry);
                if let Some(mut children) = children {
                    result.entries.append(&mut children.entries);
                    if result.error.is_none() {
                        result.error = children.error;
                    }
                }
                if cancelled() {
                    return Scan::default();
                }
            }
            result
        }
        Some(query) => search_directory(root, directory, query, cancelled, publish),
    }
}

fn list_directory(directory: ReadDir, cancelled: &impl Fn() -> bool) -> Scan {
    let mut scan = Scan::default();
    for entry in directory {
        if cancelled() {
            return Scan::default();
        }
        let (path, kind) = match entry.and_then(|entry| Ok((entry.path(), entry.file_type()?))) {
            Ok(entry) => entry,
            Err(error) => {
                scan.error.get_or_insert(error);
                continue;
            }
        };
        let is_dir = if kind.is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) => {
                    scan.error.get_or_insert(error);
                    false
                }
            }
        } else {
            kind.is_dir()
        };
        scan.entries.push(FileEntry { path, is_dir });
    }
    scan.entries.sort_unstable_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.path.as_os_str().cmp(b.path.as_os_str()))
    });
    scan
}

fn search_directory(
    root: ReadDir,
    directory: &Path,
    query: &str,
    cancelled: &impl Fn() -> bool,
    publish: &impl Fn(Scan),
) -> Scan {
    let pattern: Vec<char> = query.to_lowercase().chars().collect();
    let ascii_pattern = pattern.iter().all(|ch| ch.is_ascii());
    // DirEntry paths retain this root verbatim and append a separator when needed.
    let relative_start = directory.as_os_str().len()
        + usize::from(!directory.as_os_str().as_bytes().ends_with(b"/"));
    let mut directories = vec![root];
    let mut matches: Vec<(Reverse<i64>, PathBuf)> = Vec::new();
    let mut scan = Scan::default();
    let mut last_update = Instant::now();
    let mut dirty = false;
    while let Some(current) = directories.last_mut() {
        if cancelled() {
            return Scan::default();
        }
        let Some(entry) = current.next() else {
            directories.pop();
            continue;
        };
        let (path, kind) = match entry.and_then(|entry| Ok((entry.path(), entry.file_type()?))) {
            Ok(entry) => entry,
            Err(error) => {
                scan.error.get_or_insert(error);
                continue;
            }
        };
        if kind.is_dir() {
            match fs::read_dir(&path) {
                Ok(children) => directories.push(children),
                Err(error) => {
                    scan.error.get_or_insert(error);
                }
            }
        } else {
            // Explicit browsing may enter directory links; recursion never follows them.
            if kind.is_symlink() {
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => continue,
                    Ok(_) => {}
                    Err(error) => {
                        scan.error.get_or_insert(error);
                    }
                }
            }
            let relative = OsStr::from_bytes(&path.as_os_str().as_bytes()[relative_start..]);
            if (ascii_pattern || !relative.as_bytes().is_ascii())
                && let Some(score) = fuzzy_score(&pattern, &relative.to_string_lossy())
            {
                let item = (Reverse(score), path);
                let position = if matches.len() == SEARCH_LIMIT
                    && matches.last().is_some_and(|worst| item >= *worst)
                {
                    SEARCH_LIMIT
                } else {
                    matches.partition_point(|current| *current < item)
                };
                if matches.len() == SEARCH_LIMIT && !scan.truncated {
                    scan.truncated = true;
                    dirty = true;
                }
                if position < SEARCH_LIMIT {
                    matches.insert(position, item);
                    matches.truncate(SEARCH_LIMIT);
                    dirty = true;
                }
            }
        }
        if dirty && last_update.elapsed() >= Duration::from_millis(40) {
            let update = Scan {
                entries: matches
                    .iter()
                    .map(|(_, path)| FileEntry {
                        path: path.clone(),
                        is_dir: false,
                    })
                    .collect(),
                truncated: scan.truncated,
                ..Scan::default()
            };
            publish(update);
            dirty = false;
            last_update = Instant::now();
        }
    }
    scan.entries = matches
        .into_iter()
        .map(|(_, path)| FileEntry {
            path,
            is_dir: false,
        })
        .collect();
    scan
}

fn fuzzy_score(pattern: &[char], candidate: &str) -> Option<i64> {
    if candidate.is_ascii() {
        fuzzy_score_chars(
            pattern,
            candidate
                .bytes()
                .map(|ch| char::from(ch.to_ascii_lowercase())),
        )
    } else if pattern.is_empty() {
        Some(-(candidate.chars().flat_map(char::to_lowercase).count() as i64))
    } else {
        // Whole-string lowercasing preserves contextual mappings such as Greek sigma.
        fuzzy_score_chars(pattern, candidate.to_lowercase().chars())
    }
}

fn fuzzy_score_chars(pattern: &[char], candidate: impl Iterator<Item = char>) -> Option<i64> {
    let mut needle = pattern.iter();
    let Some(mut wanted) = needle.next() else {
        return Some(-(candidate.count() as i64));
    };
    let mut score = 0;
    let mut previous_match = None;
    let mut previous = '/';
    let mut candidate = candidate.enumerate();
    while let Some((index, ch)) = candidate.next() {
        if *wanted == ch {
            score += 32;
            score -= index as i64;
            if matches!(previous, '/' | '_' | '-' | '.' | ' ') {
                score += 24;
            }
            if previous_match.is_some_and(|previous| previous + 1 == index) {
                score += 16;
            }
            previous_match = Some(index);
            let Some(next) = needle.next() else {
                return Some(score - (index + 1 + candidate.count()) as i64);
            };
            wanted = next;
        }
        previous = ch;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::current_dir().unwrap().join(format!(
                "target/runtime-files/test-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: impl AsRef<Path>) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, []).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn finish(nav: &mut FileNav) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while nav.searching() {
            let mut fd = libc::pollfd {
                fd: nav.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(
                unsafe { libc::poll(&mut fd, 1, 5000) } > 0,
                "file result did not wake its consumer"
            );
            nav.drain_notification().unwrap();
            nav.poll();
            assert!(Instant::now() < deadline, "file scan did not finish");
        }
    }

    #[test]
    fn directory_navigation_sorts_directories_first_and_preserves_names() {
        let fixture = Fixture::new();
        fixture.file("z-dir/child");
        fixture.file("a-file");
        let raw = OsString::from_vec(b"z-\xff-file".to_vec());
        let raw_path = fixture.file(Path::new(&raw));
        symlink(fixture.0.join("z-dir"), fixture.0.join("a-link")).unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert!(nav.error().is_none());
        assert_eq!(nav.entries()[0].display_name(), "a-link");
        assert_eq!(nav.entries()[1].display_name(), "z-dir");
        assert_eq!(nav.entries()[2].display_name(), "a-file");
        assert_eq!(nav.entries()[3].path, raw_path);
        nav.move_selection(1);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.entries()[2].display_name(), "child");
        assert!(nav.entry_label(&nav.entries()[2]).starts_with("    "));
        assert!(nav.parent());
        finish(&mut nav);
        assert_eq!(nav.directory(), fixture.0);
    }

    #[test]
    fn recursive_fuzzy_search_matches_paths_without_following_directory_links() {
        let fixture = Fixture::new();
        let match_path = fixture.file("src/中文_file.rs");
        fixture.file("other.txt");
        symlink(&fixture.0, fixture.0.join("src/loop")).unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        nav.set_query("S中F.R".into());
        finish(&mut nav);
        assert!(nav.error().is_none());
        assert_eq!(nav.entries().len(), 1);
        assert_eq!(nav.entries()[0].path, match_path);
        nav.start_search();
        finish(&mut nav);
        assert_eq!(nav.entries().len(), 2);
        assert!(nav.entries().iter().all(|entry| !entry.is_dir));
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.entries()[0].display_name(), "src");
    }

    #[test]
    fn recursive_search_preserves_contextual_case_and_non_utf8_paths() {
        let fixture = Fixture::new();
        let ascii = fixture.file("k-file");
        let name = OsString::from_vec(
            "ΟΣ/İ-file-"
                .as_bytes()
                .iter()
                .copied()
                .chain([0xff])
                .collect(),
        );
        let wanted = fixture.file(Path::new(&name));
        fixture.file("other");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        nav.set_query("ος/i\u{307}".into());
        finish(&mut nav);
        assert!(nav.error().is_none());
        assert_eq!(nav.entries().len(), 1);
        assert_eq!(nav.entries()[0].path, wanted);
        nav.set_query("K".into());
        finish(&mut nav);
        assert_eq!(nav.entries().len(), 1);
        assert_eq!(nav.entries()[0].path, ascii);
    }

    #[test]
    fn rapid_query_changes_only_publish_the_latest_search() {
        let fixture = Fixture::new();
        for index in 0..250 {
            fixture.file(format!("nested/{index:03}-old.rs"));
        }
        let wanted = fixture.file("last-needle");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        for index in 0..100 {
            nav.set_query(format!("old-{index}"));
        }
        nav.set_query("lastneedle".into());
        finish(&mut nav);
        assert!(nav.error().is_none());
        assert_eq!(nav.entries().len(), 1);
        assert_eq!(nav.entries()[0].path, wanted);
    }

    #[test]
    fn completed_search_keeps_final_ranking_without_polling_partial_results() {
        let fixture = Fixture::new();
        let best = fixture.file("needle");
        fixture.file("long-path-to-needle");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.set_query("needle".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nav
            .worker
            .shared
            .0
            .lock()
            .unwrap()
            .result
            .as_ref()
            .is_some_and(|(_, _, complete)| *complete)
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        nav.poll();
        assert_eq!(nav.entries().len(), 2);
        assert_eq!(nav.selected().unwrap().path, best);
        finish(&mut nav);
    }

    #[test]
    fn recursive_search_preserves_root_spelling_and_non_utf8_names() {
        let fixture = Fixture::new();
        let name = OsString::from_vec(b"nested/needle-\xff".to_vec());
        fixture.file(&name);
        fs::create_dir(fixture.0.join("child")).unwrap();
        for suffix in ["", "/", "///", "/.", "/child/.."] {
            let directory = PathBuf::from(format!("{}{suffix}", fixture.0.display()));
            let expected = directory.join(&name);
            let mut nav = FileNav::new(directory).unwrap();
            nav.set_query("nestedneedle".into());
            finish(&mut nav);
            assert!(nav.error().is_none());
            assert_eq!(nav.entries().len(), 1);
            assert_eq!(nav.entries()[0].path.as_os_str(), expected.as_os_str());
        }
    }

    #[test]
    fn recursive_results_are_bounded_and_sorted_by_fuzzy_quality() {
        let fixture = Fixture::new();
        for index in 0..(SEARCH_LIMIT + 20) {
            fixture.file(format!("long/path/{index:03}-needle-suffix.txt"));
        }
        let best = fixture.file("needle");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        nav.set_query("needle".into());
        finish(&mut nav);
        assert!(nav.error().is_none());
        assert_eq!(nav.entries().len(), SEARCH_LIMIT);
        assert!(nav.truncated());
        assert_eq!(nav.entries()[0].path, best);
    }

    #[test]
    fn search_publishes_partial_results_and_finishes_with_a_slow_receiver() {
        let fixture = Fixture::new();
        for index in 0..80 {
            fixture.file(format!("{index:03}-file"));
        }
        let directory = fixture.0.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            search_directory(
                fs::read_dir(&directory).unwrap(),
                &directory,
                "",
                &|| {
                    // Make traversal slow enough to exercise its real update interval.
                    thread::sleep(Duration::from_millis(2));
                    false
                },
                &|update| {
                    let _ = sender.try_send(update);
                },
            )
        });
        let partial = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!partial.entries.is_empty());
        assert!(partial.entries.len() < 80);
        let complete = worker.join().unwrap();
        assert_eq!(complete.entries.len(), 80);
        assert!(complete.error.is_none());
    }

    #[test]
    fn directory_load_errors_are_reported() {
        let fixture = Fixture::new();
        let mut nav = FileNav::new(fixture.0.join("missing")).unwrap();
        finish(&mut nav);
        assert_eq!(nav.error().unwrap().kind(), io::ErrorKind::NotFound);
        assert!(nav.entries().is_empty());
    }

    #[test]
    fn tree_expansion_and_collapse_keep_the_root_and_parent_selected() {
        let fixture = Fixture::new();
        let first = fixture.file("a-dir/a-child");
        let wanted = fixture.file("a-dir/z-child");
        fixture.file("b-dir/other");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert!(nav.preview_lines().is_empty());
        assert!(!nav.preview_loading());
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.directory(), fixture.0);
        assert_eq!(nav.entries().len(), 4);
        nav.move_selection(2);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert!(nav.parent());
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("a-dir"));
        assert!(nav.parent());
        finish(&mut nav);
        assert_eq!(nav.entries().len(), 2);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert!(nav.enter_selected());
        assert_eq!(nav.selected().unwrap().path, first);
    }

    #[test]
    fn refreshing_and_search_updates_keep_the_selected_path() {
        let fixture = Fixture::new();
        fixture.file("b-file");
        let wanted = fixture.file("c-file");
        fs::write(&wanted, "selected contents").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.move_selection(1);
        finish(&mut nav);
        fixture.file("a-file");
        nav.refresh();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert!(nav.preview_lines().is_empty());
        assert!(nav.focus_preview());
        finish(&mut nav);
        assert_eq!(nav.preview_lines(), ["selected contents"]);
        nav.focus_list();
        nav.start_search();
        finish(&mut nav);
        nav.move_selection(2);
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        fixture.file("0-file");
        nav.refresh();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
    }

    #[test]
    fn preview_focus_and_scroll_keep_the_source_file_selected() {
        let fixture = Fixture::new();
        let path = fixture.file("中文'file");
        fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert_eq!(nav.focus(), FileFocus::List);
        assert!(nav.focus_preview());
        assert_eq!(nav.focus(), FileFocus::Preview);
        finish(&mut nav);
        nav.scroll_preview(200, 2);
        assert_eq!(nav.preview_scroll(), 3);
        nav.focus_list();
        assert_eq!(nav.selected().unwrap().path, path);
        assert_eq!(nav.preview_scroll(), 0);
        assert!(nav.focus_preview());
        nav.scroll_preview(-200, 2);
        assert_eq!(nav.preview_scroll(), 0);
        nav.scroll_preview(20, 20);
        assert_eq!(nav.preview_scroll(), 0);
    }

    #[test]
    fn preview_is_bounded_by_bytes_and_lines_and_escapes_terminal_controls() {
        let fixture = Fixture::new();
        let path = fixture.file("preview");
        fs::write(&path, "\t你好\u{85}\x1b[31m\r\nline\n").unwrap();
        let preview = read_preview(&path, &|| false).unwrap();
        assert_eq!(
            preview.lines.as_ref(),
            ["    你好\\u{85}\\u{1b}[31m", "line"]
        );
        assert!(!preview.truncated);
        let many_lines = "line\n".repeat(PREVIEW_LINE_LIMIT + 1);
        fs::write(&path, many_lines).unwrap();
        let preview = read_preview(&path, &|| false).unwrap();
        assert_eq!(preview.lines.len(), PREVIEW_LINE_LIMIT);
        assert!(preview.truncated);
        let bytes = format!("{}你好", "a".repeat(PREVIEW_BYTE_LIMIT - 1));
        fs::write(&path, bytes).unwrap();
        let preview = read_preview(&path, &|| false).unwrap();
        assert_eq!(preview.lines[0].len(), PREVIEW_BYTE_LIMIT - 1);
        assert!(preview.truncated);
    }

    #[test]
    fn fifo_binary_and_missing_file_previews_are_explicit_and_do_not_block() {
        let fixture = Fixture::new();
        let fifo = fixture.0.join("a-fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let binary = fixture.file("b-binary");
        fs::write(&binary, b"ELF\0\xff").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, fifo);
        assert!(nav.focus_preview());
        finish(&mut nav);
        assert_eq!(nav.preview_lines(), ["Special file; preview unavailable"]);
        assert!(nav.preview_error().is_none());
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(
            nav.preview_lines(),
            ["Binary or non-UTF-8 file; preview unavailable"]
        );
        let missing = read_preview(&fixture.0.join("missing"), &|| false)
            .err()
            .unwrap();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn changing_search_while_preview_loads_never_publishes_the_old_file() {
        let fixture = Fixture::new();
        let old = fixture.file("a-old");
        fs::write(&old, "old\n".repeat(PREVIEW_LINE_LIMIT)).unwrap();
        let wanted = fixture.file("z-dir/中文'needle");
        fs::write(&wanted, "new contents\n").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.move_selection(1);
        nav.set_query("old".into());
        nav.set_query("中needle".into());
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert!(nav.preview_lines().is_empty());
        assert!(nav.focus_preview());
        finish(&mut nav);
        assert_eq!(nav.preview_lines(), ["new contents"]);
        assert_eq!(nav.preview_scroll(), 0);
        assert!(nav.preview_error().is_none());
    }
}
