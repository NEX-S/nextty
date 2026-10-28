use crate::eventfd::{consume_notification, notify};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::{CStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

use crate::core::{Core, Event as CoreEvent, WindowSize};

pub(crate) const INPUT_CAPACITY: usize = 256 * 1024;

#[derive(Debug)]
pub enum Event {
    Wakeup,
    Title(Option<String>),
    ClipboardStore { primary: bool, text: String },
    Exit(ExitStatus),
    Error(io::Error),
}

#[derive(Clone, Debug)]
pub struct Shell {
    pub program: String,
    pub args: Vec<String>,
}

impl Shell {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessMetadata {
    pub launch_argv: Vec<String>,
    pub child_pid: u32,
    pub foreground_pgid: Option<u32>,
    // A PTY identifies a process group; procfs fields below describe its leader.
    pub foreground_pid: Option<u32>,
    pub foreground_executable: Option<PathBuf>,
    pub foreground_argv: Option<Vec<String>>,
    pub foreground_cwd: Option<PathBuf>,
    pub sampled_at_ms: Option<u64>,
    pub unavailable: Vec<String>,
}

struct EventProxy {
    sender: mpsc::Sender<Event>,
    wakeup: Arc<Wakeup>,
}

impl EventProxy {
    fn send(&self, event: Event) {
        if matches!(event, Event::Wakeup) && self.wakeup.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.sender.send(event).is_ok()
            && let Err(error) = notify(self.wakeup.fd.as_raw_fd())
        {
            eprintln!("notify terminal UI: {error}");
        }
    }
}

#[derive(Debug)]
enum Message {
    Input(Cow<'static, [u8]>),
    Resize(libc::winsize),
    Shutdown,
}

struct Wakeup {
    fd: OwnedFd,
    pending: AtomicBool,
}

pub struct Terminal {
    // Async shutdown releases this reference before disconnecting the IO owner.
    pub core: Arc<Mutex<Core>>,
    pub events: mpsc::Receiver<Event>,
    event_wakeup: Arc<Wakeup>,
    sender: Option<mpsc::Sender<Message>>,
    wakeup: Arc<Wakeup>,
    reader: Option<JoinHandle<()>>,
    child_pid: u32,
    process_metadata: Arc<Mutex<Arc<ProcessMetadata>>>,
    input_bytes: Arc<AtomicUsize>,
}

impl Terminal {
    pub fn new(
        shell: Option<Shell>,
        working_directory: Option<PathBuf>,
        size: WindowSize,
        foreground: u32,
        background: u32,
    ) -> Result<Self> {
        Self::new_inner(
            shell,
            working_directory,
            size,
            foreground,
            background,
            #[cfg(feature = "tui")]
            None,
        )
    }

    #[cfg(feature = "tui")]
    pub fn new_with_history(
        shell: Option<Shell>,
        working_directory: Option<PathBuf>,
        size: WindowSize,
        foreground: u32,
        background: u32,
        history: crate::history::HistoryConfig,
    ) -> Result<Self> {
        Self::new_inner(
            shell,
            working_directory,
            size,
            foreground,
            background,
            Some(history),
        )
    }

    fn new_inner(
        shell: Option<Shell>,
        working_directory: Option<PathBuf>,
        size: WindowSize,
        foreground: u32,
        background: u32,
        #[cfg(feature = "tui")] history: Option<crate::history::HistoryConfig>,
    ) -> Result<Self> {
        let pty_size = winsize(size)?;
        let terminal_core = Core::new(size, foreground, background);
        #[cfg(feature = "tui")]
        let mut terminal_core = terminal_core;
        #[cfg(feature = "tui")]
        if let Some(history) = history {
            terminal_core.enable_history(history)?;
        }
        let (master, slave) =
            open_pty(pty_size).map_err(|error| Error::context("cannot create PTY", error))?;
        let wakeup = Arc::new(Wakeup {
            fd: crate::eventfd::new()?,
            pending: AtomicBool::new(false),
        });
        let event_wakeup = Arc::new(Wakeup {
            fd: crate::eventfd::new()?,
            pending: AtomicBool::new(false),
        });
        #[cfg(feature = "tui")]
        let integrate = shell.is_none();
        let mut command = match shell {
            Some(shell) => {
                let mut command = Command::new(shell.program);
                command.args(shell.args);
                command
            }
            None => Command::new(
                default_shell()
                    .map_err(|error| Error::context("cannot find login shell", error))?,
            ),
        };
        if let Some(directory) = &working_directory {
            command.current_dir(directory);
        }
        #[cfg(feature = "tui")]
        let integration = if integrate {
            crate::shell_integration::Integration::prepare(&mut command)?
        } else {
            None
        };
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            // Only async-signal-safe calls belong between fork and exec.
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let launch_argv = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let mut child = command
            .spawn()
            .map_err(|error| Error::context("cannot start terminal session", error))?;
        drop(command);
        let child_pid = child.id();
        let pidfd = match fd(unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid, 0) as i32 }) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                if let Err(cleanup) = child.kill()
                    && cleanup.raw_os_error() != Some(libc::ESRCH)
                {
                    eprintln!("cannot terminate child after pidfd_open failure: {cleanup}");
                }
                if let Err(cleanup) = child.wait() {
                    eprintln!("cannot reap terminal child after pidfd_open failure: {cleanup}");
                }
                return Err(Error::context("cannot monitor terminal child", error));
            }
        };
        let core = Arc::new(Mutex::new(terminal_core));
        let (sender, receiver) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let input_bytes = Arc::new(AtomicUsize::new(0));
        let process_metadata = Arc::new(Mutex::new(Arc::new(ProcessMetadata {
            launch_argv,
            child_pid,
            unavailable: vec!["Foreground process has not been sampled yet".into()],
            ..ProcessMetadata::default()
        })));
        let io = PtyLoop {
            master,
            process_metadata: process_metadata.clone(),
            input_bytes: input_bytes.clone(),
            process: ChildGuard { child, pidfd },
            wakeup: wakeup.clone(),
            receiver,
            core: core.clone(),
            #[cfg(feature = "tui")]
            _integration: integration,
            proxy: EventProxy {
                sender: event_sender,
                wakeup: event_wakeup.clone(),
            },
        };
        let reader = Some(
            std::thread::Builder::new()
                .name("terminal-io".into())
                .spawn(move || io.run())?,
        );
        Ok(Self {
            core,
            events,
            event_wakeup,
            sender: Some(sender),
            wakeup,
            reader,
            child_pid,
            process_metadata,
            input_bytes,
        })
    }

    #[cfg(feature = "tui")]
    pub fn copy_block(
        &self,
        id: u64,
    ) -> io::Result<mpsc::Receiver<io::Result<crate::history::HistoryCopy>>> {
        self.core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .copy_block(id)
    }

    pub fn process_metadata(&self) -> Arc<ProcessMetadata> {
        self.process_metadata.lock().unwrap().clone()
    }

    pub fn current_directory(&self) -> io::Result<PathBuf> {
        #[cfg(feature = "tui")]
        if let Some(directory) = self
            .core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .current_directory()
        {
            return Ok(directory.to_path_buf());
        }
        std::fs::read_link(format!("/proc/{}/cwd", self.child_pid))
    }

    #[cfg(feature = "tui")]
    pub fn copy_selection(
        &self,
    ) -> io::Result<mpsc::Receiver<io::Result<crate::history::HistoryCopy>>> {
        self.core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .copy_selection()
    }

    pub fn write(&self, data: impl Into<Cow<'static, [u8]>>) -> io::Result<()> {
        let data = data.into();
        if data.is_empty() {
            return Ok(());
        }
        self.input_bytes.fetch_add(data.len(), Ordering::AcqRel);
        let length = data.len();
        if let Err(error) = self.send(Message::Input(data)) {
            self.input_bytes.fetch_sub(length, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    pub fn input_pending_bytes(&self) -> usize {
        self.input_bytes.load(Ordering::Acquire)
    }
    pub fn try_write(&self, data: impl Into<Cow<'static, [u8]>>) -> io::Result<()> {
        let data = data.into();
        let length = data.len();
        if length == 0 {
            return Ok(());
        }
        if length > INPUT_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input chunk exceeds 256 KiB",
            ));
        }
        self.input_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                bytes
                    .checked_add(length)
                    .filter(|bytes| *bytes <= INPUT_CAPACITY)
            })
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full"))?;
        if let Err(error) = self.send(Message::Input(data)) {
            self.input_bytes.fetch_sub(length, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    pub fn resize(&self, size: WindowSize) -> io::Result<()> {
        let pty_size = winsize(size)?;
        let mut core = self
            .core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.send(Message::Resize(pty_size))?;
        core.resize(size);
        Ok(())
    }

    fn send(&self, message: Message) -> io::Result<()> {
        self.sender
            .as_ref()
            .unwrap()
            .send(message)
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))?;
        if !self.wakeup.pending.swap(true, Ordering::AcqRel) {
            notify(self.wakeup.fd.as_raw_fd())?;
        }
        Ok(())
    }

    /// Clear before reading the grid so concurrent PTY changes schedule the next frame.
    pub fn acknowledge_wakeup(&self) {
        self.event_wakeup.pending.store(false, Ordering::Release);
    }

    pub fn event_fd(&self) -> RawFd {
        self.event_wakeup.fd.as_raw_fd()
    }

    pub fn drain_notification(&self) -> io::Result<()> {
        consume_notification(self.event_fd()).map(|_| ())
    }

    #[cfg(all(test, feature = "tui"))]
    pub(crate) fn io_cpu_clock(&self) -> libc::clockid_t {
        use std::os::unix::thread::JoinHandleExt;
        let mut clock = 0;
        assert_eq!(
            unsafe {
                libc::pthread_getcpuclockid(
                    self.reader.as_ref().unwrap().as_pthread_t(),
                    &mut clock,
                )
            },
            0
        );
        clock
    }

    #[cfg(feature = "tui")]
    pub(crate) fn shutdown(mut self) -> JoinHandle<()> {
        let _ = self.send(Message::Shutdown);
        self.reader.take().unwrap()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = self.send(Message::Shutdown);
            self.sender.take();
            if reader.join().is_err() {
                eprintln!("terminal IO thread panicked");
            }
        }
    }
}

