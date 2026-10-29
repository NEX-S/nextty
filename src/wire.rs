//! Length-prefixed, versioned messages on an owned Unix socket.
use crate::core::{
    Cell, Extra, Flags, Mode, Point, Row, Scroll, SelectionRange, SelectionType, Side, WindowSize,
};
use crate::eventfd::{consume_notification, notify};
use crate::screen::Update;
use crate::terminal::{ProcessMetadata, Shell};
use crate::vt::{Color, CursorShape, NamedColor, Rgb};
use crate::workspace::{
    Action, Axis, BlockMarker, CommandBlock, CreateOptions, Direction, HistoryCursor, HistoryPage,
    PaneView, Snapshot, TabView, WorkspaceInfo,
};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
pub(crate) const MAX_FRAME: usize = 32 * 1024 * 1024;
const MAX_PENDING: usize = MAX_FRAME + 1024 * 1024;
pub(crate) const VERSION: u8 = 14;

pub(crate) enum Response {
    ScreenDirty,
    AgentInput {
        request: u64,
        result: Result<Option<u64>, String>,
    },
    CopyError {
        request: u64,
        error: String,
    },
    History {
        request: u64,
        result: Result<HistoryPage, String>,
    },
    Agent {
        kind: u8,
        payload: Vec<u8>,
    },
    State(Snapshot, Vec<(u64, Update)>),
    List(Vec<WorkspaceInfo>),
    Clipboard {
        request: u64,
        file: File,
        incomplete: bool,
    },
    ClipboardStore {
        primary: bool,
        text: String,
    },
    Error(String),
    Search {
        pane: u64,
        request: u64,
        count: u32,
        truncated: bool,
    },
    Detached,
}
pub(crate) struct Frame(Vec<u8>);
impl Frame {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn kind(&self) -> u8 {
        self.0[5]
    }
}
struct Packet {
    bytes: Vec<u8>,
    offset: usize,
    file: Option<File>,
}
pub(crate) struct Peer {
    stream: OwnedFd,
    output_fd: Option<OwnedFd>,
    remote: bool,
    copy: Option<RemoteCopy>,
    receiver: Option<ClipboardReceiver>,
    input: Vec<u8>,
    consumed: usize,
    files: VecDeque<OwnedFd>,
    output: VecDeque<Packet>,
    queued: usize,
    read_error: Option<io::Error>,
}
struct RemoteCopy {
    request: u64,
    incomplete: bool,
    finishing: bool,
}

