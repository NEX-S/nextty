use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const READ_LIMIT: usize = 16 * 1024;
const FILE_LIMIT: usize = 8 * 1024 * 1024;
const MATCH_LIMIT: usize = 100;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOperation {
    Read {
        offset: u64,
    },
    Search {
        query: String,
    },
    Edit {
        old_text: Option<String>,
        new_text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatch {
    #[serde(with = "crate::wire::path")]
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentChange {
    pub turn: u64,
    pub tool_id: String,
    #[serde(with = "crate::wire::path")]
    pub path: PathBuf,
    pub added: u32,
    pub removed: u32,
    pub diff: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileResult {
    Read {
        #[serde(with = "crate::wire::path")]
        path: PathBuf,
        offset: u64,
        text: String,
        next_offset: u64,
        has_more: bool,
    },
    Search {
        #[serde(with = "crate::wire::path")]
        path: PathBuf,
        matches: Vec<FileMatch>,
        truncated: bool,
    },
    Edited {
        change: AgentChange,
    },
}

pub(crate) fn execute(
    path: PathBuf,
    operation: FileOperation,
    canceled: &AtomicBool,
) -> io::Result<FileResult> {
    check_canceled(canceled)?;
    match operation {
        FileOperation::Read { offset } => {
            let (mut file, metadata) = open_regular(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = read_bounded(
                &mut file,
                metadata.len().saturating_sub(offset),
                READ_LIMIT + 1,
                canceled,
            )?;
            let available = bytes.len();
            bytes.truncate(READ_LIMIT);
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => {
                    let invalid = error.utf8_error();
                    if available <= READ_LIMIT || invalid.error_len().is_some() {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, invalid));
                    }
                    let mut bytes = error.into_bytes();
                    bytes.truncate(invalid.valid_up_to());
                    String::from_utf8(bytes).unwrap()
                }
            };
            if text.contains('\0') {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
            }
            Ok(FileResult::Read {
                path,
                offset,
                next_offset: offset + text.len() as u64,
                has_more: available > text.len(),
                text,
            })
        }
        FileOperation::Search { query } => {
            let mut search = FileSearch {
                finder: memchr::memmem::Finder::new(query.as_bytes()),
                matches: Vec::new(),
                remaining: READ_LIMIT,
                truncated: false,
                canceled,
            };
            search.search(&path)?;
            Ok(FileResult::Search {
                path,
                matches: search.matches,
                truncated: search.truncated,
            })
        }
        FileOperation::Edit { old_text, new_text } => {
            let change = if let Some(old_text) = old_text {
                let target = fs::canonicalize(&path)?;
                let (mut file, metadata) = open_regular(&target)?;
                if metadata.len() > FILE_LIMIT as u64 {
                    return Err(io::Error::other("file exceeds 8 MiB edit limit"));
                }
                let bytes = read_bounded(&mut file, metadata.len(), FILE_LIMIT + 1, canceled)?;
                if bytes.len() > FILE_LIMIT {
                    return Err(io::Error::other("file exceeds 8 MiB edit limit"));
                }
                let text = std::str::from_utf8(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if text.contains('\0') {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
                }
                let Some(start) = memchr::memmem::find(text.as_bytes(), old_text.as_bytes()) else {
                    return Err(io::Error::other("old_text was not found"));
                };
                if memchr::memmem::rfind(text.as_bytes(), old_text.as_bytes()) != Some(start) {
                    return Err(io::Error::other("old_text occurs more than once"));
                }
                let pieces = [&text[..start], &new_text, &text[start + old_text.len()..]];
                let staged = stage(&target, &pieces, Some(metadata.permissions()), canceled)?;
                check_unchanged(&path, &target, &metadata, &bytes, canceled)?;
                check_canceled(canceled)?;
                fs::rename(&staged.0, &target)?;
                let after = pieces.concat();
                edit_change(path, text, &after)
            } else {
                let staged = stage(&path, &[&new_text], None, canceled)?;
                check_canceled(canceled)?;
                fs::hard_link(&staged.0, &path)?;
                edit_change(path, "", &new_text)
            };
            Ok(FileResult::Edited { change })
        }
    }
}

fn edit_change(path: PathBuf, before: &str, after: &str) -> AgentChange {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old[prefix..old.len() - suffix];
    let added = &new[prefix..new.len() - suffix];
    let mut change = AgentChange {
        path,
        added: added.len() as u32,
        removed: removed.len() as u32,
        ..AgentChange::default()
    };
    if removed.is_empty() && added.is_empty() {
        return change;
    }
    let context_before = &old[prefix.saturating_sub(2)..prefix];
    let context_after = &old[old.len() - suffix..][..suffix.min(2)];
    let context = context_before.len() + context_after.len();
    let start = prefix - context_before.len();
    let (old_count, new_count) = (removed.len() + context, added.len() + context);
    let (old_start, new_start) = (
        start + usize::from(old_count != 0),
        start + usize::from(new_count != 0),
    );
    let name = change.path.display();
    change.diff = format!(
        "--- a/{name}\n+++ b/{name}\n@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    );
    let lines = [
        (" ", context_before),
        ("-", removed),
        ("+", added),
        (" ", context_after),
    ]
    .into_iter()
    .flat_map(|(mark, lines)| lines.iter().map(move |line| (mark, *line)));
    for (mark, line) in lines {
        let missing_newline = if line.ends_with('\n') {
            ""
        } else {
            "\n\\ No newline at end of file\n"
        };
        for piece in [mark, line, missing_newline] {
            let end = piece.floor_char_boundary(piece.len().min(READ_LIMIT - change.diff.len()));
            change.diff.push_str(&piece[..end]);
            if end != piece.len() {
                change.truncated = true;
                return change;
            }
        }
    }
    change
}

fn check_canceled(canceled: &AtomicBool) -> io::Result<()> {
    if canceled.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "file operation canceled",
        ))
    } else {
        Ok(())
    }
}

fn open_regular(path: &Path) -> io::Result<(File, Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((file, metadata))
}

fn read_bounded(
    file: &mut File,
    size: u64,
    limit: usize,
    canceled: &AtomicBool,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size.min(limit as u64) as usize);
    let mut file = file.take(limit as u64);
    loop {
        check_canceled(canceled)?;
        if file.by_ref().take(64 * 1024).read_to_end(&mut bytes)? < 64 * 1024 {
            break;
        }
    }
    check_canceled(canceled)?;
    Ok(bytes)
}