// Field order also closes the PTY before child cleanup if thread startup or execution unwinds.
struct PtyLoop {
    master: OwnedFd,
    process_metadata: Arc<Mutex<Arc<ProcessMetadata>>>,
    input_bytes: Arc<AtomicUsize>,
    process: ChildGuard,
    wakeup: Arc<Wakeup>,
    receiver: mpsc::Receiver<Message>,
    core: Arc<Mutex<Core>>,
    proxy: EventProxy,
    #[cfg(feature = "tui")]
    _integration: Option<crate::shell_integration::Integration>,
}

fn process_refresh_time(last: Option<Instant>) -> Instant {
    let soon = Instant::now() + Duration::from_millis(100);
    last.map_or(soon, |last| soon.max(last + Duration::from_secs(1)))
}

impl PtyLoop {
    fn run(mut self) {
        let result = self.io_loop();
        drop(self.master);
        if let Err(error) = result {
            self.proxy.send(Event::Error(error));
        }
        match terminate_child(&mut self.process.child, self.process.pidfd.as_raw_fd()) {
            Ok(status) => self.proxy.send(Event::Exit(status)),
            Err(error) => self.proxy.send(Event::Error(error)),
        }
        // Keep Core owned here until its frontend drops, even after natural EOF.
        // Async shutdown drops the frontend Core before disconnecting this channel.
        while self.receiver.recv().is_ok() {}
    }

