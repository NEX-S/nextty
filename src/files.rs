//! Shared asynchronous file navigation for terminal front ends.

use crate::ui::TextField;
use crate::worker::Worker;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{OwnedFd, RawFd};
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
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
    }
}

/// A background job that only runs the newest request: the results of
/// superseded requests are dropped and their running work sees `cancelled`.
struct Job<Request, Output> {
    worker: Worker<(u64, Request), (u64, Output)>,
    generation: Arc<AtomicU64>,
}

impl<Request: Send + 'static, Output: Send + 'static> Job<Request, Output> {
    fn new(
        name: &'static str,
        wakeup: Arc<OwnedFd>,
        mut run: impl FnMut(Request, &dyn Fn() -> bool, &dyn Fn(Output)) + Send + 'static,
    ) -> io::Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let current = Arc::clone(&generation);
        let worker = Worker::new(name, wakeup, move |(id, request), publish| {
            let cancelled = || current.load(Ordering::Relaxed) != id;
            run(request, &cancelled, &|output| {
                if !cancelled() {
                    publish((id, output));
                }
            });
        })?;
        Ok(Self { worker, generation })
    }

    fn submit(&self, request: Request) {
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.worker.submit((id, request));
    }

    fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn take(&mut self) -> Option<Output> {
        let (id, output) = self.worker.take()?;
        (id == self.generation.load(Ordering::Relaxed)).then_some(output)
    }
}

pub struct FileNav {
    directory: PathBuf,
    expanded: Arc<HashSet<PathBuf>>,
    pub(crate) query: Option<TextField>,
    pub(crate) scan: Scan,
    selection: usize,
    scroll: usize,
    scan_job: Job<ScanRequest, (Scan, bool)>,
    scan_pending: bool,
    restore_selection: Option<PathBuf>,
    directory_selection: HashMap<PathBuf, PathBuf>,
    focus: FileFocus,
    preview_path: Option<PathBuf>,
    pub(crate) preview: Preview,
    pub(crate) preview_scroll: usize,
    preview_job: Job<PathBuf, Preview>,
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
    Browse(PathBuf, Arc<HashSet<PathBuf>>),
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
        let scan_job = Job::new(
            "nextty-files",
            Arc::clone(&wakeup),
            |request: ScanRequest, cancelled, publish| {
                let scan = match &request {
                    ScanRequest::Browse(directory, expanded) => {
                        let mut scan = Scan::default();
                        browse(directory, expanded, cancelled, &mut scan);
                        scan
                    }
                    ScanRequest::Search(directory, query) => {
                        search(directory, query, cancelled, &|partial| {
                            publish((partial, false));
                        })
                    }
                };
                // The frontend may mutate the expanded set as soon as it receives this result.
                drop(request);
                publish((scan, true));
            },
        )?;
        let preview_job = Job::new("nextty-preview", wakeup, |path: PathBuf, _, publish| {
            publish(read_preview(&path).unwrap_or_else(|error| Preview {
                error: Some(error),
                ..Preview::default()
            }));
        })?;
        let mut nav = Self {
            directory: PathBuf::new(),
            expanded: Arc::default(),
            query: None,
            scan: Scan::default(),
            selection: 0,
            scroll: 0,
            scan_job,
            scan_pending: false,
            restore_selection: None,
            directory_selection: HashMap::new(),
            focus: FileFocus::List,
            preview_path: None,
            preview: Preview::default(),
            preview_scroll: 0,
            preview_job,
            preview_pending: false,
        };
        nav.set_directory(directory)?;
        Ok(nav)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn fd(&self) -> RawFd {
        self.scan_job.worker.fd()
    }

    pub fn drain_notification(&self) -> io::Result<()> {
        crate::sys::consume_notification(self.fd()).map(|_| ())
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
        if self.selection < self.scroll + margin {
            self.scroll = self.selection.saturating_sub(margin);
        } else if self.selection >= (self.scroll + rows).saturating_sub(margin) {
            self.scroll = (self.selection + margin + 1).saturating_sub(rows);
        }
        self.scroll = self
            .scroll
            .min(self.scan.entries.len().saturating_sub(rows));
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
        self.edit_query(|query| {
            let changed = !query.text.is_empty();
            *query = TextField::default();
            changed
        });
    }

    pub(crate) fn insert_query(&mut self, text: &str) {
        self.focus_list();
        self.edit_query(|query| query.insert(text));
    }

    pub(crate) fn key_query(&mut self, symbol: u32, control: bool) {
        if self.is_search() {
            self.edit_query(|query| query.key(symbol, control));
        }
    }

