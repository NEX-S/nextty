//! Streaming VT input for the terminal's screen core.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use crate::core::{Core, Flags};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

pub(crate) const PALETTE_SIZE: usize = NamedColor::DimForeground as usize + 1;

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
#[repr(u8)]
pub enum CursorShape {
    #[default]
    Block = 0,
    Underline = 1,
    Beam = 2,
    Hidden = 3,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Attr {
    Reset,
    Flags(Flags, bool),
    Underline(Flags),
    Foreground(Color),
    Background(Color),
    UnderlineColor(Option<Color>),
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
    EscapeIntermediate(u8),
    Csi,
    Osc,
    OscEscape,
    IgnoreString,
    IgnoreStringEscape,
}

// CSI has a small fixed parameter list; an overfull or malformed sequence is ignored whole.
#[derive(Default)]
struct Csi {
    values: [u16; 32],
    count: usize,
    colons: u32,
    prefix: u8,
    intermediate: u8,
    ignored: bool,
    started: bool,
}

impl Csi {
    fn start(&mut self) {
        *self = Self {
            count: 1,
            ..Self::default()
        };
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

    fn value(&self, index: usize) -> u16 {
        self.values[..self.count].get(index).copied().unwrap_or(0)
    }
    fn count(&self, index: usize) -> usize {
        usize::from(self.value(index).max(1))
    }
    fn colon(&self, index: usize) -> bool {
        self.colons & (1 << index) != 0
    }
}

const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const OSC_RETAIN_LIMIT: usize = 16 * 1024;

/// Owns only input framing and incomplete input; all committed terminal state lives in `Core`.
#[derive(Default)]
pub struct Parser {
    state: State,
    csi: Csi,
    utf8: [u8; 4],
    utf8_len: usize,
    last_char: Option<char>,
    osc: Vec<u8>,
    deadline: Option<Instant>,

    queued: std::collections::VecDeque<(Vec<u8>, usize)>,
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

    /// Parses immediately; synchronized updates defer presentation until the mode ends.
    pub fn advance(&mut self, core: &mut Core, bytes: &[u8]) -> bool {
        let mut position = 0;
        while position < bytes.len() {
            if self.state == State::Ground && bytes[position] >= 0x20 && bytes[position] != 0x7f {
                let length = text_prefix(&bytes[position..]);
                self.text(core, &bytes[position..position + length]);
                position += length;
            } else {
                position += self.step(core, &bytes[position..]);
            }
        }
        !bytes.is_empty() && !core.mode().contains(crate::core::Mode::SYNC_UPDATE)
    }

    pub(crate) fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    pub(crate) fn advance_queued(&mut self, core: &mut Core, bytes: &[u8]) -> bool {
        let mut input = if self.queued.is_empty() {
            (!bytes.is_empty()).then_some((Cow::Borrowed(bytes), 0))
        } else {
            if !bytes.is_empty() {
                self.queued.push_back((bytes.to_vec(), 0));
            }
            None
        };
        let mut committed = false;
        // Keep one batch under the Core lock while respecting history backpressure.
        let mut remaining = 16 * 1024;
        while let Some((data, mut position)) = input.take().or_else(|| {
            self.queued
                .pop_front()
                .map(|(data, position)| (Cow::Owned(data), position))
        }) {
            while position < data.len() && remaining != 0 && core.history_read_ready() {
                let end = (position + remaining.min(512)).min(data.len());
                committed |= self.advance(core, &data[position..end]);
                remaining -= end - position;
                position = end;
            }
            if position < data.len() {
                self.queued.push_front((data.into_owned(), position));
            }
            if remaining == 0 || !core.history_read_ready() {
                break;
            }
        }
        committed && !core.mode().contains(crate::core::Mode::SYNC_UPDATE)
    }

    pub fn finish_sync(&mut self, core: &mut Core) -> bool {
        if self.deadline.take().is_none() {
            return false;
        }
        core.set_private(2026, false);
        true
    }