    fn sample_process(&self) {
        // Keep cloning and comparison outside the lock shared with workspace readers.
        let current = self.process_metadata.lock().unwrap().clone();
        let mut metadata = ProcessMetadata {
            launch_argv: current.launch_argv.clone(),
            child_pid: current.child_pid,
            ..ProcessMetadata::default()
        };
        let pgid = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        if pgid < 0 {
            metadata.unavailable.push(format!(
                "foreground process group: {}",
                io::Error::last_os_error()
            ));
        } else {
            metadata.foreground_pgid = Some(pgid as u32);
            let directory = PathBuf::from(format!("/proc/{pgid}"));
            match std::fs::read(directory.join("cmdline")) {
                Ok(bytes) => {
                    metadata.foreground_pid = Some(pgid as u32);
                    metadata.foreground_argv = Some(if bytes.is_empty() {
                        Vec::new()
                    } else {
                        bytes
                            .strip_suffix(&[0])
                            .unwrap_or(&bytes)
                            .split(|byte| *byte == 0)
                            .map(|arg| String::from_utf8_lossy(arg).into_owned())
                            .collect()
                    });
                }
                Err(error) => metadata
                    .unavailable
                    .push(format!("foreground argv: {error}")),
            }
            match std::fs::read_link(directory.join("exe")) {
                Ok(path) => metadata.foreground_executable = Some(path),
                Err(error) => metadata
                    .unavailable
                    .push(format!("foreground executable: {error}")),
            }
            match std::fs::read_link(directory.join("cwd")) {
                Ok(path) => metadata.foreground_cwd = Some(path),
                Err(error) => metadata
                    .unavailable
                    .push(format!("foreground cwd: {error}")),
            }
        }
        metadata.sampled_at_ms = current.sampled_at_ms;
        let changed = metadata != *current;
        metadata.sampled_at_ms = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        );
        let metadata = Arc::new(metadata);
        *self.process_metadata.lock().unwrap() = metadata;
        if changed {
            self.proxy.send(Event::Wakeup);
        }
    }

    fn io_loop(&mut self) -> io::Result<()> {
        let mut parser = crate::vt::Parser::new();
        let mut buffer = [0; 8 * 1024];
        let mut pending = VecDeque::new();
        let mut offset = 0;
        let mut write_open = true;
        let mut write_ready = true;
        let mut child_exited = false;
        let mut process_deadline = Some(Instant::now());
        let mut process_sampled = None;
        #[cfg(feature = "tui")]
        let mut history_notified = true;
        loop {
            let mut resize = None;
            while let Ok(message) = self.receiver.try_recv() {
                match message {
                    Message::Input(data) => {
                        pending.push_back(data);
                        process_deadline
                            .get_or_insert_with(|| process_refresh_time(process_sampled));
                    }
                    Message::Resize(size) => resize = Some(size),
                    Message::Shutdown => return Ok(()),
                }
            }
            if let Some(size) = resize {
                set_size(self.master.as_raw_fd(), size)?;
                self.proxy.send(Event::Wakeup);
            }
            let consumed = if write_open && write_ready {
                let (open, consumed) = flush(self.master.as_raw_fd(), &mut pending, &mut offset)?;
                write_open = open;
                write_ready = pending.is_empty();
                consumed
            } else if write_open {
                0
            } else {
                let discarded = pending.iter().map(|data| data.len()).sum::<usize>() - offset;
                pending.clear();
                offset = 0;
                discarded
            };
            if consumed != 0 {
                let queued = self.input_bytes.fetch_sub(consumed, Ordering::AcqRel);
                let low_watermark = INPUT_CAPACITY * 3 / 4;
                if queued > low_watermark && queued - consumed <= low_watermark {
                    self.proxy.send(Event::Wakeup);
                }
            }
            #[cfg(feature = "tui")]
            let (history_fd, history_ready) = {
                let mut core = self
                    .core
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                if core.poll_history(history_notified)? {
                    self.proxy.send(Event::Wakeup);
                }
                history_notified = false;
                let ready = core.history_read_ready();
                if ready && parser.has_queued() {
                    let committed = parser.advance_queued(&mut core, &[]);
                    let events = core.take_events();
                    drop(core);
                    self.dispatch(events, &mut pending);
                    if committed {
                        self.proxy.send(Event::Wakeup);
                    }
                    continue;
                }
                (core.history_event_fd().unwrap_or(-1), ready)
            };
            #[cfg(not(feature = "tui"))]
            let history_ready = true;
            if process_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                self.sample_process();
                process_sampled = Some(Instant::now());
                process_deadline = None;
            }
            let sync_deadline = parser.deadline();
            if sync_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                self.stop_sync(&mut parser, &mut pending)?;
                continue;
            }
            let deadline = sync_deadline.into_iter().chain(process_deadline).min();
            let pty_events = if history_ready { libc::POLLIN } else { 0 }
                | if write_open && !pending.is_empty() {
                    libc::POLLOUT
                } else {
                    0
                };
            let mut fds = [
                libc::pollfd {
                    // HUP is reported even with events=0; omit a backpressured PTY.
                    fd: if pty_events == 0 {
                        -1
                    } else {
                        self.master.as_raw_fd()
                    },
                    events: pty_events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wakeup.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if child_exited {
                        -1
                    } else {
                        self.process.pidfd.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                #[cfg(feature = "tui")]
                libc::pollfd {
                    fd: history_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = if child_exited && history_ready {
                // Drain buffered output without waiting for descendants which keep the slave open.
                0
            } else {
                deadline.map_or(-1, |deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_nanos()
                        .div_ceil(1_000_000)
                        .min(i32::MAX as u128) as i32
                })
            };
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            write_ready |= fds[0].revents & libc::POLLOUT != 0;
            #[cfg(feature = "tui")]
            {
                history_notified = fds[3].revents & libc::POLLIN != 0;
            }
            if fds[1].revents & libc::POLLIN != 0 {
                // Resetting first could consume a new wakeup and leave it suppressed.
                consume_notification(self.wakeup.fd.as_raw_fd())?;
                self.wakeup.pending.store(false, Ordering::Release);
                continue;
            }
            if fds[2].revents & libc::POLLIN != 0 {
                child_exited = true;
            }
            #[cfg(feature = "tui")]
            if history_notified {
                continue;
            }
            if history_ready
                && (fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                    || child_exited)
            {
                let count = unsafe {
                    libc::read(
                        self.master.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if count == 0 {
                    break;
                }
                if count < 0 {
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock if child_exited => break,
                        io::ErrorKind::WouldBlock => continue,
                        _ if error.raw_os_error() == Some(libc::EIO) => break,
                        _ => return Err(error),
                    }
                }
                process_deadline.get_or_insert_with(|| process_refresh_time(process_sampled));
                let (committed, events) = {
                    let mut core = self
                        .core
                        .lock()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    #[cfg(feature = "tui")]
                    let committed = parser.advance_queued(&mut core, &buffer[..count as usize]);
                    #[cfg(not(feature = "tui"))]
                    let committed = parser.advance(&mut core, &buffer[..count as usize]);
                    (committed, core.take_events())
                };
                self.dispatch(events, &mut pending);
                // Bytes held entirely in the sync buffer have not changed the core.
                if committed {
                    self.proxy.send(Event::Wakeup);
                }
            }
        }
        self.stop_sync(&mut parser, &mut pending)?;
        #[cfg(feature = "tui")]
        while parser.has_queued() {
            while let Ok(message) = self.receiver.try_recv() {
                match message {
                    Message::Shutdown => return Ok(()),
                    Message::Input(data) => {
                        self.input_bytes.fetch_sub(data.len(), Ordering::AcqRel);
                    }
                    Message::Resize(_) => (),
                }
            }
            let (ready, history_fd, committed, events) = {
                let mut core = self
                    .core
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                core.poll_history(true)?;
                let ready = core.history_read_ready();
                let committed = ready && parser.advance_queued(&mut core, &[]);
                (
                    ready,
                    core.history_event_fd().unwrap_or(-1),
                    committed,
                    core.take_events(),
                )
            };
            self.dispatch(events, &mut pending);
            if committed {
                self.proxy.send(Event::Wakeup);
            }
            if !ready {
                let mut fds = [
                    libc::pollfd {
                        fd: history_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.wakeup.fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                if fds[1].revents & libc::POLLIN != 0 {
                    consume_notification(self.wakeup.fd.as_raw_fd())?;
                    self.wakeup.pending.store(false, Ordering::Release);
                }
            }
        }
        Ok(())
    }

    fn stop_sync(
        &self,
        parser: &mut crate::vt::Parser,
        pending: &mut VecDeque<Cow<'static, [u8]>>,
    ) -> io::Result<()> {
        if parser.deadline().is_some() {
            let events = {
                let mut core = self
                    .core
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                #[cfg(feature = "tui")]
                parser.finish_sync_queued(&mut core);
                #[cfg(not(feature = "tui"))]
                parser.finish_sync(&mut core);
                core.take_events()
            };
            self.dispatch(events, pending);
            self.proxy.send(Event::Wakeup);
        }
        Ok(())
    }

    fn dispatch(&self, events: Vec<CoreEvent>, pending: &mut VecDeque<Cow<'static, [u8]>>) {
        for event in events {
            match event {
                CoreEvent::Write(data) if !data.is_empty() => {
                    self.input_bytes.fetch_add(data.len(), Ordering::AcqRel);
                    pending.push_back(data.into());
                }
                CoreEvent::Write(_) => (),
                CoreEvent::Title(title) => self.proxy.send(Event::Title(title)),
                CoreEvent::ClipboardStore { primary, text } => {
                    self.proxy.send(Event::ClipboardStore { primary, text })
                }
                CoreEvent::HistoryCleared => self.proxy.send(Event::Wakeup),
                #[cfg(feature = "tui")]
                CoreEvent::HistoryError(error) => self.proxy.send(Event::Error(error)),
            }
        }
    }
}

struct ChildGuard {
    child: Child,
    pidfd: OwnedFd,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Err(error) = terminate_child(&mut self.child, self.pidfd.as_raw_fd()) {
            eprintln!("cannot reap terminal child: {error}");
        }
    }
}

fn terminate_child(child: &mut Child, pidfd: RawFd) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    // The unreaped session leader pins this process group ID throughout shutdown.
    if unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGHUP) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        let mut pfd = libc::pollfd {
            fd: pidfd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result > 0 {
            return child.wait();
        }
        if let Err(error) = child.kill()
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(error);
        }
        return child.wait();
    }
}

