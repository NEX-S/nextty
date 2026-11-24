//! Streaming VT input for the terminal's screen core.

use serde::{Deserialize, Serialize};

use crate::core::Core;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub(crate) const fn from_packed(value: u32) -> Self {
        Self {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
    }

    pub(crate) const fn packed(self) -> u32 {
        u32::from_be_bytes([0, self.r, self.g, self.b])
    }

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
        self.packed() << 8 | 255
    }
}

/// Palette slots past the 256 indexed colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum NamedColor {
    Foreground = 256,
    Background,
    Cursor,
}

pub(crate) const PALETTE_SIZE: usize = NamedColor::Cursor as usize + 1;

impl NamedColor {
    pub fn from_index(index: u16) -> Option<Self> {
        match index {
            256 => Some(Self::Foreground),
            257 => Some(Self::Background),
            258 => Some(Self::Cursor),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Named(NamedColor),
    Spec(Rgb),
    Indexed(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CursorShape {
    #[default]
    Block = 0,
    Underline = 1,
    Beam = 2,
    Hidden = 3,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum StandardCharset {
    #[default]
    Ascii,
    SpecialGraphics,
}

impl StandardCharset {
    pub(crate) fn map(self, c: char) -> char {
        // DEC special graphics replaces 0x60..=0x7e; '_' is a blank.
        const GRAPHICS: &str = "◆▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·";
        match (self, c) {
            (Self::SpecialGraphics, '_') => ' ',
            (Self::SpecialGraphics, '`'..='~') => GRAPHICS.chars().nth(c as usize - 0x60).unwrap(),
            _ => c,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate(u8),
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
    count: usize,
    pub(crate) colons: u32,
    pub(crate) prefix: u8,
    pub(crate) intermediate: u8,
    ignored: bool,
    started: bool,
}

impl Csi {
    fn start(&mut self) {
        // Unused slots are never read; separators initialize them as they become active.
        *self = Self {
            values: self.values,
            count: 1,
            ..Self::default()
        };
        self.values[0] = 0;
    }

    fn byte(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' if self.intermediate == 0 => {
                self.started = true;
                let value = &mut self.values[self.count - 1];
                let next = u32::from(*value) * 10 + u32::from(byte - b'0');
                if next <= u32::from(u16::MAX) {
                    *value = next as u16;
                } else {
                    self.ignored = true;
                }
            }
            b';' | b':' if self.intermediate == 0 => {
                self.started = true;
                if self.count == self.values.len() {
                    self.ignored = true;
                } else {
                    if byte == b':' {
                        self.colons |= 1 << self.count;
                    }
                    self.values[self.count] = 0;
                    self.count += 1;
                }
            }
            b'<'..=b'?'
                if self.count == 1
                    && !self.started
                    && self.prefix == 0
                    && self.intermediate == 0 =>
            {
                self.prefix = byte
            }
            0x20..=0x2f if self.intermediate == 0 => self.intermediate = byte,
            _ => self.ignored = true,
        }
    }

    pub(crate) fn params(&self) -> &[u16] {
        &self.values[..self.count]
    }
    pub(crate) fn value(&self, index: usize) -> u16 {
        self.params().get(index).copied().unwrap_or(0)
    }
    pub(crate) fn count(&self, index: usize) -> usize {
        usize::from(self.value(index).max(1))
    }
    /// Whether parameter `index` is a colon-separated subparameter.
    pub(crate) fn colon(&self, index: usize) -> bool {
        self.colons & (1 << index) != 0
    }
}

const OSC_RETAIN_LIMIT: usize = 16 * 1024;

/// Frames input into text and control sequences for `Core`; only incomplete input stays here.
#[derive(Default)]
pub struct Parser {
    state: State,
    csi: Csi,
    utf8: [u8; 4],
    utf8_len: usize,
    osc: Vec<u8>,
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

    /// Parses until `bytes` or the history's backpressure ends. Returns the bytes taken and whether
    /// the grid changed in a way to present; a synchronized update defers presentation.
    pub fn advance(&mut self, core: &mut Core, bytes: &[u8]) -> (usize, bool) {
        let mut position = 0;
        // Backpressure is checked per slice; a slice scrolls at most one row per byte.
        while position < bytes.len() && core.history_read_ready() {
            let slice = &bytes[position..bytes.len().min(position + 512)];
            let mut taken = 0;
            while taken < slice.len() {
                if self.state == State::Ground && slice[taken] >= 0x20 && slice[taken] != 0x7f {
                    let length = text_prefix(&slice[taken..]);
                    self.text(core, &slice[taken..taken + length]);
                    taken += length;
                } else {
                    taken += self.step(core, &slice[taken..]);
                }
            }
            position += taken;
        }
        (
            position,
            position != 0 && !core.mode().contains(crate::core::Mode::SYNC_UPDATE),
        )
    }

    fn text(&mut self, core: &mut Core, bytes: &[u8]) {
        let joined;
        let mut bytes = if self.utf8_len == 0 {
            bytes
        } else {
            joined = [&self.utf8[..self.utf8_len], bytes].concat();
            self.utf8_len = 0;
            &joined
        };
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

    fn clear_osc(&mut self) {
        self.osc.clear();
        // A large clipboard or title must not stay resident in an idle parser.
        self.osc.shrink_to(OSC_RETAIN_LIMIT);
    }

    // Avoid a call per control byte while allowing OSC payloads to consume whole runs.
    #[inline(always)]
    fn step(&mut self, core: &mut Core, bytes: &[u8]) -> usize {
        let byte = bytes[0];
        if matches!(
            self.state,
            State::Osc | State::OscEscape | State::IgnoreString | State::IgnoreStringEscape
        ) {
            match byte {
                0x18 | 0x1a => {
                    self.clear_osc();
                    self.state = State::Ground;
                    if byte == 0x1a {
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
                    if self.state == State::OscEscape {
                        core.osc(&self.osc, "\x1b\\");
                    }
                    self.clear_osc();
                    self.state = State::Ground;
                }
                0x07 if matches!(self.state, State::Osc | State::OscEscape) => {
                    core.osc(&self.osc, "\x07");
                    self.clear_osc();
                    self.state = State::Ground;
                }
                _ if matches!(self.state, State::OscEscape | State::IgnoreStringEscape) => {
                    self.clear_osc();
                    self.state = State::Escape;
                    return self.step(core, bytes);
                }
                _ if self.state == State::Osc && byte >= 0x20 && byte != 0x7f => {
                    let length = text_prefix(bytes);
                    self.osc.extend_from_slice(&bytes[..length]);
                    return length;
                }
                _ => (),
            }
            return 1;
        }
        if byte < 0x20 || byte == 0x7f {
            if self.utf8_len != 0 {
                core.input_text("\u{fffd}");
                self.utf8_len = 0;
            }
            match byte {
                0x1b => self.state = State::Escape,
                0x18 | 0x1a => {
                    self.state = State::Ground;
                    if byte == 0x1a {
                        core.input_text("\u{fffd}");
                    }
                }
                _ => core.control(byte),
            }
            return 1;
        }
        match self.state {
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
                0x20..=0x2f => self.state = State::EscapeIntermediate(byte),
                _ => {
                    self.state = State::Ground;
                    core.escape(0, byte);
                }
            },
            State::EscapeIntermediate(intermediate) => {
                if byte >= 0x30 {
                    self.state = State::Ground;
                    core.escape(intermediate, byte);
                } else {
                    self.state = State::EscapeIntermediate(0xff);
                }
            }
            State::Csi if (0x40..=0x7e).contains(&byte) => {
                self.state = State::Ground;
                if !self.csi.ignored {
                    core.csi(&self.csi, byte);
                }
            }
            State::Csi => self.csi.byte(byte),
            _ => (),
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::{replies, terminal};
    use crate::core::{Event, Mode};

    #[test]
    fn omitted_csi_parameters_do_not_inherit_previous_values() {
        let dirty = format!("\x1b[{}m", ["31"; 32].join(";"));
        let empty = ";".repeat(31);
        for parameters in ["", ";", "1;;4", "1;;58:2::1:2:3;4", &empty] {
            let clean = format!("\x1b[{parameters}mX");
            let (mut expected, mut parser) = terminal(8, 3);
            parser.advance(&mut expected, clean.as_bytes());
            for chunk in [1, 3, clean.len()] {
                let (mut actual, mut parser) = terminal(8, 3);
                parser.advance(&mut actual, dirty.as_bytes());
                for bytes in clean.as_bytes().chunks(chunk) {
                    parser.advance(&mut actual, bytes);
                }
                assert_eq!(actual.row(0).cells, expected.row(0).cells);
                assert_eq!(actual.row(0).extras(), expected.row(0).extras());
            }
        }
    }

    #[test]
    fn bulk_text_scanning_preserves_utf8_and_control_boundaries() {
        let mut input = "0123456789abcdef中文λ→\x7fG\r\n\x1b[31mabcdefghijklmnop\tZ\x08Q\x1b[0m\r\ncombining e\u{301}".as_bytes().to_vec();
        input.extend_from_slice(b"\xfftail\0\x1b[6n");
        let (mut expected, mut scalar) = terminal(8, 3);
        for byte in &input {
            scalar.advance(&mut expected, std::slice::from_ref(byte));
        }
        let expected_replies = replies(&mut expected);
        for width in [7, 8, 9, input.len()] {
            let (mut core, mut parser) = terminal(8, 3);
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
    fn bulk_osc_preserves_payloads_and_control_boundaries_at_every_split() {
        let input = b"\x1b]2;0123456789abcdef\0\t\r\n\x7f\xe4\xb8\xad\xff\x07\x1b]52;c;YWFhYWFhYWFh\x1b\\\x1b]2;cancelled\x18\x1b]2;also cancelled\x1a\x1b]2;interrupted\x1b[2J\x1b]2;final\x1b\\X";
        for split in 0..=input.len() {
            let (mut core, mut parser) = terminal(8, 3);
            parser.advance(&mut core, &input[..split]);
            parser.advance(&mut core, &input[split..]);
            let events = core.take_events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::Title(Some(title)), Event::ClipboardStore { primary: false, text }, Event::Title(Some(last))]
                        if title == "0123456789abcdef中\u{fffd}"
                            && text == "aaaaaaaaa"
                            && last == "final"
                ),
                "split {split}: {events:?}"
            );
            assert_eq!(core.row(0).cells[1].c, 'X', "split {split}");
            assert_eq!(parser.state, State::Ground);
            assert!(parser.osc.is_empty());
        }
    }

    #[test]
    fn synchronized_queries_reply_before_presentation() {
        let (mut core, mut parser) = terminal(8, 3);
        parser.advance(&mut core, b"\x1b[?2026h");
        assert!(!parser.advance(&mut core, "中Z\x1b[6n\x1b[c".as_bytes()).1);
        assert_eq!(core.row(0).cells[0].c, '中');
        assert_eq!(core.row(0).cells[2].c, 'Z');
        assert_eq!(replies(&mut core), b"\x1b[1;4R\x1b[?6c");
        assert!(core.mode().contains(Mode::SYNC_UPDATE));
        assert!(parser.advance(&mut core, b"\x1b[?2026l").1);
        assert!(core.sync_deadline().is_none());
    }

    #[test]
    fn unchanged_titles_preserve_stack_restore_and_explicit_reset_notifications() {
        let (mut core, mut parser) = terminal(8, 3);
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
}
