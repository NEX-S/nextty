//! Shared asynchronous file navigation for terminal front ends.

use crate::ui::TextField;
use crate::worker::Worker;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SEARCH_LIMIT: usize = 200;
const PREVIEW_BYTE_LIMIT: usize = 256 * 1024;
const PREVIEW_LINE_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileFocus {
    List,
    Preview,
}

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
}

pub struct FileNav {
    directory: PathBuf,
    expanded: HashSet<PathBuf>,
    pub(crate) query: Option<TextField>,
    pub(crate) scan: Scan,
    selection: usize,
    scroll: usize,
    /// The id of the newest scan; older ones stop and their results are dropped.
    scan_generation: Arc<AtomicU64>,
    scans: Worker<(u64, ScanRequest), (u64, Scan, bool)>,
    scan_pending: bool,
    restore_selection: Option<PathBuf>,
    directory_selection: HashMap<PathBuf, PathBuf>,
    focus: FileFocus,
    preview_path: Option<PathBuf>,
    pub(crate) preview: Preview,
    pub(crate) preview_scroll: usize,
    previews: Worker<PathBuf, (PathBuf, Preview)>,
    pub(crate) preview_pending: bool,
}

#[derive(Default)]
pub(crate) struct Preview {
    pub lines: Vec<String>,
    pub error: Option<io::Error>,
    pub truncated: bool,
}

impl Preview {
    fn message(text: &str) -> Self {
        Self {
            lines: vec![text.into()],
            ..Self::default()
        }
    }
}

enum ScanRequest {
    Browse(PathBuf, HashSet<PathBuf>),
    Search(PathBuf, String),
}

#[derive(Default)]
pub(crate) struct Scan {
    pub entries: Vec<FileEntry>,
    pub error: Option<io::Error>,
}

impl FileNav {
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        let wakeup = Arc::new(crate::sys::new()?);
        let scan_generation = Arc::new(AtomicU64::new(0));
        let current = Arc::clone(&scan_generation);
        let scans = Worker::new(
            "nextty-files",
            Arc::clone(&wakeup),
            move |(id, request), publish| {
                let cancelled = || current.load(Ordering::Relaxed) != id;
                let scan = match request {
                    ScanRequest::Browse(directory, expanded) => {
                        let mut scan = Scan::default();
                        browse(&directory, &expanded, &cancelled, &mut scan);
                        scan
                    }
                    ScanRequest::Search(directory, query) => {
                        search(&directory, &query, &cancelled, &|partial| {
                            publish((id, partial, false))
                        })
                    }
                };
                publish((id, scan, true));
            },
        )?;
        let previews = Worker::new("nextty-preview", wakeup, |path: PathBuf, publish| {
            let preview = read_preview(&path).unwrap_or_else(|error| Preview {
                error: Some(error),
                ..Preview::default()
            });
            publish((path, preview));
        })?;
        let mut nav = Self {
            directory: PathBuf::new(),
            expanded: HashSet::new(),
            query: None,
            scan: Scan::default(),
            selection: 0,
            scroll: 0,
            scan_generation,
            scans,
            scan_pending: false,
            restore_selection: None,
            directory_selection: HashMap::new(),
            focus: FileFocus::List,
            preview_path: None,
            preview: Preview::default(),
            preview_scroll: 0,
            previews,
            preview_pending: false,
        };
        nav.set_directory(directory)?;
        Ok(nav)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn fd(&self) -> RawFd {
        self.scans.fd()
    }

