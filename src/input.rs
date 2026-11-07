use crate::core::Mode;
use std::borrow::Cow;
pub const SHIFT: u32 = 1;
pub const CONTROL: u32 = 2;
pub const ALT: u32 = 4;
pub const SUPER: u32 = 8;

pub(crate) mod keysym {
    pub const LEFT_TAB: u32 = 0xfe20;
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const INSERT: u32 = 0xff63;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const DELETE: u32 = 0xffff;
    #[cfg(feature = "gui")]
    pub use gui::*;

    #[cfg(feature = "gui")]
    mod gui {
        pub const KP_TAB: u32 = 0xff89;
        pub const KP_F1: u32 = 0xff91;
        pub const KP_F2: u32 = 0xff92;
        pub const KP_F3: u32 = 0xff93;
        pub const KP_F4: u32 = 0xff94;
        pub const KP_HOME: u32 = 0xff95;
        pub const KP_LEFT: u32 = 0xff96;
        pub const KP_UP: u32 = 0xff97;
        pub const KP_RIGHT: u32 = 0xff98;
        pub const KP_DOWN: u32 = 0xff99;
        pub const KP_PAGE_UP: u32 = 0xff9a;
        pub const KP_PAGE_DOWN: u32 = 0xff9b;
        pub const KP_END: u32 = 0xff9c;
        pub const KP_INSERT: u32 = 0xff9e;
        pub const KP_DELETE: u32 = 0xff9f;
        pub const F1: u32 = 0xffbe;
        pub const F2: u32 = 0xffbf;
        pub const F3: u32 = 0xffc0;
        pub const F4: u32 = 0xffc1;
        pub const F5: u32 = 0xffc2;
        pub const F6: u32 = 0xffc3;
        pub const F7: u32 = 0xffc4;
        pub const F8: u32 = 0xffc5;
        pub const F9: u32 = 0xffc6;
        pub const F10: u32 = 0xffc7;
        pub const F11: u32 = 0xffc8;
        pub const F12: u32 = 0xffc9;
    }
}

pub(crate) struct Key<'a> {
    pub symbol: u32,
    pub modifiers: u32,
    pub text: &'a str,
}

impl Key<'_> {
    pub fn control(&self) -> bool {
        self.modifiers & CONTROL != 0
    }
    pub fn shift(&self) -> bool {
        self.modifiers & SHIFT != 0
    }
    pub fn letter(&self) -> Option<char> {
        symbol_char(self.symbol).map(|c| c.to_ascii_lowercase())
    }
    pub fn paste(&self) -> bool {
        self.control() && self.letter() == Some('v')
            || self.shift() && self.symbol == keysym::INSERT
    }
    /// Unhandled printable keys fall back to their composed text.
    pub fn text_result(&self) -> KeyResult {
        if self.text.is_empty() && symbol_char(self.symbol).is_some() && !self.control() {
            KeyResult::Text
        } else {
            KeyResult::Handled
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteTarget {
    Search,
    Monitor,

    Agent(u64),
    Pane {
        pane: crate::workspace::PaneId,
        bracketed: bool,
    },
    Files,
}

pub(crate) enum KeyResult {
    Unhandled,
    Text,
    Handled,
    Copy(String, bool),
    Paste,
}

#[cfg(any(feature = "tui", test))]
#[derive(Default)]
pub(crate) struct TextDecoder {
    pending: Vec<u8>,
}

#[cfg(any(feature = "tui", test))]
impl TextDecoder {
    #[cfg(feature = "tui")]
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn decode<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, str> {
        let mut valid = 0;
        if self.pending.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => return Cow::Borrowed(text),
                Err(error) => valid = error.valid_up_to(),
            }
        }
        self.pending.extend_from_slice(bytes);
        while valid < self.pending.len() {
            match std::str::from_utf8(&self.pending[valid..]) {
                Ok(_) => valid = self.pending.len(),
                Err(error) => {
                    valid += error.valid_up_to();
                    let Some(length) = error.error_len() else {
                        break;
                    };
                    valid += length;
                }
            }
        }
        if valid == 0 {
            return Cow::Owned(String::new());
        }
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        Cow::Owned(text)
    }
}

