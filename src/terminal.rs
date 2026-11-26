use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::ffi::{CStr, OsStr, OsString};
use std::io::{self, Read, Result};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::{Core, Event, WindowSize};
use crate::history::HistoryBudget;
use crate::sys::{check, owned_fd};

pub(crate) const INPUT_CAPACITY: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shell {
    pub program: String,
    pub args: Vec<String>,
}

impl Shell {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessMetadata {
    pub launch_argv: Vec<String>,
    pub child_pid: u32,
    pub has_children: Option<bool>,
    pub foreground_pgid: Option<u32>,
    // The shell's own directory; the foreground process may have changed its own.
    #[serde(with = "crate::wire::path::option")]
    pub child_cwd: Option<PathBuf>,
    #[serde(with = "crate::wire::path::option")]
    pub foreground_executable: Option<PathBuf>,
    pub foreground_argv: Option<Vec<String>>,
    #[serde(with = "crate::wire::path::option")]
    pub foreground_cwd: Option<PathBuf>,
    pub sampled_at_ms: Option<u64>,
    pub unavailable: Vec<String>,
}

pub struct Terminal {
    pub core: Core,
    master: Option<OwnedFd>,
    // Bytes for the child, not yet accepted by the PTY. Dropped once the child's side is gone.
    input: VecDeque<u8>,
    input_open: bool,
    parser: crate::vt::Parser,
    child: Child,
    pidfd: OwnedFd,
    process_metadata: Arc<ProcessMetadata>,
    wake: Arc<OwnedFd>,
    process_results: Option<mpsc::Receiver<(ProcessMetadata, bool)>>,
    // Bytes read from the PTY; `read_pos..read_len` are not parsed yet.
    read_buffer: [u8; 8192],
    read_pos: usize,
    read_len: usize,
    read_open: bool,
    child_exited: bool,
    process_deadline: Option<Instant>,
    process_sampled: Option<Instant>,
    closing: Option<Instant>,
    _integration: Option<crate::shell_integration::Integration>,
}

#[derive(Clone, Copy, Default)]
pub struct Step {
    pub changed: bool,
    pub done: bool,
}

impl Terminal {
    pub fn new(
        shell: Option<Shell>,
        working_directory: Option<PathBuf>,
        size: WindowSize,
        foreground: u32,
        background: u32,
        history: Arc<HistoryBudget>,
    ) -> Result<Self> {
        let mut core = Core::new(size, foreground, background);
        let wake = history.notification.clone();
        core.enable_history(history);
        let (master, slave) = open_pty(winsize(size))?;
        let integrate = shell.is_none();
        let mut command = match shell {
            Some(shell) => {
                let mut command = Command::new(shell.program);
                command.args(shell.args);
                command
            }
            None => Command::new(default_shell()?),
        };
        let working_directory = working_directory.map(std::path::absolute).transpose()?;
        if let Some(directory) = &working_directory {
            command.current_dir(directory).env("PWD", directory);
        }
        let integration = if integrate {
            crate::shell_integration::Integration::prepare(&mut command)?
        } else {
            None
        };
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        unsafe {
            command.pre_exec(|| {
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
                Ok(())
            });
        }
        let launch_argv = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let child = command.spawn()?;
        let child_pid = child.id();
        let pidfd = owned_fd(unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid, 0) as i32 })?;
        Ok(Self {
            core,
            master: Some(master),
            input: VecDeque::new(),
            input_open: true,
            parser: crate::vt::Parser::new(),
            child,
            pidfd,
            wake,
            process_metadata: Arc::new(ProcessMetadata {
                launch_argv,
                child_pid,
                // Known before the first sample, so a new pane reports its directory at once.
                child_cwd: working_directory,
                unavailable: vec!["Foreground process has not been sampled yet".into()],
                ..ProcessMetadata::default()
            }),
            process_results: None,
            read_buffer: [0; 8192],
            read_pos: 0,
            read_len: 0,
            read_open: true,
            child_exited: false,
            process_deadline: Some(Instant::now()),
            process_sampled: None,
            closing: None,
            _integration: integration,
        })
    }

    pub fn pollfds(&self) -> [libc::pollfd; 2] {
        let pty = self.master.as_ref().map_or(-1, AsRawFd::as_raw_fd);
        let events = if self.reading() { libc::POLLIN } else { 0 }
            | if self.input.is_empty() {
                0
            } else {
                libc::POLLOUT
            };
        // A pidfd stays readable after exit, including while history backpressure pauses the PTY.
        let exit = if self.child_exited { 0 } else { libc::POLLIN };
        [
            crate::sys::watch(pty, events),
            crate::sys::watch(self.pidfd.as_raw_fd(), exit),
        ]
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.core
            .sync_deadline()
            .into_iter()
            .chain(self.process_deadline)
            .chain(self.closing)
            .chain(
                (self.child_exited && self.read_open && self.core.history_read_ready())
                    .then(Instant::now),
            )
            .min()
    }
    fn reading(&self) -> bool {
        self.read_open && self.read_pos == self.read_len && self.core.history_read_ready()
    }
    pub fn input_pending_bytes(&self) -> usize {
        self.input.len()
    }
    pub fn process_metadata(&self) -> Arc<ProcessMetadata> {
        Arc::clone(&self.process_metadata)
    }
    /// The directory the shell reported, else the one last sampled from its process.
    pub fn current_directory(&self) -> Option<PathBuf> {
        self.core
            .current_directory()
            .map(Path::to_path_buf)
            .or_else(|| self.process_metadata.child_cwd.clone())
    }
    pub fn write(&mut self, data: impl AsRef<[u8]>) -> io::Result<()> {
        let data = data.as_ref();
        if data.is_empty() {
            return Ok(());
        }
        self.note_activity();
        if self.input_open {
            self.input.extend(data);
            self.flush_input()?;
        }
        Ok(())
    }
    pub fn try_write(&mut self, data: impl AsRef<[u8]>) -> io::Result<()> {
        let data = data.as_ref();
        if data.len() > INPUT_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input chunk exceeds 256 KiB",
            ));
        }
        if self.input.len() + data.len() > INPUT_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "PTY input queue is full",
            ));
        }
        self.write(data)
    }
    fn flush_input(&mut self) -> io::Result<()> {
        let Some(master) = self.master.as_ref().map(AsRawFd::as_raw_fd) else {
            return Ok(());
        };
        while !self.input.is_empty() {
            let bytes = self.input.as_slices().0;
            let count = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
            if count >= 0 {
                self.input.drain(..count as usize);
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => (),
                io::ErrorKind::WouldBlock => break,
                _ if error.raw_os_error() == Some(libc::EIO) => self.close_input(),
                _ => return Err(error),
            }
        }
        Ok(())
    }
    fn close_input(&mut self) {
        self.input_open = false;
        self.input.clear();
    }
    pub fn resize(&mut self, size: WindowSize) -> io::Result<()> {
        if let Some(master) = &self.master {
            set_size(master.as_raw_fd(), winsize(size))?;
        }
        self.core.resize(size);
        Ok(())
    }
    pub fn shutdown(mut self) -> Option<Self> {
        // finish() has already reaped this child; its numeric process group may be reused.
        self.master.as_ref()?;
        self.begin_shutdown();
        Some(self)
    }
    fn begin_shutdown(&mut self) {
        if self.closing.is_none() {
            if let Err(error) = hang_up(&self.child) {
                self.core.events.push(Event::Error(error));
            }
            self.closing = Some(Instant::now() + Duration::from_millis(100));
        }
    }

    pub fn step(&mut self, pty_revents: i16, pidfd_revents: i16) -> io::Result<Step> {
        let mut step = Step::default();
        if let Some(Ok((metadata, changed))) = self.process_results.as_ref().map(|r| r.try_recv()) {
            self.process_results = None;
            step.changed |= changed;
            self.process_metadata = Arc::new(metadata);
            let sampled = Instant::now();
            self.process_sampled = Some(sampled);
            self.process_deadline = (self.process_metadata.has_children == Some(true))
                .then(|| sampled + Duration::from_secs(1));
        }
        if pty_revents & libc::POLLOUT != 0 {
            self.flush_input()?;
        }
        if pidfd_revents & libc::POLLIN != 0 {
            self.child_exited = true;
        }
        step.changed |= self.core.poll_history();
        if self.reading()
            && (self.child_exited
                || pty_revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        {
            self.read_pty()?;
        }
        if self.read_pos < self.read_len {
            let (taken, parsed) = self.parser.advance(
                &mut self.core,
                &self.read_buffer[self.read_pos..self.read_len],
            );
            self.read_pos += taken;
            step.changed |= parsed;
        }
        if self
            .core
            .sync_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
            || (!self.read_open && self.read_pos == self.read_len)
        {
            step.changed |= self.core.finish_sync();
        }
        for event in self.core.take_events() {
            match event {
                Event::Write(data) => self.write(data)?,
                event => self.core.events.push(event),
            }
        }
        let now = Instant::now();
        if self.closing.is_some_and(|deadline| deadline <= now) {
            self.child.kill()?;
            self.finish()?;
            step.done = true;
            return Ok(step);
        }
        if !self.read_open && self.read_pos == self.read_len {
            if self.child_exited {
                self.finish()?;
                step.done = true;
            } else {
                self.begin_shutdown();
            }
            return Ok(step);
        }
        if self
            .process_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.start_process_sample()?;
        }
        Ok(step)
    }

    fn read_pty(&mut self) -> io::Result<()> {
        let count = unsafe {
            libc::read(
                self.master.as_ref().unwrap().as_raw_fd(),
                self.read_buffer.as_mut_ptr().cast(),
                self.read_buffer.len(),
            )
        };
        if count > 0 {
            self.note_activity();
            (self.read_pos, self.read_len) = (0, count as usize);
            return Ok(());
        }
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => return Ok(()),
                io::ErrorKind::WouldBlock if !self.child_exited => return Ok(()),
                io::ErrorKind::WouldBlock => (),
                _ if error.raw_os_error() == Some(libc::EIO) => (),
                _ => return Err(error),
            }
        }
        self.read_open = false;
        self.close_input();
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.master = None;
        let status = self.child.wait()?;
        self.core.events.push(Event::Exit(status));
        Ok(())
    }

    /// Samples the foreground process soon after I/O, but at most once a second.
    fn note_activity(&mut self) {
        if self.process_deadline.is_none() {
            let soon = Instant::now() + Duration::from_millis(100);
            self.process_deadline = Some(
                self.process_sampled
                    .map_or(soon, |last| soon.max(last + Duration::from_secs(1))),
            );
        }
    }

    fn start_process_sample(&mut self) -> io::Result<()> {
        self.process_deadline = None;
        let (None, Some(master)) = (&self.process_results, &self.master) else {
            return Ok(());
        };
        // The ioctl cannot block; reading another process's /proc files can.
        let pgid = check(unsafe { libc::tcgetpgrp(master.as_raw_fd()) }).map(|pgid| pgid as u32);
        let current = self.process_metadata.clone();
        let wake = self.wake.clone();
        let (sender, receiver) = mpsc::channel();
        crate::sys::task(move || {
            if sender.send(sample_process(pgid, &current)).is_ok() {
                crate::sys::notify(wake.as_raw_fd());
            }
        })?;
        self.process_results = Some(receiver);
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.master = None;
        if let Err(error) = terminate_child(&mut self.child, self.pidfd.as_raw_fd()) {
            eprintln!("cannot reap terminal child: {error}");
        }
    }
}

