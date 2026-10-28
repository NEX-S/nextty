use crate::core::Mode;
#[cfg(feature = "gui")]
use crate::native::{ALT, CONTROL, SHIFT, SUPER};

#[cfg(feature = "tui")]
pub(crate) fn editing_key(bytes: &[u8]) -> Option<u32> {
    Some(match bytes {
        b"\x1b[A" | b"\x1bOA" => 0xff52,
        b"\x1b[B" | b"\x1bOB" => 0xff54,
        b"\x1b[C" | b"\x1bOC" => 0xff53,
        b"\x1b[D" | b"\x1bOD" => 0xff51,
        b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" => 0xff50,
        b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" => 0xff57,
        b"\x1b[3~" => 0xffff,
        _ => return None,
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

    let modifier = 1
        + u8::from(modifiers & SHIFT != 0)
        + 2 * u8::from(modifiers & ALT != 0)
        + 4 * u8::from(modifiers & CONTROL != 0);

    let special = match symbol {
        0xff52 | 0xff97 => Some((b'A', true)),
        0xff54 | 0xff99 => Some((b'B', true)),
        0xff53 | 0xff98 => Some((b'C', true)),
        0xff51 | 0xff96 => Some((b'D', true)),
        0xff50 | 0xff95 => Some((b'H', true)),
        0xff57 | 0xff9c => Some((b'F', true)),
        0xffbe | 0xff91 => Some((b'P', false)),
        0xffbf | 0xff92 => Some((b'Q', false)),
        0xffc0 | 0xff93 => Some((b'R', false)),
        0xffc1 | 0xff94 => Some((b'S', false)),
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
        0xff63 | 0xff9e => Some(2),
        0xffff | 0xff9f => Some(3),
        0xff55 | 0xff9a => Some(5),
        0xff56 | 0xff9b => Some(6),
        0xffc2 => Some(15),
        0xffc3 => Some(17),
        0xffc4 => Some(18),
        0xffc5 => Some(19),
        0xffc6 => Some(20),
        0xffc7 => Some(21),
        0xffc8 => Some(23),
        0xffc9 => Some(24),
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
    let control;
    let data: &[u8] = match symbol {
        0xff0d | 0xff8d => b"\r",
        0xff08 => {
            if modifiers & CONTROL != 0 {
                b"\x08"
            } else {
                b"\x7f"
            }
        }
        0xff1b => b"\x1b",
        0xfe20 => b"\x1b[Z",
        0xff09 | 0xff89 if modifiers & SHIFT != 0 => b"\x1b[Z",
        0xff09 | 0xff89 => b"\t",
        _ if modifiers & CONTROL != 0 => {
            let ch = symbol_char(symbol)?.to_ascii_lowercase();
            control = match ch {
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
            std::slice::from_ref(&control)
        }
        _ if modifiers & ALT != 0 => {
            if text.is_empty() {
                symbol_char(symbol)?.encode_utf8(&mut utf8).as_bytes()
            } else {
                text.as_bytes()
            }
        }
        _ => return None,
    };
    let alt = modifiers & ALT != 0;
    let mut bytes = Vec::with_capacity(data.len() + usize::from(alt));
    if alt {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(data);
    Some(bytes)
}

#[cfg(feature = "gui")]
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
    let mut data = Vec::with_capacity(text.len() + 12);
    data.extend_from_slice(b"\x1b[200~");
    let text = text.as_bytes();
    // Control-heavy text stays on the scalar filter to avoid repeated searches.
    const CHUNK: usize = 256;
    let split = text
        .chunks(CHUNK)
        .position(|chunk| chunk.contains(&b'\r') || chunk.contains(&0x1b))
        .map_or(text.len(), |chunk| chunk * CHUNK);
    let (plain, text) = text.split_at(split);
    data.extend_from_slice(plain);
    let mut start = 0;
    for (index, &byte) in text.iter().enumerate() {
        if (byte == b'\r' && text.get(index + 1) == Some(&b'\n')) || byte == 0x1b {
            if start < index {
                data.extend_from_slice(&text[start..index]);
            }
            start = index + 1;
        }
    }
    data.extend_from_slice(&text[start..]);
    data.extend_from_slice(b"\x1b[201~");
    data
}

pub fn mouse_bytes(code: u16, x: usize, y: usize, release: bool, mode: Mode) -> Option<Vec<u8>> {
    if mode.contains(Mode::SGR_MOUSE) {
        use std::fmt::Write;
        // Three-digit flags, a u16 column and an i32 row fit in 24 bytes.
        let mut data = String::with_capacity(24);
        write!(
            data,
            "\x1b[<{code};{};{}{}",
            x + 1,
            y + 1,
            if release { 'm' } else { 'M' }
        )
        .unwrap();
        return Some(data.into_bytes());
    }
    let limit = if mode.contains(Mode::UTF8_MOUSE) {
        2015
    } else {
        223
    };
    if x >= limit || y >= limit || code > 223 {
        return None;
    }
    let code = if release { 3 + (code & 28) } else { code };
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&[0x1b, b'[', b'M', 32 + code as u8]);
    for coordinate in [x, y] {
        if mode.contains(Mode::UTF8_MOUSE) {
            data.extend_from_slice(
                char::from_u32(33 + coordinate as u32)
                    .unwrap()
                    .encode_utf8(&mut [0; 4])
                    .as_bytes(),
            );
        } else {
            data.push(33 + coordinate as u8);
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
            "up" => 0xff52,
            "down" => 0xff54,
            "right" => 0xff53,
            "left" => 0xff51,
            "home" => 0xff50,
            "end" => 0xff57,
            "insert" => 0xff63,
            "delete" => 0xffff,
            "pageup" => 0xff55,
            "pagedown" => 0xff56,
            "enter" => 0xff0d,
            "backspace" => 0xff08,
            "escape" => 0xff1b,
            "tab" => 0xff09,
            "space" => 0x20,
            name if name.starts_with('f') && name.len() > 1 => {
                0xffbd + name[1..].parse::<u32>().unwrap()
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
            key_bytes(0xff97, 0, "", || Mode::APP_CURSOR),
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
            key_bytes(0xfe20, SHIFT, "", || panic!("unexpected mode read")),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(encode("alt-backspace", Mode::NONE), Some(vec![27, 127]));
        assert_eq!(
            key_bytes(0xff8d, 0, "", || panic!("unexpected mode read")),
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