pub(crate) fn without_controls(text: &str) -> Cow<'_, str> {
    let mut chars = text[crate::width::printable_ascii_prefix(text.as_bytes())..].chars();
    let Some(control) = chars.find(|ch| ch.is_control()) else {
        return Cow::Borrowed(text);
    };
    let start = text.len() - chars.as_str().len() - control.len_utf8();
    let Some(kept) = chars.find(|ch| !ch.is_control()) else {
        return Cow::Borrowed(&text[..start]);
    };
    let mut filtered = String::with_capacity(text.len());
    filtered.push_str(&text[..start]);
    filtered.push(kept);
    filtered.extend(chars.filter(|ch| !ch.is_control()));
    Cow::Owned(filtered)
}

#[cfg(any(feature = "tui", test))]
pub(crate) fn character_key(ch: char) -> (u32, u32) {
    match ch {
        '\0' => (32, CONTROL),
        '\t' => (keysym::TAB, 0),
        '\r' => (keysym::RETURN, 0),
        '\u{7f}' => (keysym::BACKSPACE, 0),
        '\u{1b}' => (keysym::ESCAPE, 0),
        '\u{1}'..='\u{1a}' => (u32::from(ch) + u32::from('a') - 1, CONTROL),
        _ => (u32::from(ch), 0),
    }
}

#[cfg(any(feature = "tui", test))]
pub(crate) fn sequence_key(bytes: &[u8]) -> Option<(u32, u32)> {
    Some(match bytes {
        b"\x1b" => (keysym::ESCAPE, 0),
        b"\x1b[Z" => (keysym::LEFT_TAB, SHIFT),
        b"\x1b[5~" => (keysym::PAGE_UP, 0),
        b"\x1b[6~" => (keysym::PAGE_DOWN, 0),
        b"\x1b[2~" => (keysym::INSERT, 0),
        b"\x1b[13;2u" | b"\x1b[27;2;13~" => (keysym::RETURN, SHIFT),
        b"\x1b[A" | b"\x1bOA" => (keysym::UP, 0),
        b"\x1b[B" | b"\x1bOB" => (keysym::DOWN, 0),
        b"\x1b[C" | b"\x1bOC" => (keysym::RIGHT, 0),
        b"\x1b[D" | b"\x1bOD" => (keysym::LEFT, 0),
        b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" => (keysym::HOME, 0),
        b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" => (keysym::END, 0),
        b"\x1b[3~" => (keysym::DELETE, 0),
        _ => {
            let &[modifier, final_byte] = bytes.strip_prefix(b"\x1b[1;")? else {
                return None;
            };
            let modifiers = u32::from(modifier.checked_sub(b'1')?);
            let modifiers = ((modifiers & 1) * SHIFT)
                | (((modifiers >> 1) & 1) * ALT)
                | (((modifiers >> 2) & 1) * CONTROL);
            let symbol = match final_byte {
                b'A' => keysym::UP,
                b'B' => keysym::DOWN,
                b'C' => keysym::RIGHT,
                b'D' => keysym::LEFT,
                b'H' => keysym::HOME,
                b'F' => keysym::END,
                _ => return None,
            };
            (symbol, modifiers)
        }
    })
}