    pub fn is_search(&self) -> bool {
        self.query.is_some()
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    pub fn follow_selection(&mut self, rows: usize) -> usize {
        let rows = rows.max(1);
        let margin = ((rows - 1) / 2).min(2);
        let lowest = (self.selection + margin + 1).saturating_sub(rows);
        let highest = self.selection.saturating_sub(margin);
        let last = self.scan.entries.len().saturating_sub(rows);
        self.scroll = self.scroll.clamp(lowest, highest).min(last);
        self.scroll
    }

    pub fn selected(&self) -> Option<&FileEntry> {
        self.scan.entries.get(self.selection)
    }

    /// Whether a directory load, recursive search or preview is still running.
    pub fn searching(&self) -> bool {
        self.scan_pending || self.preview_pending
    }

    pub fn focus(&self) -> FileFocus {
        self.focus
    }

    pub fn focus_preview(&mut self) {
        if self.selected().is_some_and(|entry| !entry.is_dir) {
            self.focus = FileFocus::Preview;
            self.update_preview();
        }
    }

    pub fn focus_list(&mut self) {
        self.focus = FileFocus::List;
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selection = self
            .selection
            .saturating_add_signed(delta)
            .min(self.scan.entries.len().saturating_sub(1));
        self.restore_selection = None;
        self.update_preview();
    }

    pub(crate) fn scroll(&mut self, delta: isize, visible_rows: usize) {
        match self.focus {
            FileFocus::List => self.move_selection(delta),
            FileFocus::Preview => {
                self.preview_scroll = self
                    .preview_scroll
                    .saturating_add_signed(delta)
                    .min(self.preview.lines.len().saturating_sub(visible_rows.max(1)));
            }
        }
    }

    /// Applies `edit` to the query, starting a search if there is none, and rescans on change.
    fn edit_query(&mut self, edit: impl FnOnce(&mut TextField) -> bool) {
        let starting = self.query.is_none();
        if starting {
            self.remember_selection();
        }
        if edit(self.query.get_or_insert_default()) || starting {
            self.restore_selection = None;
            self.request_scan();
        }
    }

    pub fn start_search(&mut self) {
        self.edit_query(|query| !std::mem::take(query).text.is_empty());
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.focus_list();
        self.edit_query(|query| query.insert(text));
    }

    pub(crate) fn key_query(&mut self, symbol: u32, control: bool) {
        self.edit_query(|query| query.key(symbol, control));
    }

    pub fn stop_search(&mut self) {
        self.query = None;
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
    }

    pub fn set_directory(&mut self, directory: PathBuf) -> io::Result<()> {
        self.remember_selection();
        self.directory = std::path::absolute(directory)?;
        self.query = None;
        self.expanded.clear();
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
        Ok(())
    }

    /// Collapses the selected directory, else selects its parent entry, else lists the parent directory.
    pub fn parent(&mut self) {
        if let Some(path) = self.selected().map(|entry| entry.path.clone()) {
            if self.expanded.remove(&path) {
                self.restore_selection = Some(path);
                self.request_scan();
                return;
            }
            let parent = path.parent();
            if let Some(index) = self
                .scan
                .entries
                .iter()
                .position(|entry| Some(entry.path.as_path()) == parent)
            {
                self.selection = index;
                self.update_preview();
                return;
            }
        }
        let Some(parent) = self.directory.parent().map(Path::to_path_buf) else {
            return;
        };
        self.restore_selection = Some(std::mem::replace(&mut self.directory, parent));
        self.expanded.clear();
        self.request_scan();
    }

    /// Expands the selected directory or, when it is expanded, moves into it.
    pub fn enter_selected(&mut self) -> bool {
        let Some(path) = self
            .selected()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.path.clone())
        else {
            return false;
        };
        if self.expanded.contains(&path) {
            if self
                .scan
                .entries
                .get(self.selection + 1)
                .is_some_and(|entry| entry.path.parent() == Some(path.as_path()))
            {
                self.move_selection(1);
            }
        } else {
            // Every visible directory has its ancestors expanded already.
            self.expanded.insert(path.clone());
            self.restore_selection = Some(path);
            self.request_scan();
        }
        true
    }

    pub fn entry_label(&self, entry: &FileEntry) -> String {
        let relative = entry.path.strip_prefix(&self.directory).unwrap();
        if self.is_search() {
            return relative.to_string_lossy().into_owned();
        }
        let indent = 2 * relative.components().count().saturating_sub(1);
        let marker = if !entry.is_dir {
            " "
        } else if self.expanded.contains(&entry.path) {
            "\u{f078}"
        } else {
            "\u{f054}"
        };
        format!("{:indent$}{marker} {}", "", entry.display_name())
    }

