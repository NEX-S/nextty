//! Length-prefixed messages on an owned Unix socket.
use crate::agent_files::FileResult;
use crate::core::{Cell, Extra, Flags, Mode, Point, Row, SelectionRange};
use crate::screen::Update;
use crate::vt::{Color, CursorShape, Rgb};
use crate::workspace::{Action, AgentEvent, HistoryPage, Snapshot, WorkspaceInfo};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
/// A longer frame is corrupt, and the daemon drops a client that leaves more than this unread.
const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Input and copies travel in frames of at most this many bytes.
const CHUNK: usize = 65536;
pub(crate) const VERSION: u8 = 28;
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
    History {
        request: u64,
        result: Result<HistoryPage, String>,
    },
    Agent(AgentEvent),
    State(S, U, u64),
    List(Vec<WorkspaceInfo>),
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

/// Byte fields as one block; serde would encode a `Vec<u8>` byte by byte.
pub(crate) mod bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        <&[u8]>::deserialize(deserializer).map(<[u8]>::to_vec)
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

/// Queued frames, or a copy whose next frame is read when the socket has room.
struct Packet {
    bytes: Vec<u8>,
    copy: Option<File>,
    /// Screen state, which overtakes a copy between its frames.
    state: bool,
}
pub(crate) struct Peer {
    stream: UnixStream,
    /// The copy being received.
    copy: Option<IncomingCopy>,
    input: Vec<u8>,
    consumed: usize,
    output: VecDeque<Packet>,
    output_offset: usize,
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
        })
    }
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
    pub fn pending(&self) -> bool {
        !self.output.is_empty()
    }
    /// Whether a new frame may be queued: no frame is waiting, and nothing but a copy, which the
    /// frame overtakes, is ahead of it.
    pub fn frame_ready(&self) -> bool {
        self.output
            .front()
            .is_none_or(|packet| packet.copy.is_some())
            && !self.output.iter().any(|packet| packet.state)
    }
    pub fn hello(&mut self) -> io::Result<Option<u8>> {
        self.read()?;
        match self.take_frame()? {
            Some(&[version]) => Ok(Some(version)),
            Some(_) => Err(invalid("remote nextty did not send an SSH bridge greeting")),
            None => Ok(None),
        }
    }
    /// A frontend queues all it sends; the daemon reads it as fast as the PTY takes input.
    pub fn send_action(&mut self, action: &Action) -> io::Result<()> {
        self.queue(encode(action)?, false);
        Ok(())
    }
    pub fn send_state(
        &mut self,
        snapshot: Option<&Snapshot>,
        updates: &[(u64, Update)],
        request: u64,
    ) -> io::Result<()> {
        self.queue(encode(&Response::State(snapshot, updates, request))?, true);
        self.check_backlog()
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
            self.queue(encode(&begin)?, false);
            self.output.push_back(Packet {
                bytes: Vec::new(),
                copy: Some(file),
                state: false,
            });
        } else {
            let state = matches!(response, Response::State(..));
            self.queue(encode(&response)?, state);
        }
        self.check_backlog()
    }
    fn queue(&mut self, bytes: Vec<u8>, state: bool) {
        // A screen request and the input that follows it can share a socket write.
        if !state
            && let Some(last) = self.output.back_mut()
            && !last.state
            && last.copy.is_none()
            && last.bytes.len() + bytes.len() <= CHUNK
        {
            last.bytes.extend_from_slice(&bytes);
            return;
        }
        self.output.push_back(Packet {
            bytes,
            copy: None,
            state,
        });
    }
    /// A client that stops reading must not make the daemon hold its output without bound.
    fn check_backlog(&self) -> io::Result<()> {
        let queued: usize = self.output.iter().map(|packet| packet.bytes.len()).sum();
        if queued > MAX_FRAME {
            return Err(io::Error::other("workspace socket send queue is full"));
        }
        Ok(())
    }
    /// Reads a copy's next frame: a chunk, or its end.
    fn next_chunk(&mut self) -> io::Result<()> {
        let packet = self.output.front_mut().unwrap();
        let mut chunk = Vec::with_capacity(CHUNK);
        let file = packet.copy.as_mut().unwrap();
        file.take(CHUNK as u64).read_to_end(&mut chunk)?;
        let response: Response = if chunk.is_empty() {
            packet.copy = None;
            Response::ClipboardEnd
        } else {
            Response::ClipboardChunk(chunk)
        };
        packet.bytes = encode(&response)?;
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        let mut budget = 4 * CHUNK;
        while budget > 0 {
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
                    None => self.next_chunk()?,
                }
            }
            let Some(packet) = self.output.front_mut() else {
                break;
            };
            let bytes = &packet.bytes[self.output_offset..];
            let count = match (&self.stream).write(&bytes[..bytes.len().min(budget)]) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            budget -= count;
            self.output_offset += count;
            if self.output_offset == packet.bytes.len() {
                self.output_offset = 0;
                packet.bytes.clear();
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
    }
    /// Whether a frame is complete, or has an invalid header for the decoder to report.
    pub fn buffered_action(&self) -> bool {
        !matches!(packet(&self.input[self.consumed..]), Ok(None))
    }
    /// Reads until a frame is complete or the socket has nothing more, so the frames that
    /// arrived before the end of the stream are delivered before it.
    fn read(&mut self) -> io::Result<()> {
        if self.buffered_action() {
            return Ok(());
        }
        self.input.drain(..self.consumed);
        self.consumed = 0;
        // Gives back what a large frame needed once smaller traffic resumes.
        if self.input.is_empty() {
            self.input.shrink_to(4 * CHUNK);
        }
        let fd = self.fd();
        loop {
            self.input.reserve(CHUNK);
            let spare = self.input.spare_capacity_mut();
            let count = unsafe { libc::read(fd, spare.as_mut_ptr().cast(), spare.len()) };
            if count > 0 {
                // read initialized exactly the returned prefix.
                unsafe { self.input.set_len(self.input.len() + count as usize) };
                if self.buffered_action() {
                    return Ok(());
                }
                continue;
            }
            let error = match count {
                0 => io::Error::new(io::ErrorKind::UnexpectedEof, "workspace socket closed"),
                _ => io::Error::last_os_error(),
            };
            match error.kind() {
                io::ErrorKind::WouldBlock => return Ok(()),
                io::ErrorKind::Interrupted => (),
                io::ErrorKind::UnexpectedEof if self.copy.is_some() => {
                    return Err(invalid("clipboard transfer ended before completion"));
                }
                _ => return Err(error),
            }
        }
    }
    /// The payload of the next complete frame that has arrived.
    fn take_frame(&mut self) -> io::Result<Option<&[u8]>> {
        let Some(frame) = packet(&self.input[self.consumed..])? else {
            return Ok(None);
        };
        self.consumed += frame.len() + 4;
        Ok(Some(frame))
    }
    pub fn next_action(&mut self) -> io::Result<Option<Action>> {
        self.read()?;
        self.take_frame()?.map(decode).transpose()
    }
    /// Decodes the frames one read brings, so a long stream still returns to the caller's loop.
    pub fn responses(&mut self) -> io::Result<Vec<Response>> {
        self.read()?;
        let mut responses = Vec::new();
        while let Some(frame) = self.take_frame()? {
            match decode(frame)? {
                Response::ClipboardBegin {
                    primary,
                    incomplete,
                } => {
                    let file = crate::sys::memfd(c"nextty-clipboard")?;
                    self.copy = Some(IncomingCopy {
                        primary,
                        incomplete,
                        file,
                    });
                }
                Response::ClipboardChunk(bytes) => {
                    let copy = self.copy.as_mut();
                    let copy = copy.ok_or_else(|| invalid("clipboard chunk has no start"))?;
                    copy.file.write_all(&bytes)?;
                }
                Response::ClipboardEnd => {
                    let copy = self.copy.take();
                    let mut copy = copy.ok_or_else(|| invalid("clipboard end has no start"))?;
                    copy.file.rewind()?;
                    responses.push(Response::Clipboard {
                        primary: copy.primary,
                        file: copy.file,
                        incomplete: copy.incomplete,
                    });
                }
                response => responses.push(response),
            }
        }
        Ok(responses)
    }
}