#[cfg(feature = "gui")]
pub fn key_bytes(
    symbol: u32,
    modifiers: u32,
    text: &str,
    mode: impl FnOnce() -> Mode,
) -> Option<Vec<u8>> {
    if modifiers & SUPER != 0 {
        return None;
    }

    let shift = modifiers & SHIFT != 0;
    let alt = modifiers & ALT != 0;
    let control = modifiers & CONTROL != 0;
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);

    let special = match symbol {
        keysym::UP | keysym::KP_UP => Some((b'A', true)),
        keysym::DOWN | keysym::KP_DOWN => Some((b'B', true)),
        keysym::RIGHT | keysym::KP_RIGHT => Some((b'C', true)),
        keysym::LEFT | keysym::KP_LEFT => Some((b'D', true)),
        keysym::HOME | keysym::KP_HOME => Some((b'H', true)),
        keysym::END | keysym::KP_END => Some((b'F', true)),
        keysym::F1 | keysym::KP_F1 => Some((b'P', false)),
        keysym::F2 | keysym::KP_F2 => Some((b'Q', false)),
        keysym::F3 | keysym::KP_F3 => Some((b'R', false)),
        keysym::F4 | keysym::KP_F4 => Some((b'S', false)),
        _ => None,
    };
    if let Some((final_byte, cursor)) = special {
        return Some(if modifier != 1 {
            vec![0x1b, b'[', b'1', b';', b'0' + modifier, final_byte]
        } else if !cursor || mode().contains(Mode::APP_CURSOR) {
            vec![0x1b, b'O', final_byte]
        } else {
            vec![0x1b, b'[', final_byte]
        });
    }

    let number = match symbol {
        keysym::INSERT | keysym::KP_INSERT => Some(2),
        keysym::DELETE | keysym::KP_DELETE => Some(3),
        keysym::PAGE_UP | keysym::KP_PAGE_UP => Some(5),
        keysym::PAGE_DOWN | keysym::KP_PAGE_DOWN => Some(6),
        keysym::F5 => Some(15),
        keysym::F6 => Some(17),
        keysym::F7 => Some(18),
        keysym::F8 => Some(19),
        keysym::F9 => Some(20),
        keysym::F10 => Some(21),
        keysym::F11 => Some(23),
        keysym::F12 => Some(24),
        _ => None,
    };
    if let Some(number) = number {
        let mut bytes = Vec::with_capacity(7);
        bytes.extend_from_slice(b"\x1b[");
        if number >= 10 {
            bytes.push(b'0' + number / 10);
        }
        bytes.push(b'0' + number % 10);
        if modifier != 1 {
            bytes.extend_from_slice(&[b';', b'0' + modifier]);
        }
        bytes.push(b'~');
        return Some(bytes);
    }

    let mut utf8 = [0; 4];
    let data: &[u8] = match symbol {
        keysym::RETURN | keysym::KP_ENTER => b"\r",
        keysym::BACKSPACE => {
            if control {
                b"\x08"
            } else {
                b"\x7f"
            }
        }
        keysym::ESCAPE => b"\x1b",
        keysym::LEFT_TAB => b"\x1b[Z",
        keysym::TAB | keysym::KP_TAB if shift => b"\x1b[Z",
        keysym::TAB | keysym::KP_TAB => b"\t",
        _ if control => {
            let ch = symbol_char(symbol)?.to_ascii_lowercase();
            utf8[0] = match ch {
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '/' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                ch if ch.is_ascii_alphabetic() => ch as u8 & 0x1f,
                _ => return None,
            };
            &utf8[..1]
        }
        _ if alt => {
            if text.is_empty() {
                symbol_char(symbol)?.encode_utf8(&mut utf8).as_bytes()
            } else {
                text.as_bytes()
            }
        }
        _ => return None,
    };
    let mut bytes = Vec::with_capacity(data.len() + usize::from(alt));
    if alt {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(data);
    Some(bytes)
}

pub fn symbol_char(symbol: u32) -> Option<char> {
    match symbol {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(symbol),
        0x0100_0000..=0x0110_ffff => char::from_u32(symbol & 0x00ff_ffff),
        _ => None,
    }
}

#[cfg(feature = "gui")]
pub fn paste_bytes(text: String, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        if !text.as_bytes().contains(&b'\n') {
            return text.into_bytes();
        }
        let mut data = text.into_bytes();
        let mut previous_cr = false;
        data.retain_mut(|byte| {
            let remove = previous_cr && *byte == b'\n';
            previous_cr = *byte == b'\r';
            if *byte == b'\n' {
                *byte = b'\r';
            }
            !remove
        });
        return data;
    }

    let split = memchr::memchr2(b'\r', 0x1b, text.as_bytes()).unwrap_or(text.len());
    if split == text.len() {
        return bracketed_plain(text.into_bytes());
    }
    let mut data = Vec::with_capacity(text.len() + 12);
    data.extend_from_slice(b"\x1b[200~");
    let text = text.as_bytes();
    let (plain, text) = text.split_at(split);
    data.extend_from_slice(plain);
    let mut start = 0;
    for index in memchr::memchr2_iter(b'\r', 0x1b, text) {
        let byte = text[index];
        if (byte == b'\r' && text.get(index + 1) == Some(&b'\n')) || byte == 0x1b {
            data.extend_from_slice(&text[start..index]);
            start = index + 1;
        }
    }
    data.extend_from_slice(&text[start..]);
    data.extend_from_slice(b"\x1b[201~");
    data
}