    fn print(&mut self, core: &mut Core, text: &str) {
        core.input_text(text);
        self.last_char = text.chars().next_back().or(self.last_char);
    }

    fn text(&mut self, core: &mut Core, mut bytes: &[u8]) {
        while self.utf8_len != 0 && !bytes.is_empty() {
            self.utf8[self.utf8_len] = bytes[0];
            self.utf8_len += 1;
            bytes = &bytes[1..];
            loop {
                match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
                    Ok(text) => {
                        let c = text.chars().next().unwrap();
                        core.input(c);
                        self.last_char = Some(c);
                        self.utf8_len = 0;
                        break;
                    }
                    Err(error) => {
                        let Some(length) = error.error_len() else {
                            break;
                        };
                        self.print(core, "\u{fffd}");
                        self.utf8.copy_within(length..self.utf8_len, 0);
                        self.utf8_len -= length;
                        if self.utf8_len == 0 {
                            break;
                        }
                    }
                }
            }
        }
        while !bytes.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    self.print(core, text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid != 0 {
                        self.print(core, std::str::from_utf8(&bytes[..valid]).unwrap());
                    }
                    bytes = &bytes[valid..];
                    if let Some(length) = error.error_len() {
                        self.print(core, "\u{fffd}");
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

    fn control(&mut self, core: &mut Core, byte: u8) {
        match byte {
            0x08 => core.move_backward(1),
            0x09 => core.move_tabs(1, true),
            0x0a..=0x0c => core.newline(),
            0x0d => core.carriage_return(),
            0x0e => core.set_active_charset(1),
            0x0f => core.set_active_charset(0),
            _ => (),
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
                        self.print(core, "\u{fffd}");
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
                        self.dispatch_osc(core, "\x1b\\");
                    }
                    self.clear_osc();
                    self.state = State::Ground;
                }
                0x07 if matches!(self.state, State::Osc | State::OscEscape) => {
                    self.dispatch_osc(core, "\x07");

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
                self.print(core, "\u{fffd}");
                self.utf8_len = 0;
            }
            match byte {
                0x1b => self.state = State::Escape,
                0x18 | 0x1a => {
                    self.state = State::Ground;
                    if byte == 0x1a {
                        self.print(core, "\u{fffd}");
                    }
                }
                _ => self.control(core, byte),
            }
            return 1;
        }
        match self.state {
            State::Ground => {
                self.text(core, &[byte]);
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
                0x20..=0x2f => self.state = State::EscapeIntermediate(byte),
                _ => {
                    self.state = State::Ground;
                    self.escape(core, 0, byte);
                }
            },
            State::EscapeIntermediate(intermediate) => {
                if byte >= 0x30 {
                    self.state = State::Ground;
                    self.escape(core, intermediate, byte);
                } else {
                    self.state = State::EscapeIntermediate(0xff);
                }
            }
            State::Csi if (0x40..=0x7e).contains(&byte) => {
                self.state = State::Ground;
                if !self.csi.ignored {
                    self.dispatch_csi(core, byte);
                }
            }
            State::Csi => self.csi.byte(byte),
            _ => (),
        }
        1
    }

    fn escape(&mut self, core: &mut Core, intermediate: u8, final_byte: u8) {
        match (intermediate, final_byte) {
            (0, b'7') => core.save_cursor_position(),
            (0, b'8') => core.restore_cursor_position(),
            (0, b'D') => core.linefeed(),
            (0, b'E') => {
                core.linefeed();
                core.carriage_return();
            }
            (0, b'H') => core.set_horizontal_tabstop(),
            (0, b'M') => core.reverse_index(),
            (0, b'Z') => core.identify_terminal(None),
            (0, b'=' | b'>') => core.set_keypad_application_mode(final_byte == b'='),
            (0, b'c') => {
                core.reset_state();
                self.last_char = None;
                self.deadline = None;
            }
            (b'#', b'8') => core.decaln(),
            (b'('..=b'+', b'0' | b'B') => core.configure_charset(
                usize::from(intermediate - b'('),
                if final_byte == b'0' {
                    StandardCharset::SpecialGraphics
                } else {
                    StandardCharset::Ascii
                },
            ),
            _ => (),
        }
    }