fn flush(
    fd: RawFd,
    pending: &mut VecDeque<Cow<'static, [u8]>>,
    offset: &mut usize,
) -> io::Result<(bool, usize)> {
    const BUDGET: usize = 64 * 1024;
    let mut budget = BUDGET;
    while budget != 0 {
        let Some(data) = pending.front() else { break };
        let count = if pending.len() == 1 {
            let bytes = &data[*offset..data.len().min(*offset + budget)];
            unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) }
        } else {
            let mut vectors = [libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }; 16];
            let mut remaining = budget;
            let mut count = 0;
            for (index, data) in pending.iter().take(vectors.len()).enumerate() {
                let start = if index == 0 { *offset } else { 0 };
                let length = (data.len() - start).min(remaining);
                vectors[count] = libc::iovec {
                    iov_base: data[start..].as_ptr().cast_mut().cast(),
                    iov_len: length,
                };
                count += 1;
                remaining -= length;
                if remaining == 0 {
                    break;
                }
            }
            unsafe { libc::writev(fd, vectors.as_ptr(), count as libc::c_int) }
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break,
                _ if error.raw_os_error() == Some(libc::EIO) => {
                    return Ok((false, BUDGET - budget));
                }
                _ => return Err(error),
            }
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PTY write returned zero",
            ));
        }
        *offset += count as usize;
        budget -= count as usize;
        while let Some(data) = pending.front() {
            if *offset < data.len() {
                break;
            }
            *offset -= data.len();
            pending.pop_front();
        }
    }
    Ok((true, BUDGET - budget))
}

