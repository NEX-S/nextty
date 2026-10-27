//! Shared asynchronous file navigation for terminal front ends.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions, ReadDir};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
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
    query: Option<String>,
    entries: Vec<FileEntry>,
    selection: usize,
    generation: Arc<AtomicU64>,
    scan_pending: bool,
    worker: Option<Worker>,
    error: Option<io::Error>,
    truncated: bool,
    restore_selection: Option<PathBuf>,
    directory_selection: BTreeMap<PathBuf, PathBuf>,
    focus: FileFocus,
    preview_path: Option<PathBuf>,
    preview: Preview,
    preview_scroll: usize,
    preview_generation: Arc<AtomicU64>,
    preview_pending: bool,
    preview_worker: Option<PreviewWorker>,
}

struct PreviewWorker {
    generation: u64,
    result: Receiver<Preview>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct Preview {
    lines: Vec<String>,
    error: Option<io::Error>,
    truncated: bool,
}

struct Worker {
    generation: u64,
    updates: Receiver<Update>,
    result: Receiver<Scan>,
    handle: JoinHandle<()>,
}

// Result senders are moved after this guard so panic unwinding disconnects before waking.
struct WakeOnDrop(Arc<OwnedFd>);

impl WakeOnDrop {
    fn notify(&self) {
        if let Err(error) = crate::eventfd::notify(self.0.as_raw_fd()) {
            eprintln!("notify file navigation: {error}");
        }
    }
}

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.notify();
    }
}

struct Update {
    entries: Vec<FileEntry>,
    truncated: bool,
}

#[derive(Default)]
struct Scan {
    entries: Vec<FileEntry>,
    error: Option<io::Error>,
    truncated: bool,
}