    fn dispatch_csi(&mut self, core: &mut Core, final_byte: u8) {
        let p = &self.csi;
        if p.colons != 0 && (p.prefix != 0 || p.intermediate != 0 || final_byte != b'm') {
            return;
        }
        let count = p.count(0);
        match (p.prefix, p.intermediate, final_byte) {
            (0, 0, b'@' | b'P') => core.shift_chars(count, final_byte == b'@'),
            (0, 0, b'A' | b'B' | b'e' | b'E' | b'F') => {
                core.move_vertical(count, matches!(final_byte, b'B' | b'e' | b'E'));
                if matches!(final_byte, b'E' | b'F') {
                    core.carriage_return();
                }
            }
            (0, 0, b'C' | b'a') => core.move_forward(count),
            (0, 0, b'D') => core.move_backward(count),
            (0, 0, b'G' | b'`') => core.goto_col(count - 1),
            (0, 0, b'H' | b'f') => core.goto((count - 1) as i32, p.count(1) - 1),
            (0, 0, b'd') => core.goto_line((count - 1) as i32),
            (0, 0, b'I' | b'Z') => core.move_tabs(count as u16, final_byte == b'I'),
            (0, 0, b'J') => match p.value(0) {
                0 => core.clear_screen(ClearMode::Below),
                1 => core.clear_screen(ClearMode::Above),
                2 => core.clear_screen(ClearMode::All),
                3 => core.clear_screen(ClearMode::Saved),
                _ => (),
            },
            (0, 0, b'K') => match p.value(0) {
                0 => core.clear_line(LineClearMode::Right),
                1 => core.clear_line(LineClearMode::Left),
                2 => core.clear_line(LineClearMode::All),
                _ => (),
            },
            (0, 0, b'L' | b'M') => core.shift_lines(count, final_byte == b'L'),
            (0, 0, b'S' | b'T') => core.scroll(count, final_byte == b'T'),
            (0, 0, b'X') => core.erase_chars(count),
            (0, 0, b'b') => {
                if let Some(c) = self.last_char {
                    if c.is_ascii() && count > 1 {
                        core.repeat_ascii(c as u8, count);
                    } else {
                        for _ in 0..count {
                            core.input(c);
                        }
                    }
                }
            }
            (0, 0, b'g') => match p.value(0) {
                0 => core.clear_tabs(false),
                3 => core.clear_tabs(true),
                _ => (),
            },
            (0, 0, b'h' | b'l') => {
                for mode in &p.values[..p.count] {
                    core.set_mode(*mode, final_byte == b'h');
                }
            }
            (b'?', 0, b'h' | b'l') => {
                for mode in &p.values[..p.count] {
                    core.set_private(*mode, final_byte == b'h');
                }
                if p.values[..p.count].contains(&2026) {
                    self.deadline = (final_byte == b'h').then(|| Instant::now() + SYNC_TIMEOUT);
                }
            }
            (0, 0, b'm') => self.sgr(core),
            (0 | b'>' | b'=', 0, b'c') if p.value(0) == 0 => {
                core.identify_terminal((p.prefix != 0).then_some(char::from(p.prefix)))
            }
            (0, 0, b'n') => core.device_status(usize::from(p.value(0))),
            (0 | b'?', b'$', b'p') => core.report_mode(p.value(0), p.prefix == b'?'),
            (0, b' ', b'q') => core.set_cursor_shape(match p.value(0) {
                0..=2 => CursorShape::Block,
                3..=4 => CursorShape::Underline,
                5..=6 => CursorShape::Beam,
                _ => return,
            }),
            (0, 0, b'r') => {
                core.set_scrolling_region(count, (p.value(1) != 0).then(|| usize::from(p.value(1))))
            }
            (0, 0, b's') => core.save_cursor_position(),
            (0, 0, b'u') => core.restore_cursor_position(),
            (b'?', 0, b'u') => core.report_keyboard_mode(),
            (b'?', 0, b'm') if p.value(0) == 4 => core.report_modify_other_keys(),
            (0, 0, b't') => match p.value(0) {
                14 => core.text_area_size_pixels(),
                18 => core.text_area_size_chars(),
                22 => core.push_title(),
                23 => core.pop_title(),
                _ => (),
            },
            (b'?', 0, b'W') if p.value(0) == 5 => core.set_tabs(8),
            _ => (),
        }
    }

