//! The SSH process carries framed workspace messages on stdin/stdout, without a PTY.
use crate::clipboard_reader::ClipboardReader;
use crate::wire::{Peer, Response};
use crate::workspace::{Action, check_peer, ensure_server, socket_path};
use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub const BRIDGE_VERSION: u8 = 3;

pub fn run_bridge(version: u8) -> io::Result<()> {
    if version != BRIDGE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "SSH bridge version {version} is unsupported; local and remote nextty must use bridge version {BRIDGE_VERSION}"
            ),
        ));
    }
    let mut remote = Peer::stdio()?;
    remote.send_hello(BRIDGE_VERSION)?;
    flush(&mut remote)?;
    let mut action = loop {
        if let Some(action) = remote.next_action()? {
            break action;
        }
        poll(&mut [fd(remote.fd(), libc::POLLIN)], -1)?;
    };
    let list = matches!(action, Action::List);
    match &mut action {
        Action::Create { options, .. } => {
            if options
                .directory
                .as_ref()
                .is_none_or(|path| path.is_relative())
            {
                let home = std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "remote HOME is not set")
                    })?;
                if !home.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote HOME must be absolute",
                    ));
                }
                options.directory = Some(match options.directory.take() {
                    Some(path) => home.join(path),
                    None => home,
                });
            }
        }
        Action::Attach { .. }
        | Action::AgentAttach { .. }
        | Action::List
        | Action::TerminateName(_) => (),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH bridge requires new, attach, agent attach, list, or terminate first",
            ));
        }
    }
    let socket = socket_path()?;
    if matches!(action, Action::Create { .. }) {
        ensure_server(&std::env::current_exe()?, &socket)?;
    }
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if list
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            remote.send_response(Response::List(Vec::new()))?;
            flush(&mut remote)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    check_peer(stream.as_raw_fd())?;
    let mut daemon = Peer::new(stream)?;
    daemon.send_action(&action)?;
    bridge(&mut remote, &mut daemon, list)
}

