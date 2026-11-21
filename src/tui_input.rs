use smallvec::SmallVec;
use std::time::{Duration, Instant};

const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Input {
    Bytes(SmallVec<[u8; 8]>),
    Alt(u8),
    Focus(bool),
    Mouse(PointerEvent),
    PasteStart,
    PasteData(Vec<u8>),
    PasteEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerKind {
    Press,
    Release,
    Motion,
    Scroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PointerEvent {
    pub col: u16,
    pub row: u16,
    pub button: u8,
    pub kind: PointerKind,
    pub mods: u32,
    /// The xterm button code, which is also what mouse-reporting applications expect.
    pub code: u16,
}

impl PointerEvent {
    pub fn press(self, button: u8) -> bool {
        self.kind == PointerKind::Press && self.button == button
    }

    /// Wheel travel in lines, positive towards the end.
    pub fn wheel(self) -> isize {
        let lines = crate::ui::WHEEL_LINES as isize;
        if self.button == 0 { -lines } else { lines }
    }
}

#[derive(Default)]
pub(crate) struct Decoder {
    pending: Vec<u8>,
    escape_since: Option<Instant>,
    paste: bool,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Input> {
        let mut output: Vec<_> = self.expire(now).into_iter().collect();
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < input.len() {
            let pending = &input[consumed..];
            if self.paste {
                if let Some(end) = memchr::memmem::find(pending, PASTE_END) {
                    if end != 0 {
                        output.push(Input::PasteData(pending[..end].to_vec()));
                    }
                    consumed += end + PASTE_END.len();
                    output.push(Input::PasteEnd);
                    self.paste = false;
                    continue;
                }
                let keep = (1..PASTE_END.len())
                    .rfind(|&length| pending.ends_with(&PASTE_END[..length]))
                    .unwrap_or(0);
                let ready = pending.len() - keep;
                if ready != 0 {
                    output.push(Input::PasteData(pending[..ready].to_vec()));
                    consumed += ready;
                }
                break;
            }
            if pending[0] != 0x1b {
                let length = memchr::memchr(0x1b, pending).unwrap_or(pending.len());
                output.push(Input::Bytes(SmallVec::from_slice(&pending[..length])));
                consumed += length;
                continue;
            }
            if pending.len() == 1 {
                self.escape_since.get_or_insert(now);
                break;
            }
            self.escape_since = None;
            match pending[1] {
                0x1b => {
                    output.push(Input::Bytes(smallvec::smallvec![0x1b]));
                    consumed += 1;
                }
                b'[' => {
                    let Some(end) = pending[2..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                        .map(|index| index + 2)
                    else {
                        if pending.len() == 2 {
                            self.escape_since = Some(now);
                        }
                        break;
                    };
                    let sequence = &pending[..=end];
                    consumed += end + 1;
                    output.push(match sequence {
                        b"\x1b[200~" => {
                            self.paste = true;
                            Input::PasteStart
                        }
                        b"\x1b[I" => Input::Focus(true),
                        b"\x1b[O" => Input::Focus(false),
                        _ => mouse(sequence).map_or_else(
                            || Input::Bytes(SmallVec::from_slice(sequence)),
                            Input::Mouse,
                        ),
                    });
                }
                b'O' => {
                    if pending.len() < 3 {
                        self.escape_since = Some(now);
                        break;
                    }
                    output.push(Input::Bytes(SmallVec::from_slice(&pending[..3])));
                    consumed += 3;
                }
                byte if byte.is_ascii() => {
                    output.push(Input::Alt(byte));
                    consumed += 2;
                }
                _ => {
                    let length = memchr::memchr(0x1b, &pending[1..])
                        .map_or(pending.len(), |index| index + 1);
                    output.push(Input::Bytes(SmallVec::from_slice(&pending[..length])));
                    consumed += length;
                }
            }
        }
        input.drain(..consumed);
        self.pending = input;
        output
    }

    pub fn expire(&mut self, now: Instant) -> Option<Input> {
        self.escape_since
            .take_if(|start| now.duration_since(*start) >= ESCAPE_WAIT)?;
        let event = match self.pending.as_slice() {
            [0x1b, byte] => Input::Alt(*byte),
            _ => Input::Bytes(smallvec::smallvec![0x1b]),
        };
        self.pending.clear();
        Some(event)
    }

    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.escape_since
            .map(|start| ESCAPE_WAIT.saturating_sub(now.duration_since(start)))
    }
}

fn mouse(sequence: &[u8]) -> Option<PointerEvent> {
    let (&final_byte, body) = sequence.strip_prefix(b"\x1b[<")?.split_last()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let (code, coordinates) = std::str::from_utf8(body).ok()?.split_once(';')?;
    let (column, row) = coordinates.split_once(';')?;
    let code: u16 = code.parse().ok()?;
    let kind = if code & 64 != 0 {
        PointerKind::Scroll
    } else if final_byte == b'm' {
        PointerKind::Release
    } else if code & 32 != 0 {
        PointerKind::Motion
    } else {
        PointerKind::Press
    };
    let mods = (u32::from(code & 4 != 0) * crate::input::SHIFT)
        | (u32::from(code & 8 != 0) * crate::input::ALT)
        | (u32::from(code & 16 != 0) * crate::input::CONTROL);
    Some(PointerEvent {
        col: column.parse::<u16>().ok()?.checked_sub(1)?,
        row: row.parse::<u16>().ok()?.checked_sub(1)?,
        button: (code & 3) as u8,
        kind,
        mods,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(bytes: &[u8]) -> Input {
        Input::Bytes(SmallVec::from_slice(bytes))
    }

    #[test]
    fn only_a_lone_escape_expires_and_split_csi_is_preserved() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"\x1b\x1b", now), [bytes(b"\x1b")]);
        assert_eq!(decoder.expire(now + ESCAPE_WAIT), Some(bytes(b"\x1b")));
        assert!(decoder.feed(b"\x1b", now).is_empty());
        assert!(decoder.expire(now + Duration::from_millis(24)).is_none());
        assert_eq!(decoder.expire(now + ESCAPE_WAIT), Some(bytes(b"\x1b")));
        assert!(decoder.feed(b"\x1b[1;", now).is_empty());
        assert!(decoder.expire(now + Duration::from_secs(1)).is_none());
        assert_eq!(decoder.feed(b"5A", now), [bytes(b"\x1b[1;5A")]);
        assert_eq!(decoder.feed(b"\x1bs", now), [Input::Alt(b's')]);
    }

    #[test]
    fn bare_csi_and_ss3_expire_without_consuming_the_next_key() {
        let now = Instant::now();
        for prefix in *b"[O" {
            let mut decoder = Decoder::default();
            assert!(decoder.feed(&[0x1b, prefix], now).is_empty());
            assert_eq!(decoder.timeout(now), Some(ESCAPE_WAIT));
            assert_eq!(
                decoder.feed(b"x", now + ESCAPE_WAIT),
                [Input::Alt(prefix), bytes(b"x")]
            );
            assert!(decoder.timeout(now + ESCAPE_WAIT).is_none());
        }
    }

    #[test]
    fn alt_unicode_does_not_split_a_following_ascii_key_from_its_utf8_prefix() {
        let mut decoder = Decoder::default();
        let input = "\x1béx".as_bytes();
        assert_eq!(decoder.feed(input, Instant::now()), [bytes(input)]);
    }

    #[test]
    fn bracketed_paste_preserves_all_bytes_and_never_triggers_shortcuts() {
        let payload = "中\x1bf\x1bq\n文\x1b[201x".as_bytes();
        let mut input = b"\x1b[200~".to_vec();
        input.extend_from_slice(payload);
        input.extend_from_slice(b"\x1b[201~");
        for cut in 0..=input.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.feed(&input[..cut], Instant::now());
            events.extend(decoder.feed(&input[cut..], Instant::now()));
            assert_eq!(events.first(), Some(&Input::PasteStart));
            assert_eq!(events.last(), Some(&Input::PasteEnd));
            let actual: Vec<_> = events
                .into_iter()
                .flat_map(|event| match event {
                    Input::PasteData(bytes) => bytes,
                    Input::PasteStart | Input::PasteEnd => Vec::new(),
                    event => panic!("unexpected paste event {event:?}"),
                })
                .collect();
            assert_eq!(actual, payload, "split at {cut}");
        }
    }

    #[test]
    fn focus_and_mouse_sequences_survive_every_split() {
        let input = b"\x1b[I\x1b[<20;41;9m\x1b[O";
        let release = PointerEvent {
            col: 40,
            row: 8,
            button: 0,
            kind: PointerKind::Release,
            mods: crate::input::SHIFT | crate::input::CONTROL,
            code: 20,
        };
        for cut in 0..=input.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.feed(&input[..cut], Instant::now());
            events.extend(decoder.feed(&input[cut..], Instant::now()));
            assert_eq!(
                events,
                [
                    Input::Focus(true),
                    Input::Mouse(release),
                    Input::Focus(false)
                ]
            );
        }
    }
}
