use std::time::{Duration, Instant};

const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Input {
    Bytes(Vec<u8>),
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
        let output = &mut self.ready;
        if self.pending.is_empty() && !bytes.is_empty() && !bytes.contains(&0x1b) {
            let bytes = bytes.to_vec();
            output.push(if self.paste {
                Input::PasteData(bytes)
            } else {
                Input::Bytes(bytes)
            });
            return output.drain(..);
        }
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let pending = &self.pending[consumed..];
            if self.paste {
                if let Some(end) = pending
                    .windows(PASTE_END.len())
                    .position(|part| part == PASTE_END)
                {
                    if end != 0 {
                        output.push(Input::PasteData(pending[..end].to_vec()));
                    }
                    consumed += end + PASTE_END.len();
                    output.push(Input::PasteEnd);
                    self.paste = false;
                    continue;
                }
                let keep = (1..PASTE_END.len())
                    .rev()
                    .find(|&length| pending.ends_with(&PASTE_END[..length]))
                    .unwrap_or(0);
                let ready = pending.len() - keep;
                if ready != 0 {
                    output.push(Input::PasteData(pending[..ready].to_vec()));
                    consumed += ready;
                }
                break;
            }
            if pending[0] != 0x1b {
                let length = pending
                    .iter()
                    .position(|&byte| byte == 0x1b)
                    .unwrap_or(pending.len());
                output.push(Input::Bytes(pending[..length].to_vec()));
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
                        if pending.len() >= 4096 {
                            output.push(Input::Bytes(pending.to_vec()));
                            consumed += pending.len();
                        }
                        break;
                    };
                    let sequence = &pending[..=end];
                    consumed += end + 1;
                    match sequence {
                        b"\x1b[200~" => {
                            self.paste = true;
                            output.push(Input::PasteStart);
                        }
                        b"\x1b[I" => output.push(Input::Focus(true)),
                        b"\x1b[O" => output.push(Input::Focus(false)),
                        _ => match mouse(sequence) {
                            Some(mouse) => output.push(Input::Mouse(mouse)),
                            None => output.push(Input::Bytes(sequence.to_vec())),
                        },
                    }
                }
                b'O' => {
                    if pending.len() < 3 {
                        break;
                    }
                    output.push(Input::Bytes(pending[..3].to_vec()));
                    consumed += 3;
                }
                byte if byte.is_ascii() => {
                    output.push(Input::Alt(byte));
                    consumed += 2;
                }
                _ => {
                    output.push(Input::Bytes(pending[..2].to_vec()));
                    consumed += 2;
                }
            }
        }
        self.pending.drain(..consumed);
        output.drain(..)
    }

    pub fn expire(&mut self, now: Instant) -> Option<Input> {
        if self.pending == [0x1b]
            && self
                .escape_since
                .is_some_and(|start| now.duration_since(start) >= ESCAPE_WAIT)
        {
            self.pending.clear();
            self.escape_since = None;
            Some(Input::Escape)
        } else {
            None
        }
    }

    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.escape_since
            .map(|start| ESCAPE_WAIT.saturating_sub(now.duration_since(start)))
    }
}

fn mouse(sequence: &[u8]) -> Option<Mouse> {
    let final_byte = *sequence.last()?;
    if !sequence.starts_with(b"\x1b[<") || !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let mut fields = std::str::from_utf8(&sequence[3..sequence.len() - 1])
        .ok()?
        .split(';')
        .map(str::parse::<u16>);
    let event = Mouse {
        code: fields.next()?.ok()?,
        column: fields.next()?.ok()?.checked_sub(1)?,
        row: fields.next()?.ok()?.checked_sub(1)?,
        release: final_byte == b'm',
    };
    fields.next().is_none().then_some(event)
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
            vec![Input::Bytes(b"a\x03\xc3".to_vec())]
        );
        assert!(decoder.timeout(Instant::now()).is_none());
        assert_eq!(
            decoder.feed(b"\xa9", Instant::now()).collect::<Vec<_>>(),
            vec![Input::Bytes(vec![0xa9])]
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
            vec![Input::Bytes(b"\x1b[1;5A".to_vec())]
        );
        assert_eq!(
            decoder.feed(b"\x1bs", now).collect::<Vec<_>>(),
            vec![Input::Alt(b's')]
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
                    Input::Bytes(b"\x1b[A".to_vec()),
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
