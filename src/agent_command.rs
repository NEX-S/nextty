use crate::eventfd::consume_notification;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

struct Process {
    child: Child,
    stopped: bool,
}

impl Process {
    fn stop(&mut self) -> io::Result<ExitStatus> {
        if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let status = self.child.wait()?;
        self.stopped = true;
        Ok(status)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

pub(crate) fn run(
    command: &str,
    cwd: Option<&Path>,
    wake: RawFd,
    canceled: &AtomicBool,
) -> io::Result<(String, bool)> {
    execute(command, cwd, wake, canceled, Some(TIMEOUT))
}

pub(crate) fn monitor(
    command: &str,
    cwd: Option<&Path>,
    wake: RawFd,
    canceled: &AtomicBool,
) -> io::Result<(String, bool)> {
    execute(command, cwd, wake, canceled, None)
}

fn execute(
    command: &str,
    cwd: Option<&Path>,
    wake: RawFd,
    canceled: &AtomicBool,
    timeout: Option<Duration>,
) -> io::Result<(String, bool)> {
    if canceled.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let (mut output, writer) = UnixStream::pair()?;
    output.set_nonblocking(true)?;
    let mut shell = Command::new("/bin/sh");
    shell
        .args(["-c", command])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer.try_clone()?)))
        .stderr(Stdio::from(OwnedFd::from(writer)));
    if let Some(cwd) = cwd {
        shell.current_dir(cwd);
    }
    let parent = unsafe { libc::getpid() };
    unsafe {
        shell.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    if canceled.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut process = Process {
        child: shell.spawn()?,
        stopped: false,
    };
    let pid = process.child.id() as i32;
    if canceled.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    drop(shell);
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let child = unsafe { OwnedFd::from_raw_fd(fd) };
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut eof = false;
    let mut status = None;
    let mut timed_out = false;
    loop {
        if canceled.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) && status.is_none() {
            timed_out = true;
            status = Some(process.stop()?);
        }
        if !eof {
            let mut chunk = [0; 8192];
            for _ in 0..32 {
                if canceled.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if status.is_none() && deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    break;
                }
                match output.read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(count) => {
                        let keep = count.min(OUTPUT_LIMIT - bytes.len());
                        bytes.extend_from_slice(&chunk[..keep]);
                        truncated |= keep < count;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(status) = status {
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if truncated {
                text.push_str("\n[output truncated at 32 KiB]");
            }
            if timed_out {
                text.push_str("\n[timed out after 120 seconds]");
            }
            text.push_str(&format!("\n[{status}]"));
            return Ok((text, timed_out || !status.success()));
        }
        let mut fds = [
            libc::pollfd {
                fd: if eof { -1 } else { output.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: child.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = deadline.map_or(-1, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1) as i32
        });
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[2].revents != 0 {
            consume_notification(wake)?;
        }
        if fds[1].revents != 0 {
            // The shell may leave background descendants holding its output open.
            status = Some(process.stop()?);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventfd::notify;
    use std::sync::Arc;

    fn assert_dead(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) if stat.split_whitespace().nth(2) != Some("Z") => {
                    assert!(Instant::now() < deadline, "process {pid} is still running");
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => break,
            }
        }
    }

    #[test]
    fn captures_both_streams_cwd_exit_and_bounded_output() {
        let wake = crate::eventfd::new().unwrap();
        let canceled = AtomicBool::new(false);
        let (text, error) = run(
            "pwd; printf 'stderr fixture' >&2; exit 7",
            Some(Path::new("/")),
            wake.as_raw_fd(),
            &canceled,
        )
        .unwrap();
        assert!(error);
        assert!(text.starts_with("/\n"));
        assert!(text.contains("stderr fixture"));
        assert!(text.ends_with("[exit status: 7]"));
        let (text, error) = run(
            "head -c 65536 /dev/zero | tr '\\0' x",
            None,
            wake.as_raw_fd(),
            &canceled,
        )
        .unwrap();
        assert!(!error);
        assert_eq!(
            text.bytes().take_while(|&b| b == b'x').count(),
            OUTPUT_LIMIT
        );
        assert!(text.contains("output truncated at 32 KiB"));
    }

    #[test]
    fn cancellation_wakes_idle_command_and_kills_descendants() {
        let wake = Arc::new(crate::eventfd::new().unwrap());
        let canceled = Arc::new(AtomicBool::new(false));
        let path =
            std::env::temp_dir().join(format!("nextty-agent-command-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!(
            "sleep 120 & printf '%s %s' $$ $! > '{}'; wait",
            path.display()
        );
        let worker_wake = Arc::clone(&wake);
        let worker_canceled = Arc::clone(&canceled);
        let worker = std::thread::spawn(move || {
            monitor(&command, None, worker_wake.as_raw_fd(), &worker_canceled)
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let pids = loop {
            assert!(Instant::now() < deadline);
            if let Ok(pids) = std::fs::read_to_string(&path)
                && pids.split_whitespace().count() == 2
            {
                break pids;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let started = Instant::now();
        canceled.store(true, Ordering::Release);
        notify(wake.as_raw_fd()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        for pid in pids.split_whitespace() {
            assert_dead(pid);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn monitor_waits_for_exit_after_output_closes() {
        let wake = crate::eventfd::new().unwrap();
        for code in [0, 7] {
            let (text, error) = monitor(
                &format!("printf ready; exec 1>&- 2>&-; sleep 0.02; exit {code}"),
                None,
                wake.as_raw_fd(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(error, code != 0);
            assert_eq!(text, format!("ready\n[exit status: {code}]"));
        }
    }

    #[test]
    fn shell_exit_cleans_background_processes_without_waiting_for_output_eof() {
        let wake = crate::eventfd::new().unwrap();
        let started = Instant::now();
        let (text, error) = run(
            "sleep 120 & printf '%s\\n' $!",
            None,
            wake.as_raw_fd(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!error);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_dead(text.lines().next().unwrap());
    }
}
