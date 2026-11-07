//! Shared asynchronous file navigation for terminal front ends.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write;
use std::fs::{self, OpenOptions, ReadDir};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SEARCH_LIMIT: usize = 200;
const PREVIEW_BYTE_LIMIT: usize = 256 * 1024;
const PREVIEW_LINE_LIMIT: usize = 4096;

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
    shared: Arc<Shared>,
    directory: PathBuf,
    query: Option<String>,
    scan: Scan,
    selection: usize,
    restore_selection: Option<PathBuf>,
    directory_selection: BTreeMap<PathBuf, PathBuf>,
    focus: FileFocus,
    preview_path: Option<PathBuf>,
    preview: Preview,
    preview_loading: bool,
    preview_scroll: usize,
}

// Each request runs on its own thread. Starting a request cancels older work of
// the same kind, and only the latest request's result is ever published.
struct Shared {
    wakeup: OwnedFd,
    scan: Latest<Scan>,
    preview: Latest<Preview>,
}

#[derive(Default)]
struct Latest<T> {
    generation: AtomicU64,
    result: Mutex<Option<(u64, T)>>,
}

impl<T> Latest<T> {
    fn next(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn cancelled(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Relaxed) != generation
    }

    fn publish(&self, generation: u64, result: T, wakeup: &OwnedFd) {
        let mut slot = self.result.lock().unwrap();
        // Checked under the lock so stale work never replaces a newer result.
        if !self.cancelled(generation) {
            *slot = Some((generation, result));
            crate::eventfd::notify(wakeup.as_raw_fd()).unwrap();
        }
    }

    fn take(&self) -> Option<T> {
        let mut slot = self.result.lock().unwrap();
        let (_, result) = slot.take_if(|(generation, _)| !self.cancelled(*generation))?;
        Some(result)
    }
}

#[derive(Default)]
struct Scan {
    entries: Vec<FileEntry>,
    error: Option<io::Error>,
    done: bool,
}

#[derive(Default)]
struct Preview {
    lines: Vec<String>,
    error: Option<io::Error>,
    truncated: bool,
}

impl FileNav {
    /// Starts loading an absolute directory without waiting for the filesystem.
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        let mut nav = Self {
            shared: Arc::new(Shared {
                wakeup: crate::eventfd::new()?,
                scan: Latest::default(),
                preview: Latest::default(),
            }),
            directory: PathBuf::new(),
            query: None,
            scan: Scan::default(),
            selection: 0,
            restore_selection: None,
            directory_selection: BTreeMap::new(),
            focus: FileFocus::List,
            preview_path: None,
            preview: Preview::default(),
            preview_loading: false,
            preview_scroll: 0,
        };
        nav.set_directory(directory);
        Ok(nav)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn fd(&self) -> RawFd {
        self.shared.wakeup.as_raw_fd()
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
        !self.scan.done || self.preview_loading
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
        self.preview_loading
    }

    pub fn preview_truncated(&self) -> bool {
        self.preview.truncated
    }

    pub fn preview_error(&self) -> Option<&io::Error> {
        self.preview.error.as_ref()
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
        self.set_query([self.query(), text].concat());
    }

    pub fn stop_search(&mut self) {
        if self.query.take().is_some() {
            self.restore_selection = self.directory_selection.get(&self.directory).cloned();
            self.request_scan();
        }
    }

    pub fn set_directory(&mut self, directory: PathBuf) {
        self.remember_selection();
        self.restore_selection = self.directory_selection.get(&directory).cloned();
        self.directory = directory;
        self.query = None;
        self.request_scan();
    }

    /// Lists the parent directory with the current directory selected.
    pub fn parent(&mut self) {
        if let Some(parent) = self.directory.parent().map(Path::to_path_buf) {
            let child = self.directory.clone();
            self.set_directory(parent);
            self.restore_selection = Some(child);
        }
    }

    pub fn enter_selected(&mut self) -> bool {
        let Some(entry) = self.selected().filter(|entry| entry.is_dir) else {
            return false;
        };
        self.set_directory(entry.path.clone());
        true
    }

    /// Returns an absolute, shell-quoted path, without a trailing newline.
    pub fn selected_shell_path(&self) -> Option<Vec<u8>> {
        self.selected()
            .filter(|entry| !entry.is_dir)
            .map(|entry| shell_quote(entry.path.as_os_str()))
    }

    /// Receives the latest results without waiting.
    pub fn poll(&mut self) -> io::Result<bool> {
        crate::eventfd::consume_notification(self.fd())?;
        let mut changed = false;
        if let Some(scan) = self.shared.scan.take() {
            self.publish_scan(scan);
            changed = true;
        }
        if let Some(preview) = self.shared.preview.take() {
            self.preview = preview;
            self.preview_loading = false;
            changed = true;
        }
        Ok(changed)
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
        if scan.done {
            self.restore_selection = None;
        }
        self.scan = scan;
        self.update_preview();
    }