#[cfg(feature = "gui")]
// Keep buffer shifting out of the newline-normalization path.
#[inline(never)]
fn bracketed_plain(mut data: Vec<u8>) -> Vec<u8> {
    let length = data.len();
    data.reserve_exact(12);
    data.resize(length + 6, 0);
    data.copy_within(..length, 6);
    data[..6].copy_from_slice(b"\x1b[200~");
    data.extend_from_slice(b"\x1b[201~");
    data
}

pub fn mouse_bytes(code: u16, x: usize, y: usize, release: bool, mode: Mode) -> Option<Vec<u8>> {
    if mode.contains(Mode::SGR_MOUSE) {
        // A u16 code and two 64-bit coordinates need at most 51 bytes.
        let mut bytes = [0; 51];
        let mut start = bytes.len();
        for (mut value, suffix) in [
            (y + 1, if release { b'm' } else { b'M' }),
            (x + 1, b';'),
            (usize::from(code), b';'),
        ] {
            start -= 1;
            bytes[start] = suffix;
            loop {
                start -= 1;
                bytes[start] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
        }
        start -= 3;
        bytes[start..start + 3].copy_from_slice(b"\x1b[<");
        return Some(bytes[start..].to_vec());
    }
    let utf8 = mode.contains(Mode::UTF8_MOUSE);
    let limit = if utf8 { 2015 } else { 223 };
    if x >= limit || y >= limit || code > 223 {
        return None;
    }
    let code = if release { 3 + (code & 28) } else { code };
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&[0x1b, b'[', b'M', 32 + code as u8]);
    for coordinate in [x, y] {
        let encoded = 33 + coordinate;
        if utf8 {
            data.extend_from_slice(
                char::from_u32(encoded as u32)
                    .unwrap()
                    .encode_utf8(&mut [0; 4])
                    .as_bytes(),
            );
        } else {
            data.push(encoded as u8);
        }
    }
    Some(data)
}

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;

    #[test]
    fn paste_normalizes_terminal_newlines_and_protects_bracketed_input() {
        let text = "中文\r\nline\n\r\x1b[201~";
        assert_eq!(
            paste_bytes(text.into(), false),
            "中文\rline\r\r\x1b[201~".as_bytes()
        );
        assert_eq!(
            paste_bytes(text.into(), true),
            "\x1b[200~中文\nline\n\r[201~\x1b[201~".as_bytes()
        );
    }

    #[test]
    fn sgr_mouse_and_legacy_boundaries() {
        assert_eq!(
            mouse_bytes(16, 9, 3, true, Mode::SGR_MOUSE).unwrap(),
            b"\x1b[<16;10;4m"
        );
        assert_eq!(
            mouse_bytes(0, 0, 0, false, Mode::NONE).unwrap(),
            b"\x1b[M !!"
        );
        assert!(mouse_bytes(0, 223, 0, false, Mode::NONE).is_none());
        assert!(mouse_bytes(0, 2015, 0, false, Mode::UTF8_MOUSE).is_none());
        assert_eq!(
            mouse_bytes(0, 2014, 0, false, Mode::UTF8_MOUSE).unwrap(),
            "\x1b[M ߿!".as_bytes()
        );
    }

    fn key(value: &str) -> (u32, u32) {
        let mut modifiers = 0;
        let mut parts = value.split('-').peekable();
        let mut name = value;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                name = part;
                break;
            }
            modifiers |= match part {
                "ctrl" => CONTROL,
                "alt" => ALT,
                "shift" => SHIFT,
                "cmd" => SUPER,
                _ => panic!("{part}"),
            };
        }
        let symbol = match name {
            "up" => keysym::UP,
            "down" => keysym::DOWN,
            "right" => keysym::RIGHT,
            "left" => keysym::LEFT,
            "home" => keysym::HOME,
            "end" => keysym::END,
            "insert" => keysym::INSERT,
            "delete" => keysym::DELETE,
            "pageup" => keysym::PAGE_UP,
            "pagedown" => keysym::PAGE_DOWN,
            "enter" => keysym::RETURN,
            "backspace" => keysym::BACKSPACE,
            "escape" => keysym::ESCAPE,
            "tab" => keysym::TAB,
            "space" => 0x20,
            name if name.starts_with('f') && name.len() > 1 => {
                keysym::F1 - 1 + name[1..].parse::<u32>().unwrap()
            }
            name => name.chars().next().unwrap() as u32,
        };
        (symbol, modifiers)
    }
    fn encode(value: &str, mode: Mode) -> Option<Vec<u8>> {
        let (symbol, modifiers) = key(value);
        key_bytes(symbol, modifiers, "", || mode)
    }

    #[test]
    fn text_and_mode_independent_keys_do_not_read_terminal_state() {
        for value in ["a", "ctrl-c", "ctrl-left", "enter", "f1", "shift-f4"] {
            let (symbol, modifiers) = key(value);
            assert_eq!(
                key_bytes(symbol, modifiers, "", || panic!("unexpected mode read")),
                encode(value, Mode::NONE)
            );
        }
    }
    #[test]
    fn cursor_keys_follow_application_mode_and_xterm_modifiers() {
        assert_eq!(encode("up", Mode::NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode("home", Mode::APP_CURSOR), Some(b"\x1bOH".to_vec()));
        assert_eq!(
            key_bytes(keysym::KP_UP, 0, "", || Mode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode("ctrl-alt-shift-left", Mode::APP_CURSOR),
            Some(b"\x1b[1;8D".to_vec())
        );
        assert_eq!(
            encode("ctrl-delete", Mode::NONE),
            Some(b"\x1b[3;5~".to_vec())
        );
        assert_eq!(encode("f1", Mode::NONE), Some(b"\x1bOP".to_vec()));
        assert_eq!(
            encode("shift-f12", Mode::NONE),
            Some(b"\x1b[24;2~".to_vec())
        );
    }
    #[test]
    fn control_keys_cover_shell_editing_and_terminal_controls() {
        for (key, byte) in [
            ("ctrl-space", 0),
            ("ctrl-a", 1),
            ("ctrl-c", 3),
            ("ctrl-z", 26),
            ("ctrl-[", 27),
            ("ctrl-\\", 28),
            ("ctrl-]", 29),
            ("ctrl-6", 30),
            ("ctrl-/", 31),
            ("ctrl-8", 127),
            ("backspace", 127),
            ("ctrl-backspace", 8),
            ("enter", 13),
        ] {
            assert_eq!(encode(key, Mode::NONE), Some(vec![byte]), "{key}");
        }
        assert_eq!(encode("shift-tab", Mode::NONE), Some(b"\x1b[Z".to_vec()));
        assert_eq!(
            key_bytes(keysym::LEFT_TAB, SHIFT, "", || panic!(
                "unexpected mode read"
            )),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(encode("alt-backspace", Mode::NONE), Some(vec![27, 127]));
        assert_eq!(
            key_bytes(keysym::KP_ENTER, 0, "", || panic!("unexpected mode read")),
            Some(b"\r".to_vec())
        );
        assert_eq!(encode("ctrl-alt-c", Mode::NONE), Some(vec![27, 3]));
    }
    #[test]
    fn alt_uses_actual_unicode_while_plain_text_belongs_to_ime() {
        assert_eq!(
            key_bytes('a' as u32, ALT, "你好", || Mode::NONE),
            Some("\x1b你好".as_bytes().to_vec())
        );
        assert_eq!(encode("a", Mode::NONE), None);
        assert_eq!(encode("shift-a", Mode::NONE), None);
        assert_eq!(encode("cmd-c", Mode::NONE), None);
        assert_eq!(encode("alt-space", Mode::NONE), Some(b"\x1b ".to_vec()));
    }
}