fn packet(input: &[u8]) -> io::Result<Option<&[u8]>> {
    let Some(header) = input.first_chunk() else {
        return Ok(None);
    };
    let length = u32::from_le_bytes(*header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid("invalid workspace frame length"));
    }
    Ok(input.get(4..4 + length))
}

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    // The length goes into the reserved header once the payload size is known.
    let mut bytes = postcard::to_extend(value, vec![0; 4]).map_err(io::Error::other)?;
    let length = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&length.to_le_bytes());
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
    palette: Option<Vec<Rgb>>,
    rows: Vec<WireRow>,
}
impl Serialize for Update {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let rows = self.rows.iter().map(|(index, row)| {
            // The reflow text length does not cover every styled blank or wide-wrap cell.
            let cells = &row.cells[..row.non_default_len()];
            WireRow {
                index: *index as u16,
                wrapped: row.wrapped,
                used: row.used() as u16,
                text: cells.iter().map(|cell| cell.c).collect(),
                runs: cells
                    .chunk_by(|a, b| a.fg == b.fg && a.bg == b.bg && a.flags == b.flags)
                    .map(|run| (run.len() as u16, run[0].fg, run[0].bg, run[0].flags.bits()))
                    .collect(),
                extras: (row.extras().iter())
                    .map(|(column, extra)| {
                        let zerowidth = extra.zerowidth.iter().collect();
                        (*column as u16, zerowidth, extra.underline_color)
                    })
                    .collect(),
            }
        });
        WireUpdate {
            size: (self.columns as u16, self.lines as u16),
            scroll: self.scroll,
            cursor: self.cursor,
            mode: self.mode.bits(),
            offset: self.offset as u64,
            history: self.history as u64,
            selection: self.selection,
            palette: self.palette.as_ref().map(|palette| palette.to_vec()),
            rows: rows.collect(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Update {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireUpdate::deserialize(deserializer)?;
        let columns = usize::from(wire.size.0);
        let palette = wire.palette.map(|palette| {
            let size = |_| serde::de::Error::custom("wrong workspace palette size");
            palette.try_into().map_err(size)
        });
        let rows = wire.rows.into_iter().map(|row| {
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
            let extras = (row.extras.into_iter())
                .map(|(column, text, underline_color)| {
                    let zerowidth = text.chars().collect();
                    let extra = Extra {
                        zerowidth,
                        underline_color,
                    };
                    (usize::from(column), extra)
                })
                .collect();
            let used = usize::from(row.used);
            let index = usize::from(row.index);
            (index, Row::from_parts(cells, row.wrapped, used, extras))
        });
        Ok(Update {
            columns,
            lines: usize::from(wire.size.1),
            scroll: wire.scroll,
            rows: rows.collect(),
            cursor: wire.cursor,
            mode: Mode::from_bits(wire.mode),
            palette: palette.transpose()?,
            selection: wire.selection,
            offset: wire.offset as usize,
            history: wire.history as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_files::FileOperation;
    use crate::core::WindowSize;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    fn pair() -> (Peer, Peer) {
        let (left, right) = UnixStream::pair().unwrap();
        (Peer::new(left).unwrap(), Peer::new(right).unwrap())
    }

    #[test]
    fn actions_keep_bytes_and_native_paths() {
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/source-\xff".to_vec()));
        let actions = [
            Action::Input {
                pane: 42,
                data: "中e\u{301}\x1b[A".as_bytes().to_vec(),
            },
            Action::AgentFile {
                pane: 1,
                request: 2,
                path,
                operation: FileOperation::Search {
                    query: "needle".into(),
                },
            },
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
        // A child process forked by another test can hold the closed end open for a moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let error = loop {
            if let Err(error) = receiver.next_action() {
                break error;
            }
            assert!(std::time::Instant::now() < deadline, "no EOF");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
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
        let file = |chunks: usize| {
            let mut file = crate::sys::memfd(c"test-copy").unwrap();
            let bytes: Vec<u8> = (0..CHUNK).map(|index| (index % 251) as u8).collect();
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
        let copy = Response::Clipboard {
            primary: false,
            file: file(48),
            incomplete: false,
        };
        sender.send_response(copy).unwrap();
        sender.flush().unwrap();
        // A reply queued behind the copy does not hold back the next frame.
        sender.send_response(Response::Detached).unwrap();
        assert!(sender.pending() && sender.frame_ready());
        sender.send_state(None, &[], 7).unwrap();
        assert!(!sender.frame_ready());
        assert!(matches!(
            drain(&mut sender, &mut receiver, 3).as_slice(),
            [
                Response::State(None, _, 7),
                Response::Clipboard {
                    primary: false,
                    incomplete: false,
                    ..
                },
                Response::Detached
            ]
        ));

        // More than MAX_FRAME crosses in chunks.
        let (mut sender, mut receiver) = pair();
        let copy = Response::Clipboard {
            primary: true,
            file: file(600),
            incomplete: true,
        };
        sender.send_response(copy).unwrap();
        let Some(Response::Clipboard {
            primary: true,
            file: mut copy,
            incomplete: true,
        }) = drain(&mut sender, &mut receiver, 1).pop()
        else {
            panic!("missing copy");
        };
        assert_eq!(copy.metadata().unwrap().len(), 600 * CHUNK as u64);
        let mut chunk = [0; CHUNK];
        for _ in 0..600 {
            copy.read_exact(&mut chunk).unwrap();
            assert_eq!(chunk[250..253], [250, 0, 1]);
        }

        // A sender that disappears mid-copy is an error, not a short copy.
        let (mut sender, mut receiver) = pair();
        let begin = Response::ClipboardBegin {
            primary: false,
            incomplete: false,
        };
        sender.send_response(begin).unwrap();
        let chunk = Response::ClipboardChunk(b"partial".to_vec());
        sender.send_response(chunk).unwrap();
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
