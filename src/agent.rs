use serde_json::{Value, json};
use std::{io, ops::Range};

const INPUT_LIMIT: usize = 64 * 1024;

pub(crate) struct Message {
    pub turn: u64,
    pub role: String,
    pub text: String,
    pub id: Option<String>,
}

/// The transcript the owner publishes; `Agent::restore` reads it.
pub(crate) fn snapshot(messages: &[Message], busy: bool, revision: u64) -> Value {
    json!({
        "messages": messages.iter().map(|m| (&m.role, &m.text, &m.id, m.turn)).collect::<Vec<_>>(),
        "busy": busy,
        "revision": revision,
    })
}

/// A frontend's draft and its mirror of the owner's transcript.
#[derive(Default)]
pub(crate) struct Agent {
    pub input: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub busy: bool,
    pub messages: Vec<Message>,
    pub scroll: usize,
    pub revision: u64,
    /// Requests waiting to be sent to the owner.
    pub pending: Vec<Value>,
}

impl Agent {
    pub fn restore(&mut self, mut value: Value) -> io::Result<bool> {
        let messages: Vec<(String, String, Option<String>, u64)> =
            serde_json::from_value(value["messages"].take())?;
        let busy = serde_json::from_value(value["busy"].take())?;
        let revision = serde_json::from_value(value["revision"].take())?;
        let messages: Vec<_> = messages
            .into_iter()
            .map(|(role, text, id, turn)| Message {
                role,
                text,
                id,
                turn,
            })
            .collect();
        let redraw = self.busy != busy
            || self.messages.len() != messages.len()
            || self.messages.iter().zip(&messages).any(|(old, new)| {
                old.turn != new.turn
                    || old.role != new.role
                    || old.id != new.id
                    || old.text != new.text
                        && (new.role != "Agent"
                            || new.id.is_some()
                            || !new.text.starts_with(&old.text))
            });
        self.messages = messages;
        self.busy = busy;
        self.revision = revision;
        Ok(redraw)
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.anchor = None;
    }

    pub fn set_cursor(&mut self, offset: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = offset;
    }

    pub fn insert(&mut self, text: &str) -> Result<(), String> {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        if self.input.len() - range.len() + text.len() > INPUT_LIMIT {
            return Err("Agent input exceeds 64 KiB".into());
        }
        self.cursor = range.start + text.len();
        self.input.replace_range(range, text);
        self.anchor = None;
        Ok(())
    }

    pub fn key(&mut self, symbol: u32, shift: bool) -> bool {
        let before = || {
            self.input[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i)
        };
        let after = || {
            self.cursor
                + self.input[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
        };
        let selection = self.selection();
        let offset = match symbol {
            0xff51 => {
                if !shift && let Some(range) = selection {
                    range.start
                } else {
                    before()
                }
            }
            0xff53 => {
                if !shift && let Some(range) = selection {
                    range.end
                } else {
                    after()
                }
            }
            0xff50 => self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1),
            0xff57 => {
                self.cursor
                    + self.input[self.cursor..]
                        .find('\n')
                        .unwrap_or(self.input.len() - self.cursor)
            }
            0xff08 | 0xffff => {
                let range = selection.unwrap_or_else(|| {
                    if symbol == 0xff08 {
                        before()..self.cursor
                    } else {
                        self.cursor..after()
                    }
                });
                self.cursor = range.start;
                self.input.replace_range(range, "");
                self.anchor = None;
                return true;
            }
            _ => return false,
        };
        self.set_cursor(offset, shift);
        true
    }

    pub fn abort(&mut self) {
        self.pending.push(json!({"action": "abort"}));
    }

    /// Rewinds to the user message at `index`.
    pub fn rewind(&mut self, index: usize) {
        self.pending
            .push(json!({"action": "rewind", "turn": self.messages[index].turn}));
        self.busy = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_keeps_the_next_draft_and_requests_go_to_the_owner() {
        let messages = [Message {
            role: "You".into(),
            text: "first request".into(),
            turn: 3,
            id: None,
        }];
        let mut agent = Agent::default();
        agent.insert("下一条🙂").unwrap();
        agent.anchor = Some(3);
        agent.scroll = 7;
        let cursor = agent.cursor;
        assert!(agent.restore(snapshot(&messages, true, 9)).unwrap());
        assert_eq!(agent.input, "下一条🙂");
        assert_eq!(agent.cursor, cursor);
        assert_eq!(agent.anchor, Some(3));
        assert_eq!(agent.scroll, 7);
        assert_eq!(agent.messages[0].turn, 3);
        assert_eq!(agent.messages[0].text, "first request");
        assert_eq!(agent.revision, 9);
        assert!(agent.busy);
        agent.abort();
        agent.rewind(0);
        assert_eq!(
            agent.pending,
            [
                json!({"action": "abort"}),
                json!({"action": "rewind", "turn": 3})
            ]
        );
    }

    #[test]
    fn selection_replacement_and_deletion_preserve_unicode_boundaries() {
        let mut agent = Agent::default();
        agent.insert("a中文z").unwrap();
        agent.set_cursor(1, false);
        agent.set_cursor(7, true);
        assert_eq!(&agent.input[agent.selection().unwrap()], "中文");
        agent.insert("新").unwrap();
        assert_eq!(agent.input, "a新z");
        assert_eq!(agent.cursor, 4);
        assert_eq!(agent.selection(), None);
        agent.key(0xff08, false);
        assert_eq!(agent.input, "az");
        agent.select_all();
        agent.key(0xffff, false);
        assert!(agent.input.is_empty());
        assert_eq!(agent.cursor, 0);
        assert_eq!(agent.anchor, None);
    }

    #[test]
    fn shift_navigation_extends_from_anchor_and_arrows_collapse_selection() {
        let mut agent = Agent::default();
        agent.insert("ab\n中文").unwrap();
        agent.key(0xff51, true);
        agent.key(0xff51, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(0xff53, false);
        assert_eq!(agent.cursor, 9);
        assert_eq!(agent.selection(), None);
        agent.key(0xff50, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(0xff51, false);
        assert_eq!(agent.cursor, 3);
        agent.key(0xff57, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.clear_input();
        assert!(agent.input.is_empty());
        assert_eq!(agent.anchor, None);
    }

    #[test]
    fn replacing_selected_text_can_reuse_the_input_budget() {
        let mut agent = Agent::default();
        agent.insert(&"x".repeat(INPUT_LIMIT)).unwrap();
        agent.select_all();
        agent.insert("short draft").unwrap();
        assert_eq!(agent.input, "short draft");
    }
}