fn fd(value: i32) -> io::Result<OwnedFd> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(value) })
    }
}

fn winsize(size: WindowSize) -> io::Result<libc::winsize> {
    if size.num_cols == 0 || size.num_lines == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal size must be at least 1×1",
        ));
    }
    Ok(libc::winsize {
        ws_row: size.num_lines,
        ws_col: size.num_cols,
        ws_xpixel: size.num_cols.checked_mul(size.cell_width).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal pixel width exceeds 65535",
            )
        })?,
        ws_ypixel: size
            .num_lines
            .checked_mul(size.cell_height)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "terminal pixel height exceeds 65535",
                )
            })?,
    })
}

fn set_size(fd: RawFd, size: libc::winsize) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_pty(size: libc::winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = fd(unsafe {
        libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
    })?;
    if unsafe { libc::grantpt(master.as_raw_fd()) } == -1
        || unsafe { libc::unlockpt(master.as_raw_fd()) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let slave = fd(unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })?;
    set_size(slave.as_raw_fd(), size)?;
    let mut settings = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) } == -1 {
        return Err(io::Error::last_os_error());
    }
    settings.c_iflag |= libc::IUTF8;
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

fn default_shell() -> io::Result<OsString> {
    if let Some(shell) = std::env::var_os("SHELL") {
        return Ok(shell);
    }
    let mut buffer = vec![0; 4096];
    loop {
        let mut record = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut result = std::ptr::null_mut();
        let error = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut record,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no passwd entry for current user",
            ));
        }
        return Ok(OsString::from_vec(
            unsafe { CStr::from_ptr(record.pw_shell) }
                .to_bytes()
                .to_vec(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{Flags, Mode};

    use super::*;

    fn size(lines: u16, columns: u16) -> WindowSize {
        WindowSize {
            num_lines: lines,
            num_cols: columns,
            cell_width: 8,
            cell_height: 16,
        }
    }

    fn shell(script: &str) -> Shell {
        Shell::new("/bin/sh".into(), vec!["-c".into(), script.into()])
    }

    fn session(script: &str) -> Terminal {
        Terminal::new(Some(shell(script)), None, size(24, 80), 0xd8dee9, 0x14161b).unwrap()
    }

    fn content(terminal: &Terminal) -> String {
        let core = terminal.core.lock().unwrap();
        (0..core.screen_lines())
            .map(|row| {
                let row = core.row(row);
                let mut text = String::new();
                for (column, cell) in row.cells.iter().enumerate() {
                    if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        text.push(cell.c);
                        if let Some(extra) = row.extra(column) {
                            text.extend(extra.zerowidth.iter());
                        }
                    }
                }
                text.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_for(terminal: &Terminal, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !content(terminal).contains(expected) {
            assert!(
                Instant::now() < deadline,
                "missing {expected:?}; grid={:?}",
                content(terminal)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn exit_status(terminal: &Terminal) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match terminal.events.try_recv() {
                Ok(Event::Exit(status)) => return status,
                Ok(Event::Error(error)) => panic!("PTY error: {error}"),
                _ => (),
            }
            assert!(
                Instant::now() < deadline,
                "shell did not exit; grid={:?}",
                content(terminal)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn wakeups_are_merged_without_dropping_other_events() {
        let (sender, receiver) = mpsc::channel();
        let proxy = EventProxy {
            sender,
            wakeup: Arc::new(Wakeup {
                fd: crate::eventfd::new().unwrap(),
                pending: AtomicBool::new(false),
            }),
        };
        for _ in 0..100 {
            proxy.send(Event::Wakeup);
        }
        proxy.send(Event::Title(Some("shell".into())));
        assert!(matches!(receiver.try_recv(), Ok(Event::Wakeup)));
        assert!(matches!(receiver.try_recv(), Ok(Event::Title(Some(title))) if title == "shell"));
        assert!(receiver.try_recv().is_err());
        proxy.wakeup.pending.store(false, Ordering::Release);
        proxy.send(Event::Wakeup);
        assert!(matches!(receiver.try_recv(), Ok(Event::Wakeup)));
    }

    #[test]
    fn foreground_metadata_tracks_a_real_shell_job_without_polling_idle_sessions() {
        let terminal = Terminal::new(
            Some(Shell::new("/bin/sh".into(), vec!["-i".into()])),
            Some(std::env::temp_dir()),
            size(24, 80),
            0xffffff,
            0,
        )
        .unwrap();
        let wait = |predicate: &dyn Fn(&ProcessMetadata) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let metadata = terminal.process_metadata();
                if predicate(&metadata) {
                    return metadata;
                }
                assert!(
                    Instant::now() < deadline,
                    "metadata did not update: {metadata:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let initial = wait(&|m| m.foreground_pid == Some(m.child_pid) && m.unavailable.is_empty());
        assert_eq!(initial.launch_argv, ["/bin/sh", "-i"]);
        assert_eq!(initial.foreground_cwd, Some(std::env::temp_dir()));
        terminal.write(b"/bin/sleep 30\n".as_slice()).unwrap();
        let job = wait(&|m| {
            m.foreground_executable
                .as_ref()
                .is_some_and(|p| p.file_name().is_some_and(|n| n == "sleep"))
        });
        assert_ne!(job.foreground_pid, Some(job.child_pid));
        assert_eq!(job.foreground_pid, job.foreground_pgid);
        assert_eq!(job.foreground_argv.as_ref().unwrap(), &["/bin/sleep", "30"]);
        assert_eq!(job.foreground_cwd, Some(std::env::temp_dir()));
        assert!(job.unavailable.is_empty());
        terminal.write(b"\x03".as_slice()).unwrap();
        wait(&|m| m.foreground_pid == Some(m.child_pid));
        std::thread::sleep(Duration::from_millis(1200));
        let idle = terminal.process_metadata();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            idle.sampled_at_ms,
            terminal.process_metadata().sampled_at_ms
        );
    }

    #[test]
    fn real_pty_preserves_utf8_resize_and_exit_status() {
        let terminal = session(
            "stty -echo; printf 'READY\\r\\n'; IFS= read -r line; printf 'ECHO:%s\\r\\n' \"$line\"; stty size; exit 7",
        );
        wait_for(&terminal, "READY");
        terminal.resize(size(14, 42)).unwrap();
        terminal.resize(size(13, 41)).unwrap();
        terminal.resize(size(12, 40)).unwrap();
        terminal.write(Vec::new()).unwrap();
        terminal
            .write("你好 terminal\n".as_bytes().to_vec())
            .unwrap();
        assert_eq!(exit_status(&terminal).code(), Some(7));
        assert!(content(&terminal).contains("12 40"));
        assert!(content(&terminal).contains("ECHO:你好 terminal"));
        let core = terminal.core.lock().unwrap();
        assert_eq!(core.columns(), 40);
        assert_eq!(core.screen_lines(), 12);
    }

    #[test]
    fn real_pty_restores_main_screen_after_tui_exit() {
        let terminal = session(
            "stty -echo; printf 'MAIN\\033[?1049h\\033[?1h\\033[31mTUI界\\033[0m'; read line; printf '\\033[?1l\\033[?1049lDONE'",
        );
        wait_for(&terminal, "TUI界");
        {
            let core = terminal.core.lock().unwrap();
            assert!(core.mode().contains(Mode::ALT_SCREEN));
            assert!(core.mode().contains(Mode::APP_CURSOR));
        }
        terminal.write(b"\n".as_slice()).unwrap();
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("MAINDONE"));
        let core = terminal.core.lock().unwrap();
        assert!(!core.mode().contains(Mode::ALT_SCREEN));
        assert!(!core.mode().contains(Mode::APP_CURSOR));
    }

    #[test]
    fn missing_working_directory_and_command_report_original_errors() {
        use std::error::Error as _;
        let result = Terminal::new(
            None,
            Some(PathBuf::from("/proc/nextty-missing")),
            size(24, 80),
            0,
            0,
        );
        assert_eq!(
            result
                .err()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::NotFound
        );
        let result = Terminal::new(
            Some(Shell::new("/proc/nextty-missing".into(), vec![])),
            None,
            size(24, 80),
            0,
            0,
        );
        assert_eq!(
            result
                .err()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn drop_reaps_normal_and_hup_resistant_shells() {
        for script in [
            "printf READY; read line",
            "trap '' HUP; printf READY; while :; do :; done",
        ] {
            let terminal = session(script);
            wait_for(&terminal, "READY");
            let pid = terminal.child_pid;
            let started = Instant::now();
            drop(terminal);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "shutdown took {:?}",
                started.elapsed()
            );
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "child {pid} was not reaped"
            );
        }
    }

    #[test]
    fn queued_input_preserves_bytes_across_vectored_short_writes() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    writer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let mut pending: VecDeque<Cow<'static, [u8]>> = (0..64)
            .map(|index| Cow::Owned(vec![index as u8; if index % 2 == 0 { 31 } else { 4001 }]))
            .collect();
        let mut offset = 7;
        let expected: Vec<_> = pending
            .iter()
            .flat_map(|data| data.iter().copied())
            .skip(offset)
            .collect();
        let mut actual = Vec::new();
        let mut written = 0;
        while !pending.is_empty() {
            let (open, count) = flush(writer.as_raw_fd(), &mut pending, &mut offset).unwrap();
            assert!(open);
            assert!(count > 0);
            assert!(count <= 64 * 1024);
            written += count;
            assert_eq!(
                pending.iter().map(|data| data.len()).sum::<usize>() - offset,
                expected.len() - written
            );
            reader
                .by_ref()
                .take(count as u64)
                .read_to_end(&mut actual)
                .unwrap();
        }
        assert_eq!(actual, expected);
        assert_eq!(offset, 0);
    }

    #[test]
    fn large_input_survives_partial_nonblocking_writes() {
        let terminal = session(
            "stty raw -echo; printf READY; count=$(dd bs=65536 count=4 iflag=fullblock 2>/dev/null | wc -c); printf 'COUNT:%s' \"$count\"",
        );
        wait_for(&terminal, "READY");
        for _ in 0..4 {
            terminal.write(vec![b'x'; 65536]).unwrap();
        }
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("COUNT:262144"));
    }

    #[test]
    fn successive_input_bursts_do_not_lose_wakeups() {
        let terminal = session(
            r#"stty raw -echo; exec /usr/bin/python3 -c '
import os
os.write(1, b"READY")
for count in range(1, 65):
    data = b""
    while len(data) < count:
        data += os.read(0, count - len(data))
    assert data == b"x" * count
    os.write(1, f"\x1b[H\x1b[2K{count:02}".encode())
'"#,
        );
        wait_for(&terminal, "READY");
        for count in 1..=64 {
            for _ in 0..count {
                terminal.write(b"x".as_slice()).unwrap();
                std::thread::yield_now();
            }
            wait_for(&terminal, &format!("{count:02}"));
        }
        assert!(exit_status(&terminal).success());
    }

    #[test]
    fn input_is_serviced_while_the_child_continuously_produces_output() {
        let terminal = session(
            "stty raw -echo; (while :; do printf '%4096s' x; done) & flood=$!; printf '\\033]2;READY\\007'; byte=$(dd bs=1 count=1 2>/dev/null); kill \"$flood\"; wait \"$flood\" 2>/dev/null || :; printf '\\r\\nINPUT:%s' \"$byte\"",
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match terminal
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("continuous output child did not become ready")
            {
                Event::Title(Some(title)) if title == "READY" => break,
                Event::Error(error) => panic!("PTY error: {error}"),
                Event::Exit(_) => panic!("continuous output child exited before input"),
                _ => (),
            }
        }
        terminal.write(b"x".as_slice()).unwrap();
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("INPUT:x"));
    }

    #[test]
    fn protocol_replies_do_not_require_ui_event_delivery() {
        let terminal = session(
            "stty raw -echo; printf '\\033[6n'; reply=$(dd bs=6 count=1 iflag=fullblock 2>/dev/null | od -An -tx1); printf 'REPLY:%s' \"$reply\"",
        );
        wait_for(&terminal, "REPLY: 1b 5b 31 3b 31 52");
        assert!(exit_status(&terminal).success());
    }

    #[test]
    fn repeated_protocol_replies_leave_bounded_input_available_and_fully_accounted() {
        let script = r#"import os,tty
tty.setraw(0)
def response(ending):
    data=b''
    while not data.endswith(ending):
        data+=os.read(0,1)
    return data
for _ in range(20):
    os.write(1,b'\x1b[6n')
    assert response(b'R')==b'\x1b[1;1R'
    os.write(1,b'\x1b[c')
    assert response(b'c')==b'\x1b[?6c'
    os.write(1,b'\x1b]4;1;?\x07')
    palette=response(b'\x07')
    assert palette.startswith(b'\x1b]4;1;rgb:')
queries=(b'\x1b[6n\x1b[c\x1b]4;1;?\x07')*128
os.write(1,queries+b'\x1b]2;batch-middle\x07'+queries)
expected=(b'\x1b[1;1R\x1b[?6c'+palette)*256
received=b''
while len(received)<len(expected):
    received+=os.read(0,len(expected)-len(received))
assert received==expected
os.write(1,b'QUERIES-DONE')
assert os.read(0,1)==b'x'
os.write(1,b'\r\nINPUT:x')
"#;
        let terminal = Terminal::new(
            Some(Shell::new(
                "python3".into(),
                vec!["-c".into(), script.into()],
            )),
            None,
            size(24, 80),
            0xd8dee9,
            0x14161b,
        )
        .unwrap();
        // No UI events are consumed while the child waits for the protocol responses.
        wait_for(&terminal, "QUERIES-DONE");
        assert_eq!(terminal.input_pending_bytes(), 0);
        terminal.try_write(b"x".as_slice()).unwrap();
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("INPUT:x"));
        assert_eq!(terminal.input_pending_bytes(), 0);
    }

    #[test]
    fn incomplete_synchronized_output_flushes_on_deadline_and_eof() {
        let terminal = session("stty -echo; printf '\\033[?2026hSYNC'; read line");
        wait_for(&terminal, "SYNC");
        terminal.write(b"\n".as_slice()).unwrap();
        assert!(exit_status(&terminal).success());
        let terminal = session("printf '\\033[?2026hFINAL'");
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("FINAL"));
    }

    #[test]
    fn drains_last_output_before_exit() {
        let terminal = session("printf '%65536sTAIL' X");
        assert!(exit_status(&terminal).success());
        assert!(content(&terminal).contains("XTAIL"));
    }

    #[test]
    fn shell_exit_does_not_wait_for_hup_ignoring_descendants() {
        let started = Instant::now();
        let terminal = session(
            "trap '' HUP; sleep 2 & background=$!; printf '%65536sTAIL:%s' X \"$background\"",
        );
        let status = exit_status(&terminal);
        let elapsed = started.elapsed();
        let text = content(&terminal);
        let descendant: u32 = text.split("TAIL:").nth(1).unwrap().trim().parse().unwrap();
        match fd(unsafe { libc::syscall(libc::SYS_pidfd_open, descendant, 0) as i32 }) {
            Ok(pidfd) => {
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                if result == -1 {
                    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                }
            }
            Err(error) => assert_eq!(error.raw_os_error(), Some(libc::ESRCH)),
        }
        assert!(status.success());
        assert!(
            elapsed < Duration::from_secs(1),
            "waited {elapsed:?} for a background descendant"
        );
    }

    #[test]
    fn controlling_terminal_delivers_interrupt_to_shell() {
        let terminal = session(
            "stty -echo; trap 'printf INTERRUPTED; exit 0' INT; printf READY >/dev/tty; while :; do read line; done",
        );
        wait_for(&terminal, "READY");
        terminal.write(b"\x03".as_slice()).unwrap();
        let status = exit_status(&terminal);
        assert!(
            status.success(),
            "{status}; content={:?}",
            content(&terminal)
        );
        assert!(content(&terminal).contains("INTERRUPTED"));
    }
}

#[cfg(all(test, feature = "tui"))]
mod history_pty_tests {
    use super::*;

    #[test]
    fn quiet_copy_completion_and_resize_retry_wake_the_consumer_without_damaging_rows() {
        use crate::core::{Point, SelectionType, Side};
        use crate::history::{HistoryBudget, HistoryConfig};
        use std::io::Read;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        std::fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!("copy-wakeup-{}", std::process::id()));
        let budget = HistoryBudget::new(directory.clone(), 8 * 1024 * 1024).unwrap();
        let terminal = Terminal::new_with_history(
            Some(Shell::new(
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "stty raw -echo; printf READY; exec /bin/cat".into(),
                ],
            )),
            None,
            WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 8,
                cell_height: 16,
            },
            0xffffff,
            0,
            HistoryConfig {
                budget,
                hot_rows: 8,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // Reading the channel first can consume an event before its notification is written.
        loop {
            let mut fd = libc::pollfd {
                fd: terminal.event_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 1000) }, 1);
            terminal.drain_notification().unwrap();
            terminal.acknowledge_wakeup();
            for event in terminal.events.try_iter() {
                assert!(!matches!(event, Event::Error(_) | Event::Exit(_)));
            }
            if terminal.core.lock().unwrap().row(0).cells[0].c == 'R' {
                break;
            }
        }
        let wait = || {
            let mut fd = libc::pollfd {
                fd: terminal.event_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 1000) }, 1);
            assert_ne!(fd.revents & libc::POLLIN, 0);
            terminal.drain_notification().unwrap();
            terminal.acknowledge_wakeup();
            let mut woke = false;
            for event in terminal.events.try_iter() {
                assert!(!matches!(event, Event::Error(_) | Event::Exit(_)));
                woke |= matches!(event, Event::Wakeup);
            }
            assert!(woke);
        };
        {
            let mut core = terminal.core.lock().unwrap();
            core.selection_start(
                Point { line: 0, column: 0 },
                Side::Left,
                SelectionType::Simple,
            );
            core.selection_update(Point { line: 0, column: 4 }, Side::Right);
        }
        for resize in [false, true] {
            let mut core = terminal.core.lock().unwrap();
            terminal.drain_notification().unwrap();
            terminal.acknowledge_wakeup();
            terminal.events.try_iter().for_each(drop);
            if resize {
                core.resize(WindowSize {
                    num_cols: 40,
                    num_lines: 24,
                    cell_width: 8,
                    cell_height: 16,
                });
                assert!(
                    matches!(core.copy_selection(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
                );
                drop(core);
                loop {
                    wait();
                    core = terminal.core.lock().unwrap();
                    if !core.history_loading() {
                        break;
                    }
                    drop(core);
                    assert!(Instant::now() < deadline, "resize did not complete");
                }
                assert_eq!(core.columns(), 40);
            }
            core.reset_damage();
            // Complete the copy while the PTY thread cannot acquire Core. This
            // covers notification consumption on the next iteration as well as poll.
            let mut copy = core
                .copy_selection()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
            assert!(!copy.incomplete);
            let mut text = String::new();
            copy.file.read_to_string(&mut text).unwrap();
            assert_eq!(text, "READY");
            drop(core);
            wait();
            assert!(terminal.core.lock().unwrap().dirty_rows().next().is_none());
        }
        drop(terminal);
        assert!(!directory.exists());
    }

    #[test]
    fn child_eof_commits_a_large_unterminated_sync_frame_with_zero_pixel_metrics() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/workspace-history");
        std::fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "eof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let budget =
            crate::history::HistoryBudget::new(directory.clone(), 16 * 1024 * 1024).unwrap();
        let shell = Shell::new(
            "python3".into(),
            vec![
                "-c".into(),
                "import os; os.write(1,b'\\x1b[?2026h'+b'x\\r\\n'*20000+b'LAST')".into(),
            ],
        );
        let terminal = Terminal::new_with_history(
            Some(shell),
            None,
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
            crate::history::HistoryConfig {
                budget,
                hot_rows: 8,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
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
        assert_eq!(core.history_size(), 19998);
        assert!(!core.mode().contains(crate::core::Mode::SYNC_UPDATE));
        assert_eq!(core.row(2).cells[0].c, 'L');
        drop(core);
        drop(terminal);
        assert!(!directory.exists());
    }
}