    /// Takes finished scans and previews; whether anything changed.
    pub fn poll(&mut self) -> io::Result<bool> {
        // Drained before taking results, so a result published meanwhile wakes again.
        crate::sys::consume_notification(self.fd())?;
        let mut changed = false;
        if let Some((id, scan, complete)) = self.scans.take()
            && id == self.scan_generation.load(Ordering::Relaxed)
        {
            let selected = self
                .restore_selection
                .as_deref()
                .or_else(|| self.selected().map(|entry| entry.path.as_path()));
            self.selection = selected
                .and_then(|path| scan.entries.iter().position(|entry| entry.path == path))
                .unwrap_or_else(|| self.selection.min(scan.entries.len().saturating_sub(1)));
            self.scan = scan;
            self.update_preview();
            if complete {
                self.restore_selection = None;
                self.scan_pending = false;
            }
            changed = true;
        }
        if let Some((path, preview)) = self.previews.take()
            && self.preview_path.as_ref() == Some(&path)
        {
            self.preview = preview;
            self.preview_pending = false;
            changed = true;
        }
        Ok(changed)
    }

    fn remember_selection(&mut self) {
        if !self.is_search()
            && let Some(entry) = self.scan.entries.get(self.selection)
        {
            let path = entry.path.clone();
            self.directory_selection
                .insert(self.directory.clone(), path);
        }
    }

    fn update_preview(&mut self) {
        let path = self
            .selected()
            .filter(|entry| !entry.is_dir && self.focus == FileFocus::Preview)
            .map(|entry| entry.path.clone());
        if self.preview_path == path {
            return;
        }
        self.preview = Preview::default();
        self.preview_scroll = 0;
        self.preview_pending = path.is_some();
        if let Some(path) = &path {
            self.previews.submit(path.clone());
        }
        self.preview_path = path;
    }

    fn request_scan(&mut self) {
        self.scan = Scan::default();
        self.selection = 0;
        self.focus = FileFocus::List;
        self.update_preview();
        self.scan_pending = true;
        let request = match &self.query {
            Some(query) => ScanRequest::Search(self.directory.clone(), query.text.clone()),
            None => ScanRequest::Browse(self.directory.clone(), self.expanded.clone()),
        };
        let id = self.scan_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.scans.submit((id, request));
    }
}

fn read_preview(path: &Path) -> io::Result<Preview> {
    // NONBLOCK keeps a FIFO from blocking the open.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(Preview::message("Special file; preview unavailable"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(PREVIEW_BYTE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    let mut truncated = bytes.len() > PREVIEW_BYTE_LIMIT;
    bytes.truncate(PREVIEW_BYTE_LIMIT);
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => Some(text),
        // The byte limit may cut a character in half.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        _ => None,
    };
    let Some(text) = text.filter(|text| !text.contains('\0')) else {
        return Ok(Preview::message(
            "Binary or non-UTF-8 file; preview unavailable",
        ));
    };
    let mut source = text.split_terminator('\n');
    let lines = source
        .by_ref()
        .take(PREVIEW_LINE_LIMIT)
        .map(|line| escape_controls(line.strip_suffix('\r').unwrap_or(line)))
        .collect();
    truncated |= source.next().is_some();
    Ok(Preview {
        lines,
        truncated,
        error: None,
    })
}

fn escape_controls(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => result.push_str("    "),
            ch if ch.is_control() => result.extend(ch.escape_unicode()),
            ch => result.push(ch),
        }
    }
    result
}

/// The value of `result`, or None after keeping its error unless an earlier one was kept.
fn keep_error<T>(result: io::Result<T>, error: &mut Option<io::Error>) -> Option<T> {
    result.map_err(|e| error.get_or_insert(e)).ok()
}

/// An entry's path, whether it is a directory (following symlinks) and whether it is a symlink.
/// Unreadable entries are skipped or, for a dangling symlink, listed as files.
fn entry_info(
    entry: io::Result<fs::DirEntry>,
    error: &mut Option<io::Error>,
) -> Option<(PathBuf, bool, bool)> {
    let entry = entry.and_then(|entry| Ok((entry.path(), entry.file_type()?)));
    let (path, kind) = keep_error(entry, error)?;
    let is_dir = if kind.is_symlink() {
        keep_error(fs::metadata(&path), error).is_some_and(|metadata| metadata.is_dir())
    } else {
        kind.is_dir()
    };
    Some((path, is_dir, kind.is_symlink()))
}

