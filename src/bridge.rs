//! The SSH process carries workspace bytes on stdin/stdout, without a PTY.
use crate::wire::HELLO;
use crate::workspace::{ensure_server, socket_path};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

/// Forwards bytes between SSH and the local daemon until either side closes. `new` also starts
/// the daemon; `list` ends quietly when there is none.
pub fn run_bridge(command: &str) -> io::Result<()> {
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    output.write_all(&HELLO)?;
    let socket = socket_path()?;
    if command == "new" {
        ensure_server(&std::env::current_exe()?, &socket)?;
    }
    let daemon = match UnixStream::connect(socket) {
        Err(error)
            if command == "list"
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            return Ok(());
        }
        daemon => daemon.map_err(|error| {
            io::Error::new(error.kind(), format!("connect workspace server: {error}"))
        })?,
    };
    let mut upstream = daemon.try_clone()?;
    std::thread::spawn(move || {
        if let Err(error) = pump(&mut io::stdin(), &mut upstream) {
            eprintln!("bridge: {error}");
        }
        // Ends the read below once SSH has gone away.
        let _ = upstream.shutdown(Shutdown::Both);
    });
    pump(&mut &daemon, &mut output)
}

/// Not `io::copy`: its kernel fast path can hold back what a pipe has already received.
fn pump(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0; 65536];
    loop {
        match from.read(&mut buffer)? {
            0 => return Ok(()),
            count => to.write_all(&buffer[..count])?,
        }
    }
}