fn bridge(remote: &mut Peer, daemon: &mut Peer, list: bool) -> io::Result<()> {
    let mut pending = None;
    let mut copies = VecDeque::new();
    let mut reader: Option<ClipboardReader> = None;
    let mut copy = false;
    let mut copy_turn = false;
    let mut initial = true;
    let mut ending = false;
    let mut daemon_ready = true;
    loop {
        if daemon_ready {
            daemon.flush()?;
        }
        daemon_ready = !daemon.pending();
        // Alternate file chunks and screen messages so history copies remain bounded
        // without delaying interactive state or a session termination behind the file.
        if !remote.pending() {
            if !copy && !copies.is_empty() {
                let (request, incomplete) = copies.pop_front().unwrap();
                remote.begin_copy(request, incomplete)?;
                copy = true;
            } else {
                let mut sent_chunk = false;
                if copy
                    && (copy_turn || pending.is_none())
                    && let Some(bytes) = reader.as_mut().unwrap().read()?
                {
                    remote.copy_chunk(&bytes)?;
                    if bytes.is_empty() {
                        copy = false;
                    }
                    copy_turn = false;
                    sent_chunk = true;
                }
                if !sent_chunk && let Some(frame) = pending.take() {
                    remote.send_frame(frame)?;
                    copy_turn = true;
                }
            }
        }
        if ending && pending.is_none() && !remote.pending() {
            return Ok(());
        }
        let read_remote = !ending && !daemon.pending();
        let read_daemon = !ending && pending.is_none() && copies.len() < 8;
        let mut fds = [
            fd(remote.fd(), if read_remote { libc::POLLIN } else { 0 }),
            fd(
                remote.output_fd(),
                if remote.pending() { libc::POLLOUT } else { 0 },
            ),
            fd(
                daemon.fd(),
                (if read_daemon { libc::POLLIN } else { 0 })
                    | if daemon.pending() { libc::POLLOUT } else { 0 },
            ),
            fd(
                reader.as_ref().map_or(-1, ClipboardReader::fd),
                if !remote.pending() && copy {
                    libc::POLLIN
                } else {
                    0
                },
            ),
        ];
        let buffered = read_remote && remote.buffered_action()
            || read_daemon && daemon.buffered_action()
            || !remote.pending()
                && (pending.is_some()
                    || !copy && !copies.is_empty()
                    || copy && !reader.as_ref().unwrap().pending());
        poll(&mut fds, if buffered { 0 } else { -1 })?;
        if fds[0].revents & libc::POLLNVAL != 0
            || fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            return Ok(());
        }
        if fds[1].revents & libc::POLLOUT != 0 {
            remote.flush()?;
        }
        daemon_ready |=
            fds[2].revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0;
        if read_remote
            && (remote.buffered_action()
                || fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        {
            let mut bytes = 0;
            for _ in 0..256 {
                match remote.next_frame() {
                    Ok(Some(frame)) => {
                        bytes += frame.len();
                        daemon.send_frame(frame)?;
                    }
                    Ok(None) => break,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                }
                if bytes >= 256 * 1024 || !remote.buffered_action() {
                    break;
                }
            }
        }
        if read_daemon
            && (daemon.buffered_action() || fds[2].revents != 0)
            && let Some(frame) = daemon.next_frame()?
        {
            if frame.kind() == 5 || list && frame.kind() == 2 || initial && frame.kind() == 4 {
                ending = true;
                reader = None;
                copy = false;
                copies.clear();
            }
            if frame.kind() == 1 || frame.kind() == 12 {
                initial = false;
            }
            match frame.kind() {
                3 => {
                    let (request, file, incomplete) = daemon.take_clipboard(frame)?;
                    if reader.is_none() {
                        reader = Some(ClipboardReader::new(65536)?);
                    }
                    reader.as_ref().unwrap().queue(file)?;
                    copies.push_back((request, incomplete));
                }
                _ => pending = Some(frame),
            }
        }
    }
}
fn flush(peer: &mut Peer) -> io::Result<()> {
    while peer.pending() {
        peer.flush()?;
        if peer.pending() {
            poll(&mut [fd(peer.output_fd(), libc::POLLOUT)], -1)?;
        }
    }
    Ok(())
}
fn fd(fd: i32, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    }
}
fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    loop {
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Snapshot;
    use std::io::{Read, Seek, Write};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant};

    #[test]
    fn agent_owner_frames_remain_opaque_and_ack_starts_the_session() {
        let (remote_socket, bridge_socket) = UnixStream::pair().unwrap();
        let (daemon_socket, server_socket) = UnixStream::pair().unwrap();
        let relay = std::thread::spawn(move || {
            bridge(
                &mut Peer::remote(bridge_socket).unwrap(),
                &mut Peer::new(daemon_socket).unwrap(),
                false,
            )
        });
        let mut remote = Peer::remote(remote_socket).unwrap();
        let mut server = Peer::new(server_socket).unwrap();
        remote
            .send_action(&Action::AgentAttach {
                name: "remote".into(),
                token: vec![0, 255, 1],
            })
            .unwrap();
        remote.flush().unwrap();
        poll(&mut [fd(server.fd(), libc::POLLIN)], 5000).unwrap();
        assert!(
            matches!(server.next_action().unwrap(), Some(Action::AgentAttach { name, token }) if name == "remote" && token == [0, 255, 1])
        );
        server
            .send_response(Response::Agent {
                kind: 4,
                payload: vec![],
            })
            .unwrap();
        server
            .send_response(Response::Agent {
                kind: 2,
                payload: vec![0, 255, 2],
            })
            .unwrap();
        server
            .send_response(Response::Error("normal error after owner ACK".into()))
            .unwrap();
        server.send_response(Response::Detached).unwrap();
        server.flush().unwrap();
        let mut kinds = Vec::new();
        let mut error_seen = false;
        receive_until(&mut remote, |response| match response {
            Response::Agent { kind, payload } => {
                if kind == 2 {
                    assert_eq!(payload, [0, 255, 2]);
                }
                kinds.push(kind);
                false
            }
            Response::Error(error) => {
                assert_eq!(error, "normal error after owner ACK");
                error_seen = true;
                false
            }
            Response::Detached => {
                assert!(error_seen);
                assert_eq!(kinds, [4, 2]);
                true
            }
            _ => panic!("unexpected response"),
        });
        relay.join().unwrap().unwrap();
    }
    #[test]
    fn actions_and_noninitial_errors_continue_until_detached() {
        let (remote_socket, bridge_socket) = UnixStream::pair().unwrap();
        let (daemon_socket, server_socket) = UnixStream::pair().unwrap();
        let relay = std::thread::spawn(move || {
            bridge(
                &mut Peer::remote(bridge_socket).unwrap(),
                &mut Peer::new(daemon_socket).unwrap(),
                false,
            )
        });
        let mut server = Peer::new(server_socket).unwrap();
        let mut remote = Peer::remote(remote_socket).unwrap();
        remote
            .send_action(&Action::Input {
                pane: 42,
                data: "中e\u{301}\x1b[A".as_bytes().to_vec(),
            })
            .unwrap();
        remote.flush().unwrap();
        poll(&mut [fd(server.fd(), libc::POLLIN)], 5000).unwrap();
        assert!(matches!(
            server.next_action().unwrap(),
            Some(Action::Input { pane: 42, data }) if data == "中e\u{301}\x1b[A".as_bytes()
        ));
        send_state(&mut server);
        server
            .send_response(Response::Error("pane does not exist".into()))
            .unwrap();
        server.send_response(Response::Detached).unwrap();
        server.flush().unwrap();
        let mut state = false;
        let mut error = false;
        receive_until(&mut remote, |response| match response {
            Response::State(..) => {
                state = true;
                false
            }
            Response::Error(message) => {
                assert!(state);
                assert_eq!(message, "pane does not exist");
                error = true;
                false
            }
            Response::Detached => {
                assert!(error);
                true
            }
            _ => panic!("unexpected response"),
        });
        relay.join().unwrap().unwrap();
    }

    #[test]
    fn final_list_and_initial_error_are_delivered_before_close() {
        for response in [
            Response::List(Vec::new()),
            Response::Error("workspace does not exist".into()),
        ] {
            let list = matches!(response, Response::List(_));
            let (remote_socket, bridge_socket) = UnixStream::pair().unwrap();
            let (daemon_socket, server_socket) = UnixStream::pair().unwrap();
            let relay = std::thread::spawn(move || {
                bridge(
                    &mut Peer::remote(bridge_socket).unwrap(),
                    &mut Peer::new(daemon_socket).unwrap(),
                    list,
                )
            });
            let mut server = Peer::new(server_socket).unwrap();
            let mut remote = Peer::remote(remote_socket).unwrap();
            server.send_response(response).unwrap();
            server.flush().unwrap();
            drop(server);
            receive_until(&mut remote, |response| {
                if list {
                    assert!(matches!(response, Response::List(list) if list.is_empty()));
                } else {
                    assert!(
                        matches!(response, Response::Error(error) if error == "workspace does not exist")
                    );
                }
                true
            });
            relay.join().unwrap().unwrap();
            assert_closed(&mut remote);
        }
    }

    #[test]
    fn clipboard_stream_keeps_state_live_and_detach_cancels_it() {
        let directory = std::env::current_dir().unwrap().join(format!(
            "target/remote-ssh-20261024/transport/bridge-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bridge::tests::clipboard_bridge_child",
                "--nocapture",
            ])
            .env("NEXTTY_TEST_BRIDGE_COPY", &directory)
            .env("XDG_CACHE_HOME", &directory)
            .status()
            .unwrap();
        assert!(result.success());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn clipboard_bridge_child() {
        let Some(directory) = std::env::var_os("NEXTTY_TEST_BRIDGE_COPY").map(PathBuf::from) else {
            return;
        };
        let path = directory.join("source");
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let bytes: Vec<u8> = (0..65536).map(|index| (index % 251) as u8).collect();
        for _ in 0..600 {
            source.write_all(&bytes).unwrap();
        }
        source.rewind().unwrap();
        let (remote_socket, bridge_socket) = UnixStream::pair().unwrap();
        let (daemon_socket, server_socket) = UnixStream::pair().unwrap();
        let relay = std::thread::spawn(move || {
            bridge(
                &mut Peer::remote(bridge_socket).unwrap(),
                &mut Peer::new(daemon_socket).unwrap(),
                false,
            )
        });
        let mut server = Peer::new(server_socket).unwrap();
        let mut remote = Peer::remote(remote_socket).unwrap();
        server
            .send_response(Response::Clipboard {
                request: 31,
                file: source,
                incomplete: true,
            })
            .unwrap();
        let empty_path = directory.join("empty-source");
        let empty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&empty_path)
            .unwrap();
        std::fs::remove_file(empty_path).unwrap();
        server
            .send_response(Response::Clipboard {
                request: 33,
                file: empty,
                incomplete: false,
            })
            .unwrap();
        send_state(&mut server);
        server.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut state = false;
        let mut copied = None;
        receive_until(&mut remote, |response| match response {
            Response::State(..) => {
                state = true;
                false
            }
            Response::Clipboard {
                request: 31,
                file,
                incomplete: true,
            } => {
                assert!(
                    state,
                    "screen state was held behind the complete clipboard file"
                );
                copied = Some(file);
                false
            }
            Response::Clipboard {
                request: 33,
                file,
                incomplete: false,
            } => {
                assert!(copied.is_some(), "the second copy overtook the first");
                assert_eq!(file.metadata().unwrap().len(), 0);
                true
            }
            _ => panic!("unexpected response"),
        });
        let mut copied = copied.unwrap();
        assert_eq!(copied.metadata().unwrap().len(), 600 * 65536);
        assert_eq!(copied.metadata().unwrap().nlink(), 0);
        let mut buffer = [0; 65536];
        for _ in 0..600 {
            copied.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, bytes.as_slice());
        }
        assert_eq!(copied.read(&mut buffer).unwrap(), 0);
        copied.set_len(2 << 30).unwrap();
        copied.rewind().unwrap();
        server
            .send_response(Response::Clipboard {
                request: 32,
                file: copied,
                incomplete: false,
            })
            .unwrap();
        send_state(&mut server);
        server.flush().unwrap();
        receive_until(&mut remote, |response| {
            assert!(
                matches!(response, Response::State(..)),
                "a sparse 2 GiB copy completed before the state update"
            );
            true
        });
        server.send_response(Response::Detached).unwrap();
        server.flush().unwrap();
        receive_until(&mut remote, |response| {
            assert!(
                matches!(response, Response::Detached),
                "detach was blocked behind the active clipboard"
            );
            true
        });
        relay.join().unwrap().unwrap();
        assert_closed(&mut remote);
    }
    fn send_state(peer: &mut Peer) {
        peer.send_response(Response::State(
            Snapshot {
                name: "copy-test".into(),
                controller: true,
                focused: 0,
                active_tab: 0,
                tabs: Vec::new(),
                panes: Vec::new(),
            },
            Vec::new(),
        ))
        .unwrap();
    }
    fn receive_until(peer: &mut Peer, mut done: impl FnMut(Response) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut responses = peer.responses(None).unwrap().into_iter();
            while let Some(response) = responses.next() {
                if done(response) {
                    assert!(
                        responses.next().is_none(),
                        "unexpected extra bridge response"
                    );
                    return;
                }
            }
            assert!(Instant::now() < deadline, "bridge response timed out");
            poll(&mut [fd(peer.fd(), libc::POLLIN)], 50).unwrap();
        }
    }
    fn assert_closed(peer: &mut Peer) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match peer.responses(None) {
                Ok(responses) => assert!(responses.is_empty(), "unexpected extra bridge response"),
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
                    return;
                }
            }
            assert!(Instant::now() < deadline, "bridge did not close");
            poll(&mut [fd(peer.fd(), libc::POLLIN)], 50).unwrap();
        }
    }
    #[test]
    fn coalesced_input_drains_in_order_under_backpressure_before_remote_eof() {
        let (remote_socket, bridge_socket) = UnixStream::pair().unwrap();
        let (daemon_socket, server_socket) = UnixStream::pair().unwrap();
        let capacity: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    daemon_socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&capacity as *const libc::c_int).cast(),
                    std::mem::size_of_val(&capacity) as libc::socklen_t,
                )
            },
            0
        );
        let relay = std::thread::spawn(move || {
            bridge(
                &mut Peer::remote(bridge_socket).unwrap(),
                &mut Peer::new(daemon_socket).unwrap(),
                false,
            )
            .unwrap();
        });
        let writer = std::thread::spawn(move || {
            let mut remote = Peer::remote(remote_socket).unwrap();
            let mut data = vec![b'x'; 1024];
            for index in 0..600u32 {
                data[..4].copy_from_slice(&index.to_le_bytes());
                remote.send_input(7, &data).unwrap();
            }
            flush(&mut remote).unwrap();
        });
        let mut server = Peer::new(server_socket).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        for index in 0..600u32 {
            loop {
                assert!(Instant::now() < deadline, "bridge input stalled");
                if let Some(action) = server.next_action().unwrap() {
                    let Action::Input { pane, data } = action else {
                        panic!("unexpected action")
                    };
                    assert_eq!(pane, 7);
                    assert_eq!(data.len(), 1024);
                    assert_eq!(&data[..4], &index.to_le_bytes());
                    assert!(data[4..].iter().all(|byte| *byte == b'x'));
                    break;
                }
                poll(&mut [fd(server.fd(), libc::POLLIN)], 50).unwrap();
            }
        }
        writer.join().unwrap();
        relay.join().unwrap();
        assert_eq!(
            server.next_action().err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