/// Lists `directory` with the subdirectories in `expanded` listed in place below their parent.
fn browse(
    directory: &Path,
    expanded: &HashSet<PathBuf>,
    cancelled: &dyn Fn() -> bool,
    scan: &mut Scan,
) {
    let Some(listing) = keep_error(fs::read_dir(directory), &mut scan.error) else {
        return;
    };
    let mut entries = Vec::new();
    for entry in listing {
        if cancelled() {
            return;
        }
        if let Some((path, is_dir, _)) = entry_info(entry, &mut scan.error) {
            entries.push(FileEntry { path, is_dir });
        }
    }
    entries.sort_unstable_by(|a, b| {
        (!a.is_dir, a.path.as_os_str()).cmp(&(!b.is_dir, b.path.as_os_str()))
    });
    for entry in entries {
        let children = (entry.is_dir && expanded.contains(&entry.path)).then(|| entry.path.clone());
        scan.entries.push(entry);
        if let Some(children) = children {
            browse(&children, expanded, cancelled, scan);
        }
    }
}

/// Fuzzy-matches the paths of all files below `directory`, best first. Directory links are not followed.
fn search(
    directory: &Path,
    query: &str,
    cancelled: &dyn Fn() -> bool,
    publish: &dyn Fn(Scan),
) -> Scan {
    let pattern: Vec<char> = query.to_lowercase().chars().collect();
    let mut error = None;
    let mut matches: Vec<(Reverse<i64>, PathBuf)> = Vec::new();
    let results = |matches: &[(Reverse<i64>, PathBuf)], error| Scan {
        entries: matches
            .iter()
            .map(|(_, path)| FileEntry {
                path: path.clone(),
                is_dir: false,
            })
            .collect(),
        error,
    };
    let mut directories = vec![directory.to_path_buf()];
    let mut last_update = Instant::now();
    let mut dirty = false;
    while let Some(current) = directories.pop() {
        let Some(listing) = keep_error(fs::read_dir(current), &mut error) else {
            continue;
        };
        for entry in listing {
            if cancelled() {
                return Scan::default();
            }
            let Some((path, is_dir, symlink)) = entry_info(entry, &mut error) else {
                continue;
            };
            if is_dir {
                if !symlink {
                    directories.push(path);
                }
                continue;
            }
            let relative = path.strip_prefix(directory).unwrap().to_string_lossy();
            if let Some(score) = fuzzy_score(&pattern, &relative) {
                let candidate = (Reverse(score), path);
                let position = matches.partition_point(|current| *current < candidate);
                if position < SEARCH_LIMIT {
                    matches.insert(position, candidate);
                    matches.truncate(SEARCH_LIMIT);
                    dirty = true;
                }
            }
            if dirty && last_update.elapsed() >= Duration::from_millis(40) {
                publish(results(&matches, None));
                dirty = false;
                last_update = Instant::now();
            }
        }
    }
    results(&matches, error)
}