    fn update_preview(&mut self) {
        let path = self.selected().map(|entry| &entry.path);
        if self.preview_path.as_ref() == path {
            return;
        }
        self.preview_path = path.cloned();
        self.preview_scroll = 0;
        let generation = self.shared.preview.next();
        let previous = std::mem::take(&mut self.preview);
        self.preview_loading = false;
        let Some(path) = self.preview_path.clone() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let spawned = thread::Builder::new()
            .name("nextty-preview".into())
            .spawn(move || {
                drop(previous);
                let preview = read_preview(&path).unwrap_or_else(|error| Preview {
                    error: Some(error),
                    ..Preview::default()
                });
                shared.preview.publish(generation, preview, &shared.wakeup);
            });
        match spawned {
            Ok(_) => self.preview_loading = true,
            Err(error) => self.preview.error = Some(error),
        }
    }

    fn request_scan(&mut self) {
        let generation = self.shared.scan.next();
        let previous = std::mem::take(&mut self.scan);
        self.selection = 0;
        self.focus = FileFocus::List;
        self.update_preview();
        let shared = Arc::clone(&self.shared);
        let directory = self.directory.clone();
        let query = self.query.clone();
        let spawned = thread::Builder::new()
            .name("nextty-files".into())
            .spawn(move || {
                // Release old directory lists off the input thread.
                drop(previous);
                let cancelled = || shared.scan.cancelled(generation);
                let publish = |scan| shared.scan.publish(generation, scan, &shared.wakeup);
                publish(match fs::read_dir(&directory) {
                    Ok(root) => match query {
                        None => list_directory(root, &cancelled),
                        Some(query) => {
                            search_directory(root, &directory, &query, &cancelled, &publish)
                        }
                    },
                    Err(error) => Scan {
                        error: Some(error),
                        done: true,
                        ..Scan::default()
                    },
                });
            });
        if let Err(error) = spawned {
            self.scan = Scan {
                error: Some(error),
                done: true,
                ..Scan::default()
            };
        }
    }
}

fn read_preview(path: &Path) -> io::Result<Preview> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            if entries.len() == PREVIEW_LINE_LIMIT {
                return Ok(Preview {
                    lines: sorted_names(entries),
                    truncated: true,
                    error: None,
                });
            }
            let entry = entry?;
            entries.push((entry.file_type()?.is_dir(), entry.file_name()));
        }
        return Ok(Preview {
            lines: sorted_names(entries),
            ..Preview::default()
        });
    }
    let special = || Ok(message("Special file; preview unavailable"));
    if !metadata.is_file() {
        return special();
    }
    // NONBLOCK closes the metadata/open race with a file replaced by a FIFO.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return special();
    }
    let mut bytes = Vec::new();
    file.take(PREVIEW_BYTE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > PREVIEW_BYTE_LIMIT;
    bytes.truncate(PREVIEW_BYTE_LIMIT);
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        // The byte limit may split the final character.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
        }
        Err(_) => "\0",
    };
    if text.contains('\0') {
        return Ok(message("Binary or non-UTF-8 file; preview unavailable"));
    }
    let mut lines = text.lines();
    Ok(Preview {
        lines: lines
            .by_ref()
            .take(PREVIEW_LINE_LIMIT)
            .map(preview_text)
            .collect(),
        truncated: truncated || lines.next().is_some(),
        error: None,
    })
}

fn sorted_names(mut entries: Vec<(bool, std::ffi::OsString)>) -> Vec<String> {
    entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    entries
        .into_iter()
        .map(|(is_dir, name)| {
            let name = preview_text(&name.to_string_lossy());
            if is_dir { name + "/" } else { name }
        })
        .collect()
}

fn message(text: &str) -> Preview {
    Preview {
        lines: vec![text.into()],
        ..Preview::default()
    }
}

fn preview_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => result.push_str("    "),
            ch if ch.is_control() => write!(result, "{}", ch.escape_unicode()).unwrap(),
            ch => result.push(ch),
        }
    }
    result
}