impl Scan {
    fn record_error(&mut self, error: io::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

impl FileNav {
    /// Starts loading a directory without waiting for filesystem traversal.
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        let directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        let mut nav = Self {
            wakeup: Arc::new(crate::eventfd::new()?),
            directory,
            query: None,
            entries: Vec::new(),
            selection: 0,
            generation: Arc::new(AtomicU64::new(0)),
            scan_pending: false,
            worker: None,
            error: None,
            truncated: false,
            restore_selection: None,
            directory_selection: BTreeMap::new(),
            focus: FileFocus::List,
            preview_path: None,
            preview: Preview::default(),
            preview_scroll: 0,
            preview_generation: Arc::new(AtomicU64::new(0)),
            preview_pending: false,
            preview_worker: None,
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
        &self.entries
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    pub fn selected(&self) -> Option<&FileEntry> {
        self.entries.get(self.selection)
    }

    /// Whether a directory load, recursive search or preview is still running.
    pub fn searching(&self) -> bool {
        self.scan_pending || self.worker.is_some() || self.preview_loading()
    }

    pub fn focus(&self) -> FileFocus {
        self.focus
    }

    pub fn focus_preview(&mut self) -> bool {
        if self.selected().is_some_and(|entry| !entry.is_dir) {
            self.focus = FileFocus::Preview;
            true
        } else {
            false
        }
    }

    pub fn focus_list(&mut self) {
        self.focus = FileFocus::List;
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
        self.preview_pending || self.preview_worker.is_some()
    }

    pub fn preview_truncated(&self) -> bool {
        self.preview.truncated
    }

    pub fn preview_error(&self) -> Option<&io::Error> {
        self.preview.error.as_ref()
    }

    /// Search retains the best SEARCH_LIMIT matches and still scans for better ones.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Partial results remain usable when an inaccessible descendant is encountered.
    pub fn error(&self) -> Option<&io::Error> {
        self.error.as_ref()
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selection = self
            .selection
            .saturating_add_signed(delta)
            .min(self.entries.len().saturating_sub(1));
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
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
        Ok(())
    }

    pub fn parent(&mut self) -> bool {
        let Some(parent) = self.directory.parent() else {
            return false;
        };
        let parent = parent.to_path_buf();
        self.remember_selection();
        self.restore_selection = Some(std::mem::replace(&mut self.directory, parent));
        self.query = None;
        self.request_scan();
        true
    }

    pub fn enter_selected(&mut self) -> bool {
        let Some(entry) = self.selected().filter(|entry| entry.is_dir) else {
            return false;
        };
        let directory = entry.path.clone();
        self.remember_selection();
        self.directory = directory;
        self.query = None;
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
        true
    }

    /// Returns an absolute, shell-quoted path, without a trailing newline.
    pub fn selected_shell_path(&self) -> Option<Vec<u8>> {
        self.selected()
            .filter(|entry| !entry.is_dir)
            .map(|entry| shell_quote(entry.path.as_os_str()))
    }

    /// Receives available results and starts the latest pending request; never waits.
    /// Drain available results after a notification or navigation change.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let generation = self.generation.load(Ordering::Relaxed);
        // Unseen partial results must not pin the selection to an outdated ranking.
        let scan = match self.worker.as_ref().map(|worker| worker.result.try_recv()) {
            Some(Ok(scan)) => Some(scan),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                let worker = self.worker.take().unwrap();
                worker
                    .handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                unreachable!("file worker exited without publishing its result");
            }
            _ => None,
        };
        let mut previous = Vec::new();
        if let Some(scan) = scan {
            let worker = self.worker.take().unwrap();
            if worker.generation == generation {
                self.publish_entries(scan.entries);
                self.truncated = scan.truncated;
                self.error = scan.error;
                self.restore_selection = None;
            } else {
                previous = scan.entries;
            }
            changed = true;
        } else {
            while let Some(worker) = &self.worker {
                let Ok(update) = worker.updates.try_recv() else {
                    break;
                };
                if worker.generation == generation {
                    self.publish_entries(update.entries);
                    self.truncated = update.truncated;
                    changed = true;
                }
            }
        }
        changed |= self.start_pending((previous, Preview::default()));
        self.update_preview();
        let preview = match self
            .preview_worker
            .as_ref()
            .map(|worker| worker.result.try_recv())
        {
            Some(Ok(preview)) => Some(preview),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                let worker = self.preview_worker.take().unwrap();
                worker
                    .handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                unreachable!("preview worker exited without publishing its result");
            }
            _ => None,
        };
        let mut previous = Preview::default();
        if let Some(preview) = preview {
            let worker = self.preview_worker.take().unwrap();
            if worker.generation == self.preview_generation.load(Ordering::Relaxed) {
                self.preview = preview;
            } else {
                previous = preview;
            }
            changed = true;
        }
        changed |= self.start_preview(previous);
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

    fn publish_entries(&mut self, entries: Vec<FileEntry>) {
        let selected = self
            .restore_selection
            .as_deref()
            .or_else(|| self.selected().map(|entry| entry.path.as_path()));
        self.selection = selected
            .and_then(|path| entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selection.min(entries.len().saturating_sub(1)));
        self.entries = entries;
    }

    fn update_preview(&mut self) {
        let path = self.selected().map(|entry| &entry.path);
        if self.preview_path.as_ref() == path {
            return;
        }
        self.preview_path = path.cloned();
        let previous = std::mem::take(&mut self.preview);
        self.preview_scroll = 0;
        self.preview_generation.fetch_add(1, Ordering::Relaxed);
        self.preview_pending = self.preview_path.is_some();
        self.start_preview(previous);
    }

    fn start_preview(&mut self, previous: Preview) -> bool {
        if self.preview_worker.is_some() || !self.preview_pending {
            return false;
        }
        self.preview_pending = false;
        let path = self.preview_path.clone().unwrap();
        let generation = self.preview_generation.load(Ordering::Relaxed);
        let cancellation = Arc::clone(&self.preview_generation);
        let wakeup = Arc::clone(&self.wakeup);
        let (sender, result) = mpsc::sync_channel(1);
        match thread::Builder::new()
            .name("nextty-preview".into())
            .spawn(move || {
                let _wake = WakeOnDrop(wakeup);
                let output = sender;
                drop(previous);
                let cancelled = || cancellation.load(Ordering::Relaxed) != generation;
                let result = read_preview(&path, &cancelled).unwrap_or_else(|error| Preview {
                    error: Some(error),
                    ..Preview::default()
                });
                let _ = output.send(result);
            }) {
            Ok(handle) => {
                self.preview_worker = Some(PreviewWorker {
                    generation,
                    result,
                    handle,
                });
            }
            Err(error) => self.preview.error = Some(error),
        }
        true
    }

    fn request_scan(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        let previous = (
            std::mem::take(&mut self.entries),
            std::mem::take(&mut self.preview),
        );
        self.selection = 0;
        self.focus = FileFocus::List;
        self.update_preview();
        self.error = None;
        self.truncated = false;
        self.scan_pending = true;
        self.start_pending(previous);
    }

    fn start_pending(&mut self, previous: (Vec<FileEntry>, Preview)) -> bool {
        if self.worker.is_some() || !self.scan_pending {
            return false;
        }
        self.scan_pending = false;
        let directory = self.directory.clone();
        let query = self.query.clone();
        let generation = self.generation.load(Ordering::Relaxed);
        let cancellation = Arc::clone(&self.generation);
        let (sender, updates) = mpsc::sync_channel(1);
        let wakeup = Arc::clone(&self.wakeup);
        let (result_sender, result) = mpsc::sync_channel(1);
        match thread::Builder::new()
            .name("nextty-files".into())
            .spawn(move || {
                let wake = WakeOnDrop(wakeup);
                let output = result_sender;
                // Release old directory lists and previews off the input thread.
                drop(previous);
                let cancelled = || cancellation.load(Ordering::Relaxed) != generation;
                let _ = output.send(scan(
                    &directory,
                    query.as_deref(),
                    &cancelled,
                    sender,
                    &wake,
                ));
            }) {
            Ok(handle) => {
                self.worker = Some(Worker {
                    generation,
                    updates,
                    result,
                    handle,
                });
            }
            Err(error) => self.error = Some(error),
        }
        true
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
    if metadata.is_dir() {
        let mut preview = Preview::default();
        let mut entries = Vec::new();
        let mut bytes = 0;
        for entry in fs::read_dir(path)? {
            if cancelled() {
                return Ok(Preview::default());
            }
            let entry = entry?;
            let is_dir = entry.file_type()?.is_dir();
            let mut name = preview_text(&entry.file_name().to_string_lossy());
            if is_dir {
                name.push('/');
            }
            if entries.len() == PREVIEW_LINE_LIMIT || bytes + name.len() > PREVIEW_BYTE_LIMIT {
                preview.truncated = true;
                break;
            }
            bytes += name.len();
            entries.push((is_dir, name));
        }
        entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        preview.lines = entries.into_iter().map(|(_, name)| name).collect();
        return Ok(preview);
    }
    if !metadata.is_file() {
        return Ok(Preview {
            lines: vec!["Special file; preview unavailable".into()],
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
            lines: vec!["Special file; preview unavailable".into()],
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
    let Some(text) = text.filter(|text| !text.contains('\0')) else {
        return Ok(Preview {
            lines: vec!["Binary or non-UTF-8 file; preview unavailable".into()],
            ..Preview::default()
        });
    };
    let mut lines = Vec::new();
    for line in text.split_terminator('\n') {
        if lines.len() == PREVIEW_LINE_LIMIT {
            truncated = true;
            break;
        }
        lines.push(preview_text(line.strip_suffix('\r').unwrap_or(line)));
    }
    Ok(Preview {
        lines,
        truncated,
        error: None,
    })
}

fn preview_text(text: &str) -> String {
    if text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return text.to_owned();
    }
    let mut result = String::with_capacity(text.len());
    let mut start = 0;
    for (index, ch) in text.char_indices().filter(|(_, ch)| ch.is_control()) {
        result.push_str(&text[start..index]);
        match ch {
            '\t' => result.push_str("    "),
            ch => {
                use std::fmt::Write;
                write!(result, "\\u{{{:x}}}", u32::from(ch)).unwrap();
            }
        }
        start = index + ch.len_utf8();
    }
    result.push_str(&text[start..]);
    result
}

/// Quotes Unix filename bytes for insertion into a Bash or Zsh command line.
/// Controls use ANSI-C escapes so the interactive editor cannot treat them as keys.
pub fn shell_quote(path: &OsStr) -> Vec<u8> {
    if path.as_bytes().iter().any(u8::is_ascii_control) {
        let mut quoted = b"$'".to_vec();
        for &byte in path.as_bytes() {
            if byte.is_ascii_control() {
                quoted.extend_from_slice(&[
                    b'\\',
                    b'0' + (byte >> 6),
                    b'0' + ((byte >> 3) & 7),
                    b'0' + (byte & 7),
                ]);
            } else {
                if matches!(byte, b'\'' | b'\\') {
                    quoted.push(b'\\');
                }
                quoted.push(byte);
            }
        }
        quoted.push(b'\'');
        return quoted;
    }
    let mut quoted = Vec::with_capacity(path.len() + 2);
    quoted.push(b'\'');
    let bytes = path.as_bytes();
    let first_quote = bytes
        .iter()
        .position(|&byte| byte == b'\'')
        .unwrap_or(bytes.len());
    quoted.extend_from_slice(&bytes[..first_quote]);
    for &byte in &bytes[first_quote..] {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

fn scan(
    directory: &Path,
    query: Option<&str>,
    cancelled: &impl Fn() -> bool,
    updates: SyncSender<Update>,
    wake: &WakeOnDrop,
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
        None => list_directory(root, cancelled),
        Some(query) => search_directory(root, directory, query, cancelled, updates, wake),
    }
}

fn list_directory(directory: ReadDir, cancelled: &impl Fn() -> bool) -> Scan {
    let mut scan = Scan::default();
    for entry in directory {
        if cancelled() {
            return Scan::default();
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                scan.record_error(error);
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) => {
                let is_dir = if kind.is_symlink() {
                    match fs::metadata(&path) {
                        Ok(metadata) => metadata.is_dir(),
                        Err(error) => {
                            scan.record_error(error);
                            false
                        }
                    }
                } else {
                    kind.is_dir()
                };
                scan.entries.push(FileEntry { path, is_dir });
            }
            Err(error) => scan.record_error(error),
        }
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
    updates: SyncSender<Update>,
    wake: &WakeOnDrop,
) -> Scan {
    let pattern: Vec<char> = query.to_lowercase().chars().collect();
    let ascii_pattern = pattern.iter().all(|ch| ch.is_ascii());
    // DirEntry paths retain this root verbatim and append a separator when needed.
    let relative_start = directory.as_os_str().len()
        + usize::from(!directory.as_os_str().as_bytes().ends_with(b"/"));
    let mut directories = vec![root];
    let mut matches: Vec<(i64, FileEntry)> = Vec::new();
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
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                scan.record_error(error);
                continue;
            }
        };
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                scan.record_error(error);
                continue;
            }
        };
        if kind.is_dir() {
            match fs::read_dir(&path) {
                Ok(children) => directories.push(children),
                Err(error) => scan.record_error(error),
            }
        } else {
            // Explicit browsing may enter directory links; recursion never follows them.
            if kind.is_symlink() {
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => continue,
                    Ok(_) => {}
                    Err(error) => scan.record_error(error),
                }
            }
            let relative = OsStr::from_bytes(&path.as_os_str().as_bytes()[relative_start..]);
            if (ascii_pattern || !relative.as_bytes().is_ascii())
                && let Some(score) = fuzzy_score(&pattern, &relative.to_string_lossy())
            {
                let item = (
                    score,
                    FileEntry {
                        path,
                        is_dir: false,
                    },
                );
                let compare = |current: &(i64, FileEntry)| {
                    item.0
                        .cmp(&current.0)
                        .then_with(|| current.1.path.cmp(&item.1.path))
                };
                let position = if matches.len() == SEARCH_LIMIT
                    && matches.last().is_some_and(|last| compare(last).is_le())
                {
                    SEARCH_LIMIT
                } else {
                    matches.partition_point(|current| compare(current).is_lt())
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
            let update = Update {
                entries: matches.iter().map(|(_, entry)| entry.clone()).collect(),
                truncated: scan.truncated,
            };
            // A slow front end retains at most one snapshot. The joined result is final.
            if updates.try_send(update).is_ok() {
                dirty = false;
                wake.notify();
            }
            last_update = Instant::now();
        }
    }
    scan.entries = matches.into_iter().map(|(_, entry)| entry).collect();
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
    use std::process::Command;
    use std::sync::atomic::AtomicUsize;

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
        assert_eq!(nav.entries()[0].display_name(), "child");
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
    fn completed_search_takes_precedence_over_an_unseen_partial_selection() {
        let fixture = Fixture::new();
        let best = fixture.file("needle");
        let partial = fixture.file("long-path-to-needle");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.set_query("needle".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nav.worker.as_ref().unwrap().handle.is_finished() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let (sender, updates) = mpsc::sync_channel(1);
        sender
            .send(Update {
                entries: vec![FileEntry {
                    path: partial,
                    is_dir: false,
                }],
                truncated: false,
            })
            .unwrap();
        nav.worker.as_mut().unwrap().updates = updates;
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
        let wake = WakeOnDrop(Arc::new(crate::eventfd::new().unwrap()));
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
                sender,
                &wake,
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
    fn shell_paths_round_trip_original_bytes_without_execution_or_newline() {
        let fixture = Fixture::new();
        let name = OsString::from_vec(
            "空 格'\\\n\r\x1b\x7f\t$(echo nope);"
                .as_bytes()
                .iter()
                .copied()
                .chain([0xff])
                .collect(),
        );
        let path = fixture.file(Path::new(&name));
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        let quoted = nav.selected_shell_path().unwrap();
        assert!(quoted.iter().all(|byte| !byte.is_ascii_control()));
        let mut script = b"printf '%s' ".to_vec();
        script.extend_from_slice(&quoted);
        for shell in ["/bin/bash", "/bin/zsh"] {
            let output = Command::new(shell)
                .arg("-c")
                .arg(OsString::from_vec(script.clone()))
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}");
            assert_eq!(output.stdout, path.as_os_str().as_bytes(), "{shell}");
            assert!(output.stderr.is_empty(), "{shell}");
        }
    }

    #[test]
    fn ordinary_shell_paths_keep_posix_single_quotes() {
        assert_eq!(
            shell_quote(OsStr::new("空 格'\\file")),
            "'空 格'\\''\\file'".as_bytes()
        );
    }

    #[test]
    fn every_ascii_control_is_encoded_without_raw_key_bytes() {
        let bytes: Vec<_> = (1..=31).chain([127]).collect();
        let quoted = shell_quote(OsStr::from_bytes(&bytes));
        assert!(quoted.iter().all(|byte| !byte.is_ascii_control()));
        let mut script = b"printf '%s' ".to_vec();
        script.extend_from_slice(&quoted);
        for shell in ["/bin/bash", "/bin/zsh"] {
            let output = Command::new(shell)
                .arg("-c")
                .arg(OsString::from_vec(script.clone()))
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}");
            assert_eq!(output.stdout, bytes, "{shell}");
        }
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
    fn returning_to_parent_selects_the_directory_and_reentry_restores_its_file() {
        let fixture = Fixture::new();
        fixture.file("a-dir/a-child");
        let wanted = fixture.file("a-dir/z-child");
        fixture.file("b-dir/other");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert_eq!(nav.preview_lines(), ["a-child", "z-child"]);
        assert!(!nav.focus_preview());
        assert!(nav.enter_selected());
        finish(&mut nav);
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert!(nav.parent());
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("a-dir"));
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
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
        assert_eq!(nav.preview_lines(), ["selected contents"]);
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
        nav.scroll_preview(200, 2);
        assert_eq!(nav.preview_scroll(), 3);
        nav.focus_list();
        assert_eq!(nav.selected().unwrap().path, path);
        assert_eq!(nav.preview_scroll(), 3);
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
        assert_eq!(preview.lines, ["    你好\\u{85}\\u{1b}[31m", "line"]);
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
        assert_eq!(nav.preview_lines(), ["new contents"]);
        assert_eq!(nav.preview_scroll(), 0);
        assert!(nav.preview_error().is_none());
    }
}