fn sample_process(pgid: io::Result<u32>, current: &ProcessMetadata) -> (ProcessMetadata, bool) {
    let mut metadata = ProcessMetadata {
        launch_argv: current.launch_argv.clone(),
        child_pid: current.child_pid,
        sampled_at_ms: current.sampled_at_ms,
        ..ProcessMetadata::default()
    };
    let mut unavailable = Vec::new();
    fn note<T>(unavailable: &mut Vec<String>, what: &str, result: io::Result<T>) -> Option<T> {
        result
            .inspect_err(|error| unavailable.push(format!("{what}: {error}")))
            .ok()
    }
    match pgid {
        Err(error) => unavailable.push(format!("foreground process group: {error}")),
        Ok(pgid) => {
            metadata.foreground_pgid = Some(pgid);
            let exe = std::fs::read_link(format!("/proc/{pgid}/exe"));
            metadata.foreground_executable = note(&mut unavailable, "foreground executable", exe);
            let cmdline = std::fs::read(format!("/proc/{pgid}/cmdline"));
            if let Some(bytes) = note(&mut unavailable, "foreground argv", cmdline) {
                let argv = bytes.split_inclusive(|byte| *byte == 0);
                metadata.foreground_argv = Some(
                    argv.map(|arg| {
                        String::from_utf8_lossy(arg)
                            .trim_end_matches('\0')
                            .to_owned()
                    })
                    .collect(),
                );
            }
            let cwd = std::fs::read_link(format!("/proc/{pgid}/cwd"));
            metadata.foreground_cwd = note(&mut unavailable, "foreground cwd", cwd);
        }
    }
    let pid = metadata.child_pid;
    metadata.has_children = match std::fs::File::open(format!("/proc/{pid}/task/{pid}/children"))
        .and_then(|mut file| file.read_exact(&mut [0]))
    {
        Ok(()) => Some(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Some(false),
        Err(error) => {
            unavailable.push(format!("child processes: {error}"));
            None
        }
    };
    metadata.child_cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    metadata.unavailable = unavailable;
    let changed = metadata != *current;
    metadata.sampled_at_ms = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    (metadata, changed)
}

/// SIGHUP to the child's session, whose process group id is the child's pid.
fn hang_up(child: &Child) -> io::Result<()> {
    if unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGHUP) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

fn terminate_child(child: &mut Child, pidfd: RawFd) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    hang_up(child)?;
    let mut pfd = libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    crate::sys::poll(std::slice::from_mut(&mut pfd), 100)?;
    if pfd.revents == 0 {
        child.kill()?;
    }
    child.wait()
}

