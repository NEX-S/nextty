use crate::input::PointerKind;
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
pub(crate) struct PointerEvent {
    pub column: u16,
    pub row: u16,
    pub kind: PointerKind,
    pub modifiers: u32,
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
                    .unwrap_or_else(|| partial_utf8(pending));
                let ready = pending.len() - keep;
                if ready != 0 {
                    output.push(Input::PasteData(pending[..ready].to_vec()));
                    consumed += ready;
                }
                break;
            }
            let waiting = self.escape_since.take();
            let (event, length) = match pending {
                [0x1b] => {
                    self.escape_since = Some(waiting.unwrap_or(now));
                    break;
                }
                // A bare introducer may still be Alt with that key.
                [0x1b, b'[' | b'O'] => {
                    self.escape_since = Some(now);
                    break;
                }
                [0x1b, 0x1b, ..] => (Input::Bytes(smallvec::smallvec![0x1b]), 1),
                [0x1b, b'[', rest @ ..] => {
                    let Some(end) = rest.iter().position(|byte| (0x40..=0x7e).contains(byte))
                    else {
                        break;
                    };
                    let sequence = &pending[..end + 3];
                    let event = match sequence {
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
                    };
                    (event, sequence.len())
                }
                [0x1b, b'O', ..] => (Input::Bytes(SmallVec::from_slice(&pending[..3])), 3),
                [0x1b, byte, ..] if byte.is_ascii() => (Input::Alt(*byte), 2),
                _ => {
                    // Text runs to the next escape in whole characters; Alt sends a non-ASCII
                    // character as ESC and the character.
                    let start = usize::from(pending[0] == 0x1b);
                    let length = memchr::memchr(0x1b, &pending[start..]).map_or_else(
                        || pending.len() - partial_utf8(pending),
                        |index| start + index,
                    );
                    if length == start {
                        break;
                    }
                    let text = SmallVec::from_slice(&pending[..length]);
                    (Input::Bytes(text), length)
                }
            };
            output.push(event);
            consumed += length;
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

/// Trailing bytes that begin a UTF-8 character the next read completes; they wait so text
/// always arrives in whole characters.
fn partial_utf8(bytes: &[u8]) -> usize {
    (bytes.len().saturating_sub(3)..bytes.len())
        .find(|&start| {
            std::str::from_utf8(&bytes[start..])
                .is_err_and(|error| error.valid_up_to() == 0 && error.error_len().is_none())
        })
        .map_or(0, |start| bytes.len() - start)
}

fn mouse(sequence: &[u8]) -> Option<PointerEvent> {
    let (&final_byte, body) = sequence.strip_prefix(b"\x1b[<")?.split_last()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let (code, coordinates) = std::str::from_utf8(body).ok()?.split_once(';')?;
    let (column, row) = coordinates.split_once(';')?;
    let code: u16 = code.parse().ok()?;
    let kind = match (code & !28, final_byte) {
        (64, _) => PointerKind::Wheel(1),
        (65, _) => PointerKind::Wheel(-1),
        (button @ 0..=2, b'M') => PointerKind::Press(button as u8),
        (button @ 0..=2, _) => PointerKind::Release(button as u8),
        // Motion, and the buttons and wheels nextty does not use.
        _ => PointerKind::Motion,
    };
    let modifiers = (u32::from(code & 4 != 0) * crate::input::SHIFT)
        | (u32::from(code & 8 != 0) * crate::input::ALT)
        | (u32::from(code & 16 != 0) * crate::input::CONTROL);
    Some(PointerEvent {
        column: column.parse::<u16>().ok()?.checked_sub(1)?,
        row: row.parse::<u16>().ok()?.checked_sub(1)?,
        kind,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(bytes: &[u8]) -> Input {
        Input::Bytes(SmallVec::from_slice(bytes))
    }

    /// The events of `input` read in two parts, for every place the reads can split it.
    fn split_everywhere(input: &[u8]) -> impl Iterator<Item = Vec<Input>> + '_ {
        (0..=input.len()).map(|cut| {
            let mut decoder = Decoder::default();
            let mut events = decoder.feed(&input[..cut], Instant::now());
            events.extend(decoder.feed(&input[cut..], Instant::now()));
            events
        })
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
    fn text_arrives_in_whole_characters_and_alt_unicode_keeps_its_prefix() {
        for text in ["中", "\x1bé"] {
            for events in split_everywhere(text.as_bytes()) {
                assert_eq!(events, [bytes(text.as_bytes())]);
            }
        }
        let input = "\x1béx".as_bytes();
        assert_eq!(
            Decoder::default().feed(input, Instant::now()),
            [bytes(input)]
        );
    }

    #[test]
    fn bracketed_paste_preserves_all_bytes_and_never_triggers_shortcuts() {
        let payload = "中\x1bf\x1bq\n文\x1b[201x".as_bytes();
        let input = [&b"\x1b[200~"[..], payload, b"\x1b[201~"].concat();
        for (cut, events) in split_everywhere(&input).enumerate() {
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
        let release = PointerEvent {
            column: 40,
            row: 8,
            kind: PointerKind::Release(0),
            modifiers: crate::input::SHIFT | crate::input::CONTROL,
        };
        for events in split_everywhere(b"\x1b[I\x1b[<20;41;9m\x1b[O") {
            let expected = [
                Input::Focus(true),
                Input::Mouse(release),
                Input::Focus(false),
            ];
            assert_eq!(events, expected);
        }
    }
}
