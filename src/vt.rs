//! Streaming VT input for the terminal's screen core.
#[cfg(feature = "tui")]
use std::borrow::Cow;
use std::time::{Duration, Instant};

use crate::core::Core;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[cfg(feature = "gui")]
    pub(crate) fn dimmed(self) -> Self {
        let channels = [self.r, self.g, self.b];
        let maximum = f32::from(*channels.iter().max().unwrap()) / 255.;
        let minimum = f32::from(*channels.iter().min().unwrap()) / 255.;
        let light = (maximum + minimum) * 0.5;
        let next = light * 0.66;
        let factor = if maximum == minimum {
            0.
        } else {
            (1. - (2. * next - 1.).abs()) / (1. - (2. * light - 1.).abs())
        };
        let [r, g, b] = channels.map(|value| {
            let adjusted = ((f32::from(value) / 255. - light) * factor + next) * 255.;
            adjusted.round().clamp(0., 255.) as u8
        });
        Self { r, g, b }
    }

    #[cfg(feature = "gui")]
    pub(crate) fn rgba(self) -> u32 {
        u32::from_be_bytes([self.r, self.g, self.b, 255])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum NamedColor {
    Black = 0,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Foreground = 256,
    Background,
    Cursor,
    DimBlack,
    DimRed,
    DimGreen,
    DimYellow,
    DimBlue,
    DimMagenta,
    DimCyan,
    DimWhite,
    BrightForeground,
    DimForeground,
}

impl NamedColor {
    pub fn from_index(index: u16) -> Option<Self> {
        const COLORS: [NamedColor; 29] = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
        ];
        let index = match index {
            0..=15 => index as usize,
            256..=268 => index as usize - 240,
            _ => return None,
        };
        Some(COLORS[index])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Named(NamedColor),
    Spec(Rgb),
    Indexed(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum StandardCharset {
    #[default]
    Ascii,
    SpecialGraphics,
}

impl StandardCharset {
    pub(crate) fn map(self, c: char) -> char {
        if !matches!(self, Self::SpecialGraphics) {
            return c;
        }
        match c {
            '_' => ' ',
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => c,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ClearMode {
    Below,
    Above,
    All,
    Saved,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum LineClearMode {
    Right,
    Left,
    All,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    IgnoreString,
    IgnoreStringEscape,
}

// CSI has a small fixed parameter list; an overfull or malformed sequence is ignored whole.
#[derive(Default)]
pub(crate) struct Csi {
    values: [u16; 32],
    len: usize,
    /// Bit `n` is set when parameter `n` follows a colon rather than a semicolon.
    pub(crate) colons: u32,
    pub(crate) prefix: u8,
    pub(crate) intermediate: u8,
    ignored: bool,
    started: bool,
}

impl Csi {
    fn start(&mut self) {
        *self = Self {
            len: 1,
            ..Self::default()
        };
    }

    fn byte(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' if self.intermediate == 0 => {
                self.started = true;
                let value = &mut self.values[self.len - 1];
                let next = u32::from(*value) * 10 + u32::from(byte - b'0');
                if next <= u32::from(u16::MAX) {
                    *value = next as u16;
                } else {
                    self.ignored = true;
                }
            }
            b';' | b':' if self.intermediate == 0 => {
                self.started = true;
                if self.len == self.values.len() {
                    self.ignored = true;
                } else {
                    if byte == b':' {
                        self.colons |= 1 << self.len;
                    }
                    self.len += 1;
                }
            }
            b'<'..=b'?'
                if self.len == 1 && !self.started && self.prefix == 0 && self.intermediate == 0 =>
            {
                self.prefix = byte
            }
            0x20..=0x2f if self.intermediate == 0 => self.intermediate = byte,
            _ => self.ignored = true,
        }
    }

    pub(crate) fn params(&self) -> &[u16] {
        &self.values[..self.len]
    }
    pub(crate) fn value(&self, index: usize) -> u16 {
        self.params().get(index).copied().unwrap_or(0)
    }
    pub(crate) fn count(&self, index: usize) -> usize {
        usize::from(self.value(index).max(1))
    }
    fn sync(&self) -> bool {
        self.prefix == b'?'
            && self.intermediate == 0
            && self.colons == 0
            && self.params().contains(&2026)
    }
}

const SYNC_LIMIT: usize = 2 * 1024 * 1024;
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const OSC_RETAIN_LIMIT: usize = 16 * 1024;

/// Owns only input framing and incomplete input; all committed terminal state lives in `Core`.
#[derive(Default)]
pub struct Parser {
    state: State,
    csi: Csi,
    escape_intermediate: u8,
    utf8: [u8; 4],
    utf8_len: usize,
    osc: Vec<u8>,
    sync: Vec<u8>,
    deadline: Option<Instant>,
    replaying: bool,
    #[cfg(feature = "tui")]
    queued: std::collections::VecDeque<(Vec<u8>, usize, bool)>,
    #[cfg(feature = "tui")]
    streaming: bool,
}

#[inline]
fn text_prefix(bytes: &[u8]) -> usize {
    // The caller classified the first byte; short styled text often ends right after it.
    if bytes.get(1).is_none_or(|b| *b < 0x20 || *b == 0x7f) {
        return 1;
    }
    let mut rest = bytes;
    while let Some((word, tail)) = rest.split_first_chunk::<8>() {
        let word = u64::from_ne_bytes(*word);
        let c0 = word.wrapping_sub(0x2020_2020_2020_2020) & !word;
        let del = word ^ 0x7f7f_7f7f_7f7f_7f7f;
        let controls =
            (c0 | (del.wrapping_sub(0x0101_0101_0101_0101) & !del)) & 0x8080_8080_8080_8080;
        if controls != 0 {
            break;
        }
        rest = tail;
    }
    // Borrowing can mark neighbouring bytes; locate the exact boundary below.
    bytes.len() - rest.len()
        + rest
            .iter()
            .position(|b| *b < 0x20 || *b == 0x7f)
            .unwrap_or(rest.len())
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Returns whether this call committed input, rather than only buffering a synchronized frame.
    pub fn advance(&mut self, core: &mut Core, bytes: &[u8]) -> bool {
        self.advance_count(core, bytes).1
    }

    fn advance_count(&mut self, core: &mut Core, bytes: &[u8]) -> (usize, bool) {
        let mut position = 0;
        let mut committed = false;
        while position < bytes.len() {
            if self.deadline.is_some() {
                // Scan only framing while the actual core and its UTF-8/REP state remain untouched.
                if self.state == State::Ground && bytes[position] >= 0x20 && bytes[position] != 0x7f
                {
                    let length = text_prefix(&bytes[position..]);
                    let length = length.min(SYNC_LIMIT - self.sync.len());
                    self.sync
                        .extend_from_slice(&bytes[position..position + length]);
                    position += length;
                } else {
                    let byte = bytes[position];
                    self.sync.push(byte);
                    position += 1;
                    if self.step(core, byte, true) {
                        committed |= self.finish_sync(core);
                        #[cfg(feature = "tui")]
                        if self.streaming {
                            break;
                        }
                        continue;
                    }
                }
                if self.sync.len() == SYNC_LIMIT {
                    committed |= self.finish_sync(core);
                    #[cfg(feature = "tui")]
                    if self.streaming {
                        break;
                    }
                }
            } else if self.state == State::Ground
                && bytes[position] >= 0x20
                && bytes[position] != 0x7f
            {
                let length = text_prefix(&bytes[position..]);
                self.text(core, &bytes[position..position + length]);
                position += length;
                committed = true;
            } else {
                self.step(core, bytes[position], false);
                position += 1;
                committed = true;
            }
        }
        (position, committed)
    }

    #[cfg(feature = "tui")]
    pub(crate) fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }
    #[cfg(feature = "tui")]
    pub(crate) fn advance_queued(&mut self, core: &mut Core, bytes: &[u8]) -> bool {
        let mut input = if self.queued.is_empty() {
            (!bytes.is_empty()).then_some((Cow::Borrowed(bytes), 0, false))
        } else {
            if !bytes.is_empty() {
                self.queued.push_back((bytes.to_vec(), 0, false));
            }
            None
        };
        let mut committed = false;
        // A PTY read is at most 16 KiB. Keep its ready history quanta under one
        // Core lock, so a TUI's temporary drawing cursor does not become a frame.
        let mut remaining = 16 * 1024;
        loop {
            let next = input.take().or_else(|| {
                self.queued
                    .pop_front()
                    .map(|(data, position, replay)| (Cow::Owned(data), position, replay))
            });
            let Some((data, mut position, replay)) = next else {
                break;
            };
            while position < data.len() && remaining != 0 && core.history_read_ready() {
                self.streaming = true;
                let end = (position + remaining.min(512)).min(data.len());
                let (consumed, changed) = self.advance_count(core, &data[position..end]);
                self.streaming = false;
                position += consumed;
                remaining -= consumed;
                committed |= changed;
                if !replay && self.replaying {
                    break;
                }
            }
            if position < data.len() {
                let data = data.into_owned();
                if !replay && self.replaying {
                    // finish_sync queued the frame first; resume the source after its replay.
                    self.queued.insert(1, (data, position, replay));
                } else {
                    self.queued.push_front((data, position, replay));
                }
            } else if replay {
                self.replaying = false;
                core.set_private_mode(2026, false);
                return true;
            }
            if remaining == 0 || !core.history_read_ready() {
                break;
            }
        }
        committed && !self.replaying && !core.mode().contains(crate::core::Mode::SYNC_UPDATE)
    }
    #[cfg(feature = "tui")]
    pub(crate) fn finish_sync_queued(&mut self, core: &mut Core) {
        self.streaming = true;
        self.finish_sync(core);
        self.streaming = false;
    }

    /// Commits a complete or timed-out synchronized frame under the caller's core lock.
    pub fn finish_sync(&mut self, core: &mut Core) -> bool {
        if self.deadline.take().is_none() {
            return false;
        }
        let bytes = std::mem::take(&mut self.sync);
        self.state = State::Ground;
        self.csi = Csi::default();
        self.osc.clear();
        self.utf8_len = 0;
        self.replaying = true;
        #[cfg(feature = "tui")]
        if self.streaming {
            self.queued.push_front((bytes, 0, true));
            return false;
        }
        self.advance(core, &bytes);
        self.replaying = false;
        core.set_private_mode(2026, false);
        !bytes.is_empty()
    }

    fn text(&mut self, core: &mut Core, mut bytes: &[u8]) {
        while self.utf8_len != 0 && !bytes.is_empty() {
            self.utf8[self.utf8_len] = bytes[0];
            self.utf8_len += 1;
            bytes = &bytes[1..];
            loop {
                match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
                    Ok(text) => {
                        core.input_text(text);
                        self.utf8_len = 0;
                        break;
                    }
                    Err(error) if error.error_len().is_some() => {
                        let length = error.error_len().unwrap();
                        core.input_text("\u{fffd}");
                        self.utf8.copy_within(length..self.utf8_len, 0);
                        self.utf8_len -= length;
                        if self.utf8_len == 0 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        while !bytes.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    core.input_text(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid != 0 {
                        core.input_text(std::str::from_utf8(&bytes[..valid]).unwrap());
                    }
                    bytes = &bytes[valid..];
                    if let Some(length) = error.error_len() {
                        core.input_text("\u{fffd}");
                        bytes = &bytes[length..];
                    } else {
                        self.utf8[..bytes.len()].copy_from_slice(bytes);
                        self.utf8_len = bytes.len();
                        break;
                    }
                }
            }
        }
    }

    #[inline]
    fn clear_osc(&mut self) {
        // Keep ordinary OSC storage reusable; a large clipboard or title must not
        // leave its entire allocation resident after the sequence has ended.
        if self.osc.capacity() > OSC_RETAIN_LIMIT {
            self.drop_osc();
        }
        self.osc.clear();
    }

    #[cold]
    fn drop_osc(&mut self) {
        self.osc = Vec::new();
    }

    /// A control interrupting an incomplete UTF-8 sequence prints one replacement.
    fn interrupt_utf8(&mut self, core: &mut Core) {
        if self.utf8_len != 0 {
            core.input_text("\u{fffd}");
            self.utf8_len = 0;
        }
    }

    fn step(&mut self, core: &mut Core, byte: u8, scan: bool) -> bool {
        if matches!(
            self.state,
            State::Osc | State::OscEscape | State::IgnoreString | State::IgnoreStringEscape
        ) {
            match byte {
                0x18 | 0x1a => {
                    self.clear_osc();
                    self.state = State::Ground;
                    if !scan && byte == 0x1a {
                        core.input_text("\u{fffd}");
                    }
                }
                0x1b => {
                    self.state = match self.state {
                        State::Osc | State::OscEscape => State::OscEscape,
                        _ => State::IgnoreStringEscape,
                    };
                }
                b'\\' if matches!(self.state, State::OscEscape | State::IgnoreStringEscape) => {
                    if self.state == State::OscEscape && !scan {
                        core.osc(&self.osc, "\x1b\\");
                    }
                    self.clear_osc();
                    self.state = State::Ground;
                }
                0x07 if matches!(self.state, State::Osc | State::OscEscape) => {
                    if !scan {
                        core.osc(&self.osc, "\x07");
                    }
                    self.clear_osc();
                    self.state = State::Ground;
                }
                _ if matches!(self.state, State::OscEscape | State::IgnoreStringEscape) => {
                    self.clear_osc();
                    self.state = State::Escape;
                    return self.step(core, byte, scan);
                }
                _ if self.state == State::Osc && !scan && byte >= 0x20 && byte != 0x7f => {
                    self.osc.push(byte)
                }
                _ => (),
            }
            return false;
        }
        if byte == 0x1b {
            if !scan {
                self.interrupt_utf8(core);
            }
            self.state = State::Escape;
            return false;
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.state = State::Ground;
            if !scan {
                self.interrupt_utf8(core);
                if byte == 0x1a {
                    core.input_text("\u{fffd}");
                }
            }
            return false;
        }
        if byte < 0x20 || byte == 0x7f {
            if !scan {
                self.interrupt_utf8(core);
                core.control(byte);
            }
            return false;
        }
        match self.state {
            State::Ground => {
                if !scan {
                    self.text(core, &[byte]);
                }
            }
            State::Escape => match byte {
                b'[' => {
                    self.csi.start();
                    self.state = State::Csi;
                }
                b']' => {
                    self.osc.clear();
                    self.state = State::Osc;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::IgnoreString,
                0x20..=0x2f => {
                    self.escape_intermediate = byte;
                    self.state = State::EscapeIntermediate;
                }
                _ => {
                    self.state = State::Ground;
                    if !scan {
                        core.esc(0, byte);
                    } else if byte == b'c' {
                        // RIS resets synchronized output too; replay it at the batch boundary.
                        return true;
                    }
                }
            },
            State::EscapeIntermediate => {
                if byte >= 0x30 {
                    self.state = State::Ground;
                    if !scan {
                        core.esc(self.escape_intermediate, byte);
                    }
                } else {
                    self.escape_intermediate = 0xff;
                }
            }
            State::Csi if (0x40..=0x7e).contains(&byte) => {
                self.state = State::Ground;
                if !self.csi.ignored {
                    if !scan {
                        core.csi(&self.csi, byte);
                    }
                    if self.csi.sync() {
                        if scan && byte == b'l' {
                            return true;
                        }
                        // A replayed frame must not reopen the frame it is committing.
                        if byte == b'h' && !self.replaying {
                            self.deadline = Some(Instant::now() + SYNC_TIMEOUT);
                        }
                    }
                }
            }
            State::Csi => self.csi.byte(byte),
            _ => (),
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Event, Mode, Point, WindowSize};

    fn terminal() -> (Core, Parser) {
        (
            Core::new(
                WindowSize {
                    num_cols: 8,
                    num_lines: 3,
                    cell_width: 10,
                    cell_height: 20,
                },
                0xd8dee9,
                0x14161b,
            ),
            Parser::new(),
        )
    }

    fn replies(core: &mut Core) -> Vec<u8> {
        core.take_events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Write(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn bulk_text_scanning_preserves_utf8_and_control_boundaries() {
        let mut input = "0123456789abcdef中文λ→\x7fG\r\n\x1b[31mabcdefghijklmnop\tZ\x08Q\x1b[0m\r\ncombining e\u{301}".as_bytes().to_vec();
        input.extend_from_slice(b"\xfftail\0\x1b[6n");
        let (mut expected, mut scalar) = terminal();
        for byte in &input {
            scalar.advance(&mut expected, std::slice::from_ref(byte));
        }
        let expected_replies = replies(&mut expected);
        for width in [7, 8, 9, input.len()] {
            let (mut core, mut parser) = terminal();
            for chunk in input.chunks(width) {
                parser.advance(&mut core, chunk);
            }
            for row in 0..core.screen_lines() {
                assert_eq!(core.row(row).cells, expected.row(row).cells);
                assert_eq!(core.row(row).extras(), expected.row(row).extras());
                assert_eq!(core.row(row).wrapped, expected.row(row).wrapped);
            }
            assert_eq!(core.cursor(), expected.cursor());
            assert_eq!(core.mode(), expected.mode());
            assert_eq!(core.history_size(), expected.history_size());
            assert_eq!(replies(&mut core), expected_replies);
        }
    }

    #[test]
    fn timed_out_frame_preserves_a_split_control_sequence_for_the_next_read() {
        let (mut core, mut parser) = terminal();
        parser.advance(&mut core, b"a\x1b[?2026h\x1b[2;");
        assert_eq!(core.cursor().0, Point { line: 0, column: 1 });
        assert!(parser.finish_sync(&mut core));
        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
        parser.advance(&mut core, b"3HX");
        assert_eq!(core.row(1).cells[2].c, 'X');
    }

    #[test]
    fn full_synchronized_buffer_commits_without_losing_input_after_its_boundary() {
        let (mut core, mut parser) = terminal();
        parser.advance(&mut core, b"\x1b[?2026h");
        let mut bytes = vec![b'\r'; SYNC_LIMIT - 2];
        bytes.extend_from_slice("中Z".as_bytes());
        assert!(parser.advance(&mut core, &bytes));
        assert_eq!(core.row(0).cells[0].c, '中');
        assert_eq!(core.row(0).cells[2].c, 'Z');
        assert!(parser.deadline().is_none());
        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn queued_pty_read_does_not_publish_a_tui_cursor_between_show_and_restore() {
        let (mut core, mut parser) = terminal();
        parser.advance(&mut core, b"\x1b[2;3H");
        let expected = core.cursor();
        let mut update = b"\x1b[?25l\x1b[1;1H".to_vec();
        // tmux enables the cursor before restoring its insertion point. Keep that
        // restoration in the same PTY read but across the parser's history quantum.
        update.resize(512 - b"\x1b[?25h".len(), b'x');
        update.extend_from_slice(b"\x1b[?25h\x1b[2;3H");
        assert!(parser.advance_queued(&mut core, &update));
        assert_eq!(core.cursor(), expected);
        assert!(!parser.has_queued());
    }

    #[cfg(feature = "tui")]
    #[test]
    fn queued_frames_replay_before_their_source_remainders_at_every_split() {
        let input = "A\x1b[?2026h\r\x1b[2K中\u{301}\x1b[2b\x1b[?2026lB\x1b[?2026h\r\nsecond\x1b[6n\x1bcfresh\x1b[?2026h\r\nthird\x1b[6n\x1b[?2026lC".as_bytes();
        let (mut expected, mut direct) = terminal();
        direct.advance(&mut expected, input);
        let expected_replies = replies(&mut expected);
        for split in 0..=input.len() {
            let (mut core, mut parser) = terminal();
            for chunk in [&input[..split], &input[split..]] {
                parser.advance_queued(&mut core, chunk);
                while parser.has_queued() {
                    parser.advance_queued(&mut core, &[]);
                }
            }
            for line in 0..core.screen_lines() {
                assert_eq!(
                    core.row(line).cells,
                    expected.row(line).cells,
                    "split {split}"
                );
                assert_eq!(core.row(line).extras(), expected.row(line).extras());
                assert_eq!(core.row(line).wrapped, expected.row(line).wrapped);
            }
            assert_eq!(core.cursor(), expected.cursor());
            assert_eq!(core.mode(), expected.mode());
            assert_eq!(replies(&mut core), expected_replies);
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn queued_full_frame_resumes_partial_utf8_then_the_next_frame_in_order() {
        let (mut core, mut parser) = terminal();
        let mut input = b"\x1b[?2026h".to_vec();
        input.extend(std::iter::repeat_n(b'\r', SYNC_LIMIT - 2));
        input.extend_from_slice("中Z\x1b[?2026h\r\nnext\x1b[?2026l!".as_bytes());
        parser.advance_queued(&mut core, &input);
        while parser.has_queued() {
            parser.advance_queued(&mut core, &[]);
        }
        assert_eq!(core.row(0).cells[0].c, '中');
        assert_eq!(core.row(0).cells[2].c, 'Z');
        assert_eq!(
            core.row(1).cells[..5]
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "next!"
        );
        assert!(parser.deadline().is_none());
        assert!(!core.mode().contains(Mode::SYNC_UPDATE));
    }

    #[test]
    fn palette_queries_and_resets_preserve_color_state_and_damage() {
        let (mut core, mut parser) = terminal();
        parser.advance(
            &mut core,
            b"\x1b]4;1;#123;2;rgb:0000/8000/ffff\x07\x1b]4;1;?;2;?\x1b\\",
        );
        assert_eq!(
            replies(&mut core),
            b"\x1b]4;1;rgb:1010/2020/3030\x1b\\\x1b]4;2;rgb:0000/7f7f/ffff\x1b\\"
        );
        core.reset_damage();
        parser.advance(&mut core, b"\x1b]4;1;#123;2;rgb:0000/8000/ffff\x07");
        assert_eq!(core.dirty_rows().count(), 0);
        parser.advance(&mut core, b"\x1b]104;9999\x07");
        assert_eq!(
            core.color(Color::Indexed(1)),
            Rgb {
                r: 16,
                g: 32,
                b: 48
            }
        );
        parser.advance(&mut core, b"\x1b]104\x07");
        assert_eq!(
            core.color(Color::Indexed(1)),
            Rgb {
                r: 224,
                g: 108,
                b: 117
            }
        );
        assert_eq!(core.dirty_rows().count(), core.screen_lines());
        core.reset_damage();
        parser.advance(
            &mut core,
            b"\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07",
        );
        assert_eq!(core.dirty_rows().count(), 0);
    }

    #[test]
    fn malformed_escape_intermediates_and_misplaced_prefix_do_not_execute() {
        let (mut core, mut parser) = terminal();
        parser.advance(&mut core, b"abc\x1b##c\x1b[0?6hX\x1b[2:3HY");
        assert_eq!(
            core.row(0)
                .cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>(),
            "abcXY   "
        );
        assert!(!core.mode().contains(Mode::ORIGIN));
    }
    #[test]
    fn existing_tab_cursor_shape_query_and_sgr_paths_remain_available() {
        let (mut core, mut parser) = terminal();
        core.resize(WindowSize {
            num_cols: 20,
            num_lines: 3,
            cell_width: 10,
            cell_height: 20,
        });
        parser.advance(
            &mut core,
            b"\x1b[3g\x1b[?5W\tX\x1b]50;CursorShape=1\x07\x1b[?4m\x1b[>4m\x1b[1;2;21mY",
        );
        assert_eq!(core.row(0).cells[8].c, 'X');
        assert_eq!(core.cursor().1, CursorShape::Beam);
        assert_eq!(replies(&mut core), b"\x1b[>4;0m");
        assert_eq!(core.row(0).cells[9].flags, crate::core::Flags::DIM);
    }

    #[test]
    fn sub_cancels_each_control_string_with_the_visible_substitution() {
        for prefix in *b"]PX^_" {
            let (mut core, mut parser) = terminal();
            parser.advance(&mut core, &[b'A', 0x1b, prefix]);
            parser.advance(&mut core, b"hidden\x1aB");
            assert_eq!(
                core.row(0).cells[..3]
                    .iter()
                    .map(|cell| cell.c)
                    .collect::<String>(),
                "A\u{fffd}B"
            );
            assert!(core.take_events().is_empty());
        }
    }

    #[test]
    fn unchanged_titles_preserve_stack_restore_and_explicit_reset_notifications() {
        let (mut core, mut parser) = terminal();
        let titles = |core: &mut Core| {
            core.take_events()
                .into_iter()
                .filter_map(|event| match event {
                    crate::core::Event::Title(title) => Some(title),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        parser.advance(
            &mut core,
            b"\x1b[22t\x1b]2;one\x07\x1b]0;one\x07\x1b[22t\x1b]2;two\x07\x1b[23t\x1b[23t",
        );
        assert_eq!(
            titles(&mut core),
            [
                Some("one".into()),
                Some("two".into()),
                Some("one".into()),
                None
            ]
        );
        parser.advance(&mut core, b"\x1bc");
        assert_eq!(titles(&mut core), [None]);
        parser.advance(&mut core, b"\x1b]2;\x07\x1b]0;\x07");
        assert_eq!(titles(&mut core), [Some(String::new())]);
    }

    #[test]
    fn large_osc_payloads_are_complete_but_do_not_remain_in_the_idle_parser() {
        let (mut core, mut parser) = terminal();
        let title = "中文标题".repeat(OSC_RETAIN_LIMIT);
        parser.advance(&mut core, b"\x1b]2;");
        parser.advance(&mut core, title.as_bytes());
        parser.advance(&mut core, b"\x1b\\");
        assert!(
            matches!(core.take_events().as_slice(), [Event::Title(Some(value))] if value == &title)
        );
        assert!(parser.osc.capacity() <= OSC_RETAIN_LIMIT);

        parser.advance(&mut core, b"\x1b]52;c;");
        parser.advance(&mut core, "YWFh".repeat(OSC_RETAIN_LIMIT).as_bytes());
        parser.advance(&mut core, b"\x07");
        let events = core.take_events();
        assert!(
            matches!(events.as_slice(), [Event::ClipboardStore { primary: false, text }] if text == &"a".repeat(OSC_RETAIN_LIMIT * 3))
        );
        assert!(parser.osc.capacity() <= OSC_RETAIN_LIMIT);

        parser.advance(&mut core, b"\x1b]2;");
        parser.advance(&mut core, title.as_bytes());
        parser.advance(&mut core, b"\x18\x1b]2;short\x07");
        assert!(
            matches!(core.take_events().as_slice(), [Event::Title(Some(value))] if value == "short")
        );
        assert!(parser.osc.capacity() <= OSC_RETAIN_LIMIT);
    }
}