fn winsize(size: WindowSize) -> libc::winsize {
    libc::winsize {
        ws_row: size.num_lines,
        ws_col: size.num_cols,
        ws_xpixel: size.num_cols * size.cell_width,
        ws_ypixel: size.num_lines * size.cell_height,
    }
}

fn set_size(fd: RawFd, size: libc::winsize) -> io::Result<()> {
    check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) }).map(drop)
}

fn open_pty(size: libc::winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = owned_fd(unsafe {
        libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
    })?;
    check(unsafe { libc::grantpt(master.as_raw_fd()) })?;
    check(unsafe { libc::unlockpt(master.as_raw_fd()) })?;
    let slave = owned_fd(unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })?;
    set_size(slave.as_raw_fd(), size)?;
    let mut settings = unsafe { std::mem::zeroed::<libc::termios>() };
    check(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) })?;
    settings.c_iflag |= libc::IUTF8;
    check(unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) })?;
    Ok((master, slave))
}

fn default_shell() -> io::Result<OsString> {
    if let Some(shell) = std::env::var_os("SHELL") {
        return Ok(shell);
    }
    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no passwd entry for current user",
        ));
    }
    let shell = unsafe { CStr::from_ptr((*entry).pw_shell) };
    Ok(OsStr::from_bytes(shell.to_bytes()).to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::core::Flags;
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    pub(crate) fn drive(terminal: &mut Terminal) {
        let mut fds = terminal.pollfds();
        crate::sys::poll(&mut fds, 0).unwrap();
        terminal.step(fds[0].revents, fds[1].revents).unwrap();
    }

    pub(crate) fn next_event(terminal: &mut Terminal, timeout: Duration) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        let mut events = VecDeque::new();
        loop {
            if let Some(event) = events.pop_front() {
                return Some(event);
            }
            events.extend(terminal.core.take_events());
            if !events.is_empty() {
                continue;
            }
            crate::terminal::tests::drive(terminal);
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub(crate) fn wait_metadata(
        terminal: &mut Terminal,
        predicate: &dyn Fn(&ProcessMetadata) -> bool,
    ) -> Arc<ProcessMetadata> {
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
            crate::terminal::tests::drive(terminal);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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

    pub fn test_terminal(
        shell: Option<Shell>,
        working_directory: Option<PathBuf>,
        size: WindowSize,
        foreground: u32,
        background: u32,
    ) -> Result<Terminal> {
        Terminal::new(
            shell,
            working_directory,
            size,
            foreground,
            background,
            crate::history::tests::budget(8 << 20),
        )
    }

    fn session(script: &str) -> Terminal {
        test_terminal(Some(shell(script)), None, size(24, 80), 0xd8dee9, 0x14161b).unwrap()
    }

    fn content(terminal: &Terminal) -> String {
        let core = &terminal.core;
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

    fn wait_for(terminal: &mut Terminal, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !content(terminal).contains(expected) {
            crate::terminal::tests::drive(terminal);
            assert!(
                Instant::now() < deadline,
                "missing {expected:?}; grid={:?}",
                content(terminal)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn exit_status(terminal: &mut Terminal) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match next_event(terminal, deadline.saturating_duration_since(Instant::now())) {
                Some(Event::Exit(status)) => return status,
                Some(Event::Error(error)) => panic!("PTY error: {error}"),
                Some(_) => (),
                None => panic!("shell did not exit; grid={:?}", content(terminal)),
            }
        }
    }

    #[test]
    fn process_metadata_roundtrips_non_utf8_paths() {
        let path = |bytes: &[u8]| Some(PathBuf::from(OsString::from_vec(bytes.to_vec())));
        let metadata = ProcessMetadata {
            foreground_executable: path(b"/bin/\xff"),
            foreground_cwd: path(b"/tmp/\xfe"),
            ..ProcessMetadata::default()
        };
        let bytes = postcard::to_allocvec(&metadata).unwrap();
        assert_eq!(
            postcard::from_bytes::<ProcessMetadata>(&bytes).unwrap(),
            metadata
        );
    }

    #[test]
    fn explicit_working_directory_sets_pwd_before_shell_startup() {
        for directory in [
            PathBuf::from("target"),
            std::env::current_dir().unwrap().join("target"),
        ] {
            let mut terminal = test_terminal(
                Some(Shell::new(
                    "/usr/bin/python3".into(),
                    vec![
                        "-c".into(),
                        "import os; assert os.environ['PWD'] == os.getcwd()".into(),
                    ],
                )),
                Some(directory),
                size(24, 80),
                0xd8dee9,
                0x14161b,
            )
            .unwrap();
            assert!(
                exit_status(&mut terminal).success(),
                "{}",
                content(&terminal)
            );
        }
    }

    #[test]
    fn foreground_metadata_tracks_a_real_shell_job_without_polling_idle_sessions() {
        let mut terminal = test_terminal(
            Some(Shell::new("/bin/sh".into(), vec!["-i".into()])),
            Some(std::env::temp_dir()),
            size(24, 80),
            0xffffff,
            0,
        )
        .unwrap();
        let initial = wait_metadata(&mut terminal, &|m| {
            m.foreground_pgid == Some(m.child_pid)
                && m.has_children == Some(false)
                && m.unavailable.is_empty()
        });
        assert_eq!(initial.launch_argv, ["/bin/sh", "-i"]);
        assert_eq!(initial.foreground_cwd, Some(std::env::temp_dir()));
        terminal.write(b"/bin/sleep 30\n".as_slice()).unwrap();
        let job = wait_metadata(&mut terminal, &|m| {
            m.foreground_executable
                .as_ref()
                .is_some_and(|p| p.file_name().is_some_and(|n| n == "sleep"))
        });
        assert_ne!(job.foreground_pgid, Some(job.child_pid));
        assert_eq!(job.has_children, Some(true));
        assert_eq!(job.foreground_argv.as_ref().unwrap(), &["/bin/sleep", "30"]);
        assert_eq!(job.foreground_cwd, Some(std::env::temp_dir()));
        assert!(job.unavailable.is_empty());
        terminal.write(b"\x03".as_slice()).unwrap();
        wait_metadata(&mut terminal, &|m| m.foreground_pgid == Some(m.child_pid));
        assert!(terminal.process_deadline.is_none());
        let idle = terminal.process_metadata();
        drive(&mut terminal);
        assert_eq!(
            idle.sampled_at_ms,
            terminal.process_metadata().sampled_at_ms
        );
        terminal.write(b"exec /bin/sh -i -s\n".as_slice()).unwrap();
        let reexecuted = wait_metadata(&mut terminal, &|metadata| {
            metadata
                .foreground_argv
                .as_ref()
                .is_some_and(|args| args == &["/bin/sh", "-i", "-s"])
        });
        assert_eq!(reexecuted.foreground_pgid, initial.foreground_pgid);
        assert_eq!(
            reexecuted.foreground_executable,
            initial.foreground_executable
        );
        terminal
            .write(b"cd /; exec /bin/sleep 30\n".as_slice())
            .unwrap();
        let replaced = wait_metadata(&mut terminal, &|metadata| {
            metadata
                .foreground_executable
                .as_ref()
                .is_some_and(|path| path.file_name().is_some_and(|name| name == "sleep"))
        });
        assert_eq!(replaced.foreground_pgid, Some(replaced.child_pid));
        assert_eq!(
            replaced.foreground_argv.as_ref().unwrap(),
            &["/bin/sleep", "30"]
        );
        assert_eq!(replaced.foreground_cwd, Some(PathBuf::from("/")));
    }

    #[test]
    fn real_pty_preserves_utf8_resize_and_exit_status() {
        let mut terminal = session(
            "stty -echo; printf 'READY\\r\\n'; IFS= read -r line; printf 'ECHO:%s\\r\\n' \"$line\"; stty size; exit 7",
        );
        wait_for(&mut terminal, "READY");
        terminal.resize(size(14, 42)).unwrap();
        terminal.resize(size(13, 41)).unwrap();
        terminal.resize(size(12, 40)).unwrap();
        terminal.write(Vec::new()).unwrap();
        terminal.write("你好 terminal\n".as_bytes()).unwrap();
        assert_eq!(exit_status(&mut terminal).code(), Some(7));
        assert!(content(&terminal).contains("12 40"));
        assert!(content(&terminal).contains("ECHO:你好 terminal"));
        let core = &terminal.core;
        assert_eq!(core.columns(), 40);
        assert_eq!(core.screen_lines(), 12);
    }

    #[test]
    fn drop_reaps_normal_and_hup_resistant_shells() {
        for script in [
            "printf READY; read line",
            "trap '' HUP; printf READY; while :; do :; done",
        ] {
            let mut terminal = session(script);
            wait_for(&mut terminal, "READY");
            let pid = terminal.child.id();
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
    fn large_input_survives_partial_nonblocking_writes() {
        let mut terminal = session(
            "stty raw -echo; printf READY; count=$(dd bs=65536 count=4 iflag=fullblock 2>/dev/null | wc -c); printf 'COUNT:%s' \"$count\"",
        );
        wait_for(&mut terminal, "READY");
        for _ in 0..4 {
            terminal.write(vec![b'x'; 65536]).unwrap();
        }
        assert!(exit_status(&mut terminal).success());
        assert!(content(&terminal).contains("COUNT:262144"));
    }

    #[test]
    fn input_is_serviced_while_the_child_continuously_produces_output() {
        let mut terminal = session(
            "stty raw -echo; (while :; do printf '%4096s' x; done) & flood=$!; printf '\\033]2;READY\\007'; byte=$(dd bs=1 count=1 2>/dev/null); kill \"$flood\"; wait \"$flood\" 2>/dev/null || :; printf '\\r\\nINPUT:%s' \"$byte\"",
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match crate::terminal::tests::next_event(
                &mut terminal,
                deadline.saturating_duration_since(Instant::now()),
            )
            .expect("continuous output child did not become ready")
            {
                Event::Title(Some(title)) if title == "READY" => break,
                Event::Error(error) => panic!("PTY error: {error}"),
                Event::Exit(_) => panic!("continuous output child exited before input"),
                _ => (),
            }
        }
        terminal.write(b"x".as_slice()).unwrap();
        assert!(exit_status(&mut terminal).success());
        assert!(content(&terminal).contains("INPUT:x"));
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
        let mut terminal = test_terminal(
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
        wait_for(&mut terminal, "QUERIES-DONE");
        assert_eq!(terminal.input_pending_bytes(), 0);
        terminal.try_write(b"x".as_slice()).unwrap();
        assert!(exit_status(&mut terminal).success());
        assert!(content(&terminal).contains("INPUT:x"));
        assert_eq!(terminal.input_pending_bytes(), 0);
    }

    #[test]
    fn incomplete_synchronized_output_flushes_on_deadline_and_eof() {
        let mut terminal = session("stty -echo; printf '\\033[?2026hSYNC'; read line");
        wait_for(&mut terminal, "SYNC");
        terminal.write(b"\n".as_slice()).unwrap();
        assert!(exit_status(&mut terminal).success());
        let mut terminal = session("printf '\\033[?2026hFINAL'");
        assert!(exit_status(&mut terminal).success());
        assert!(content(&terminal).contains("FINAL"));
    }

    #[test]
    fn drains_last_output_before_exit() {
        let mut terminal = session("printf '%65536sTAIL' X");
        assert!(exit_status(&mut terminal).success());
        assert!(content(&terminal).contains("XTAIL"));
    }

    #[test]
    fn shell_exit_does_not_wait_for_hup_ignoring_descendants() {
        let started = Instant::now();
        let mut terminal = session("trap '' HUP; sleep 2 & printf '%65536sTAIL' X");
        assert!(exit_status(&mut terminal).success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn controlling_terminal_delivers_interrupt_to_shell() {
        let mut terminal = session(
            "stty -echo; trap 'printf INTERRUPTED; exit 0' INT; printf READY >/dev/tty; while :; do read line; done",
        );
        wait_for(&mut terminal, "READY");
        terminal.write(b"\x03".as_slice()).unwrap();
        let status = exit_status(&mut terminal);
        assert!(
            status.success(),
            "{status}; content={:?}",
            content(&terminal)
        );
        assert!(content(&terminal).contains("INTERRUPTED"));
    }
}
