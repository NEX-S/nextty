use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

pub(crate) fn new() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn notify(fd: RawFd) -> io::Result<()> {
    loop {
        let value = 1u64;
        if unsafe { libc::write(fd, (&value as *const u64).cast(), 8) } == 8 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(()),
            _ => return Err(error),
        }
    }
}

pub(crate) fn consume_notification(fd: RawFd) -> io::Result<bool> {
    loop {
        let mut value = 0u64;
        if unsafe { libc::read(fd, (&mut value as *mut u64).cast(), 8) } == 8 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(error),
        }
    }
}