/// Matches the lowercase `pattern` characters in order, scoring early, word-start and
/// consecutive matches higher and longer candidates lower.
fn fuzzy_score(pattern: &[char], candidate: &str) -> Option<i64> {
    let candidate = candidate.to_lowercase();
    let mut needle = pattern.iter().peekable();
    let mut score = 0;
    let mut previous_match = None;
    let mut previous = '/';
    for (index, ch) in candidate.chars().enumerate() {
        if needle.next_if_eq(&&ch).is_some() {
            score += 32 - index as i64;
            if matches!(previous, '/' | '_' | '-' | '.' | ' ') {
                score += 24;
            }
            if previous_match.is_some_and(|previous| previous + 1 == index) {
                score += 16;
            }
            previous_match = Some(index);
        }
        previous = ch;
    }
    needle
        .peek()
        .is_none()
        .then(|| score - candidate.chars().count() as i64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicUsize;

    /// A directory that tests fill with empty files and that is removed when dropped.
    pub(crate) struct Fixture(pub PathBuf);

    impl Fixture {
        pub fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("target/runtime-files/test-{}-{id}", std::process::id());
            let path = std::env::current_dir().unwrap().join(name);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn file(&self, name: impl AsRef<Path>) -> PathBuf {
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

    fn set_query(nav: &mut FileNav, query: &str) {
        nav.edit_query(|field| {
            *field = TextField::default();
            field.insert(query);
            true
        });
    }

    /// Waits for the running scan and preview.
    pub(crate) fn finish(nav: &mut FileNav) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while nav.searching() {
            assert!(Instant::now() < deadline, "file scan did not finish");
            crate::sys::poll(&mut [crate::sys::watch(nav.fd(), libc::POLLIN)], 5000).unwrap();
            nav.poll().unwrap();
        }
    }

    #[test]
    fn tree_lists_directories_first_and_expands_and_collapses_in_place() {
        let fixture = Fixture::new();
        let first = fixture.file("z-dir/a-child");
        let child = fixture.file("z-dir/z-child");
        fixture.file("a-file");
        let raw = OsString::from_vec(b"z-\xff-file".to_vec());
        let raw_path = fixture.file(Path::new(&raw));
        symlink(fixture.0.join("z-dir"), fixture.0.join("a-link")).unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        let names: Vec<_> = nav.scan.entries.iter().map(|e| e.display_name()).collect();
        assert_eq!(names[..3], ["a-link", "z-dir", "a-file"]);
        assert_eq!(nav.scan.entries[3].path, raw_path);
        nav.move_selection(1);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 6);
        assert!(nav.entry_label(&nav.scan.entries[3]).starts_with("    "));
        assert!(nav.enter_selected());
        assert_eq!(nav.selected().unwrap().path, first);
        nav.move_selection(1);
        assert_eq!(nav.selected().unwrap().path, child);
        nav.parent();
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("z-dir"));
        nav.parent();
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 4);
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("z-dir"));
    }

    #[test]
    fn recursive_fuzzy_search_skips_directory_links_and_stopping_restores_the_selection() {
        let fixture = Fixture::new();
        let match_path = fixture.file("src/中文_file.rs");
        let other = fixture.file("other.txt");
        symlink(&fixture.0, fixture.0.join("src/loop")).unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.move_selection(1);
        set_query(&mut nav, "S中F.R");
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, match_path);
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, other);
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
            set_query(&mut nav, &format!("old-{index}"));
        }
        set_query(&mut nav, "lastneedle");
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, wanted);
    }

    #[test]
    fn recursive_results_are_bounded_and_sorted_by_fuzzy_quality() {
        let fixture = Fixture::new();
        for index in 0..(SEARCH_LIMIT + 20) {
            fixture.file(format!("long/path/{index:03}-needle-suffix.txt"));
        }
        let best = fixture.file("needle");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        set_query(&mut nav, "needle");
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries.len(), SEARCH_LIMIT);
        assert_eq!(nav.scan.entries[0].path, best);
    }

    #[test]
    fn preview_is_bounded_by_bytes_and_lines_and_escapes_terminal_controls() {
        let fixture = Fixture::new();
        let path = fixture.file("preview");
        fs::write(&path, "\t你好\u{85}\x1b[31m\r\nline\n").unwrap();
        let preview = read_preview(&path).unwrap();
        assert_eq!(preview.lines, ["    你好\\u{85}\\u{1b}[31m", "line"]);
        assert!(!preview.truncated);
        let many_lines = "line\n".repeat(PREVIEW_LINE_LIMIT + 1);
        fs::write(&path, many_lines).unwrap();
        let preview = read_preview(&path).unwrap();
        assert_eq!(preview.lines.len(), PREVIEW_LINE_LIMIT);
        assert!(preview.truncated);
        let bytes = format!("{}你好", "a".repeat(PREVIEW_BYTE_LIMIT - 1));
        fs::write(&path, bytes).unwrap();
        let preview = read_preview(&path).unwrap();
        assert_eq!(preview.lines[0].len(), PREVIEW_BYTE_LIMIT - 1);
        assert!(preview.truncated);
    }

    #[test]
    fn fifo_and_binary_previews_are_explicit_and_do_not_block() {
        let fixture = Fixture::new();
        let fifo = fixture.0.join("a-fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let binary = fixture.file("b-binary");
        fs::write(&binary, b"ELF\0\xff").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, fifo);
        nav.focus_preview();
        finish(&mut nav);
        assert_eq!(nav.preview.lines, ["Special file; preview unavailable"]);
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(
            nav.preview.lines,
            ["Binary or non-UTF-8 file; preview unavailable"]
        );
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
        nav.focus_preview();
        set_query(&mut nav, "old");
        set_query(&mut nav, "中needle");
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert_eq!(nav.focus(), FileFocus::List);
        assert_eq!(nav.preview.lines.len(), 0);
        nav.focus_preview();
        finish(&mut nav);
        assert_eq!(nav.preview.lines, ["new contents"]);
    }
}