enum ReceiveWork {
    Begin,
    Chunk(Vec<u8>),
    End,
}
struct ReceiveState {
    work: Option<ReceiveWork>,
    ready: Option<io::Result<File>>,
    closed: bool,
}
struct ReceiveShared {
    state: Mutex<ReceiveState>,
    changed: Condvar,
    notification: OwnedFd,
}
struct ClipboardReceiver(Arc<ReceiveShared>);
impl ClipboardReceiver {
    fn new() -> io::Result<Self> {
        let shared = Arc::new(ReceiveShared {
            state: Mutex::new(ReceiveState {
                work: None,
                ready: None,
                closed: false,
            }),
            changed: Condvar::new(),
            notification: crate::eventfd::new()?,
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("nextty-recv".into())
            .spawn(move || receive_clipboard(worker))?;
        Ok(Self(shared))
    }
    fn fd(&self) -> RawFd {
        self.0.notification.as_raw_fd()
    }
    fn available(&self) -> bool {
        let state = self.0.state.lock().unwrap();
        !state.closed && state.work.is_none()
    }
    fn ready(&self) -> bool {
        self.0.state.lock().unwrap().ready.is_some()
    }
    fn submit(&self, work: impl FnOnce() -> ReceiveWork) -> bool {
        let mut state = self.0.state.lock().unwrap();
        if state.closed || state.work.is_some() {
            return false;
        }
        state.work = Some(work());
        self.0.changed.notify_one();
        true
    }
    fn take(&self) -> io::Result<Option<io::Result<File>>> {
        consume_notification(self.fd())?;
        Ok(self.0.state.lock().unwrap().ready.take())
    }
}
impl Drop for ClipboardReceiver {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().closed = true;
        self.0.changed.notify_one();
    }
}
fn receive_clipboard(shared: Arc<ReceiveShared>) {
    let mut file: Option<File> = None;
    loop {
        let mut state = shared.state.lock().unwrap();
        while !state.closed && state.work.is_none() {
            state = shared.changed.wait(state).unwrap();
        }
        if state.closed {
            let work = state.work.take();
            let ready = state.ready.take();
            drop(state);
            drop(work);
            drop(ready);
            return;
        }
        let work = state.work.take().unwrap();
        drop(state);
        // Releasing the slot before IO permits one pending chunk, at most 128 KiB total.
        let result = notify(shared.notification.as_raw_fd()).and_then(|()| match work {
            ReceiveWork::Begin => clipboard_file().map(|created| {
                file = Some(created);
                None
            }),
            ReceiveWork::Chunk(bytes) => file.as_mut().unwrap().write_all(&bytes).map(|()| None),
            ReceiveWork::End => file.as_mut().unwrap().rewind().map(|()| file.take()),
        });
        let ready = match result {
            Ok(None) => continue,
            Ok(Some(file)) => Ok(file),
            Err(error) => {
                drop(file.take());
                Err(error)
            }
        };
        let mut state = shared.state.lock().unwrap();
        if state.closed {
            drop(state);
            drop(ready);
            continue;
        }
        let failed = ready.is_err();
        state.ready = Some(ready);
        let pending = if failed {
            state.closed = true;
            state.work.take()
        } else {
            None
        };
        drop(state);
        drop(pending);
        if let Err(error) = notify(shared.notification.as_raw_fd()) {
            eprintln!("clipboard receiver: {error}");
        }
        if failed {
            return;
        }
    }
}
impl Peer {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        Self::from_stream(stream, false)
    }
    pub fn remote(stream: UnixStream) -> io::Result<Self> {
        Self::from_stream(stream, true)
    }
    fn from_stream(stream: UnixStream, remote: bool) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self::from_fds(stream.into(), None, remote))
    }
    pub fn stdio() -> io::Result<Self> {
        let duplicate = |fd| {
            let copy = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
            let flags = unsafe { libc::fcntl(copy.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(copy.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(copy)
        };
        Ok(Self::from_fds(duplicate(0)?, Some(duplicate(1)?), true))
    }
    fn from_fds(stream: OwnedFd, output_fd: Option<OwnedFd>, remote: bool) -> Self {
        Self {
            stream,
            output_fd,
            remote,
            copy: None,
            receiver: None,
            input: Vec::new(),
            consumed: 0,
            files: VecDeque::new(),
            output: VecDeque::new(),
            queued: 0,
            read_error: None,
        }
    }
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
    pub fn output_fd(&self) -> RawFd {
        self.output_fd.as_ref().unwrap_or(&self.stream).as_raw_fd()
    }
    pub fn pending(&self) -> bool {
        !self.output.is_empty()
    }
    pub fn receiver_fd(&self) -> RawFd {
        self.receiver.as_ref().map_or(-1, ClipboardReceiver::fd)
    }
    fn receive_blocked(&self) -> bool {
        self.copy.as_ref().is_some_and(|copy| copy.finishing)
            || self.remote
                && self.buffered_action()
                && matches!(self.input.get(self.consumed + 5), Some(7..=9))
                && self
                    .receiver
                    .as_ref()
                    .is_some_and(|receiver| !receiver.available())
    }
    pub fn wants_read(&self) -> bool {
        !self.receive_blocked()
    }
    pub fn needs_poll(&self) -> bool {
        self.receiver.as_ref().is_some_and(ClipboardReceiver::ready)
            || !self.receive_blocked() && (self.buffered_action() || self.read_error.is_some())
    }
    pub fn send_hello(&mut self, version: u8) -> io::Result<()> {
        let mut encoder = Encoder::new();
        encoder.u8(10);
        encoder.u8(version);
        self.queue(encoder.0, None)
    }
    pub fn hello(&mut self) -> io::Result<Option<u8>> {
        self.read()?;
        let Some(packet) = packet(&self.input[self.consumed..])? else {
            return Ok(None);
        };
        let mut decoder = Decoder::new(packet)?;
        if decoder.u8()? != 10 || !self.files.is_empty() {
            return Err(invalid("remote nextty did not send an SSH bridge greeting"));
        }
        let version = decoder.u8()?;
        decoder.finish()?;
        self.consumed += packet.len() + 4;
        Ok(Some(version))
    }
    pub fn send_action(&mut self, action: &Action) -> io::Result<()> {
        // The largest fixed-size action is 30 bytes, including the frame header.
        let mut encoder = Encoder::with_capacity(32);
        encode_action(&mut encoder, action);
        self.queue(encoder.0, None)
    }
    pub fn send_input(&mut self, pane: u64, data: &[u8]) -> io::Result<()> {
        const CHUNK: usize = 65536;
        let required = data
            .len()
            .checked_add(data.len().div_ceil(CHUNK).saturating_mul(18))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "input is too large; no bytes were queued",
                )
            })?;
        if required > MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input exceeds the 33 MiB send buffer; no bytes were queued",
            ));
        }
        if self.queued + required > MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace send buffer is full; no input bytes were queued",
            ));
        }
        for chunk in data.chunks(CHUNK) {
            let mut encoder = Encoder::with_capacity(18 + chunk.len());
            encode_input(&mut encoder, pane, chunk);
            self.queue(encoder.0, None)?;
        }
        Ok(())
    }
    pub fn send_state(
        &mut self,
        snapshot: &Snapshot,
        updates: &[(u64, &Update)],
        previous: &HashMap<u64, Arc<ProcessMetadata>>,
    ) -> io::Result<()> {
        let mut encoder = Encoder::with_capacity(256);
        encoder.u8(1);
        encode_snapshot(&mut encoder, snapshot, previous);
        encoder.u32(updates.len() as u32);
        for (id, update) in updates {
            encoder.u64(*id);
            encode_update(&mut encoder, update);
        }
        self.queue(encoder.0, None)
    }
    pub fn send_response(&mut self, response: Response) -> io::Result<()> {
        let mut encoder = Encoder::new();
        let file = match response {
            Response::History { request, result } => {
                encoder.u8(16);
                encoder.u64(request);
                encoder.bool(result.is_ok());
                match result {
                    Ok(page) => {
                        encoder.string(&page.text);
                        encoder.u64(page.cursor.epoch);
                        encoder.u64(page.cursor.row);
                        encoder.u64(page.cursor.offset);
                        encoder.bool(page.has_more);
                        encoder.bool(page.matched.is_some());
                        if let Some(matched) = page.matched {
                            encoder.bool(matched);
                        }
                        encoder.bool(page.incomplete);
                        encoder.bool(page.command.is_some());
                        if let Some(command) = page.command {
                            encode_command(&mut encoder, &command);
                        }
                    }
                    Err(error) => encoder.string(&error),
                }
                None
            }
            Response::CopyError { request, error } => {
                encoder.u8(15);
                encoder.u64(request);
                encoder.string(&error);
                None
            }
            Response::AgentInput { request, result } => {
                encoder.u8(14);
                encoder.u64(request);
                encoder.bool(result.is_ok());
                match result {
                    Ok(block) => {
                        encoder.bool(block.is_some());
                        if let Some(block) = block {
                            encoder.u64(block);
                        }
                    }
                    Err(error) => encoder.string(&error),
                }
                None
            }
            Response::ScreenDirty => {
                encoder.u8(13);
                None
            }
            Response::Agent { kind, payload } => {
                encoder.u8(12);
                encoder.u8(kind);
                encoder.bytes(&payload);
                None
            }
            Response::State(snapshot, updates) => {
                encoder.u8(1);
                encode_snapshot(&mut encoder, &snapshot, &HashMap::new());
                encoder.u32(updates.len() as u32);
                for (id, update) in updates {
                    encoder.u64(id);
                    encode_update(&mut encoder, &update);
                }
                None
            }
            Response::List(list) => {
                encoder.u8(2);
                encoder.u32(list.len() as u32);
                for item in list {
                    encoder.string(&item.name);
                    encoder.u32(item.tabs as u32);
                    encoder.u32(item.panes as u32);
                    encoder.u32(item.attached as u32);
                }
                None
            }
            Response::Clipboard {
                request,
                file,
                incomplete,
            } => {
                encoder.u8(3);
                encoder.u64(request);
                encoder.bool(incomplete);
                Some(file)
            }
            Response::Error(error) => {
                encoder.u8(4);
                encoder.string(&error);
                None
            }
            Response::ClipboardStore { primary, text } => {
                encoder.u8(6);
                encoder.bool(primary);
                encoder.string(&text);
                None
            }
            Response::Search {
                pane,
                request,
                count,
                truncated,
            } => {
                encoder.0.reserve(22);
                encoder.u8(11);
                encoder.u64(pane);
                encoder.u64(request);
                encoder.bool(truncated);
                encoder.u32(count);
                None
            }
            Response::Detached => {
                encoder.u8(5);
                None
            }
        };
        self.queue(encoder.0, file)
    }
    pub fn begin_copy(&mut self, request: u64, incomplete: bool) -> io::Result<()> {
        let mut encoder = Encoder::new();
        encoder.u8(7);
        encoder.u64(request);
        encoder.bool(incomplete);
        self.queue(encoder.0, None)
    }
    pub fn copy_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut encoder = Encoder::with_capacity(10 + bytes.len());
        encoder.u8(if bytes.is_empty() { 9 } else { 8 });
        if !bytes.is_empty() {
            encoder.bytes(bytes);
        }
        self.queue(encoder.0, None)
    }
    fn queue(&mut self, mut bytes: Vec<u8>, file: Option<File>) -> io::Result<()> {
        if self.remote && file.is_some() {
            return Err(invalid("file descriptors cannot cross the SSH bridge"));
        }
        let length = bytes.len() - 4;
        if length > MAX_FRAME || self.queued + bytes.len() > MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace socket send queue is full",
            ));
        }
        bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
        self.queued += bytes.len();
        self.output.push_back(Packet {
            bytes,
            offset: 0,
            file,
        });
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        let mut budget: usize = 256 * 1024;
        let output_fd = self.output_fd();
        while budget > 0 {
            let Some(packet) = self.output.front() else {
                break;
            };
            let bytes = &packet.bytes[packet.offset..];
            let result = if let Some(file) = &packet.file {
                send_with_fd(
                    output_fd,
                    &bytes[..bytes.len().min(budget)],
                    file.as_raw_fd(),
                )
            } else {
                let count = if self.output.len() > 1 && bytes.len() < 64 * 1024 {
                    let mut vectors = [libc::iovec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    }; 64];
                    let mut remaining = budget.min(64 * 1024);
                    let mut count = 0;
                    for packet in self.output.iter().take(vectors.len()) {
                        if packet.file.is_some() {
                            break;
                        }
                        let bytes = &packet.bytes[packet.offset..];
                        let length = bytes.len().min(remaining);
                        vectors[count] = libc::iovec {
                            iov_base: bytes.as_ptr().cast_mut().cast(),
                            iov_len: length,
                        };
                        count += 1;
                        remaining -= length;
                        if remaining == 0 {
                            break;
                        }
                    }
                    unsafe { libc::writev(output_fd, vectors.as_ptr(), count as libc::c_int) }
                } else {
                    let bytes = &bytes[..bytes.len().min(budget)];
                    unsafe { libc::write(output_fd, bytes.as_ptr().cast(), bytes.len()) }
                };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count as usize)
                }
            };
            match result {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "workspace socket closed while writing",
                    ));
                }
                Ok(count) => {
                    self.queued -= count;
                    budget -= count;
                    let mut remaining = count;
                    while remaining != 0 {
                        let packet = self.output.front_mut().unwrap();
                        let sent = remaining.min(packet.bytes.len() - packet.offset);
                        packet.offset += sent;
                        packet.file = None;
                        remaining -= sent;
                        if packet.offset == packet.bytes.len() {
                            self.output.pop_front();
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub fn cancel_output(&mut self) {
        self.output.clear();
        self.queued = 0;
    }
    pub fn buffered_action(&self) -> bool {
        let input = &self.input[self.consumed..];
        if input.len() < 4 {
            return false;
        }
        let length = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
        length > MAX_FRAME || input.len() >= length + 4
    }
    fn read(&mut self) -> io::Result<()> {
        const READ_BUDGET: usize = 256 * 1024;
        if self.buffered_action() {
            return Ok(());
        }
        self.input.drain(..self.consumed);
        // Keep bulk storage while large frames continue; release it after small traffic resumes.
        if self.consumed != 0 && self.consumed <= READ_BUDGET && self.input.is_empty() {
            self.input.shrink_to(READ_BUDGET);
        }
        self.consumed = 0;
        let mut budget: usize = if self.read_error.is_some() {
            0
        } else {
            READ_BUDGET
        };
        while budget > 0 {
            if self.input.len() == self.input.capacity() {
                self.input.reserve(4096);
            }
            let available = (self.input.capacity() - self.input.len()).min(65536);
            let bytes = &mut self.input.spare_capacity_mut()[..available];
            let result = if self.output_fd.is_some() {
                let count = unsafe {
                    libc::read(
                        self.stream.as_raw_fd(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok((count as usize, Vec::new()))
                }
            } else {
                receive(self.stream.as_raw_fd(), bytes)
            };
            match result {
                Ok((0, _)) => {
                    self.read_error = Some(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "workspace socket closed",
                    ));
                    break;
                }
                Ok((count, files)) => {
                    if self.remote && !files.is_empty() {
                        return Err(invalid("remote messages cannot contain file descriptors"));
                    }
                    // recvmsg/read initialized exactly the returned prefix.
                    unsafe { self.input.set_len(self.input.len() + count) };
                    self.files.extend(files);
                    budget = budget.saturating_sub(count);
                    if self.input.len() > MAX_PENDING || self.files.len() > 8 {
                        return Err(invalid("workspace receive buffer exceeded its limit"));
                    }
                    if count < available && self.buffered_action() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.read_error = Some(error);
                    break;
                }
            }
        }
        if !self.buffered_action()
            && let Some(error) = self.read_error.take()
        {
            if self.copy.is_some() && error.kind() == io::ErrorKind::UnexpectedEof {
                return Err(invalid("remote clipboard transfer ended before completion"));
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn next_action(&mut self) -> io::Result<Option<Action>> {
        self.read()?;
        let packet = packet(&self.input[self.consumed..])?;
        if !self.files.is_empty() {
            return Err(invalid("client messages cannot contain file descriptors"));
        }
        let Some(packet) = packet else {
            return Ok(None);
        };
        let mut decoder = Decoder::new(packet)?;
        let action = decode_action(&mut decoder)?;
        decoder.finish()?;
        self.consumed += packet.len() + 4;
        Ok(Some(action))
    }
    // The bridge checks framing here; the destination still decodes each message.
    pub fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        self.read()?;
        let Some(packet) = packet(&self.input[self.consumed..])? else {
            return Ok(None);
        };
        Decoder::new(packet)?.u8()?;
        let end = self.consumed + packet.len() + 4;
        let frame = Frame(self.input[self.consumed..end].to_vec());
        self.consumed = end;
        Ok(Some(frame))
    }
    pub fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        self.queue(frame.0, None)
    }
    pub fn take_clipboard(&mut self, frame: Frame) -> io::Result<(u64, File, bool)> {
        debug_assert_eq!(frame.kind(), 3);
        let mut decoder = Decoder::new(&frame.0[4..])?;
        decoder.u8()?;
        let request = decoder.u64()?;
        let incomplete = decoder.bool()?;
        decoder.finish()?;
        let file = self
            .files
            .pop_front()
            .ok_or_else(|| invalid("clipboard response has no file descriptor"))?;
        Ok((request, File::from(file), incomplete))
    }
    pub fn responses(&mut self, previous: Option<&Snapshot>) -> io::Result<Vec<Response>> {
        let result = self.receive_responses(previous);
        if result.is_err() {
            self.copy = None;
            self.receiver = None;
        }
        result
    }
    fn receive_responses(&mut self, previous: Option<&Snapshot>) -> io::Result<Vec<Response>> {
        let mut result = Vec::new();
        if let Some(receiver) = &self.receiver
            && let Some(ready) = receiver.take()?
        {
            let file = ready?;
            let copy = self.copy.take().unwrap();
            result.push(Response::Clipboard {
                request: copy.request,
                file,
                incomplete: copy.incomplete,
            });
            return Ok(result);
        }
        if self.copy.as_ref().is_some_and(|copy| copy.finishing) {
            return Ok(result);
        }
        self.read()?;
        while let Some(packet) = packet(&self.input[self.consumed..])? {
            let mut d = Decoder::new(packet)?;
            let response = match d.u8()? {
                16 => {
                    let request = d.u64()?;
                    let result = if d.bool()? {
                        Ok(HistoryPage {
                            text: d.string(crate::history::READ_LIMIT)?,
                            cursor: HistoryCursor {
                                epoch: d.u64()?,
                                row: d.u64()?,
                                offset: d.u64()?,
                            },
                            has_more: d.bool()?,
                            matched: if d.bool()? { Some(d.bool()?) } else { None },
                            incomplete: d.bool()?,
                            command: if d.bool()? {
                                Some(decode_command(&mut d)?)
                            } else {
                                None
                            },
                        })
                    } else {
                        Err(d.string(65536)?)
                    };
                    Some(Response::History { request, result })
                }
                15 => Some(Response::CopyError {
                    request: d.u64()?,
                    error: d.string(65536)?,
                }),
                14 => {
                    let request = d.u64()?;
                    let result = if d.bool()? {
                        Ok(if d.bool()? { Some(d.u64()?) } else { None })
                    } else {
                        Err(d.string(65536)?)
                    };
                    Some(Response::AgentInput { request, result })
                }
                13 => Some(Response::ScreenDirty),
                12 => Some(Response::Agent {
                    kind: d.u8()?,
                    payload: d.bytes(MAX_FRAME)?.to_vec(),
                }),
                1 => {
                    let previous = result
                        .iter()
                        .rev()
                        .find_map(|response| match response {
                            Response::State(snapshot, _) => Some(snapshot),
                            _ => None,
                        })
                        .or(previous);
                    let snapshot = decode_snapshot(&mut d, previous)?;
                    let count = d.count(1024)?;
                    let mut updates = Vec::with_capacity(count);
                    for _ in 0..count {
                        updates.push((d.u64()?, decode_update(&mut d)?));
                    }
                    Some(Response::State(snapshot, updates))
                }
                2 => {
                    let count = d.count(4096)?;
                    let mut list = Vec::with_capacity(count);
                    for _ in 0..count {
                        list.push(WorkspaceInfo {
                            name: d.string(128)?,
                            tabs: d.u32()? as usize,
                            panes: d.u32()? as usize,
                            attached: d.u32()? as usize,
                        });
                    }
                    Some(Response::List(list))
                }
                3 => {
                    let request = d.u64()?;
                    let incomplete = d.bool()?;
                    let fd = self
                        .files
                        .pop_front()
                        .ok_or_else(|| invalid("clipboard response has no file descriptor"))?;
                    Some(Response::Clipboard {
                        request,
                        file: File::from(fd),
                        incomplete,
                    })
                }
                4 => Some(Response::Error(d.string(65536)?)),
                5 => {
                    d.finish()?;
                    self.copy = None;
                    self.receiver = None;
                    Some(Response::Detached)
                }
                6 => Some(Response::ClipboardStore {
                    primary: d.bool()?,
                    text: d.string(MAX_FRAME)?,
                }),
                11 => {
                    let pane = d.u64()?;
                    let request = d.u64()?;
                    let truncated = d.bool()?;
                    let count = d.u32()?;
                    Some(Response::Search {
                        pane,
                        request,
                        count,
                        truncated,
                    })
                }
                7 if self.remote => {
                    let request = d.u64()?;
                    let incomplete = d.bool()?;
                    d.finish()?;
                    if self.copy.is_some() {
                        return Err(invalid("remote clipboard transfer is already active"));
                    }
                    if self.receiver.is_none() {
                        self.receiver = Some(ClipboardReceiver::new()?);
                    }
                    if !self
                        .receiver
                        .as_ref()
                        .unwrap()
                        .submit(|| ReceiveWork::Begin)
                    {
                        break;
                    }
                    self.copy = Some(RemoteCopy {
                        request,
                        incomplete,
                        finishing: false,
                    });
                    None
                }
                8 if self.remote => {
                    let bytes = d.bytes(65536)?;
                    d.finish()?;
                    if self.copy.is_none() {
                        return Err(invalid("remote clipboard chunk has no start"));
                    }
                    if !self
                        .receiver
                        .as_ref()
                        .unwrap()
                        .submit(|| ReceiveWork::Chunk(bytes.to_vec()))
                    {
                        break;
                    }
                    None
                }
                9 if self.remote => {
                    d.finish()?;
                    let copy = self
                        .copy
                        .as_mut()
                        .ok_or_else(|| invalid("remote clipboard end has no start"))?;
                    if !self.receiver.as_ref().unwrap().submit(|| ReceiveWork::End) {
                        break;
                    }
                    copy.finishing = true;
                    None
                }
                _ => return Err(invalid("unknown workspace response")),
            };
            d.finish()?;
            self.consumed += packet.len() + 4;
            if let Some(response) = response {
                result.push(response);
            }
            if self.copy.as_ref().is_some_and(|copy| copy.finishing) {
                break;
            }
        }
        Ok(result)
    }
}

fn clipboard_file() -> io::Result<File> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME or XDG_CACHE_HOME is required for remote clipboard storage",
            )
        })?;
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard cache directory must be absolute",
        ));
    }
    let directory = root.join("nextty/clipboard");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote clipboard directory must be owned and private",
        ));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_TMPFILE)
        .mode(0o600)
        .open(directory)
}