    fn sgr(&self, core: &mut Core) {
        let p = &self.csi;
        let mut index = 0;
        while index < p.count {
            let value = p.values[index];
            let mut end = index + 1;
            while end < p.count && p.colon(end) {
                end += 1;
            }
            let colon = end > index + 1;
            let attr = match value {
                0 => Some(Attr::Reset),
                1 => Some(Attr::Flags(Flags::BOLD, true)),
                2 => Some(Attr::Flags(Flags::DIM, true)),
                3 => Some(Attr::Flags(Flags::ITALIC, true)),
                4 => match if colon { p.values[index + 1] } else { 1 } {
                    0 => Some(Attr::Flags(Flags::ALL_UNDERLINES, false)),
                    1 => Some(Attr::Underline(Flags::UNDERLINE)),
                    2 => Some(Attr::Underline(Flags::DOUBLE_UNDERLINE)),
                    3 => Some(Attr::Underline(Flags::UNDERCURL)),
                    4 => Some(Attr::Underline(Flags::DOTTED_UNDERLINE)),
                    5 => Some(Attr::Underline(Flags::DASHED_UNDERLINE)),
                    _ => None,
                },
                7 => Some(Attr::Flags(Flags::INVERSE, true)),
                8 => Some(Attr::Flags(Flags::HIDDEN, true)),
                9 => Some(Attr::Flags(Flags::STRIKEOUT, true)),
                21 => Some(Attr::Flags(Flags::BOLD, false)),
                22 => Some(Attr::Flags(Flags::BOLD | Flags::DIM, false)),
                23 => Some(Attr::Flags(Flags::ITALIC, false)),
                24 => Some(Attr::Flags(Flags::ALL_UNDERLINES, false)),
                27 => Some(Attr::Flags(Flags::INVERSE, false)),
                28 => Some(Attr::Flags(Flags::HIDDEN, false)),
                29 => Some(Attr::Flags(Flags::STRIKEOUT, false)),
                30..=37 | 90..=97 => Some(Attr::Foreground(ansi_color(if value < 90 {
                    value - 30
                } else {
                    value - 90 + 8
                }))),
                39 => Some(Attr::Foreground(Color::Named(NamedColor::Foreground))),
                40..=47 | 100..=107 => Some(Attr::Background(ansi_color(if value < 100 {
                    value - 40
                } else {
                    value - 100 + 8
                }))),
                49 => Some(Attr::Background(Color::Named(NamedColor::Background))),
                38 | 48 | 58 => {
                    let mode = p.value(index + 1);
                    let channels = if mode == 2 {
                        3
                    } else if mode == 5 {
                        1
                    } else {
                        0
                    };
                    let start = if colon && mode == 2 && end - index == 6 {
                        index + 3
                    } else {
                        index + 2
                    };
                    let available = if colon { end } else { p.count };
                    let color = if channels != 0 && start + channels <= available {
                        if mode == 5 {
                            u8::try_from(p.values[start]).ok().map(Color::Indexed)
                        } else {
                            match (
                                u8::try_from(p.values[start]),
                                u8::try_from(p.values[start + 1]),
                                u8::try_from(p.values[start + 2]),
                            ) {
                                (Ok(r), Ok(g), Ok(b)) => Some(Color::Spec(Rgb { r, g, b })),
                                _ => None,
                            }
                        }
                    } else {
                        None
                    };
                    if !colon {
                        end = (start + channels).min(p.count);
                    }
                    color.map(|color| match value {
                        38 => Attr::Foreground(color),
                        48 => Attr::Background(color),
                        _ => Attr::UnderlineColor(Some(color)),
                    })
                }
                59 => Some(Attr::UnderlineColor(None)),
                _ => None,
            };
            if let Some(attr) = attr {
                core.terminal_attribute(attr);
            }
            index = end;
        }
    }

