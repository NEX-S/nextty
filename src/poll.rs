use std::io;
use std::time::{Duration, Instant};

pub fn timeout_millis(duration: Duration) -> i32 {
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as i32
}

pub fn poll(fds: &mut [libc::pollfd], mut timeout: i32) -> io::Result<()> {
    let deadline = (timeout > 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
    loop {
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        timeout = deadline.map_or(timeout, |deadline| {
            timeout_millis(deadline.saturating_duration_since(Instant::now()))
        });
    }
}
