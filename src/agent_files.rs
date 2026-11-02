use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const READ_LIMIT: usize = 16 * 1024;
const FILE_LIMIT: usize = 8 * 1024 * 1024;
const MATCH_LIMIT: usize = 100;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatch {
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileResult {
    Read {
        path: PathBuf,
        offset: u64,
        text: String,
        next_offset: u64,
        has_more: bool,
    },
    Search {
        path: PathBuf,
        matches: Vec<FileMatch>,
        truncated: bool,
    },
    Edited {
        path: PathBuf,
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
            let mut file = open_regular(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            let bytes = read_bounded(&mut file, READ_LIMIT + 1, canceled)?;
            let end = bytes.len().min(READ_LIMIT);
            let text = match std::str::from_utf8(&bytes[..end]) {
                Ok(text) => text,
                Err(error) if bytes.len() > end && error.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
                }
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            };
            if text.contains('\0') {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
            }
            Ok(FileResult::Read {
                path,
                offset,
                text: text.to_owned(),
                next_offset: offset + text.len() as u64,
                has_more: bytes.len() > text.len(),
            })
        }
        FileOperation::Search { query } => {
            let mut matches = Vec::new();
            let mut remaining = READ_LIMIT;
            let mut truncated = false;
            search(
                &path,
                &query,
                &mut matches,
                &mut remaining,
                &mut truncated,
                canceled,
            )?;
            Ok(FileResult::Search {
                path,
                matches,
                truncated,
            })
        }
        FileOperation::Edit { old_text, new_text } => {
            if let Some(old_text) = old_text {
                let target = fs::canonicalize(&path)?;
                let mut file = open_regular(&target)?;
                let metadata = file.metadata()?;
                if metadata.len() > FILE_LIMIT as u64 {
                    return Err(io::Error::other("file exceeds 8 MiB edit limit"));
                }
                let bytes = read_bounded(&mut file, FILE_LIMIT + 1, canceled)?;
                if bytes.len() > FILE_LIMIT {
                    return Err(io::Error::other("file exceeds 8 MiB edit limit"));
                }
                let text = std::str::from_utf8(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if text.contains('\0') {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
                }
                let Some(start) = text.find(&old_text) else {
                    return Err(io::Error::other("old_text was not found"));
                };
                if text.rfind(&old_text) != Some(start) {
                    return Err(io::Error::other("old_text occurs more than once"));
                }
                let mut replacement = text.to_owned();
                replacement.replace_range(start..start + old_text.len(), &new_text);
                let staged = stage(
                    &target,
                    &replacement,
                    Some(metadata.permissions()),
                    canceled,
                )?;
                check_unchanged(&path, &target, &metadata, &bytes, canceled)?;
                check_canceled(canceled)?;
                fs::rename(&staged.0, &target)?;
            } else {
                let staged = stage(&path, &new_text, None, canceled)?;
                check_canceled(canceled)?;
                fs::hard_link(&staged.0, &path)?;
            }
            Ok(FileResult::Edited { path })
        }
    }
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

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

fn read_bounded(file: &mut File, limit: usize, canceled: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(file.metadata()?.len().min(limit as u64) as usize);
    let mut file = file.take(limit as u64);
    let mut buffer = [0; 8192];
    loop {
        check_canceled(canceled)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    check_canceled(canceled)?;
    Ok(bytes)
}

fn search(
    path: &Path,
    query: &str,
    matches: &mut Vec<FileMatch>,
    remaining: &mut usize,
    truncated: &mut bool,
    canceled: &AtomicBool,
) -> io::Result<bool> {
    check_canceled(canceled)?;
    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_dir() {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            check_canceled(canceled)?;
            entries.push(entry?.path());
        }
        entries.sort();
        for entry in entries {
            if search(&entry, query, matches, remaining, truncated, canceled)? {
                return Ok(true);
            }
        }
    } else if kind.is_file() || (kind.is_symlink() && fs::metadata(path)?.is_file()) {
        let mut file = open_regular(path)?;
        if file.metadata()?.len() > FILE_LIMIT as u64 {
            *truncated = true;
            return Ok(false);
        }
        let bytes = read_bounded(&mut file, FILE_LIMIT + 1, canceled)?;
        if bytes.len() > FILE_LIMIT {
            *truncated = true;
            return Ok(false);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(false);
        };
        if text.contains('\0') {
            return Ok(false);
        }
        for (line, text) in text.split_terminator('\n').enumerate() {
            check_canceled(canceled)?;
            let Some(found) = text.find(query) else {
                continue;
            };
            if matches.len() == MATCH_LIMIT || *remaining == 0 {
                *truncated = true;
                return Ok(true);
            }
            let start = if text.len() > *remaining { found } else { 0 };
            let mut end = text.len().min(start + *remaining);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            *truncated |= start != 0 || end != text.len();
            if end == start && !text.is_empty() {
                return Ok(true);
            }
            *remaining -= end - start;
            matches.push(FileMatch {
                path: path.to_owned(),
                line: line as u64 + 1,
                text: text[start..end].to_owned(),
            });
        }
    }
    Ok(false)
}

struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn stage(
    target: &Path,
    text: &str,
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
    for bytes in text.as_bytes().chunks(8192) {
        check_canceled(canceled)?;
        file.write_all(bytes)?;
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
    let mut current = open_regular(target)?;
    let current_bytes = read_bounded(&mut current, FILE_LIMIT + 1, canceled)?;
    let metadata = fs::metadata(target)?;
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
        || version(original) != version(&metadata)
        || current_bytes != bytes
    {
        return Err(io::Error::other("file changed during edit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
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
        let a = directory.0.join("a");
        let z = directory.0.join("z");
        fs::write(&z, "ignored\nsecond a+b?\r\n").unwrap();
        fs::write(&a, "a+b? first\nregex aaaab\n").unwrap();
        fs::write(directory.0.join("binary"), b"a+b?\0").unwrap();
        fs::write(directory.0.join("invalid"), b"a+b?\xff").unwrap();
        symlink(&directory.0, directory.0.join("loop")).unwrap();
        let FileResult::Search {
            matches, truncated, ..
        } = execute(
            directory.0.clone(),
            FileOperation::Search {
                query: "a+b?".into(),
            },
            &AtomicBool::new(false),
        )
        .unwrap()
        else {
            panic!()
        };
        assert!(!truncated);
        assert_eq!(
            matches,
            vec![
                FileMatch {
                    path: a,
                    line: 1,
                    text: "a+b? first".into()
                },
                FileMatch {
                    path: z,
                    line: 2,
                    text: "second a+b?\r".into()
                },
            ]
        );
    }

    #[test]
    fn search_bounds_match_count_long_lines_and_file_size() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let canceled = AtomicBool::new(false);
        fs::write(&path, "needle\n".repeat(150)).unwrap();
        let FileResult::Search {
            matches, truncated, ..
        } = execute(
            path.clone(),
            FileOperation::Search {
                query: "needle".into(),
            },
            &canceled,
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(matches.len(), MATCH_LIMIT);
        assert_eq!(matches.last().unwrap().line, MATCH_LIMIT as u64);
        assert!(truncated);

        fs::write(
            &path,
            format!("{}needle{}", "prefix".repeat(10000), "界".repeat(10000)),
        )
        .unwrap();
        let FileResult::Search {
            matches, truncated, ..
        } = execute(
            path.clone(),
            FileOperation::Search {
                query: "needle".into(),
            },
            &canceled,
        )
        .unwrap()
        else {
            panic!()
        };
        assert!(truncated);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].text.starts_with("needle"));
        assert!(matches[0].text.len() <= READ_LIMIT);

        File::create(&path)
            .unwrap()
            .set_len(FILE_LIMIT as u64 + 1)
            .unwrap();
        let FileResult::Search {
            matches, truncated, ..
        } = execute(
            path,
            FileOperation::Search {
                query: "needle".into(),
            },
            &canceled,
        )
        .unwrap()
        else {
            panic!()
        };
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
            execute(
                link.clone(),
                FileOperation::Edit {
                    old_text: Some("old".into()),
                    new_text: "new".into()
                },
                &AtomicBool::new(false),
            )
            .unwrap(),
            FileResult::Edited { path: link.clone() }
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
            assert!(
                execute(
                    path.clone(),
                    FileOperation::Edit {
                        old_text: Some(old_text.into()),
                        new_text: "new".into()
                    },
                    &canceled,
                )
                .is_err()
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\n");
        }
        let mut file = open_regular(&path).unwrap();
        let metadata = file.metadata().unwrap();
        let original = read_bounded(&mut file, FILE_LIMIT + 1, &canceled).unwrap();
        fs::write(&path, "bbb\n").unwrap();
        assert!(check_unchanged(&path, &path, &metadata, &original, &canceled).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "bbb\n");
    }

    #[test]
    fn new_files_do_not_overwrite_and_cancellation_leaves_files_intact() {
        let directory = Directory::new();
        let path = directory.0.join("text");
        let operation = FileOperation::Edit {
            old_text: None,
            new_text: "new\n".into(),
        };
        execute(path.clone(), operation.clone(), &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            execute(path.clone(), operation.clone(), &AtomicBool::new(false))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        let canceled = AtomicBool::new(true);
        assert_eq!(
            execute(
                path.clone(),
                FileOperation::Edit {
                    old_text: Some("new".into()),
                    new_text: "replacement".into()
                },
                &canceled,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        let missing = directory.0.join("missing");
        assert_eq!(
            execute(missing.clone(), operation, &canceled)
                .unwrap_err()
                .kind(),
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
            execute(
                path,
                FileOperation::Edit {
                    old_text: Some("old".into()),
                    new_text: "new".into()
                },
                &canceled,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
        let FileResult::Search {
            matches, truncated, ..
        } = execute(
            directory.0.clone(),
            FileOperation::Search {
                query: "any".into(),
            },
            &canceled,
        )
        .unwrap()
        else {
            panic!()
        };
        assert!(matches.is_empty());
        assert!(!truncated);
    }
}
