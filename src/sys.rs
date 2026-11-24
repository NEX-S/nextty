use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

pub(crate) fn check(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

pub(crate) fn owned_fd(result: i32) -> io::Result<OwnedFd> {
    check(result).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An anonymous file in memory.
pub(crate) fn memfd(name: &std::ffi::CStr) -> io::Result<std::fs::File> {
    owned_fd(unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) }).map(Into::into)
}

pub(crate) fn new() -> io::Result<OwnedFd> {
    owned_fd(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })
}

/// A `pollfd` that ignores `fd` while no events are wanted.
pub(crate) fn watch(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    }
}

/// A full eventfd counter already means "woken"; any other failure is a bug.
pub(crate) fn notify(fd: RawFd) {
    let value = 1u64;
    if unsafe { libc::write(fd, (&value as *const u64).cast(), 8) } < 0 {
        let error = io::Error::last_os_error();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "notify: {error}");
    }
}

pub(crate) fn consume_notification(fd: RawFd) -> io::Result<bool> {
    let mut value = 0u64;
    match check(unsafe { libc::read(fd, (&raw mut value).cast(), 8) } as i32) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        result => result.map(|_| true),
    }
}

/// Runs `run` on its own thread; the caller learns about its result through a channel it supplies.
pub(crate) fn task(run: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name("task".into())
        .spawn(run)
        .map(drop)
}

pub fn timeout_millis(duration: Duration) -> i32 {
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as i32
}

/// The sooner of a poll timeout (`-1` waits forever) and `deadline`.
pub fn timeout_until(timeout: i32, deadline: Instant) -> i32 {
    let remaining = timeout_millis(deadline.saturating_duration_since(Instant::now()));
    if timeout < 0 {
        remaining
    } else {
        timeout.min(remaining)
    }
}

/// A wait that a signal interrupts starts again with the whole timeout.
pub fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    loop {
        match check(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) }) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            result => return result.map(drop),
        }
    }
}