/// Quotes Unix filename bytes for insertion into a Bash or Zsh command line.
/// Controls use ANSI-C escapes so the interactive editor cannot treat them as keys.
pub fn shell_quote(path: &OsStr) -> Vec<u8> {
    let bytes = path.as_bytes();
    if bytes.iter().any(u8::is_ascii_control) {
        let mut quoted = b"$'".to_vec();
        for &byte in bytes {
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
    let mut quoted = vec![b'\''];
    for &byte in bytes {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

fn list_directory(directory: ReadDir, cancelled: &impl Fn() -> bool) -> Scan {
    let mut scan = Scan {
        done: true,
        ..Scan::default()
    };
    for entry in directory {
        if cancelled() {
            break;
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
    // DirEntry paths retain this root verbatim and append a separator when needed.
    let relative_start = directory.as_os_str().len()
        + usize::from(!directory.as_os_str().as_bytes().ends_with(b"/"));
    let mut directories = vec![root];
    // The best matches first: highest score, then path.
    let mut matches: Vec<(Reverse<i64>, PathBuf)> = Vec::new();
    let mut error = None;
    let mut last_update = Instant::now();
    let mut dirty = false;
    let entries = |matches: &[(Reverse<i64>, PathBuf)]| {
        matches
            .iter()
            .map(|(_, path)| FileEntry {
                path: path.clone(),
                is_dir: false,
            })
            .collect()
    };
    while let Some(current) = directories.last_mut() {
        if cancelled() {
            break;
        }
        let Some(entry) = current.next() else {
            directories.pop();
            continue;
        };
        let (path, kind) = match entry.and_then(|entry| Ok((entry.path(), entry.file_type()?))) {
            Ok(entry) => entry,
            Err(next) => {
                error.get_or_insert(next);
                continue;
            }
        };
        if kind.is_dir() {
            match fs::read_dir(&path) {
                Ok(children) => directories.push(children),
                Err(next) => {
                    error.get_or_insert(next);
                }
            }
            continue;
        }
        // Explicit browsing may enter directory links; recursion never follows them.
        if kind.is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => continue,
                Ok(_) => {}
                Err(next) => {
                    error.get_or_insert(next);
                }
            }
        }
        let relative = OsStr::from_bytes(&path.as_os_str().as_bytes()[relative_start..]);
        let Some(score) = fuzzy_score(&pattern, &relative.to_string_lossy()) else {
            continue;
        };
        let item = (Reverse(score), path);
        if matches.len() < SEARCH_LIMIT || matches.last().is_some_and(|worst| item < *worst) {
            let position = matches.partition_point(|current| *current < item);
            matches.insert(position, item);
            matches.truncate(SEARCH_LIMIT);
            dirty = true;
        }
        if dirty && last_update.elapsed() >= Duration::from_millis(40) {
            publish(Scan {
                entries: entries(&matches),
                ..Scan::default()
            });
            dirty = false;
            last_update = Instant::now();
        }
    }
    Scan {
        entries: entries(&matches),
        error,
        done: true,
    }
}

fn fuzzy_score(pattern: &[char], candidate: &str) -> Option<i64> {
    if candidate.is_ascii() {
        fuzzy_score_chars(
            pattern,
            candidate
                .bytes()
                .map(|ch| char::from(ch.to_ascii_lowercase())),
        )
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
    use std::cell::RefCell;
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
            nav.poll().unwrap();
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
        nav.set_query("K".into());
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
        assert_eq!(nav.entries()[0].path, best);
    }

    #[test]
    fn search_publishes_partial_results_before_it_finishes() {
        let fixture = Fixture::new();
        for index in 0..80 {
            fixture.file(format!("{index:03}-file"));
        }
        let partials = RefCell::new(Vec::new());
        let complete = search_directory(
            fs::read_dir(&fixture.0).unwrap(),
            &fixture.0,
            "",
            &|| {
                // Make traversal slow enough to exercise its real update interval.
                thread::sleep(Duration::from_millis(2));
                false
            },
            &|scan: Scan| partials.borrow_mut().push((scan.entries.len(), scan.done)),
        );
        assert!(
            partials
                .borrow()
                .iter()
                .any(|&(count, done)| count > 0 && count < 80 && !done)
        );
        assert_eq!(complete.entries.len(), 80);
        assert!(complete.done && complete.error.is_none());
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
        nav.parent();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, fixture.0.join("a-dir"));
        assert!(nav.enter_selected());
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
    }

    #[test]
    fn ending_a_search_restores_the_selected_path_and_preview() {
        let fixture = Fixture::new();
        fixture.file("b-file");
        let wanted = fixture.file("c-file");
        fs::write(&wanted, "selected contents").unwrap();
        let mut nav = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut nav);
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(nav.preview_lines(), ["selected contents"]);
        nav.set_query("b".into());
        finish(&mut nav);
        assert_ne!(nav.selected().unwrap().path, wanted);
        nav.stop_search();
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert_eq!(nav.preview_lines(), ["selected contents"]);
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
        assert_eq!(nav.preview_lines(), ["Special file; preview unavailable"]);
        assert!(nav.preview_error().is_none());
        nav.move_selection(1);
        finish(&mut nav);
        assert_eq!(
            nav.preview_lines(),
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
        nav.set_query("old".into());
        nav.set_query("中needle".into());
        finish(&mut nav);
        assert_eq!(nav.selected().unwrap().path, wanted);
        assert_eq!(nav.preview_lines(), ["new contents"]);
        assert_eq!(nav.preview_scroll(), 0);
        assert!(nav.preview_error().is_none());
    }
}
