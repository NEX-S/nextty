use smallvec::SmallVec;
use std::time::{Duration, Instant};

const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Input {
    Bytes(SmallVec<[u8; 8]>),
    Alt(u8),
    Escape,
    Focus(bool),
    Mouse(Mouse),
    PasteStart,
    PasteData(Vec<u8>),
    PasteEnd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Mouse {
    pub code: u16,
    pub column: u16,
    pub row: u16,
    pub release: bool,
}

#[derive(Default)]
pub(crate) struct Decoder {
    pending: Vec<u8>,
    ready: Vec<Input>,
    escape_since: Option<Instant>,
    paste: bool,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> std::vec::Drain<'_, Input> {
        if self.pending.is_empty() && !bytes.is_empty() && !bytes.contains(&0x1b) {
            let output = &mut self.ready;
            output.push(if self.paste {
                Input::PasteData(bytes.to_vec())
            } else {
                Input::Bytes(SmallVec::from_slice(bytes))
            });
            return output.drain(..);
        }
        if let Some(event) = self.expire(now) {
            self.ready.push(event);
        }
        let output = &mut self.ready;
        let input = if self.pending.is_empty() {
            bytes
        } else {
            self.pending.extend_from_slice(bytes);
            &self.pending
        };
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
                    output.push(Input::Escape);
                    consumed += 1;
                }
                b'[' => {
                    let Some(end) = pending[2..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                        .map(|index| index + 2)
                    else {
                        if pending.len() == 2 {
                            self.escape_since.get_or_insert(now);
                        }
                        if pending.len() >= 4096 {
                            output.push(Input::Bytes(SmallVec::from_slice(pending)));
                            consumed += pending.len();
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
                        self.escape_since.get_or_insert(now);
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
        if self.pending.is_empty() {
            self.pending.extend_from_slice(&bytes[consumed..]);
        } else {
            self.pending.drain(..consumed);
        }
        output.drain(..)
    }

    pub fn expire(&mut self, now: Instant) -> Option<Input> {
        self.escape_since
            .take_if(|start| now.duration_since(*start) >= ESCAPE_WAIT)?;
        let event = match self.pending.as_slice() {
            [0x1b, byte] => Input::Alt(*byte),
            _ => Input::Escape,
        };
        self.pending.clear();
        Some(event)
    }

    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.escape_since
            .map(|start| ESCAPE_WAIT.saturating_sub(now.duration_since(start)))
    }
}

fn mouse(sequence: &[u8]) -> Option<Mouse> {
    let (&final_byte, body) = sequence.strip_prefix(b"\x1b[<")?.split_last()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let (code, coordinates) = std::str::from_utf8(body).ok()?.split_once(';')?;
    let (column, row) = coordinates.split_once(';')?;
    Some(Mouse {
        code: code.parse().ok()?,
        column: column.parse::<u16>().ok()?.checked_sub(1)?,
        row: row.parse::<u16>().ok()?.checked_sub(1)?,
        release: final_byte == b'm',
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_escapes_remain_separate_keys() {
        let mut decoder = Decoder::default();
        let now = Instant::now();
        assert_eq!(
            decoder.feed(b"\x1b\x1b", now).collect::<Vec<_>>(),
            vec![Input::Escape]
        );
        assert_eq!(decoder.expire(now + ESCAPE_WAIT), Some(Input::Escape));
    }

    #[test]
    fn ordinary_input_has_no_escape_timeout() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder
                .feed(b"a\x03\xc3", Instant::now())
                .collect::<Vec<_>>(),
            vec![Input::Bytes(b"a\x03\xc3".to_vec().into())]
        );
        assert!(decoder.timeout(Instant::now()).is_none());
        assert_eq!(
            decoder.feed(b"\xa9", Instant::now()).collect::<Vec<_>>(),
            vec![Input::Bytes(vec![0xa9].into())]
        );
    }

    #[test]
    fn only_a_lone_escape_expires_and_split_csi_is_preserved() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        assert!(decoder.feed(b"\x1b", now).next().is_none());
        assert!(decoder.expire(now + Duration::from_millis(24)).is_none());
        assert_eq!(decoder.expire(now + ESCAPE_WAIT), Some(Input::Escape));
        assert!(decoder.feed(b"\x1b[1;", now).next().is_none());
        assert!(decoder.expire(now + Duration::from_secs(1)).is_none());
        assert_eq!(
            decoder.feed(b"5A", now).collect::<Vec<_>>(),
            vec![Input::Bytes(b"\x1b[1;5A".to_vec().into())]
        );
        assert_eq!(
            decoder.feed(b"\x1bs", now).collect::<Vec<_>>(),
            vec![Input::Alt(b's')]
        );
    }

    #[test]
    fn bare_csi_and_ss3_expire_without_consuming_the_next_key() {
        let now = Instant::now();
        for prefix in *b"[O" {
            let mut decoder = Decoder::default();
            assert!(decoder.feed(&[0x1b, prefix], now).next().is_none());
            assert_eq!(decoder.timeout(now), Some(ESCAPE_WAIT));
            assert_eq!(
                decoder.feed(b"x", now + ESCAPE_WAIT).collect::<Vec<_>>(),
                vec![Input::Alt(prefix), Input::Bytes(b"x".to_vec().into())]
            );
            assert!(decoder.timeout(now + ESCAPE_WAIT).is_none());
        }
    }

    #[test]
    fn alt_unicode_does_not_split_a_following_ascii_key_from_its_utf8_prefix() {
        let mut decoder = Decoder::default();
        let bytes = "\x1béx".as_bytes();
        assert_eq!(
            decoder.feed(bytes, Instant::now()).collect::<Vec<_>>(),
            vec![Input::Bytes(bytes.to_vec().into())]
        );
    }

    #[test]
    fn bracketed_paste_preserves_all_bytes_and_never_triggers_shortcuts() {
        let payload = "中\x1bf\x1bq\n文\x1b[201x".as_bytes();
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"\x1b[201~");
        for cut in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder
                .feed(&bytes[..cut], Instant::now())
                .collect::<Vec<_>>();
            events.extend(decoder.feed(&bytes[cut..], Instant::now()));
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
    fn a_large_coalesced_burst_keeps_event_order_and_a_fragmented_tail() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        let mut burst = b"\x1b[A\x1b[I\x1bs".repeat(4096);
        burst.extend_from_slice(b"\x1b[<20;41;");
        let events = decoder.feed(&burst, now).collect::<Vec<_>>();
        assert_eq!(events.len(), 4096 * 3);
        for events in events.chunks_exact(3) {
            assert_eq!(
                events,
                [
                    Input::Bytes(b"\x1b[A".to_vec().into()),
                    Input::Focus(true),
                    Input::Alt(b's')
                ]
            );
        }
        assert_eq!(
            decoder.feed(b"9m", now).collect::<Vec<_>>(),
            [Input::Mouse(Mouse {
                code: 20,
                column: 40,
                row: 8,
                release: true
            })]
        );
    }

    #[test]
    fn focus_and_mouse_sequences_survive_every_split() {
        let bytes = b"\x1b[I\x1b[<20;41;9m\x1b[O";
        for cut in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder
                .feed(&bytes[..cut], Instant::now())
                .collect::<Vec<_>>();
            events.extend(decoder.feed(&bytes[cut..], Instant::now()));
            assert_eq!(
                events,
                vec![
                    Input::Focus(true),
                    Input::Mouse(Mouse {
                        code: 20,
                        column: 40,
                        row: 8,
                        release: true,
                    }),
                    Input::Focus(false),
                ]
            );
        }
    }
}