fn packet(input: &[u8]) -> io::Result<Option<&[u8]>> {
    if input.len() < 4 {
        return Ok(None);
    }
    let length = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid("invalid workspace frame length"));
    }
    Ok(input.get(4..4 + length))
}

fn send_with_fd(socket: RawFd, bytes: &[u8], file: RawFd) -> io::Result<usize> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 4];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as _) as usize };
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as _) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), file);
        let result = libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL);
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}
fn receive(
    socket: RawFd,
    bytes: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 16];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let result = unsafe { libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut files = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS {
                return Err(invalid("unexpected workspace socket control message"));
            }
            let count =
                ((*header).cmsg_len - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
            for index in 0..count {
                files.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                    libc::CMSG_DATA(header).cast::<RawFd>().add(index),
                )));
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(invalid("workspace socket control message truncated"));
    }
    Ok((result as usize, files))
}

struct Encoder(Vec<u8>);
impl Encoder {
    fn new() -> Self {
        Self::with_capacity(5)
    }
    fn with_capacity(capacity: usize) -> Self {
        // Fill the frame length in place after encoding, without copying the payload.
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(&[0, 0, 0, 0, VERSION]);
        Self(bytes)
    }
    fn u8(&mut self, n: u8) {
        self.0.push(n);
    }
    fn bool(&mut self, value: bool) {
        self.u8(value as u8);
    }
    fn u16(&mut self, n: u16) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }
    fn u32(&mut self, n: u32) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }
    fn u64(&mut self, n: u64) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }
    fn i32(&mut self, n: i32) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.0.extend_from_slice(bytes);
    }
    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
    fn path(&mut self, value: &Option<PathBuf>) {
        self.bool(value.is_some());
        if let Some(path) = value {
            self.bytes(path.as_os_str().as_bytes());
        }
    }
    fn size(&mut self, size: WindowSize) {
        self.u16(size.num_cols);
        self.u16(size.num_lines);
        self.u16(size.cell_width);
        self.u16(size.cell_height);
    }
    fn point(&mut self, point: Point) {
        self.i32(point.line);
        self.u32(point.column as u32);
    }
}
struct Decoder<'a> {
    bytes: &'a [u8],
}
impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> io::Result<Self> {
        let mut result = Self { bytes };
        if result.u8()? != VERSION {
            return Err(invalid(
                "workspace protocol version differs; terminate the old server before upgrading",
            ));
        }
        Ok(result)
    }
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        let (bytes, remaining) = self
            .bytes
            .split_at_checked(count)
            .ok_or_else(|| invalid("truncated workspace message"))?;
        self.bytes = remaining;
        Ok(bytes)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> io::Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid boolean")),
        }
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn count(&mut self, limit: usize) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n > limit {
            Err(invalid("workspace field exceeds limit"))
        } else {
            Ok(n)
        }
    }
    fn bytes(&mut self, limit: usize) -> io::Result<&'a [u8]> {
        let count = self.count(limit)?;
        self.take(count)
    }
    fn string(&mut self, limit: usize) -> io::Result<String> {
        String::from_utf8(self.bytes(limit)?.to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    fn strings(&mut self, limit: usize) -> io::Result<Vec<String>> {
        let count = self.count(limit)?;
        let mut strings = Vec::with_capacity(count);
        for _ in 0..count {
            strings.push(self.string(65536)?);
        }
        Ok(strings)
    }
    fn path(&mut self) -> io::Result<Option<PathBuf>> {
        if self.bool()? {
            Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(
                self.bytes(65536)?.to_vec(),
            ))))
        } else {
            Ok(None)
        }
    }
    fn size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            num_cols: self.u16()?,
            num_lines: self.u16()?,
            cell_width: self.u16()?,
            cell_height: self.u16()?,
        })
    }
    fn point(&mut self) -> io::Result<Point> {
        let line = self.i32()?;
        let column = self.u32()? as usize;
        if column > 4096 {
            return Err(invalid("column exceeds terminal limit"));
        }
        Ok(Point { line, column })
    }
    fn finish(&self) -> io::Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(invalid("unexpected bytes after workspace message"))
        }
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn encode_options(e: &mut Encoder, options: &CreateOptions) {
    e.size(options.size);
    e.u32(options.foreground);
    e.u32(options.background);
    e.path(&options.directory);
    e.bool(options.program.is_some());
    if let Some(shell) = &options.program {
        e.string(&shell.program);
        e.u32(shell.args.len() as u32);
        for arg in &shell.args {
            e.string(arg);
        }
    }
    e.u64(options.history_capacity);
}
fn decode_options(d: &mut Decoder<'_>) -> io::Result<CreateOptions> {
    let size = d.size()?;
    let foreground = d.u32()?;
    let background = d.u32()?;
    let directory = d.path()?;
    let program = if d.bool()? {
        let program = d.string(65536)?;
        let count = d.count(4096)?;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            args.push(d.string(65536)?);
        }
        Some(Shell::new(program, args))
    } else {
        None
    };
    Ok(CreateOptions {
        size,
        foreground,
        background,
        directory,
        program,
        history_capacity: d.u64()?,
    })
}
fn encode_action(e: &mut Encoder, action: &Action) {
    match action {
        Action::ScreenRequest => e.u8(32),
        Action::AgentOpen => e.u8(27),
        Action::AgentView(payload) => {
            e.u8(28);
            e.bytes(payload);
        }
        Action::AgentRequest(payload) => {
            e.u8(29);
            e.bytes(payload);
        }
        Action::AgentAttach { name, token } => {
            e.u8(30);
            e.string(name);
            e.bytes(token);
        }
        Action::AgentUpdate { kind, payload } => {
            e.u8(31);
            e.u8(*kind);
            e.bytes(payload);
        }
        Action::Create { name, options } => {
            e.u8(1);
            e.string(name);
            encode_options(e, options);
        }
        Action::Attach { name, size } => {
            e.u8(2);
            e.string(name);
            e.size(*size);
        }
        Action::List => e.u8(3),
        Action::Input { pane, data } => encode_input(e, *pane, data),
        Action::AgentInput {
            pane,
            request,
            generation,
            prompt,
            data,
        } => {
            e.u8(33);
            e.u64(*pane);
            e.u64(*request);
            e.u64(*generation);
            e.bool(prompt.is_some());
            if let Some(prompt) = prompt {
                e.u64(*prompt);
            }
            e.bytes(data);
        }
        Action::CancelHistory { request } => {
            e.u8(35);
            e.u64(*request);
        }
        Action::ReadHistory {
            pane,
            request,
            block,
            cursor,
            query,
        } => {
            e.u8(34);
            e.u64(*pane);
            e.u64(*request);
            e.bool(block.is_some());
            if let Some(block) = block {
                e.u64(*block);
            }
            encode_cursor(e, *cursor);
            e.bool(query.is_some());
            if let Some(query) = query {
                e.string(query);
            }
        }
        Action::Resize(size) => {
            e.u8(5);
            e.size(*size);
        }
        Action::Split { pane, axis } => {
            e.u8(6);
            e.u64(*pane);
            e.u8(match axis {
                Axis::LeftRight => 0,
                Axis::TopBottom => 1,
            });
        }
        Action::ResizeSplit {
            first,
            second,
            position,
        } => {
            e.u8(23);
            e.u64(*first);
            e.u64(*second);
            e.u16(*position);
        }
        Action::NewTab { pane } => {
            e.u8(7);
            e.u64(*pane);
        }
        Action::Focus(pane) => {
            e.u8(8);
            e.u64(*pane);
        }
        Action::Navigate(direction) => {
            e.u8(9);
            e.u8(match direction {
                Direction::Left => 0,
                Direction::Down => 1,
                Direction::Up => 2,
                Direction::Right => 3,
            });
        }
        Action::Tab(delta) => {
            e.u8(10);
            e.i32(*delta);
        }
        Action::CloseTab(tab) => {
            e.u8(22);
            e.u64(*tab);
        }
        Action::ClosePane(pane) => {
            e.u8(11);
            e.u64(*pane);
        }
        Action::Scroll { pane, scroll } => {
            e.u8(12);
            e.u64(*pane);
            match scroll {
                Scroll::Delta(delta) => {
                    e.u8(0);
                    e.i32(*delta);
                }
                Scroll::Top => e.u8(1),
                Scroll::Bottom => e.u8(2),
            }
        }
        Action::SelectionStart {
            pane,
            point,
            side,
            kind,
        } => {
            e.u8(13);
            e.u64(*pane);
            e.point(*point);
            encode_side(e, *side);
            e.u8(match kind {
                SelectionType::Simple => 0,
                SelectionType::Semantic => 1,
                SelectionType::Lines => 2,
                SelectionType::Object => 3,
            });
        }
        Action::SelectionUpdate { pane, point, side } => {
            e.u8(14);
            e.u64(*pane);
            e.point(*point);
            encode_side(e, *side);
        }
        Action::SelectionClear { pane } => {
            e.u8(15);
            e.u64(*pane);
        }
        Action::CopySelection { pane, request } => {
            e.u8(16);
            e.u64(*pane);
            e.u64(*request);
        }
        Action::PromptJump { pane, previous } => {
            e.u8(17);
            e.u64(*pane);
            e.bool(*previous);
        }
        Action::CopyBlock {
            pane,
            block,
            request,
        } => {
            e.u8(18);
            e.u64(*pane);
            e.u64(*block);
            e.u64(*request);
        }
        Action::Search {
            pane,
            request,
            query,
        } => {
            e.u8(24);
            e.u64(*pane);
            e.u64(*request);
            e.string(query);
        }
        Action::SearchSelect {
            pane,
            request,
            index,
        } => {
            e.u8(25);
            e.u64(*pane);
            e.u64(*request);
            e.u32(*index);
        }
        Action::SearchCancel { pane, request } => {
            e.u8(26);
            e.u64(*pane);
            e.u64(*request);
        }
        Action::Terminate => e.u8(19),
        Action::Detach => e.u8(20),
        Action::TerminateName(name) => {
            e.u8(21);
            e.string(name);
        }
    }
}
fn encode_input(e: &mut Encoder, pane: u64, data: &[u8]) {
    e.0.reserve(1 + 8 + 4 + data.len());
    e.u8(4);
    e.u64(pane);
    e.bytes(data);
}
fn encode_side(e: &mut Encoder, side: Side) {
    e.u8(match side {
        Side::Left => 0,
        Side::Right => 1,
    });
}
fn decode_side(d: &mut Decoder<'_>) -> io::Result<Side> {
    match d.u8()? {
        0 => Ok(Side::Left),
        1 => Ok(Side::Right),
        _ => Err(invalid("invalid selection side")),
    }
}
fn decode_action(d: &mut Decoder<'_>) -> io::Result<Action> {
    Ok(match d.u8()? {
        33 => Action::AgentInput {
            pane: d.u64()?,
            request: d.u64()?,
            generation: d.u64()?,
            prompt: if d.bool()? { Some(d.u64()?) } else { None },
            data: d.bytes(MAX_FRAME)?.to_vec(),
        },
        35 => Action::CancelHistory { request: d.u64()? },
        34 => Action::ReadHistory {
            pane: d.u64()?,
            request: d.u64()?,
            block: if d.bool()? { Some(d.u64()?) } else { None },
            cursor: decode_cursor(d)?,
            query: if d.bool()? {
                Some(d.string(MAX_FRAME)?)
            } else {
                None
            },
        },
        32 => Action::ScreenRequest,
        27 => Action::AgentOpen,
        28 => Action::AgentView(d.bytes(MAX_FRAME)?.to_vec()),
        29 => Action::AgentRequest(d.bytes(MAX_FRAME)?.to_vec()),
        30 => Action::AgentAttach {
            name: d.string(128)?,
            token: d.bytes(64)?.to_vec(),
        },
        31 => Action::AgentUpdate {
            kind: d.u8()?,
            payload: d.bytes(MAX_FRAME)?.to_vec(),
        },
        1 => Action::Create {
            name: d.string(128)?,
            options: decode_options(d)?,
        },
        2 => Action::Attach {
            name: d.string(128)?,
            size: d.size()?,
        },
        3 => Action::List,
        4 => Action::Input {
            pane: d.u64()?,
            data: d.bytes(65536)?.to_vec(),
        },
        5 => Action::Resize(d.size()?),
        6 => Action::Split {
            pane: d.u64()?,
            axis: match d.u8()? {
                0 => Axis::LeftRight,
                1 => Axis::TopBottom,
                _ => return Err(invalid("invalid split axis")),
            },
        },
        7 => Action::NewTab { pane: d.u64()? },
        8 => Action::Focus(d.u64()?),
        9 => Action::Navigate(match d.u8()? {
            0 => Direction::Left,
            1 => Direction::Down,
            2 => Direction::Up,
            3 => Direction::Right,
            _ => return Err(invalid("invalid direction")),
        }),
        10 => Action::Tab(d.i32()?),
        11 => Action::ClosePane(d.u64()?),
        12 => Action::Scroll {
            pane: d.u64()?,
            scroll: match d.u8()? {
                0 => Scroll::Delta(d.i32()?),
                1 => Scroll::Top,
                2 => Scroll::Bottom,
                _ => return Err(invalid("invalid scroll operation")),
            },
        },
        13 => {
            let pane = d.u64()?;
            let point = d.point()?;
            let side = decode_side(d)?;
            let kind = match d.u8()? {
                0 => SelectionType::Simple,
                1 => SelectionType::Semantic,
                2 => SelectionType::Lines,
                3 => SelectionType::Object,
                _ => return Err(invalid("invalid selection kind")),
            };
            Action::SelectionStart {
                pane,
                point,
                side,
                kind,
            }
        }
        14 => Action::SelectionUpdate {
            pane: d.u64()?,
            point: d.point()?,
            side: decode_side(d)?,
        },
        15 => Action::SelectionClear { pane: d.u64()? },
        16 => Action::CopySelection {
            pane: d.u64()?,
            request: d.u64()?,
        },
        17 => Action::PromptJump {
            pane: d.u64()?,
            previous: d.bool()?,
        },
        18 => Action::CopyBlock {
            pane: d.u64()?,
            block: d.u64()?,
            request: d.u64()?,
        },
        19 => Action::Terminate,
        20 => Action::Detach,
        21 => Action::TerminateName(d.string(128)?),
        22 => Action::CloseTab(d.u64()?),
        23 => Action::ResizeSplit {
            first: d.u64()?,
            second: d.u64()?,
            position: d.u16()?,
        },
        24 => Action::Search {
            pane: d.u64()?,
            request: d.u64()?,
            query: d.string(MAX_FRAME)?,
        },
        25 => Action::SearchSelect {
            pane: d.u64()?,
            request: d.u64()?,
            index: d.u32()?,
        },
        26 => Action::SearchCancel {
            pane: d.u64()?,
            request: d.u64()?,
        },
        _ => return Err(invalid("unknown workspace action")),
    })
}
fn encode_process(e: &mut Encoder, process: &crate::terminal::ProcessMetadata) {
    e.u32(process.launch_argv.len() as u32);
    for arg in &process.launch_argv {
        e.string(arg);
    }
    e.u32(process.child_pid);
    for pid in [process.foreground_pgid, process.foreground_pid] {
        e.bool(pid.is_some());
        if let Some(pid) = pid {
            e.u32(pid);
        }
    }
    e.path(&process.foreground_executable);
    e.bool(process.foreground_argv.is_some());
    if let Some(argv) = &process.foreground_argv {
        e.u32(argv.len() as u32);
        for arg in argv {
            e.string(arg);
        }
    }
    e.path(&process.foreground_cwd);
    e.bool(process.sampled_at_ms.is_some());
    if let Some(time) = process.sampled_at_ms {
        e.u64(time);
    }
    e.u32(process.unavailable.len() as u32);
    for reason in &process.unavailable {
        e.string(reason);
    }
}
fn decode_process(d: &mut Decoder<'_>) -> io::Result<crate::terminal::ProcessMetadata> {
    let launch_argv = d.strings(65536)?;
    let child_pid = d.u32()?;
    let foreground_pgid = if d.bool()? { Some(d.u32()?) } else { None };
    let foreground_pid = if d.bool()? { Some(d.u32()?) } else { None };
    let foreground_executable = d.path()?;
    let foreground_argv = if d.bool()? {
        Some(d.strings(65536)?)
    } else {
        None
    };
    let foreground_cwd = d.path()?;
    let sampled_at_ms = if d.bool()? { Some(d.u64()?) } else { None };
    let unavailable = d.strings(16)?;
    Ok(crate::terminal::ProcessMetadata {
        launch_argv,
        child_pid,
        foreground_pgid,
        foreground_pid,
        foreground_executable,
        foreground_argv,
        foreground_cwd,
        sampled_at_ms,
        unavailable,
    })
}
fn encode_cursor(e: &mut Encoder, cursor: Option<HistoryCursor>) {
    e.bool(cursor.is_some());
    if let Some(cursor) = cursor {
        e.u64(cursor.epoch);
        e.u64(cursor.row);
        e.u64(cursor.offset);
    }
}
fn decode_cursor(d: &mut Decoder<'_>) -> io::Result<Option<HistoryCursor>> {
    Ok(if d.bool()? {
        Some(HistoryCursor {
            epoch: d.u64()?,
            row: d.u64()?,
            offset: d.u64()?,
        })
    } else {
        None
    })
}
fn encode_command(e: &mut Encoder, command: &CommandBlock) {
    e.u64(command.id);
    e.bool(command.complete);
    e.bool(command.has_output);
    e.bool(command.status.is_some());
    if let Some(status) = command.status {
        e.i32(status);
    }
}
fn decode_command(d: &mut Decoder<'_>) -> io::Result<CommandBlock> {
    Ok(CommandBlock {
        id: d.u64()?,
        complete: d.bool()?,
        has_output: d.bool()?,
        status: if d.bool()? { Some(d.i32()?) } else { None },
    })
}
fn encode_snapshot(
    e: &mut Encoder,
    snapshot: &Snapshot,
    previous: &HashMap<u64, Arc<ProcessMetadata>>,
) {
    e.string(&snapshot.name);
    e.bool(snapshot.controller);
    e.u64(snapshot.focused);
    e.u64(snapshot.active_tab);
    e.u32(snapshot.tabs.len() as u32);
    for tab in &snapshot.tabs {
        e.u64(tab.id);
        e.string(&tab.title);
    }
    e.u32(snapshot.panes.len() as u32);
    for pane in &snapshot.panes {
        e.u64(pane.id);
        e.u16(pane.rect.x);
        e.u16(pane.rect.y);
        e.u16(pane.rect.cols);
        e.u16(pane.rect.rows);
        e.string(&pane.title);
        e.path(&pane.cwd);
        e.u64(pane.input_generation);
        e.bool(pane.shell_prompt.is_some());
        if let Some(prompt) = pane.shell_prompt {
            e.u64(prompt);
        }
        let changed = previous
            .get(&pane.id)
            .is_none_or(|process| !Arc::ptr_eq(process, &pane.process));
        e.bool(changed);
        if changed {
            encode_process(e, &pane.process);
        }
        e.u32(pane.blocks.len() as u32);
        for block in &pane.blocks {
            e.u64(block.id);
            e.i32(block.line);
            e.bool(block.complete);
            e.bool(block.output.is_some());
            if let Some((start, end)) = block.output {
                e.point(start);
                e.point(end);
            }
            e.bool(block.status.is_some());
            if let Some(status) = block.status {
                e.i32(status);
            }
        }
        e.u32(pane.command_blocks.len() as u32);
        for block in &pane.command_blocks {
            encode_command(e, block);
        }
    }
}
fn decode_snapshot(d: &mut Decoder<'_>, previous: Option<&Snapshot>) -> io::Result<Snapshot> {
    let name = d.string(128)?;
    let controller = d.bool()?;
    let focused = d.u64()?;
    let active_tab = d.u64()?;
    let count = d.count(1024)?;
    let mut tabs = Vec::with_capacity(count);
    for _ in 0..count {
        tabs.push(TabView {
            id: d.u64()?,
            title: d.string(65536)?,
        });
    }
    let count = d.count(1024)?;
    let mut panes = Vec::with_capacity(count);
    for _ in 0..count {
        let id = d.u64()?;
        let rect = crate::workspace::Rect {
            x: d.u16()?,
            y: d.u16()?,
            cols: d.u16()?,
            rows: d.u16()?,
        };
        let title = d.string(65536)?;
        let cwd = d.path()?;
        let input_generation = d.u64()?;
        let shell_prompt = if d.bool()? { Some(d.u64()?) } else { None };
        let process = if d.bool()? {
            Arc::new(decode_process(d)?)
        } else {
            previous
                .filter(|snapshot| snapshot.name == name)
                .and_then(|snapshot| snapshot.panes.iter().find(|pane| pane.id == id))
                .map(|pane| Arc::clone(&pane.process))
                .ok_or_else(|| invalid("pane process metadata has no prior snapshot"))?
        };
        let count = d.count(4096)?;
        let mut blocks = Vec::with_capacity(count);
        for _ in 0..count {
            let id = d.u64()?;
            let line = d.i32()?;
            let complete = d.bool()?;
            let output = if d.bool()? {
                Some((d.point()?, d.point()?))
            } else {
                None
            };
            let status = if d.bool()? { Some(d.i32()?) } else { None };
            blocks.push(BlockMarker {
                id,
                line,
                complete,
                status,
                output,
            });
        }
        let count = d.count(4)?;
        let mut command_blocks = Vec::with_capacity(count);
        for _ in 0..count {
            command_blocks.push(decode_command(d)?);
        }
        panes.push(PaneView {
            id,
            rect,
            title,
            cwd,
            process,
            blocks,
            command_blocks,
            input_generation,
            shell_prompt,
        });
    }
    Ok(Snapshot {
        name,
        controller,
        focused,
        active_tab,
        tabs,
        panes,
    })
}
fn encode_color(e: &mut Encoder, color: Color) {
    match color {
        Color::Named(name) => {
            let [lo, hi] = (name as u16).to_le_bytes();
            e.0.extend_from_slice(&[0, lo, hi]);
        }
        Color::Indexed(index) => e.0.extend_from_slice(&[1, index]),
        Color::Spec(rgb) => e.0.extend_from_slice(&[2, rgb.r, rgb.g, rgb.b]),
    }
}
fn decode_color(d: &mut Decoder<'_>) -> io::Result<Color> {
    match d.u8()? {
        0 => Ok(Color::Named(
            NamedColor::from_index(d.u16()?).ok_or_else(|| invalid("invalid named color"))?,
        )),
        1 => Ok(Color::Indexed(d.u8()?)),
        2 => {
            let rgb = d.take(3)?;
            Ok(Color::Spec(Rgb {
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
            }))
        }
        _ => Err(invalid("invalid color kind")),
    }
}
fn encode_update(e: &mut Encoder, update: &Update) {
    e.u16(update.columns as u16);
    e.u16(update.lines as u16);
    e.point(update.cursor.0);
    e.u8(match update.cursor.1 {
        CursorShape::Block => 0,
        CursorShape::Underline => 1,
        CursorShape::Beam => 2,
        CursorShape::Hidden => 3,
    });
    e.u32(update.mode.bits());
    e.u64(update.offset as u64);
    e.u64(update.history as u64);
    e.bool(update.selection.is_some());
    if let Some(selection) = update.selection {
        e.point(selection.start);
        e.point(selection.end);
    }
    e.bool(update.palette.is_some());
    if let Some(palette) = &update.palette {
        e.0.reserve(palette.len() * 3);
        for rgb in palette.iter() {
            e.0.extend_from_slice(&[rgb.r, rgb.g, rgb.b]);
        }
    }
    e.u32(update.rows.len() as u32);
    for (index, row) in &update.rows {
        e.u16(*index as u16);
        e.bool(row.wrapped);
        e.u16(row.used() as u16);
        // The reflow text length does not cover every styled blank or wide-wrap cell.
        let stored = row.non_default_len();
        e.u16(stored as u16);
        let length_at = e.0.len();
        e.u16(0);
        for cell in &row.cells[..stored] {
            if cell.c.is_ascii() {
                e.u8(cell.c as u8);
            } else {
                e.0.extend_from_slice(cell.c.encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
        let length = (e.0.len() - length_at - 2) as u16;
        e.0[length_at..length_at + 2].copy_from_slice(&length.to_le_bytes());
        for run in row.cells[..stored].chunk_by(|left, right| {
            left.fg == right.fg && left.bg == right.bg && left.flags == right.flags
        }) {
            let style = run[0];
            e.u16(run.len() as u16);
            encode_color(e, style.fg);
            encode_color(e, style.bg);
            e.u16(style.flags.bits());
        }
        e.u32(row.extras().len() as u32);
        for (column, extra) in row.extras() {
            e.u16(*column as u16);
            let zerowidth = extra.zerowidth.as_slice();
            e.u32(zerowidth.iter().map(|ch| ch.len_utf8()).sum::<usize>() as u32);
            for ch in zerowidth {
                e.0.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            }
            e.bool(extra.underline_color.is_some());
            if let Some(color) = extra.underline_color {
                encode_color(e, color);
            }
        }
    }
}
fn decode_update(d: &mut Decoder<'_>) -> io::Result<Update> {
    let columns = d.u16()? as usize;
    let lines = d.u16()? as usize;
    if !(1..=4096).contains(&columns) || lines == 0 || lines > 2048 || columns * lines > 1_048_576 {
        return Err(invalid("invalid screen dimensions"));
    }
    let cursor_point = d.point()?;
    let shape = match d.u8()? {
        0 => CursorShape::Block,
        1 => CursorShape::Underline,
        2 => CursorShape::Beam,
        3 => CursorShape::Hidden,
        _ => return Err(invalid("invalid cursor shape")),
    };
    if cursor_point.column >= columns {
        return Err(invalid("cursor exceeds screen columns"));
    }
    let mode = Mode::from_bits(d.u32()?).ok_or_else(|| invalid("invalid terminal mode"))?;
    let offset = usize::try_from(d.u64()?).map_err(|_| invalid("display offset overflow"))?;
    let history = usize::try_from(d.u64()?).map_err(|_| invalid("history size overflow"))?;
    let selection = if d.bool()? {
        Some(SelectionRange {
            start: d.point()?,
            end: d.point()?,
        })
    } else {
        None
    };
    let palette = if d.bool()? {
        let mut palette = [Rgb::default(); 269];
        let bytes = d.take(palette.len() * 3)?;
        for (rgb, bytes) in palette.iter_mut().zip(bytes.chunks_exact(3)) {
            *rgb = Rgb {
                r: bytes[0],
                g: bytes[1],
                b: bytes[2],
            };
        }
        Some(Box::new(palette))
    } else {
        None
    };
    let count = d.count(lines)?;
    let mut rows = Vec::with_capacity(count);
    let mut seen = [0u64; 32];
    for _ in 0..count {
        let index = d.u16()? as usize;
        let mask = 1u64 << (index & 63);
        if index >= lines || seen[index >> 6] & mask != 0 {
            return Err(invalid("duplicate or invalid screen row"));
        }
        seen[index >> 6] |= mask;
        let wrapped = d.bool()?;
        let used = d.u16()? as usize;
        let stored = d.u16()? as usize;
        if stored > columns {
            return Err(invalid("screen row exceeds columns"));
        }
        let length = d.u16()? as usize;
        if length < stored || length > stored * 4 {
            return Err(invalid("invalid screen text length"));
        }
        let text = std::str::from_utf8(d.take(length)?)
            .map_err(|_| invalid("invalid screen character encoding"))?;
        let ascii = stored != 0 && length == stored;
        if ascii && !text.is_ascii() {
            return Err(invalid("missing screen characters"));
        }
        let mut chars = text.chars();
        let mut cells = Vec::with_capacity(columns);
        while cells.len() < stored {
            let count = d.u16()? as usize;
            if count == 0 || count > stored - cells.len() {
                return Err(invalid("invalid screen style run length"));
            }
            let fg = decode_color(d)?;
            let bg = decode_color(d)?;
            let flags = Flags::from_bits(d.u16()?).ok_or_else(|| invalid("invalid cell flags"))?;
            if count == 1 {
                let c = if ascii {
                    char::from(text.as_bytes()[cells.len()])
                } else {
                    chars
                        .next()
                        .ok_or_else(|| invalid("missing screen characters"))?
                };
                cells.push(Cell { c, fg, bg, flags });
            } else if ascii {
                let end = cells.len() + count;
                cells.extend(text.as_bytes()[cells.len()..end].iter().map(|&c| Cell {
                    c: char::from(c),
                    fg,
                    bg,
                    flags,
                }));
            } else {
                let end = cells.len() + count;
                cells.extend((&mut chars).take(count).map(|c| Cell { c, fg, bg, flags }));
                if cells.len() != end {
                    return Err(invalid("missing screen characters"));
                }
            }
        }
        if !ascii && chars.next().is_some() {
            return Err(invalid("screen text exceeds columns"));
        }
        cells.resize(columns, Cell::default());
        let count = d.count(columns)?;
        let mut extras = Vec::with_capacity(count);
        for _ in 0..count {
            let column = d.u16()? as usize;
            let bytes = d.bytes(65536)?;
            let zerowidth = if bytes.is_empty() {
                Default::default()
            } else {
                std::str::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .chars()
                    .collect()
            };
            let underline_color = if d.bool()? {
                Some(decode_color(d)?)
            } else {
                None
            };
            extras.push((
                column,
                Extra {
                    zerowidth,
                    underline_color,
                },
            ));
        }
        let row = Row::from_parts(cells, wrapped, used, extras)
            .ok_or_else(|| invalid("invalid screen row contents"))?;
        rows.push((index, row));
    }
    Ok(Update {
        columns,
        lines,
        rows,
        cursor: (cursor_point, shape),
        mode,
        palette,
        selection,
        offset,
        history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_deltas_inherit_the_latest_snapshot_in_and_across_reads() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        let mut snapshot = Snapshot {
            name: "delta".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: Vec::new(),
            panes: vec![PaneView {
                id: 1,
                rect: crate::workspace::Rect::default(),
                title: String::new(),
                cwd: None,
                process: Arc::new(ProcessMetadata {
                    child_pid: 1,
                    foreground_argv: Some(vec!["old".into()]),
                    ..ProcessMetadata::default()
                }),
                blocks: Vec::new(),
                command_blocks: Vec::new(),
                input_generation: 0,
                shell_prompt: None,
            }],
        };
        let mut known = HashMap::new();
        sender.send_state(&snapshot, &[], &known).unwrap();
        let full_size = sender.queued;
        known.insert(1, Arc::clone(&snapshot.panes[0].process));
        sender.send_state(&snapshot, &[], &known).unwrap();
        assert!(sender.queued - full_size < full_size);
        sender
            .send_response(Response::Error("marker".into()))
            .unwrap();
        snapshot.panes[0].process = Arc::new(ProcessMetadata {
            child_pid: 1,
            foreground_argv: Some(vec!["new".into()]),
            ..ProcessMetadata::default()
        });
        sender.send_state(&snapshot, &[], &known).unwrap();
        known.insert(1, Arc::clone(&snapshot.panes[0].process));
        sender.send_state(&snapshot, &[], &known).unwrap();
        sender.flush().unwrap();
        let responses = receiver.responses(None).unwrap();
        let [
            Response::State(first, _),
            Response::State(second, _),
            Response::Error(_),
            Response::State(changed, _),
            Response::State(last, _),
        ] = responses.as_slice()
        else {
            panic!("state sequence changed")
        };
        assert!(Arc::ptr_eq(
            &first.panes[0].process,
            &second.panes[0].process
        ));
        assert_eq!(
            first.panes[0].process.foreground_argv.as_ref().unwrap(),
            &["old"]
        );
        assert_eq!(
            changed.panes[0].process.foreground_argv.as_ref().unwrap(),
            &["new"]
        );
        assert!(Arc::ptr_eq(
            &changed.panes[0].process,
            &last.panes[0].process
        ));
        sender.send_state(&snapshot, &[], &known).unwrap();
        sender.flush().unwrap();
        let next = receiver.responses(Some(last)).unwrap();
        let Response::State(next, _) = &next[0] else {
            panic!("state not delivered")
        };
        assert!(Arc::ptr_eq(&last.panes[0].process, &next.panes[0].process));
    }

    #[test]
    fn remote_state_preserves_process_metadata_and_unknown_fields() {
        let expected = crate::terminal::ProcessMetadata {
            launch_argv: vec!["gdb".into(), "--args".into(), "sudo".into()],
            child_pid: 21,
            foreground_pgid: Some(22),
            foreground_pid: Some(22),
            foreground_executable: Some("/usr/bin/gdb".into()),
            foreground_argv: Some(vec!["gdb".into(), "".into(), "调试".into()]),
            foreground_cwd: None,
            sampled_at_ms: Some(123456),
            unavailable: vec!["foreground cwd: Permission denied (os error 13)".into()],
        };
        let command_blocks = vec![
            CommandBlock {
                id: 42,
                complete: false,
                status: Some(7),
                has_output: true,
            },
            CommandBlock {
                id: 43,
                complete: false,
                status: None,
                has_output: false,
            },
        ];
        let snapshot = Snapshot {
            name: "remote".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: Vec::new(),
            panes: vec![PaneView {
                id: 1,
                rect: crate::workspace::Rect::default(),
                title: "debugger".into(),
                cwd: None,
                process: Arc::new(expected.clone()),
                blocks: Vec::new(),
                command_blocks: command_blocks.clone(),
                input_generation: 23,
                shell_prompt: Some(44),
            }],
        };
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender
            .send_response(Response::State(snapshot, Vec::new()))
            .unwrap();
        sender.flush().unwrap();
        receiver.read().unwrap();
        let responses = receiver.responses(None).unwrap();
        let Response::State(snapshot, _) = &responses[0] else {
            panic!("state not delivered")
        };
        assert_eq!(*snapshot.panes[0].process, expected);
        assert_eq!(snapshot.panes[0].input_generation, 23);
        assert_eq!(snapshot.panes[0].shell_prompt, Some(44));
        assert_eq!(snapshot.panes[0].command_blocks, command_blocks);
    }

    #[test]
    fn history_response_preserves_eof_cursor_search_and_command_facts() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        let cursor = HistoryCursor {
            epoch: 7,
            row: 21,
            offset: 5,
        };
        for (request, matched) in [None, Some(false), Some(true)].into_iter().enumerate() {
            sender
                .send_response(Response::History {
                    request: request as u64,
                    result: Ok(HistoryPage {
                        text: "中ab".into(),
                        cursor,
                        has_more: false,
                        matched,
                        incomplete: true,
                        command: Some(CommandBlock {
                            id: 9,
                            complete: true,
                            status: Some(7),
                            has_output: true,
                        }),
                    }),
                })
                .unwrap();
        }
        sender.flush().unwrap();
        let responses = receiver.responses(None).unwrap();
        assert_eq!(responses.len(), 3);
        for (response, matched) in responses.iter().zip([None, Some(false), Some(true)]) {
            let Response::History {
                result: Ok(page), ..
            } = response
            else {
                panic!("history page not delivered")
            };
            assert_eq!(page.text, "中ab");
            assert_eq!(page.cursor, cursor);
            assert!(!page.has_more);
            assert_eq!(page.matched, matched);
            assert!(page.incomplete);
            assert_eq!(page.command.as_ref().unwrap().status, Some(7));
        }
    }
    #[test]
    fn search_results_follow_remote_greeting_without_clipboard_state() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender.send_hello(1).unwrap();
        sender.flush().unwrap();
        assert_eq!(receiver.hello().unwrap(), Some(1));
        sender
            .send_response(Response::Search {
                pane: 3,
                request: 4,
                count: 10_000,
                truncated: true,
            })
            .unwrap();
        sender.flush().unwrap();
        receiver.read().unwrap();
        let responses = receiver.responses(None).unwrap();
        assert!(matches!(
            &responses[..],
            [Response::Search {
                pane: 3,
                request: 4,
                count: 10_000,
                truncated: true
            }]
        ));
    }
    #[test]
    fn bulk_receive_storage_is_reused_until_small_messages_resume() {
        fn transfer(sender: &mut Peer, receiver: &mut Peer, expected: &str) {
            sender
                .send_response(Response::ClipboardStore {
                    primary: false,
                    text: expected.into(),
                })
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                sender.flush().unwrap();
                let messages = receiver.responses(None).unwrap();
                if !messages.is_empty() {
                    assert_eq!(messages.len(), 1);
                    assert!(
                        matches!(&messages[0], Response::ClipboardStore { text, .. } if text == expected)
                    );
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "message transfer stalled"
                );
            }
        }
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let large = "x".repeat(1024 * 1024);
        transfer(&mut sender, &mut receiver, &large);
        assert!(receiver.responses(None).unwrap().is_empty());
        let capacity = receiver.input.capacity();
        transfer(&mut sender, &mut receiver, &large);
        assert!(receiver.responses(None).unwrap().is_empty());
        assert_eq!(receiver.input.capacity(), capacity);
        transfer(&mut sender, &mut receiver, "done");
        assert!(receiver.responses(None).unwrap().is_empty());
        assert!(receiver.input.capacity() < capacity / 2);
    }

    #[test]
    fn partial_and_coalesced_frames_roundtrip() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        sender
            .send_action(&Action::Input {
                pane: 42,
                data: "你好\u{1b}[A".as_bytes().to_vec(),
            })
            .unwrap();
        sender.send_action(&Action::List).unwrap();
        sender.flush().unwrap();
        let first = receiver.next_action().unwrap().unwrap();
        let second = receiver.next_action().unwrap().unwrap();
        assert!(
            matches!(first, Action::Input { pane: 42, data } if data == "你好\u{1b}[A".as_bytes())
        );
        assert!(matches!(second, Action::List));
    }
    #[test]
    fn coalesced_actions_preserve_a_fragmented_trailing_frame_and_final_eof() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let mut encoder = Encoder::new();
        encode_action(&mut encoder, &Action::List);
        let mut frame = encoder.0;
        let length = (frame.len() - 4) as u32;
        frame[..4].copy_from_slice(&length.to_le_bytes());
        left.write_all(&frame.repeat(256)).unwrap();
        left.write_all(&frame[..2]).unwrap();
        for _ in 0..256 {
            assert!(matches!(
                receiver.next_action().unwrap(),
                Some(Action::List)
            ));
        }
        assert!(receiver.next_action().unwrap().is_none());
        left.write_all(&frame[2..]).unwrap();
        drop(left);
        assert!(matches!(
            receiver.next_action().unwrap(),
            Some(Action::List)
        ));
        assert_eq!(
            receiver.next_action().err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn framed_relay_preserves_bytes_fragments_and_final_eof() {
        let (mut source, input) = UnixStream::pair().unwrap();
        let (output, sink) = UnixStream::pair().unwrap();
        let mut relay_input = Peer::remote(input).unwrap();
        let mut relay_output = Peer::new(output).unwrap();
        let mut receiver = Peer::new(sink).unwrap();
        let mut encoder = Encoder::new();
        encode_action(
            &mut encoder,
            &Action::Input {
                pane: 42,
                data: "中e\u{301}\x1b[A".as_bytes().to_vec(),
            },
        );
        let mut bytes = encoder.0;
        let length = (bytes.len() - 4) as u32;
        bytes[..4].copy_from_slice(&length.to_le_bytes());
        source.write_all(&bytes[..2]).unwrap();
        assert!(relay_input.next_frame().unwrap().is_none());
        source.write_all(&bytes[2..8]).unwrap();
        assert!(relay_input.next_frame().unwrap().is_none());
        source.write_all(&bytes[8..]).unwrap();
        source.write_all(&bytes[..3]).unwrap();
        let frame = relay_input.next_frame().unwrap().unwrap();
        assert_eq!(frame.kind(), 4);
        relay_output.send_frame(frame).unwrap();
        assert_eq!(relay_output.output.front().unwrap().bytes, bytes);
        assert!(relay_input.next_frame().unwrap().is_none());
        source.write_all(&bytes[3..]).unwrap();
        drop(source);
        relay_output
            .send_frame(relay_input.next_frame().unwrap().unwrap())
            .unwrap();
        assert_eq!(
            relay_input.next_frame().err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        relay_output.flush().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                receiver.next_action().unwrap(),
                Some(Action::Input { pane: 42, data }) if data == "中e\u{301}\x1b[A".as_bytes()
            ));
        }
    }
    #[test]
    fn framed_relay_checks_lengths_versions_and_remote_descriptors() {
        for bytes in [
            0u32.to_le_bytes().to_vec(),
            (MAX_FRAME as u32 + 1).to_le_bytes().to_vec(),
            vec![2, 0, 0, 0, VERSION + 1, 1],
            vec![1, 0, 0, 0, VERSION],
        ] {
            let (mut source, input) = UnixStream::pair().unwrap();
            source.write_all(&bytes).unwrap();
            assert_eq!(
                Peer::remote(input)
                    .unwrap()
                    .next_frame()
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let (source, input) = UnixStream::pair().unwrap();
        let mut source = Peer::new(source).unwrap();
        source
            .send_response(Response::Clipboard {
                request: 7,
                file: File::open("Cargo.toml").unwrap(),
                incomplete: false,
            })
            .unwrap();
        source.flush().unwrap();
        let error = Peer::remote(input).unwrap().next_frame().err().unwrap();
        assert!(
            error
                .to_string()
                .contains("cannot contain file descriptors")
        );
    }
    #[test]
    fn framed_clipboard_uses_the_local_descriptor() {
        let (source, input) = UnixStream::pair().unwrap();
        let mut source = Peer::new(source).unwrap();
        let mut receiver = Peer::new(input).unwrap();
        source
            .send_response(Response::Clipboard {
                request: 91,
                file: File::open("Cargo.toml").unwrap(),
                incomplete: true,
            })
            .unwrap();
        source.flush().unwrap();
        let frame = receiver.next_frame().unwrap().unwrap();
        assert_eq!(frame.kind(), 3);
        let (request, mut file, incomplete) = receiver.take_clipboard(frame).unwrap();
        assert_eq!(request, 91);
        assert!(incomplete);
        use std::io::Read;
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("[package]"));
        assert!(receiver.files.is_empty());
    }
    #[test]
    fn clipboard_file_descriptor_is_transferred_without_copying_payload() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let file = File::open("Cargo.toml").unwrap();
        sender
            .send_response(Response::Clipboard {
                request: 7,
                file,
                incomplete: true,
            })
            .unwrap();
        sender.flush().unwrap();
        let response = receiver.responses(None).unwrap().pop().unwrap();
        if let Response::Clipboard {
            request,
            mut file,
            incomplete,
        } = response
        {
            use std::io::Read;
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            assert!(text.starts_with("[package]"));
            assert_eq!(request, 7);
            assert!(incomplete);
        } else {
            panic!("wrong response");
        }
    }
    #[test]
    fn remote_clipboard_is_streamed_past_the_frame_limit() {
        let directory = std::env::current_dir().unwrap().join(format!(
            "target/remote-ssh-20261024/transport/wire-copy-{}-{}",
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
                "wire::tests::remote_clipboard_child",
                "--nocapture",
            ])
            .env("NEXTTY_TEST_COPY", "1")
            .env("XDG_CACHE_HOME", &directory)
            .status()
            .unwrap();
        assert!(result.success());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn remote_clipboard_child() {
        if std::env::var_os("NEXTTY_TEST_COPY").is_none() {
            return;
        }
        use std::io::Read;
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::remote(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender.begin_copy(97, true).unwrap();
        sender.flush().unwrap();
        assert!(receiver.responses(None).unwrap().is_empty());
        let bytes: Vec<u8> = (0..65536).map(|index| (index % 251) as u8).collect();
        for _ in 0..600 {
            sender.copy_chunk(&bytes).unwrap();
            sender.flush().unwrap();
            assert!(receiver.responses(None).unwrap().is_empty());
            while receiver.buffered_action() {
                if !receiver.needs_poll() {
                    wait_receiver(&receiver);
                }
                assert!(receiver.responses(None).unwrap().is_empty());
            }
            assert!(sender.queued <= bytes.len() + 10);
            assert!(receiver.input.len() <= bytes.len() + 10);
        }
        sender.copy_chunk(&[]).unwrap();
        sender.flush().unwrap();
        let response = remote_response(&mut receiver).unwrap().pop().unwrap();
        let Response::Clipboard {
            request,
            mut file,
            incomplete,
        } = response
        else {
            panic!("wrong response");
        };
        assert_eq!(request, 97);
        assert!(incomplete);
        assert_eq!(file.metadata().unwrap().len(), 600 * 65536);
        assert_eq!(file.metadata().unwrap().nlink(), 0);
        let mut buffer = [0; 65536];
        for _ in 0..600 {
            file.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, bytes.as_slice());
        }
        assert_eq!(file.read(&mut buffer).unwrap(), 0);
        sender.begin_copy(98, false).unwrap();
        sender.copy_chunk(&[]).unwrap();
        sender
            .send_response(Response::Error("after clipboard".into()))
            .unwrap();
        sender.flush().unwrap();
        assert!(matches!(
            remote_response(&mut receiver).unwrap().as_slice(),
            [Response::Clipboard {
                request: 98,
                incomplete: false,
                ..
            }]
        ));
        assert!(receiver.needs_poll());
        assert!(
            matches!(receiver.responses(None).unwrap().as_slice(), [Response::Error(message)] if message == "after clipboard")
        );
        sender.begin_copy(99, false).unwrap();
        sender.copy_chunk(b"partial").unwrap();
        sender.flush().unwrap();
        assert!(receiver.responses(None).unwrap().is_empty());
        let receiving = Arc::downgrade(&receiver.receiver.as_ref().unwrap().0);
        drop(sender);
        let error = remote_response(&mut receiver).err().unwrap();
        assert!(error.to_string().contains("before completion"));
        drop(receiver);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while receiving.strong_count() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard receiver did not finish cleanup"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::remote(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender.begin_copy(100, false).unwrap();
        sender.copy_chunk(b"complete before EOF").unwrap();
        sender.copy_chunk(&[]).unwrap();
        sender.flush().unwrap();
        drop(sender);
        let response = remote_response(&mut receiver).unwrap().pop().unwrap();
        let Response::Clipboard {
            request, mut file, ..
        } = response
        else {
            panic!("wrong response")
        };
        assert_eq!(request, 100);
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "complete before EOF");
        assert_eq!(
            receiver.responses(None).err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let directory =
            PathBuf::from(std::env::var_os("XDG_CACHE_HOME").unwrap()).join("nextty/clipboard");
        std::fs::remove_dir(&directory).unwrap();
        File::create(&directory).unwrap();
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::remote(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender.begin_copy(101, false).unwrap();
        sender.copy_chunk(&[]).unwrap();
        sender.flush().unwrap();
        assert_eq!(
            remote_response(&mut receiver).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        let leftovers: Vec<_> = std::fs::read_dir(std::env::var_os("XDG_CACHE_HOME").unwrap())
            .unwrap()
            .collect();
        assert_eq!(leftovers.len(), 1);
    }
    fn wait_receiver(receiver: &Peer) {
        let mut fds = [
            libc::pollfd {
                fd: if receiver.wants_read() {
                    receiver.fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: receiver.receiver_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        assert!(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 5000) } > 0);
    }
    fn remote_response(receiver: &mut Peer) -> io::Result<Vec<Response>> {
        loop {
            let responses = receiver.responses(None)?;
            if !responses.is_empty() {
                return Ok(responses);
            }
            if !receiver.needs_poll() {
                wait_receiver(receiver);
            }
        }
    }
    #[test]
    fn remote_clipboard_rejects_missing_start_and_local_stream_tags() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::remote(left).unwrap();
        let mut receiver = Peer::remote(right).unwrap();
        sender.copy_chunk(b"orphan").unwrap();
        sender.flush().unwrap();
        assert!(
            receiver
                .responses(None)
                .err()
                .unwrap()
                .to_string()
                .contains("no start")
        );
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::remote(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        sender.begin_copy(1, false).unwrap();
        sender.flush().unwrap();
        assert!(receiver.responses(None).is_err());
    }
    #[test]
    fn malformed_boundary_is_rejected() {
        assert!(Decoder::new(&[255]).is_err());
        let mut decoder = Decoder::new(&[VERSION, 2]).unwrap();
        assert!(decoder.bool().is_err());
        let mut encoder = Encoder::new();
        encoder.u8(6);
        encoder.u64(1);
        encoder.u8(7);
        assert!(decode_action(&mut Decoder::new(&encoder.0[4..]).unwrap()).is_err());
    }
    #[test]
    fn fragmented_header_and_body_wait_for_complete_frame() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let mut encoder = Encoder::new();
        encode_action(
            &mut encoder,
            &Action::Input {
                pane: 3,
                data: b"fragment".to_vec(),
            },
        );
        let mut packet = encoder.0;
        let length = (packet.len() - 4) as u32;
        packet[..4].copy_from_slice(&length.to_le_bytes());
        left.write_all(&packet[..2]).unwrap();
        assert!(receiver.next_action().unwrap().is_none());
        left.write_all(&packet[2..8]).unwrap();
        assert!(receiver.next_action().unwrap().is_none());
        left.write_all(&packet[8..]).unwrap();
        assert!(
            matches!(receiver.next_action().unwrap(), Some(Action::Input { pane: 3, data }) if data == b"fragment")
        );
    }
    #[test]
    fn styled_unicode_screen_roundtrip_preserves_color_roles_and_extras() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 12,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(
            &mut core,
            "\u{1b}[1;31mA中e\u{301}\u{1b}[0m plain".as_bytes(),
        );
        let original = Update::capture(&core, true);
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &original);
        let decoded = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
        assert_eq!(decoded.rows[0].1.cells, original.rows[0].1.cells);
        assert_eq!(decoded.rows[0].1.extras(), original.rows[0].1.extras());
        assert_eq!(decoded.palette, original.palette);
        assert_eq!(decoded.cursor, original.cursor);
        assert_eq!(decoded.mode, original.mode);
        assert_eq!(
            decoded.rows[0].1.cells[0].bg,
            Color::Named(crate::vt::NamedColor::Background)
        );
    }
    #[test]
    fn sparse_and_many_style_runs_preserve_every_cell_and_extra() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 4096,
                num_lines: 2,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = crate::vt::Parser::new();
        for column in 0..4096 {
            parser.advance(
                &mut core,
                format!(
                    "\x1b[38;2;{};{};{}m中",
                    column % 256,
                    column / 16 % 256,
                    column / 256 % 256
                )
                .as_bytes(),
            );
        }
        let mut original = Update::capture(&core, true);
        let mut cells = vec![Cell::default(); 4096];
        cells[0].c = 'A';
        cells[4095].bg = Color::Indexed(3);
        original.rows[1].1 = Row::from_parts(
            cells,
            false,
            1,
            vec![(
                3072,
                Extra {
                    zerowidth: vec!['\u{301}'].into(),
                    underline_color: Some(Color::Indexed(12)),
                },
            )],
        )
        .unwrap();
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &original);
        let decoded = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
        for ((_, left), (_, right)) in original.rows.iter().zip(&decoded.rows) {
            assert_eq!(left.cells, right.cells);
            assert_eq!(left.extras(), right.extras());
            assert_eq!(left.used(), right.used());
            assert_eq!(left.wrapped, right.wrapped);
        }
        // Entirely default tails are restored, independently of the text used count.
        original.rows[1].1.cells[4095] = Cell::default();
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &original);
        let decoded = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
        assert_eq!(decoded.rows[1].1.cells, original.rows[1].1.cells);
        assert_eq!(decoded.rows[1].1.extras(), original.rows[1].1.extras());
    }

    #[test]
    fn malformed_text_runs_and_truncated_updates_are_rejected() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 12,
                num_lines: 2,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(&mut core, b"payload");
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &Update::capture(&core, true));
        let bytes = &encoder.0[4..];
        for end in 0..bytes.len() {
            let result = Decoder::new(&bytes[..end]).and_then(|mut d| decode_update(&mut d));
            assert!(result.is_err(), "accepted truncated update at {end}");
        }
        let text_at = bytes
            .windows(7)
            .position(|bytes| bytes == b"payload")
            .unwrap();
        let mut invalid_utf8 = bytes.to_vec();
        invalid_utf8[text_at] = 0xff;
        assert!(decode_update(&mut Decoder::new(&invalid_utf8).unwrap()).is_err());
        let mut empty_run = bytes.to_vec();
        empty_run[text_at - 2..text_at].fill(0);
        assert!(decode_update(&mut Decoder::new(&empty_run).unwrap()).is_err());
        let mut oversized_run = bytes.to_vec();
        oversized_run[text_at - 2..text_at].copy_from_slice(&8u16.to_le_bytes());
        assert!(decode_update(&mut Decoder::new(&oversized_run).unwrap()).is_err());
        let style_at = text_at + 7;
        let mut empty_style = bytes.to_vec();
        empty_style[style_at..style_at + 2].fill(0);
        assert!(decode_update(&mut Decoder::new(&empty_style).unwrap()).is_err());
        let mut oversized_style = bytes.to_vec();
        oversized_style[style_at..style_at + 2].copy_from_slice(&8u16.to_le_bytes());
        assert!(decode_update(&mut Decoder::new(&oversized_style).unwrap()).is_err());
    }

    #[test]
    fn one_column_content_screen_roundtrips_after_a_narrow_split() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 1,
                num_lines: 2,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(&mut core, b"x");
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &Update::capture(&core, true));
        let update = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
        let mut screen = crate::screen::Screen::default();
        screen.apply(update).unwrap();
        assert_eq!(screen.columns(), 1);
        assert_eq!(screen.row(0).cells[0].c, 'x');
    }

    #[test]
    fn screen_preserves_prompt_flags_and_explicit_right_prompt() {
        for scope in [1, 2] {
            let mut core = crate::core::Core::new(
                WindowSize {
                    num_cols: 8,
                    num_lines: 2,
                    cell_width: 0,
                    cell_height: 0,
                },
                0xcccccc,
                0x202020,
            );
            crate::vt::Parser::new().advance(
                &mut core,
                format!("\x1b[1;5H\x1b]133;P;{scope}\x07main\x1b]133;P;0\x07").as_bytes(),
            );
            let mut encoder = Encoder::new();
            encode_update(&mut encoder, &Update::capture(&core, true));
            assert_eq!(encoder.0[4], VERSION);
            let update = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
            assert!(update.rows[0].1.cells[7].flags.contains(Flags::PROMPT));
            assert_eq!(
                update.rows[0].1.cells[7]
                    .flags
                    .contains(Flags::RIGHT_PROMPT),
                scope == 2
            );
        }
    }

    fn deliver_screen(
        core: &crate::core::Core,
        screen: &mut crate::screen::Screen,
        full: bool,
    ) -> bool {
        let update = Update::capture(core, full);
        let has_palette = update.palette.is_some();
        let mut encoder = Encoder::new();
        encode_update(&mut encoder, &update);
        let decoded = decode_update(&mut Decoder::new(&encoder.0[4..]).unwrap()).unwrap();
        assert_eq!(decoded.palette, update.palette);
        screen.apply(decoded).unwrap();
        has_palette
    }

    #[test]
    fn sparse_updates_inherit_palette_and_resize_requires_a_complete_palette() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xc0c1c2,
            0x202122,
        );
        let mut parser = crate::vt::Parser::new();
        let mut screen = crate::screen::Screen::default();
        assert!(deliver_screen(&core, &mut screen, true));
        let original = screen.palette;
        core.reset_damage();
        parser.advance(&mut core, b"xy");
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.palette, original);
        assert_eq!(screen.row(0).cells[1].c, 'y');

        core.reset_damage();
        screen.reset_damage();
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        assert!(Update::capture(&core, false).rows.is_empty());
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.dirty_rows().count(), 0);
        core.selection_update(Point { line: 0, column: 1 }, Side::Right);
        assert!(Update::capture(&core, false).rows.is_empty());
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.dirty_rows().collect::<Vec<_>>(), [0]);
        assert_eq!(core.selection_to_string().as_deref(), Some("xy"));
        let selection = screen.selection_range();
        assert!(selection.is_some());
        core.reset_damage();
        parser.advance(&mut core, b"\x1b[1;4H\x1b[?25l");
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.palette, original);
        assert_eq!(screen.selection_range(), selection);
        assert_eq!(screen.cursor(), core.cursor());
        assert_eq!(screen.mode(), core.mode());
        screen.reset_damage();
        assert!(core.clear_selection());
        assert!(Update::capture(&core, false).rows.is_empty());
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.selection_range(), None);
        assert_eq!(screen.dirty_rows().collect::<Vec<_>>(), [0]);
        screen.reset_damage();
        assert!(!core.clear_selection());
        assert!(!deliver_screen(&core, &mut screen, false));
        assert_eq!(screen.dirty_rows().count(), 0);
        core.reset_damage();
        assert!(
            deliver_screen(&core, &mut screen, true),
            "forced full captures must carry the palette even without damage"
        );

        core.resize(WindowSize {
            num_cols: 10,
            num_lines: 4,
            cell_width: 0,
            cell_height: 0,
        });
        let mut missing_palette = Update::capture(&core, false);
        assert!(missing_palette.palette.take().is_some());
        assert_eq!(
            screen.apply(missing_palette).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            screen.columns(),
            8,
            "a malformed resize must not partially apply"
        );
        assert!(deliver_screen(&core, &mut screen, false));
        assert_eq!((screen.columns(), screen.screen_lines()), (10, 4));
        assert_eq!(screen.palette, original);
    }

    #[test]
    fn every_palette_set_reset_and_terminal_reset_reaches_the_frontend() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xc0c1c2,
            0x202122,
        );
        let mut screen = crate::screen::Screen::default();
        let mut parser = crate::vt::Parser::new();
        deliver_screen(&core, &mut screen, true);
        let original = screen.palette;
        for sequence in [
            "\x1b]4;1;#112233;17;#445566;255;#778899\x07",
            "\x1b]10;#102030;#405060;#708090\x07",
            "\x1b]104;1\x07",
            "\x1b]104\x07",
            "\x1b]110\x07",
            "\x1b]111\x07",
            "\x1b]112\x07",
            "\x1b]4;1;#abcdef\x07\x1b]10;#123456\x07\x1bc",
        ] {
            core.reset_damage();
            parser.advance(&mut core, sequence.as_bytes());
            assert!(
                deliver_screen(&core, &mut screen, false),
                "missing palette after {sequence:?}"
            );
            for index in 0..269 {
                let color = if index < 256 {
                    Color::Indexed(index as u8)
                } else {
                    Color::Named(NamedColor::from_index(index as u16).unwrap())
                };
                assert_eq!(
                    screen.color(color),
                    core.color(color),
                    "palette slot {index} after {sequence:?}"
                );
            }
            core.reset_damage();
            parser.advance(&mut core, b"\x1b]4;1;?\x07\x1b]10;?\x07");
            assert!(
                !deliver_screen(&core, &mut screen, false),
                "queries do not change the palette"
            );
        }
        assert_eq!(screen.palette, original);
    }

    #[test]
    fn complete_final_message_is_delivered_before_eof() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        sender.send_response(Response::Detached).unwrap();
        sender.flush().unwrap();
        drop(sender);
        assert!(matches!(
            receiver.responses(None).unwrap().as_slice(),
            [Response::Detached]
        ));
        assert!(
            matches!(receiver.responses(None), Err(error) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
    #[test]
    fn large_input_is_chunked_in_order_and_rejected_atomically_when_too_large() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let data: Vec<_> = (0..200000).map(|i| (i % 251) as u8).collect();
        sender.send_input(9, &data).unwrap();
        let mut received = Vec::new();
        while received.len() < data.len() {
            sender.flush().unwrap();
            while let Some(action) = receiver.next_action().unwrap() {
                if let Action::Input { pane, data } = action {
                    assert_eq!(pane, 9);
                    assert!(data.len() <= 65536);
                    received.extend(data);
                } else {
                    panic!("wrong action");
                }
            }
        }
        assert_eq!(received, data);
        assert!(sender.send_input(9, &vec![0; MAX_PENDING]).is_err());
        assert!(!sender.pending());
    }
    #[test]
    fn queued_frames_preserve_order_and_descriptors_across_short_writes() {
        use std::io::Read;
        use std::time::{Duration, Instant};
        let (left, right) = UnixStream::pair().unwrap();
        let capacity: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    left.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&capacity as *const libc::c_int).cast(),
                    std::mem::size_of_val(&capacity) as libc::socklen_t,
                )
            },
            0
        );
        let mut sender = Peer::new(left).unwrap();
        let mut receiver = Peer::new(right).unwrap();
        for index in 0..80 {
            sender
                .send_response(Response::Error(format!(
                    "{index}:{}",
                    "x".repeat(12000 + index)
                )))
                .unwrap();
            if matches!(index, 20 | 60) {
                sender
                    .send_response(Response::Clipboard {
                        request: index as u64,
                        file: File::open(if index == 20 {
                            "/dev/zero"
                        } else {
                            "/dev/null"
                        })
                        .unwrap(),
                        incomplete: false,
                    })
                    .unwrap();
            }
        }
        sender.flush().unwrap();
        assert!(
            sender.pending(),
            "small socket must backpressure the sender"
        );
        let mut responses = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sender.pending() || responses.len() < 82 {
            assert!(Instant::now() < deadline, "queued responses did not drain");
            responses.extend(receiver.responses(None).unwrap());
            sender.flush().unwrap();
        }
        let mut responses = responses.into_iter();
        for index in 0..80 {
            assert!(matches!(responses.next(), Some(Response::Error(text))
                if text == format!("{index}:{}", "x".repeat(12000 + index))));
            if matches!(index, 20 | 60) {
                let Some(Response::Clipboard {
                    request,
                    mut file,
                    incomplete,
                }) = responses.next()
                else {
                    panic!("missing clipboard descriptor");
                };
                assert_eq!(request, index as u64);
                assert!(!incomplete);
                let mut byte = [9];
                if index == 20 {
                    assert_eq!(file.read(&mut byte).unwrap(), 1);
                    assert_eq!(byte, [0]);
                } else {
                    assert_eq!(file.read(&mut byte).unwrap(), 0);
                }
            }
        }
        assert!(responses.next().is_none());
    }
}