    pub fn stop_search(&mut self) {
        if self.query.take().is_some() {
            self.restore_selection = self.directory_selection.get(&self.directory).cloned();
            self.request_scan();
        }
    }

    pub fn set_directory(&mut self, directory: PathBuf) -> io::Result<()> {
        self.remember_selection();
        self.directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        self.query = None;
        self.expanded = Arc::default();
        self.restore_selection = self.directory_selection.get(&self.directory).cloned();
        self.request_scan();
        Ok(())
    }

    pub fn parent(&mut self) {
        if let Some(entry) = self.scan.entries.get(self.selection) {
            let path = &entry.path;
            if Arc::make_mut(&mut self.expanded).remove(path) {
                self.restore_selection = Some(path.clone());
                self.request_scan();
                return;
            }
            if let Some(parent) = path.parent().filter(|parent| *parent != self.directory)
                && let Some(index) = self
                    .scan
                    .entries
                    .iter()
                    .position(|entry| entry.path == parent)
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
        self.expanded = Arc::default();
        self.query = None;
        self.request_scan();
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
        let expanded = Arc::make_mut(&mut self.expanded);
        for directory in path
            .ancestors()
            .take_while(|directory| *directory != self.directory)
        {
            expanded.insert(directory.to_path_buf());
        }
        self.query = None;
        self.restore_selection = Some(path);
        self.request_scan();
        true
    }

    pub fn entry_label(&self, entry: &FileEntry) -> String {
        let relative = entry.path.strip_prefix(&self.directory);
        if self.is_search() {
            return relative
                .unwrap_or(&entry.path)
                .to_string_lossy()
                .into_owned();
        }
        let depth = relative.unwrap().components().count().saturating_sub(1);
        let marker = if entry.is_dir {
            if self.expanded.contains(&entry.path) {
                ""
            } else {
                ""
            }
        } else {
            " "
        };
        format!(
            "{:depth$}{marker} {}",
            "",
            entry.display_name(),
            depth = depth * 2
        )
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some((scan, complete)) = self.scan_job.take() {
            self.publish_scan(scan);
            if complete {
                self.restore_selection = None;
                self.scan_pending = false;
            }
            changed = true;
        }
        if let Some(preview) = self.preview_job.take() {
            self.preview = preview;
            self.preview_pending = false;
            changed = true;
        }
        changed
    }

    fn remember_selection(&mut self) {
        if !self.is_search()
            && let Some(entry) = self.scan.entries.get(self.selection)
        {
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
        self.update_preview();
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
        match &path {
            Some(path) => self.preview_job.submit(path.clone()),
            None => self.preview_job.cancel(),
        }
        self.preview_path = path;
    }

    fn request_scan(&mut self) {
        self.scan = Scan::default();
        self.preview = Preview::default();
        self.selection = 0;
        self.focus = FileFocus::List;
        self.update_preview();
        self.scan_pending = true;
        self.scan_job.submit(match &self.query {
            Some(query) => ScanRequest::Search(self.directory.clone(), query.text.clone()),
            None => ScanRequest::Browse(self.directory.clone(), self.expanded.clone()),
        });
    }
}

impl Drop for FileNav {
    // A remote filesystem syscall may not return promptly; cancelling lets the
    // workers finish on their own without making the frontend wait for them.
    fn drop(&mut self) {
        self.scan_job.cancel();
        self.preview_job.cancel();
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

/// An entry's path, whether it is a directory (following symlinks) and whether it is a symlink.
/// Unreadable entries are skipped or, for a dangling symlink, listed as files; the first error is kept.
fn entry_info(
    entry: io::Result<fs::DirEntry>,
    error: &mut Option<io::Error>,
) -> Option<(PathBuf, bool, bool)> {
    let (path, kind) = match entry.and_then(|entry| Ok((entry.path(), entry.file_type()?))) {
        Ok(entry) => entry,
        Err(e) => {
            error.get_or_insert(e);
            return None;
        }
    };
    let is_dir = if kind.is_symlink() {
        fs::metadata(&path).map_or_else(
            |e| {
                error.get_or_insert(e);
                false
            },
            |metadata| metadata.is_dir(),
        )
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
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        Err(error) => {
            scan.error.get_or_insert(error);
            return;
        }
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
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.path.as_os_str().cmp(b.path.as_os_str()))
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
            .map(|(_, relative)| FileEntry {
                path: directory.join(relative),
                is_dir: false,
            })
            .collect(),
        error,
    };
    let mut directories = match fs::read_dir(directory) {
        Ok(root) => vec![root],
        Err(error) => return results(&[], Some(error)),
    };
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
        let Some((path, is_dir, symlink)) = entry_info(entry, &mut error) else {
            continue;
        };
        if is_dir {
            if !symlink {
                match fs::read_dir(&path) {
                    Ok(children) => directories.push(children),
                    Err(e) => {
                        error.get_or_insert(e);
                    }
                }
            }
            continue;
        }
        let relative = path.strip_prefix(directory).unwrap();
        if let Some(score) = fuzzy_score(&pattern, &relative.to_string_lossy()) {
            let candidate = (Reverse(score), relative.to_path_buf());
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
    results(&matches, error)
}

fn fuzzy_score(pattern: &[char], candidate: &str) -> Option<i64> {
    if candidate.is_ascii() {
        let letters = candidate
            .bytes()
            .map(|ch| char::from(ch.to_ascii_lowercase()));
        fuzzy_score_chars(pattern, letters)
    } else {
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
            score += 32 - index as i64;
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
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
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

    fn set_query(nav: &mut FileNav, query: &str) {
        nav.edit_query(|field| {
            *field = TextField::default();
            field.insert(query);
            true
        });
    }

    fn finish(nav: &mut FileNav) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while nav.searching() {
            assert!(Instant::now() < deadline, "file scan did not finish");
            crate::sys::poll(&mut [crate::sys::watch(nav.fd(), libc::POLLIN)], 5000).unwrap();
            nav.drain_notification().unwrap();
            nav.poll();
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
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries[0].display_name(), "a-link");
        assert_eq!(nav.scan.entries[1].display_name(), "z-dir");
        assert_eq!(nav.scan.entries[2].display_name(), "a-file");
        assert_eq!(nav.scan.entries[3].path, raw_path);
        nav.move_selection(1);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.scan.entries[2].display_name(), "child");
        assert!(nav.entry_label(&nav.scan.entries[2]).starts_with("    "));
        nav.parent();
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
        set_query(&mut nav, "S中F.R");
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, match_path);
        nav.start_search();
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 2);
        assert!(nav.scan.entries.iter().all(|entry| !entry.is_dir));
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.scan.entries[0].display_name(), "src");
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
        set_query(&mut nav, "ος/i\u{307}");
        finish(&mut nav);
        assert!(nav.scan.error.is_none());
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, wanted);
        set_query(&mut nav, "ifile");
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, wanted);
        set_query(&mut nav, "K");
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 1);
        assert_eq!(nav.scan.entries[0].path, ascii);
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
    fn tree_expansion_and_collapse_keep_the_root_and_parent_selected() {
        let fixture = Fixture::new();
        let first = fixture.file("a-dir/a-child");
        let wanted = fixture.file("a-dir/z-child");
        fixture.file("b-dir/other");
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        assert!(nav.preview.lines.is_empty());
        assert!(!nav.preview_pending);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.directory(), fixture.0);
        assert_eq!(nav.scan.entries.len(), 4);
        nav.move_selection(2);
        assert_eq!(nav.selected().unwrap().path, wanted);
        nav.parent();
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("a-dir"));
        nav.parent();
        finish(&mut nav);
        assert_eq!(nav.scan.entries.len(), 2);
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert!(nav.enter_selected());
        assert_eq!(nav.selected().unwrap().path, first);
    }

    #[test]
    fn search_updates_keep_the_selected_path() {
        let fixture = Fixture::new();
        fixture.file("b-file");
        let wanted = fixture.file("c-file");
        fs::write(&wanted, "selected contents").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert_eq!(nav.focus(), FileFocus::List);
        assert_eq!(nav.preview.lines.len(), 0);
        nav.focus_preview();
        finish(&mut nav);
        assert_eq!(nav.preview.lines, ["selected contents"]);
        nav.focus_list();
        fixture.file("a-file");
        nav.start_search();
        finish(&mut nav);
        nav.move_selection(2);
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
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
        nav.focus_preview();
        finish(&mut nav);
        assert_eq!(nav.preview.lines, ["Special file; preview unavailable"]);
        assert!(nav.preview.error.is_none());
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(
            nav.preview.lines,
            ["Binary or non-UTF-8 file; preview unavailable"]
        );
        let missing = read_preview(&fixture.0.join("missing")).err().unwrap();
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
        assert_eq!(nav.preview_scroll, 0);
        assert!(nav.preview.error.is_none());
    }
}