    fn dispatch_osc(&self, core: &mut Core, terminator: &str) {
        let mut parts = self.osc.split(|b| *b == b';');
        let command_bytes = parts.next().unwrap();
        let Some(command) = decimal(command_bytes) else {
            return;
        };
        match command {
            0 | 2 => {
                let offset = (command_bytes.len() + 1).min(self.osc.len());
                core.set_title(Some(String::from_utf8_lossy(&self.osc[offset..])));
            }

            7 => {
                if let Some(directory) = parts.next() {
                    core.set_directory_uri(directory);
                }
            }

            133 => {
                if let Some(marker) = parts.next() {
                    core.shell_marker(marker, parts.next());
                }
            }
            4 => {
                while let (Some(index), Some(value)) = (parts.next(), parts.next()) {
                    if let Some(index) = decimal(index).filter(|i| *i < 256) {
                        if value == b"?" {
                            core.dynamic_color_sequence(
                                format_args!("4;{index}"),
                                index,
                                terminator,
                            );
                        } else if let Some(color) = parse_color(value) {
                            core.set_color(index, Some(color));
                        }
                    }
                }
            }
            10..=12 => {
                for (command, value) in (command..=12).zip(parts) {
                    let index = command + 246;
                    if value == b"?" {
                        core.dynamic_color_sequence(command, index, terminator);
                    } else if let Some(color) = parse_color(value) {
                        core.set_color(index, Some(color));
                    }
                }
            }
            50 => {
                if let Some(value) = parts
                    .next()
                    .and_then(|part| part.strip_prefix(b"CursorShape="))
                    && let Some(shape) = match value.first() {
                        Some(b'0') => Some(CursorShape::Block),
                        Some(b'1') => Some(CursorShape::Beam),
                        Some(b'2') => Some(CursorShape::Underline),
                        _ => None,
                    }
                {
                    core.set_cursor_shape(shape);
                }
            }
            52 => {
                if let (Some(target), Some(data)) = (parts.next(), parts.next()) {
                    let clipboard = target.first().copied().unwrap_or(b'c');
                    if !matches!(clipboard, b'c' | b'p' | b's') {
                        return;
                    }
                    if data == b"?" {
                        core.clipboard_load(clipboard, terminator);
                    } else {
                        core.clipboard_store(clipboard == b'p', data);
                    }
                }
            }
            104 => {
                let mut supplied = false;
                for part in parts {
                    supplied |= !part.is_empty();
                    if let Some(index) = decimal(part).filter(|i| *i < 256) {
                        core.set_color(index, None);
                    }
                }
                if !supplied {
                    for index in 0..256 {
                        core.set_color(index, None);
                    }
                }
            }
            110..=112 => core.set_color(command + 146, None),
            _ => (),
        }
    }
}

fn ansi_color(index: u16) -> Color {
    Color::Named(NamedColor::from_index(index).unwrap())
}

fn decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0usize, |value, byte| {
        if byte.is_ascii_digit() {
            value.checked_mul(10)?.checked_add(usize::from(byte - b'0'))
        } else {
            None
        }
    })
}

