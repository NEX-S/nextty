//! Length-prefixed messages on an owned Unix socket.
use crate::agent_files::FileResult;
use crate::core::{Cell, Extra, Flags, Mode, Point, Row, SelectionRange};
use crate::screen::Update;
use crate::vt::{Color, CursorShape, Rgb};
use crate::workspace::{Action, AgentEvent, HistoryPage, Snapshot, WorkspaceInfo};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
const MAX_FRAME: usize = 32 * 1024 * 1024;
const MAX_PENDING: usize = MAX_FRAME + 1024 * 1024;
const COPY_CHUNK: usize = 65536;
pub(crate) const VERSION: u8 = 26;
/// The remote bridge's first bytes: a frame whose payload is `VERSION`, independent of `Response`.
pub(crate) const HELLO: [u8; 5] = [1, 0, 0, 0, VERSION];

/// The daemon encodes `State` from borrowed data, so a frame needs no per-client copy.
#[derive(Serialize, Deserialize)]
pub enum Response<S = Option<Snapshot>, U = Vec<(u64, Update)>> {
    ScreenDirty,
    AgentInput {
        request: u64,
        result: Result<Option<u64>, String>,
    },
    AgentFile {
        request: u64,
        result: Result<FileResult, String>,
    },
    AgentNewTab {
        request: u64,
        result: Result<u64, String>,
    },
    CopyError(String),
    History {
        request: u64,
        result: Result<HistoryPage, String>,
    },
    Agent(AgentEvent),
    State(S, U, u64),
    List(Vec<WorkspaceInfo>),
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
    ClipboardBegin {
        primary: bool,
        incomplete: bool,
    },
    ClipboardChunk(#[serde(with = "bytes")] Vec<u8>),
    ClipboardEnd,
    /// A finished copy; it travels as `ClipboardBegin`, chunks and `ClipboardEnd`.
    /// Last, because serde numbers the variants after a skipped one differently when decoding.
    #[serde(skip)]
    Clipboard {
        primary: bool,
        file: File,
        incomplete: bool,
    },
}

pub(crate) mod bytes {
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Vec<u8>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("bytes")
            }
            fn visit_bytes<E>(self, value: &[u8]) -> Result<Vec<u8>, E> {
                Ok(value.to_vec())
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}

/// Paths travel as the OS's own bytes; serde's `PathBuf` would reject names that are not UTF-8.
pub(crate) mod path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(value: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        value.as_os_str().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        OsString::deserialize(deserializer).map(PathBuf::from)
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            value: &Option<PathBuf>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            value.as_deref().map(Path::as_os_str).serialize(serializer)
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<PathBuf>, D::Error> {
            Ok(Option::<OsString>::deserialize(deserializer)?.map(PathBuf::from))
        }
    }
}

