use crate::eventfd::{consume_notification, notify};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

enum Command {
    Queue(File),
    ReadNext,
    Discard(File),
}

struct Shared {
    notification: OwnedFd,
    cancelled: AtomicBool,
}

pub(crate) struct ClipboardReader {
    commands: mpsc::Sender<Command>,
    chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
    shared: Arc<Shared>,
    pending: bool,
}

impl ClipboardReader {
    pub(crate) fn new(chunk_size: usize) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            notification: crate::eventfd::new()?,
            cancelled: AtomicBool::new(false),
        });
        let (commands, receiver) = mpsc::channel();
        let (sender, chunks) = mpsc::sync_channel(1);
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("nextty-reader".into())
            .spawn(move || run(receiver, sender, worker, chunk_size))?;
        Ok(Self {
            commands,
            chunks,
            shared,
            pending: false,
        })
    }

    pub(crate) fn queue(&self, file: File) -> io::Result<()> {
        self.command(Command::Queue(file))
    }

    pub(crate) fn discard(&self, file: File) -> io::Result<()> {
        self.command(Command::Discard(file))
    }

    fn command(&self, command: Command) -> io::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| io::Error::other("clipboard reader stopped"))
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.shared.notification.as_raw_fd()
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    pub(crate) fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.pending {
            consume_notification(self.fd())?;
        }
        match self.chunks.try_recv() {
            Ok(chunk) => {
                self.pending = false;
                return chunk.map(Some);
            }
            Err(mpsc::TryRecvError::Empty) => (),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("clipboard reader stopped"));
            }
        }
        if !self.pending {
            self.command(Command::ReadNext)?;
            self.pending = true;
        }
        Ok(None)
    }
}

impl Drop for ClipboardReader {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }
}

fn run(
    commands: mpsc::Receiver<Command>,
    chunks: mpsc::SyncSender<io::Result<Vec<u8>>>,
    shared: Arc<Shared>,
    chunk_size: usize,
) {
    let mut files = VecDeque::new();
    'commands: while let Ok(command) = commands.recv() {
        if shared.cancelled.load(Ordering::Acquire) {
            drop(command);
            break;
        }
        match command {
            Command::Queue(file) => files.push_back(file),
            Command::Discard(file) => drop(file),
            Command::ReadNext => {
                let mut bytes = vec![0; chunk_size];
                let mut count = 0;
                let result = loop {
                    if shared.cancelled.load(Ordering::Acquire) {
                        break 'commands;
                    }
                    match files.front_mut().unwrap().read(&mut bytes[count..]) {
                        Ok(0) => {
                            if count == 0 {
                                files.pop_front();
                            }
                            bytes.truncate(count);
                            break Ok(bytes);
                        }
                        Ok(length) => {
                            count += length;
                            if count == chunk_size {
                                break Ok(bytes);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                        Err(error) => {
                            files.pop_front();
                            break Err(error);
                        }
                    }
                };
                if shared.cancelled.load(Ordering::Acquire) {
                    break;
                }
                if chunks.send(result).is_err() {
                    break;
                }
                if let Err(error) = notify(shared.notification.as_raw_fd()) {
                    eprintln!("clipboard reader: {error}");
                    break;
                }
            }
        }
    }
    files.clear();
    // The sole sender cannot enqueue after cancellation. Drain File payloads here
    // before dropping the receiver, since mpsc storage can outlive its receiver.
    for command in commands.try_iter() {
        drop(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use std::os::fd::FromRawFd;

    #[test]
    fn short_file_chunks_leave_eof_between_queued_files() {
        let mut reader = ClipboardReader::new(3072).unwrap();
        for bytes in [vec![b'a'; 3074], b"next".to_vec()] {
            let fd = unsafe { libc::memfd_create(c"clipboard-test".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(&bytes).unwrap();
            file.rewind().unwrap();
            reader.queue(file).unwrap();
        }
        for expected in [
            vec![b'a'; 3072],
            b"aa".to_vec(),
            vec![],
            b"next".to_vec(),
            vec![],
        ] {
            loop {
                if let Some(bytes) = reader.read().unwrap() {
                    assert_eq!(bytes, expected);
                    break;
                }
                let mut fd = libc::pollfd {
                    fd: reader.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
            }
        }
    }
}