fn parse_color(bytes: &[u8]) -> Option<Rgb> {
    fn hex_value(bytes: &[u8]) -> Option<u16> {
        if bytes.is_empty() || bytes.len() > 4 {
            return None;
        }
        bytes.iter().try_fold(0u16, |value, byte| {
            Some(value * 16 + char::from(*byte).to_digit(16)? as u16)
        })
    }
    fn channel(bytes: &[u8]) -> Option<u8> {
        let value = u32::from(hex_value(bytes)?);
        Some((value * 255 / ((1u32 << (bytes.len() * 4)) - 1)) as u8)
    }
    if let Some(hex) = bytes.strip_prefix(b"#") {
        if !matches!(hex.len(), 3 | 6 | 9 | 12) {
            return None;
        }
        let width = hex.len() / 3;
        let legacy = |part: &[u8]| {
            let value = hex_value(part)?;
            Some(match width {
                1 => value << 4,
                2 => value,
                3 => value >> 4,
                _ => value >> 8,
            } as u8)
        };
        return Some(Rgb {
            r: legacy(&hex[..width])?,
            g: legacy(&hex[width..width * 2])?,
            b: legacy(&hex[width * 2..])?,
        });
    }
    let rgb = bytes.strip_prefix(b"rgb:")?;
    let mut parts = rgb.split(|b| *b == b'/');
    let color = Rgb {
        r: channel(parts.next()?)?,
        g: channel(parts.next()?)?,
        b: channel(parts.next()?)?,
    };
    parts.next().is_none().then_some(color)
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
    fn repeated_text_matches_literal_text_across_wraps_and_modes() {
        for mode in [
            "",
            "\x1b[4h",
            "\x1b(0",
            "\x1b[?7l",
            "\x1b[31;48;5;42;58;5;3;4m",
        ] {
            for count in [0, 1, 2, 63, 64, 65, 140, 300] {
                let setup = format!("\x1b[?1049h中\u{301}abc\r\nold text\x1b[H{mode}q");
                let (mut expected, mut literal) = terminal();
                literal.advance(&mut expected, setup.as_bytes());
                literal.advance(&mut expected, "q".repeat(count.max(1)).as_bytes());
                let (mut core, mut parser) = terminal();
                parser.advance(&mut core, setup.as_bytes());
                parser.advance(&mut core, format!("\x1b[{count}b").as_bytes());
                assert_eq!(core.cursor(), expected.cursor(), "{mode:?} {count}");
                for row in 0..core.screen_lines() {
                    assert_eq!(core.row(row).cells, expected.row(row).cells);
                    assert_eq!(core.row(row).extras(), expected.row(row).extras());
                    assert_eq!(core.row(row).used(), expected.row(row).used());
                    assert_eq!(core.row(row).wrapped, expected.row(row).wrapped);
                }
            }
        }
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
    fn bulk_osc_preserves_payloads_and_control_boundaries_at_every_split() {
        let input = b"\x1b]2;0123456789abcdef\0\t\r\n\x7f\xe4\xb8\xad\xff\x07\x1b]52;c;YWFhYWFhYWFh\x1b\\\x1b]2;cancelled\x18\x1b]2;also cancelled\x1a\x1b]2;interrupted\x1b[2J\x1b]2;final\x1b\\X";
        for split in 0..=input.len() {
            let (mut core, mut parser) = terminal();
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
    fn synchronized_queries_reply_before_presentation() {
        let (mut core, mut parser) = terminal();
        parser.advance(&mut core, b"\x1b[?2026h");
        assert!(!parser.advance(&mut core, "中Z\x1b[6n\x1b[c".as_bytes()));
        assert_eq!(core.row(0).cells[0].c, '中');
        assert_eq!(core.row(0).cells[2].c, 'Z');
        assert_eq!(replies(&mut core), b"\x1b[1;4R\x1b[?6c");
        assert!(core.mode().contains(Mode::SYNC_UPDATE));
        assert!(parser.advance(&mut core, b"\x1b[?2026l"));
        assert!(parser.deadline().is_none());
    }

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

    #[test]
    fn queued_and_direct_parsing_match_at_every_split() {
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

    #[test]
    fn queued_large_frame_preserves_utf8_and_frame_order() {
        let (mut core, mut parser) = terminal();
        let mut input = b"\x1b[?2026h".to_vec();
        input.extend(std::iter::repeat_n(b'\r', 32 * 1024 - 2));
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