/// One frame, or a copy whose next frame is read from its file when the socket has room.
struct Packet {
    bytes: Vec<u8>,
    copy: Option<File>,
    state: bool,
}
pub(crate) struct Peer {
    stream: UnixStream,
    copy: Option<IncomingCopy>,
    input: Vec<u8>,
    consumed: usize,
    output: VecDeque<Packet>,
    output_offset: usize,
    queued: usize,
    read_error: Option<io::Error>,
}
struct IncomingCopy {
    primary: bool,
    incomplete: bool,
    file: File,
}
impl Peer {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            copy: None,
            input: Vec::new(),
            consumed: 0,
            output: VecDeque::new(),
            output_offset: 0,
            queued: 0,
            read_error: None,
        })
    }
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
    pub fn pending(&self) -> bool {
        !self.output.is_empty()
    }
    pub fn needs_poll(&self) -> bool {
        self.buffered_action() || self.read_error.is_some()
    }
    pub fn hello(&mut self) -> io::Result<Option<u8>> {
        self.read()?;
        let Some(packet) = packet(&self.input[self.consumed..])? else {
            return Ok(None);
        };
        let [version] = *packet else {
            return Err(invalid("remote nextty did not send an SSH bridge greeting"));
        };
        self.consumed += HELLO.len();
        Ok(Some(version))
    }
    pub fn send_action(&mut self, action: &Action) -> io::Result<()> {
        self.queue(encode(action)?, false)
    }
    pub fn send_input(&mut self, pane: u64, data: &[u8]) -> io::Result<()> {
        const CHUNK: usize = 65536;
        let required = data.len() + data.len().div_ceil(CHUNK) * 18;
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
            self.send_action(&Action::Input {
                pane,
                data: chunk.to_vec(),
            })?;
        }
        Ok(())
    }
    pub fn send_state(
        &mut self,
        snapshot: Option<&Snapshot>,
        updates: &[(u64, Update)],
        request: u64,
    ) -> io::Result<()> {
        self.queue(encode(&Response::State(snapshot, updates, request))?, true)
    }
    pub fn send_response(&mut self, response: Response) -> io::Result<()> {
        if let Response::Clipboard {
            primary,
            file,
            incomplete,
        } = response
        {
            let begin: Response = Response::ClipboardBegin {
                primary,
                incomplete,
            };
            self.queue(encode(&begin)?, false)?;
            self.output.push_back(Packet {
                bytes: Vec::new(),
                copy: Some(file),
                state: false,
            });
            return Ok(());
        }
        self.queue(encode(&response)?, false)
    }
    fn queue_copy(&mut self) -> io::Result<()> {
        let packet = self.output.front_mut().unwrap();
        let file = packet.copy.as_mut().unwrap();
        let mut bytes = vec![0; COPY_CHUNK];
        let mut count = 0;
        while count < bytes.len() {
            match file.read(&mut bytes[count..]) {
                Ok(0) => break,
                Ok(n) => count += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        bytes.truncate(count);
        let response: Response = if count != 0 {
            Response::ClipboardChunk(bytes)
        } else {
            packet.copy = None;
            Response::ClipboardEnd
        };
        let bytes = encode(&response)?;
        self.queued += bytes.len();
        packet.bytes = bytes;
        Ok(())
    }
    fn queue(&mut self, bytes: Vec<u8>, state: bool) -> io::Result<()> {
        let queued = self.queued + bytes.len();
        if queued > MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace socket send queue is full",
            ));
        }
        self.queued = queued;
        self.output.push_back(Packet {
            bytes,
            copy: None,
            state,
        });
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        let mut budget: usize = 256 * 1024;
        while budget > 0 {
            // A copy produces its next frame on demand; screen state overtakes it between frames.
            if self
                .output
                .front()
                .is_some_and(|packet| packet.copy.is_some() && packet.bytes.is_empty())
            {
                match self.output.iter().position(|packet| packet.state) {
                    Some(index) => {
                        let state = self.output.remove(index).unwrap();
                        self.output.push_front(state);
                    }
                    None => self.queue_copy()?,
                }
            }
            let Some(packet) = self.output.front() else {
                break;
            };
            let bytes = &packet.bytes[self.output_offset..];
            let count = match (&self.stream).write(&bytes[..bytes.len().min(budget)]) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            self.queued -= count;
            budget -= count;
            let packet = self.output.front_mut().unwrap();
            self.output_offset += count;
            if self.output_offset == packet.bytes.len() {
                if packet.copy.is_some() {
                    packet.bytes.clear();
                }
                self.output_offset = 0;
                if packet.copy.is_none() {
                    self.output.pop_front();
                }
            }
        }
        Ok(())
    }
    pub fn cancel_output(&mut self) {
        self.output.clear();
        self.output_offset = 0;
        self.queued = 0;
    }
    pub fn buffered_action(&self) -> bool {
        // Invalid headers are ready for the decoder to report.
        !matches!(packet(&self.input[self.consumed..]), Ok(None))
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
            let fd = self.fd();
            let bytes = &mut self.input.spare_capacity_mut()[..available];
            let result = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            match result {
                0 => {
                    self.read_error = Some(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "workspace socket closed",
                    ));
                    break;
                }
                count if count > 0 => {
                    let count = count as usize;
                    // read initialized exactly the returned prefix.
                    unsafe { self.input.set_len(self.input.len() + count) };
                    budget = budget.saturating_sub(count);
                    if self.input.len() > MAX_PENDING {
                        return Err(invalid("workspace receive buffer exceeded its limit"));
                    }
                    if count < available && self.buffered_action() {
                        break;
                    }
                }
                _ => {
                    let error = io::Error::last_os_error();
                    match error.kind() {
                        io::ErrorKind::WouldBlock => break,
                        io::ErrorKind::Interrupted => (),
                        _ => {
                            self.read_error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
        if !self.buffered_action()
            && let Some(error) = self.read_error.take()
        {
            if self.copy.is_some() && error.kind() == io::ErrorKind::UnexpectedEof {
                return Err(invalid("clipboard transfer ended before completion"));
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn next_action(&mut self) -> io::Result<Option<Action>> {
        self.read()?;
        let packet = packet(&self.input[self.consumed..])?;
        let Some(packet) = packet else {
            return Ok(None);
        };
        let action = decode(packet)?;
        self.consumed += packet.len() + 4;
        Ok(Some(action))
    }
    pub fn responses(&mut self) -> io::Result<Vec<Response>> {
        self.receive_responses().inspect_err(|_| {
            self.copy = None;
        })
    }
    fn receive_responses(&mut self) -> io::Result<Vec<Response>> {
        let mut result = Vec::new();
        self.read()?;
        while let Some(packet) = packet(&self.input[self.consumed..])? {
            let response = decode::<Response>(packet)?;
            self.consumed += packet.len() + 4;
            let response = match response {
                Response::ClipboardBegin {
                    primary,
                    incomplete,
                } => {
                    if self.copy.is_some() {
                        return Err(invalid("clipboard transfer is already active"));
                    }
                    self.copy = Some(IncomingCopy {
                        primary,
                        incomplete,
                        file: clipboard_file()?,
                    });
                    None
                }
                Response::ClipboardChunk(bytes) => {
                    self.copy
                        .as_mut()
                        .ok_or_else(|| invalid("clipboard chunk has no start"))?
                        .file
                        .write_all(&bytes)?;
                    None
                }
                Response::ClipboardEnd => {
                    let mut copy = self
                        .copy
                        .take()
                        .ok_or_else(|| invalid("clipboard end has no start"))?;
                    copy.file.rewind()?;
                    Some(Response::Clipboard {
                        primary: copy.primary,
                        file: copy.file,
                        incomplete: copy.incomplete,
                    })
                }
                Response::Detached => {
                    self.copy = None;
                    Some(Response::Detached)
                }
                response => Some(response),
            };
            if let Some(response) = response {
                result.push(response);
            }
        }
        Ok(result)
    }
}

fn clipboard_file() -> io::Result<File> {
    let directory = crate::config::cache_directory()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME or XDG_CACHE_HOME is required for clipboard storage",
            )
        })?
        .join("clipboard");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
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

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    // The length goes into the reserved header once the payload size is known.
    let mut bytes = postcard::to_extend(value, vec![0; 4])
        .map_err(|error| invalid(&format!("workspace message encoding: {error}")))?;
    let length = bytes.len() - 4;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "workspace message exceeds the frame limit",
        ));
    }
    bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
    Ok(bytes)
}
fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> io::Result<T> {
    let (value, remaining) = postcard::take_from_bytes(bytes)
        .map_err(|error| invalid(&format!("workspace message decoding: {error}")))?;
    if !remaining.is_empty() {
        return Err(invalid("unexpected bytes after workspace message"));
    }
    Ok(value)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
/// Rows are sent as the text of their stored cells plus style runs, which stays small over SSH.
#[derive(Serialize, Deserialize)]
struct WireRow {
    index: u16,
    wrapped: bool,
    used: u16,
    text: String,
    runs: Vec<(u16, Color, Color, u16)>,
    extras: Vec<(u16, String, Option<Color>)>,
}
#[derive(Serialize, Deserialize)]
struct WireUpdate {
    size: (u16, u16),
    scroll: u16,
    cursor: (Point, CursorShape),
    mode: u32,
    offset: u64,
    history: u64,
    selection: Option<SelectionRange>,
    /// Empty when the palette is unchanged.
    #[serde(with = "bytes")]
    palette: Vec<u8>,
    rows: Vec<WireRow>,
}
impl Serialize for Update {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireUpdate {
            size: (self.columns as u16, self.lines as u16),
            scroll: self.scroll,
            cursor: self.cursor,
            mode: self.mode.bits(),
            offset: self.offset as u64,
            history: self.history as u64,
            selection: self.selection,
            palette: self.palette.as_deref().map_or_else(Vec::new, |palette| {
                palette
                    .iter()
                    .flat_map(|rgb| [rgb.r, rgb.g, rgb.b])
                    .collect()
            }),
            rows: self
                .rows
                .iter()
                .map(|(index, row)| {
                    // The reflow text length does not cover every styled blank or wide-wrap cell.
                    let cells = &row.cells[..row.non_default_len()];
                    WireRow {
                        index: *index as u16,
                        wrapped: row.wrapped,
                        used: row.used() as u16,
                        text: cells.iter().map(|cell| cell.c).collect(),
                        runs: cells
                            .chunk_by(|a, b| a.fg == b.fg && a.bg == b.bg && a.flags == b.flags)
                            .map(|run| {
                                (run.len() as u16, run[0].fg, run[0].bg, run[0].flags.bits())
                            })
                            .collect(),
                        extras: row
                            .extras()
                            .iter()
                            .map(|(column, extra)| {
                                let zerowidth = extra.zerowidth.iter().collect();
                                (*column as u16, zerowidth, extra.underline_color)
                            })
                            .collect(),
                    }
                })
                .collect(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Update {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireUpdate::deserialize(deserializer)?;
        let columns = usize::from(wire.size.0);
        let palette = (!wire.palette.is_empty()).then(|| {
            let (colors, _) = wire.palette.as_chunks::<3>();
            Box::new(std::array::from_fn(|index| {
                let [r, g, b] = colors[index];
                Rgb { r, g, b }
            }))
        });
        let mut rows = Vec::with_capacity(wire.rows.len());
        for row in wire.rows {
            let index = usize::from(row.index);
            let mut chars = row.text.chars();
            let mut cells = Vec::with_capacity(columns);
            for (count, fg, bg, flags) in row.runs {
                let flags = Flags::from_bits(flags);
                cells.extend(chars.by_ref().take(usize::from(count)).map(|c| Cell {
                    c,
                    fg,
                    bg,
                    flags,
                }));
            }
            cells.resize(columns, Cell::default());
            let extras = row
                .extras
                .into_iter()
                .map(|(column, text, underline_color)| {
                    let zerowidth = text.chars().collect();
                    let extra = Extra {
                        zerowidth,
                        underline_color,
                    };
                    (usize::from(column), extra)
                })
                .collect();
            let row = Row::from_parts(cells, row.wrapped, usize::from(row.used), extras);
            rows.push((index, row));
        }
        Ok(Update {
            columns,
            lines: usize::from(wire.size.1),
            scroll: wire.scroll,
            rows,
            cursor: wire.cursor,
            mode: Mode::from_bits(wire.mode),
            palette,
            selection: wire.selection,
            offset: wire.offset as usize,
            history: wire.history as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_files::{AgentChange, FileMatch, FileOperation, FileResult};
    use crate::core::WindowSize;
    use crate::workspace::AgentModelPatch;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn pair() -> (Peer, Peer) {
        let (left, right) = UnixStream::pair().unwrap();
        (Peer::new(left).unwrap(), Peer::new(right).unwrap())
    }

    // A child process forked by another test can hold the closed end open for a moment.
    fn eof<T>(mut next: impl FnMut() -> io::Result<T>) -> io::Error {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Err(error) = next() {
                return error;
            }
            assert!(std::time::Instant::now() < deadline, "no EOF");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn messages_keep_bytes_and_native_paths() {
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/source-\xff".to_vec()));
        let patch = AgentModelPatch {
            session: 3,
            changes: Some(Arc::new(vec![AgentChange {
                path: path.clone(),
                added: 2,
                diff: "@@ -1 +1,2 @@\n-old\n+new\n+line\n".into(),
                ..AgentChange::default()
            }])),
            ..AgentModelPatch::default()
        };
        let actions = [
            Action::Input {
                pane: 42,
                data: "中e\u{301}\x1b[A".as_bytes().to_vec(),
            },
            Action::AgentFile {
                pane: 1,
                request: 2,
                path: path.clone(),
                operation: FileOperation::Search {
                    query: "needle".into(),
                },
            },
            Action::AgentUpdate(patch),
        ];
        let (mut sender, mut receiver) = pair();
        for action in &actions {
            sender.send_action(action).unwrap();
        }
        sender.flush().unwrap();
        for action in &actions {
            let received = receiver.next_action().unwrap().unwrap();
            assert_eq!(format!("{received:?}"), format!("{action:?}"));
        }
        let results = [
            Ok(FileResult::Read {
                path: path.clone(),
                offset: 17,
                text: "你好\n".into(),
                next_offset: 24,
                has_more: true,
            }),
            Ok(FileResult::Search {
                path: path.clone(),
                matches: vec![FileMatch {
                    path: path.join("a"),
                    line: 19,
                    text: "needle line".into(),
                }],
                truncated: true,
            }),
            Err("file request cancelled".into()),
        ];
        for (request, result) in results.iter().enumerate() {
            let result = result.clone();
            receiver
                .send_response(Response::AgentFile {
                    request: request as u64,
                    result,
                })
                .unwrap();
        }
        receiver.flush().unwrap();
        let received = sender.responses().unwrap();
        assert_eq!(received.len(), results.len());
        for (response, expected) in received.into_iter().zip(results) {
            let Response::AgentFile { result, .. } = response else {
                panic!("unexpected response");
            };
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn coalesced_and_fragmented_frames_arrive_in_order_before_eof() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let mut receiver = Peer::new(right).unwrap();
        let frame = encode(&Action::Input {
            pane: 3,
            data: b"fragment".to_vec(),
        })
        .unwrap();
        left.write_all(&frame.repeat(256)).unwrap();
        left.write_all(&frame[..2]).unwrap();
        for _ in 0..256 {
            assert!(matches!(
                receiver.next_action().unwrap(),
                Some(Action::Input { pane: 3, .. })
            ));
        }
        assert!(receiver.next_action().unwrap().is_none());
        left.write_all(&frame[2..8]).unwrap();
        assert!(receiver.next_action().unwrap().is_none());
        left.write_all(&frame[8..]).unwrap();
        drop(left);
        assert!(matches!(
            receiver.next_action().unwrap(),
            Some(Action::Input { pane: 3, data }) if data == b"fragment"
        ));
        assert_eq!(
            eof(|| receiver.next_action()).kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn complete_final_message_is_delivered_before_eof() {
        let (mut sender, mut receiver) = pair();
        sender.send_response(Response::Detached).unwrap();
        sender.flush().unwrap();
        drop(sender);
        assert!(matches!(
            receiver.responses().unwrap().as_slice(),
            [Response::Detached]
        ));
        assert_eq!(
            eof(|| receiver.responses()).kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn large_input_is_chunked_in_order_and_rejected_atomically_when_too_large() {
        let (mut sender, mut receiver) = pair();
        let data: Vec<_> = (0..200000).map(|i| (i % 251) as u8).collect();
        sender.send_input(9, &data).unwrap();
        let mut received = Vec::new();
        while received.len() < data.len() {
            sender.flush().unwrap();
            while let Some(action) = receiver.next_action().unwrap() {
                let Action::Input { pane, data } = action else {
                    panic!("wrong action");
                };
                assert_eq!(pane, 9);
                assert!(data.len() <= 65536);
                received.extend(data);
            }
        }
        assert_eq!(received, data);
        assert!(sender.send_input(9, &vec![0; MAX_PENDING]).is_err());
        assert!(!sender.pending());
    }

    #[test]
    fn updates_roundtrip_through_the_row_codec() {
        let mut core = crate::core::Core::new(
            WindowSize {
                num_cols: 4096,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = crate::vt::Parser::new();
        parser.advance(
            &mut core,
            "\x1b[1;31mA中e\u{301}\x1b[0m plain\r\n\x1b[1;4]\x1b]133;P;1\x07".as_bytes(),
        );
        for column in 0..2000 {
            parser.advance(
                &mut core,
                format!("\x1b[38;2;{};{};0m中", column % 256, column / 16 % 256).as_bytes(),
            );
        }
        let original = Update::capture(&core, true);
        let bytes = encode(&original).unwrap();
        let decoded: Update = decode(&bytes[4..]).unwrap();
        assert_eq!(decoded.rows.len(), original.rows.len());
        for ((_, left), (_, right)) in original.rows.iter().zip(&decoded.rows) {
            assert_eq!(left.cells, right.cells);
            assert_eq!(left.extras(), right.extras());
            assert_eq!((left.used(), left.wrapped), (right.used(), right.wrapped));
        }
        assert_eq!(decoded.palette, original.palette);
        assert_eq!(decoded.cursor, original.cursor);
    }

    #[test]
    fn copies_stream_in_order_beside_state_frames_and_past_the_frame_limit() {
        let directory = std::env::current_dir().unwrap().join(format!(
            "target/wire-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "wire::tests::copy_stream_child", "--nocapture"])
            .env("NEXTTY_TEST_COPY", "1")
            .env("XDG_CACHE_HOME", &directory)
            .status()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(status.success());
    }

    // Runs in a child process: copies are staged in the cache directory it is given.
    #[test]
    fn copy_stream_child() {
        use std::os::unix::fs::MetadataExt;
        if std::env::var_os("NEXTTY_TEST_COPY").is_none() {
            return;
        }
        let file = |chunks: usize| {
            let mut file = clipboard_file().unwrap();
            let bytes: Vec<u8> = (0..65536).map(|index| (index % 251) as u8).collect();
            for _ in 0..chunks {
                file.write_all(&bytes).unwrap();
            }
            file.rewind().unwrap();
            file
        };
        let drain = |sender: &mut Peer, receiver: &mut Peer, count: usize| {
            let mut responses = Vec::new();
            while responses.len() < count {
                sender.flush().unwrap();
                responses.extend(receiver.responses().unwrap());
            }
            responses
        };

        // A copy several frames long: screen state overtakes it, everything else stays in order.
        let (mut sender, mut receiver) = pair();
        sender
            .send_response(Response::Clipboard {
                primary: false,
                file: file(48),
                incomplete: false,
            })
            .unwrap();
        sender.send_state(None, &[], 7).unwrap();
        sender.send_response(Response::Detached).unwrap();
        sender.flush().unwrap();
        assert!(sender.pending(), "the copy must not finish in one flush");
        let first = receiver.responses().unwrap();
        assert!(matches!(first.as_slice(), [Response::State(None, _, 7)]));
        let mut rest = first;
        rest.extend(drain(&mut sender, &mut receiver, 3 - rest.len()));
        assert!(matches!(
            rest.as_slice(),
            [
                Response::State(..),
                Response::Clipboard {
                    primary: false,
                    incomplete: false,
                    ..
                },
                Response::Detached
            ]
        ));

        // More than MAX_FRAME crosses in chunks, into an unlinked file.
        let (mut sender, mut receiver) = pair();
        sender
            .send_response(Response::Clipboard {
                primary: true,
                file: file(600),
                incomplete: true,
            })
            .unwrap();
        let Some(Response::Clipboard {
            primary: true,
            file: mut copy,
            incomplete: true,
        }) = drain(&mut sender, &mut receiver, 1).pop()
        else {
            panic!("missing copy");
        };
        assert_eq!(copy.metadata().unwrap().len(), 600 * 65536);
        assert_eq!(copy.metadata().unwrap().nlink(), 0);
        let mut chunk = [0; 65536];
        for _ in 0..600 {
            copy.read_exact(&mut chunk).unwrap();
            assert_eq!(chunk[250..253], [250, 0, 1]);
        }

        // A sender that disappears mid-copy is an error, not a short copy.
        let (mut sender, mut receiver) = pair();
        let frames: [Response; 2] = [
            Response::ClipboardBegin {
                primary: false,
                incomplete: false,
            },
            Response::ClipboardChunk(b"partial".to_vec()),
        ];
        for response in frames {
            sender.queue(encode(&response).unwrap(), false).unwrap();
        }
        sender.flush().unwrap();
        drop(sender);
        let error = loop {
            match receiver.responses() {
                Ok(responses) => assert!(responses.is_empty()),
                Err(error) => break error,
            }
        };
        assert!(error.to_string().contains("before completion"));
    }
}