struct FileSearch<'a> {
    finder: memchr::memmem::Finder<'a>,
    matches: Vec<FileMatch>,
    remaining: usize,
    truncated: bool,
    canceled: &'a AtomicBool,
}

impl FileSearch<'_> {
    fn search(&mut self, path: &Path) -> io::Result<bool> {
        check_canceled(self.canceled)?;
        if fs::symlink_metadata(path)?.is_dir() {
            let mut names = Vec::new();
            for entry in fs::read_dir(path)? {
                check_canceled(self.canceled)?;
                names.push(entry?.file_name());
            }
            names.sort_unstable();
            for name in names {
                if self.search(&path.join(name))? {
                    return Ok(true);
                }
            }
        } else if fs::metadata(path)?.is_file() {
            let (mut file, metadata) = open_regular(path)?;
            if metadata.len() > FILE_LIMIT as u64 {
                self.truncated = true;
                return Ok(false);
            }
            let bytes = read_bounded(&mut file, metadata.len(), FILE_LIMIT + 1, self.canceled)?;
            if bytes.len() > FILE_LIMIT {
                self.truncated = true;
                return Ok(false);
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return Ok(false);
            };
            if text.contains('\0') {
                return Ok(false);
            }
            let mut offset = 0;
            let mut line = 1;
            while offset < text.len() {
                check_canceled(self.canceled)?;
                let Some(found) = self.finder.find(&bytes[offset..]) else {
                    break;
                };
                let found = offset + found;
                let skipped = &bytes[offset..found];
                line += memchr::memchr_iter(b'\n', skipped).count() as u64;
                let start =
                    memchr::memrchr(b'\n', skipped).map_or(offset, |index| offset + index + 1);
                let match_end = found + self.finder.needle().len();
                let end = memchr::memchr(b'\n', &bytes[match_end..])
                    .map_or(bytes.len(), |end| match_end + end);
                offset = memchr::memchr(b'\n', &bytes[found..])
                    .map_or(bytes.len(), |end| found + end + 1);
                let text = &text[start..end];
                let found = found - start;
                if self.matches.len() == MATCH_LIMIT || self.remaining == 0 {
                    self.truncated = true;
                    return Ok(true);
                }
                let start = if text.len() > self.remaining {
                    found
                } else {
                    0
                };
                let end = text.floor_char_boundary(text.len().min(start + self.remaining));
                self.truncated |= start != 0 || end != text.len();
                if end == start && !text.is_empty() {
                    return Ok(true);
                }
                self.remaining -= end - start;
                self.matches.push(FileMatch {
                    path: path.to_owned(),
                    line,
                    text: text[start..end].to_owned(),
                });
                line += 1;
            }
        }
        Ok(false)
    }
}

struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn stage(
    target: &Path,
    parts: &[&str],
    permissions: Option<Permissions>,
    canceled: &AtomicBool,
) -> io::Result<StagedFile> {
    check_canceled(canceled)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let path = parent.join(format!(
        ".nextty-agent-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed),
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if permissions.is_some() { 0o600 } else { 0o666 })
        .open(&path)?;
    let staged = StagedFile(path);
    for part in parts {
        file.write_all(part.as_bytes())?;
    }
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    Ok(staged)
}

fn check_unchanged(
    path: &Path,
    target: &Path,
    original: &Metadata,
    bytes: &[u8],
    canceled: &AtomicBool,
) -> io::Result<()> {
    let (mut file, opened) = open_regular(target)?;
    let current = read_bounded(&mut file, opened.len(), FILE_LIMIT + 1, canceled)?;
    let version = |metadata: &Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    if fs::canonicalize(path)? != target
        || version(original) != version(&fs::metadata(target)?)
        || current != bytes
    {
        return Err(io::Error::other("file changed during edit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "nextty-agent-files-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn search(path: PathBuf, query: &str) -> (PathBuf, Vec<FileMatch>, bool) {
        let operation = FileOperation::Search {
            query: query.into(),
        };
        match execute(path, operation, &AtomicBool::new(false)).unwrap() {
            FileResult::Search {
                path,
                matches,
                truncated,
            } => (path, matches, truncated),
            _ => panic!(),
        }
    }

    fn edit(
        path: PathBuf,
        old: Option<&str>,
        new: &str,
        canceled: &AtomicBool,
    ) -> io::Result<FileResult> {
        let operation = FileOperation::Edit {
            old_text: old.map(Into::into),
            new_text: new.into(),
        };
        execute(path, operation, canceled)
    }

    #[test]
    fn read_pages_preserve_exact_text_and_utf8_byte_offsets() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let source = format!(
            "{}界\r\n{}\tend\n",
            "x".repeat(READ_LIMIT - 1),
            "🙂".repeat(10000)
        );
        fs::write(&path, &source).unwrap();
        let canceled = AtomicBool::new(false);
        let mut offset = 0;
        let mut actual = String::new();
        loop {
            let FileResult::Read {
                path: result_path,
                offset: result_offset,
                text,
                next_offset,
                has_more,
            } = execute(path.clone(), FileOperation::Read { offset }, &canceled).unwrap()
            else {
                panic!();
            };
            assert_eq!(result_path, path);
            assert_eq!(result_offset, offset);
            assert!(text.len() <= READ_LIMIT);
            assert_eq!(next_offset, offset + text.len() as u64);
            assert_eq!(text, source[offset as usize..next_offset as usize]);
            actual.push_str(&text);
            offset = next_offset;
            if !has_more {
                break;
            }
        }
        assert_eq!(actual, source);
    }

    #[test]
    fn search_is_literal_ordered_and_skips_binary_and_symlink_directories() {
        let directory = Directory::new();
        fs::write(directory.0.join("z"), "ignored\nsecond a+b?\r\n").unwrap();
        fs::write(directory.0.join("a"), "a+b? first\nregex aaaab\n").unwrap();
        let name = OsStr::from_bytes(b"m-\xff");
        fs::write(directory.0.join(name), "a+b? middle\n").unwrap();
        fs::write(directory.0.join("binary"), b"a+b?\0").unwrap();
        fs::write(directory.0.join("invalid"), b"a+b?\xff").unwrap();
        symlink(&directory.0, directory.0.join("loop")).unwrap();
        let root = &directory.0;
        let (path, matches, truncated) = search(root.clone(), "a+b?");
        assert!(!truncated);
        assert_eq!(&path, root);
        let found = |file: &OsStr, line, text: &str| FileMatch {
            path: root.join(file),
            line,
            text: text.into(),
        };
        let expected = [
            found("a".as_ref(), 1, "a+b? first"),
            found(name, 1, "a+b? middle"),
            found("z".as_ref(), 2, "second a+b?\r"),
        ];
        assert_eq!(matches, expected);
    }

    #[test]
    fn search_matches_multiline_literals_and_tracks_following_line_numbers() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        fs::write(
            &path,
            "before\nneedle\nneedle\nneedle\nafter\nneedle\nneedle\n",
        )
        .unwrap();
        let (_, matches, truncated) = search(path.clone(), "needle\nneedle");
        assert!(!truncated);
        assert_eq!(
            matches,
            [2, 3, 6].map(|line| FileMatch {
                path: path.clone(),
                line,
                text: "needle\nneedle".into(),
            })
        );
    }

    #[test]
    fn search_bounds_match_count_long_lines_and_file_size() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        fs::write(&path, "needle\n".repeat(150)).unwrap();
        let (_, matches, truncated) = search(path.clone(), "needle");
        assert_eq!(matches.len(), MATCH_LIMIT);
        assert_eq!(matches.last().unwrap().line, MATCH_LIMIT as u64);
        assert!(truncated);

        fs::write(
            &path,
            format!("{}needle{}", "prefix".repeat(10000), "界".repeat(10000)),
        )
        .unwrap();
        let (_, matches, truncated) = search(path.clone(), "needle");
        assert!(truncated);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].text.starts_with("needle"));
        assert!(matches[0].text.len() <= READ_LIMIT);

        File::create(&path)
            .unwrap()
            .set_len(FILE_LIMIT as u64 + 1)
            .unwrap();
        let (_, matches, truncated) = search(path, "needle");
        assert!(truncated);
        assert!(matches.is_empty());
    }

    #[test]
    fn edit_preserves_current_unrelated_text_symlinks_and_permissions() {
        let directory = Directory::new();
        let target = directory.0.join("target");
        let link = directory.0.join("link");
        fs::write(&target, "prefix\nold\noriginal suffix\n").unwrap();
        fs::write(&target, "prefix\nold\nuser changed suffix\n").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o751)).unwrap();
        symlink("target", &link).unwrap();
        assert_eq!(
            edit(link.clone(), Some("old"), "new", &AtomicBool::new(false)).unwrap(),
            FileResult::Edited {
                change: edit_change(
                    link.clone(),
                    "prefix\nold\nuser changed suffix\n",
                    "prefix\nnew\nuser changed suffix\n"
                )
            }
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "prefix\nnew\nuser changed suffix\n"
        );
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[test]
    fn edit_rejects_missing_ambiguous_and_changed_content() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let canceled = AtomicBool::new(false);
        fs::write(&path, "aaa\n").unwrap();
        for old_text in ["missing", "aa"] {
            assert!(edit(path.clone(), Some(old_text), "new", &canceled).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\n");
        }
        let (mut file, metadata) = open_regular(&path).unwrap();
        let original = read_bounded(&mut file, metadata.len(), FILE_LIMIT + 1, &canceled).unwrap();
        fs::write(&path, "bbb\n").unwrap();
        assert!(check_unchanged(&path, &path, &metadata, &original, &canceled).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "bbb\n");
    }

    #[test]
    fn edit_content_comparison_checks_late_changes_and_length() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let original = vec![b'x'; 128 * 1024 + 3];
        let canceled = AtomicBool::new(false);
        fs::write(&path, &original).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        check_unchanged(&path, &path, &metadata, &original, &canceled).unwrap();
        let mut changed = original.clone();
        for index in [0, 64 * 1024 - 1, 64 * 1024, original.len() - 1] {
            changed[index] = b'y';
            fs::write(&path, &changed).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!(
                check_unchanged(&path, &path, &metadata, &original, &canceled)
                    .unwrap_err()
                    .to_string(),
                "file changed during edit"
            );
            changed[index] = b'x';
        }
        for length in [original.len() - 1, original.len() + 1] {
            changed.resize(length, b'x');
            fs::write(&path, &changed).unwrap();
            assert!(check_unchanged(&path, &path, &metadata, &original, &canceled).is_err());
            let current = fs::metadata(&path).unwrap();
            assert!(check_unchanged(&path, &path, &current, &original, &canceled).is_err());
        }
    }

    #[test]
    fn new_files_do_not_overwrite_and_cancellation_leaves_files_intact() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let create =
            |path: &Path, canceled: &AtomicBool| edit(path.into(), None, "new\n", canceled);
        create(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            create(&path, &AtomicBool::new(false)).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let canceled = AtomicBool::new(true);
        let replace = edit(path.clone(), Some("new"), "replacement", &canceled);
        assert_eq!(replace.unwrap_err().kind(), io::ErrorKind::Interrupted);
        let missing = directory.0.join("missing");
        assert_eq!(
            create(&missing, &canceled).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!missing.exists());
        assert_eq!(fs::read_to_string(path).unwrap(), "new\n");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn special_files_are_never_opened_for_blocking_reads() {
        let directory = Directory::new();
        let path = directory.0.join("fifo");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let canceled = AtomicBool::new(false);
        assert_eq!(
            execute(path.clone(), FileOperation::Read { offset: 0 }, &canceled)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            edit(path, Some("old"), "new", &canceled)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let (_, matches, truncated) = search(directory.0.clone(), "any");
        assert!(matches.is_empty());
        assert!(!truncated);
    }
    #[test]
    fn edit_evidence_uses_committed_whole_lines_and_reports_clipped_text() {
        let directory = Directory::new();
        let path = directory.0.join("file");
        fs::write(&path, "head\nprefix old suffix\ntail\n").unwrap();
        let result = edit(
            path.clone(),
            Some("old"),
            "new\nnext",
            &AtomicBool::new(false),
        );
        let FileResult::Edited { change } = result.unwrap() else {
            panic!()
        };
        assert_eq!((change.added, change.removed), (2, 1));
        assert!(
            change
                .diff
                .contains("-prefix old suffix\n+prefix new\n+next suffix\n")
        );
        assert!(change.diff.contains(" head\n"));
        assert!(change.diff.contains(" tail\n"));
        assert!(!change.truncated);
        let noop = edit_change(path.clone(), "same", "same");
        assert_eq!((noop.added, noop.removed, noop.diff.as_str()), (0, 0, ""));
        let created = edit_change(path.clone(), "", "one\ntwo");
        assert_eq!((created.added, created.removed), (2, 0));
        assert!(created.diff.contains("\\ No newline at end of file"));
        let clipped = edit_change(path, "", &"中".repeat(READ_LIMIT));
        assert_eq!(clipped.added, 1);
        assert!(clipped.truncated);
        assert!(clipped.diff.len() <= READ_LIMIT);
    }
}
