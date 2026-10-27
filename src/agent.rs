use crate::harness::{Event, Harness, TerminalRequest};
use crate::workspace::{Action, Client};
use std::sync::atomic::Ordering;
use std::{io, ops::Range, path::PathBuf};

const INPUT_LIMIT: usize = 64 * 1024;
const TRANSCRIPT_LIMIT: usize = 256 * 1024;

#[derive(PartialEq, Eq)]
pub(crate) struct Message {
    pub turn: u64,
    pub role: String,
    pub text: String,
    pub id: Option<String>,
}

#[derive(Default)]
pub(crate) struct Agent {
    pub input: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub busy: bool,
    pub messages: Vec<Message>,
    pub scroll: usize,
    pub revision: u64,
    pub rewound: Option<u64>,
    pub remote: bool,
    pub pending: Vec<serde_json::Value>,
    turn: u64,
    rewind: Option<(u64, String)>,
    runtime: Option<Harness>,
    response: Option<usize>,
    monitors: Vec<(u64, TerminalRequest)>,
}

impl Agent {
    pub fn mirror() -> Self {
        Self {
            remote: true,
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "messages": self.messages.iter().map(|m| (&m.role, &m.text, &m.id, m.turn)).collect::<Vec<_>>(),
            "busy": self.busy,
            "revision": self.revision,
            "turn": self.turn,
            "rewound": self.rewound,
        })
    }

    pub fn restore(&mut self, mut value: serde_json::Value) -> io::Result<Option<usize>> {
        let messages: Vec<(String, String, Option<String>, u64)> = serde_json::from_value(
            value
                .get_mut("messages")
                .map_or(serde_json::Value::Null, serde_json::Value::take),
        )
        .map_err(io::Error::other)?;
        let busy = serde_json::from_value(value["busy"].clone()).map_err(io::Error::other)?;
        let revision =
            serde_json::from_value(value["revision"].clone()).map_err(io::Error::other)?;
        let turn = serde_json::from_value(value["turn"].clone()).map_err(io::Error::other)?;
        let rewound = serde_json::from_value(value["rewound"].clone()).map_err(io::Error::other)?;
        let messages: Vec<_> = messages
            .into_iter()
            .map(|(role, text, id, turn)| Message {
                role,
                text,
                id,
                turn,
            })
            .collect();
        let first_changed = self
            .messages
            .iter()
            .zip(&messages)
            .position(|(old, new)| old != new)
            .or_else(|| {
                (self.messages.len() != messages.len())
                    .then_some(self.messages.len().min(messages.len()))
            });
        self.messages = messages;
        self.busy = busy;
        self.revision = revision;
        self.turn = turn;
        self.rewound = rewound;
        Ok(first_changed)
    }

    pub fn fd(&self) -> i32 {
        self.runtime.as_ref().map_or(-1, Harness::fd)
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

    pub fn is_goal(&self) -> io::Result<bool> {
        let goal = self
            .input
            .trim_start()
            .strip_prefix("/goal")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if goal.is_some_and(|rest| rest.trim().is_empty()) {
            return Err(io::Error::other(
                "Use /goal followed by the task to complete",
            ));
        }
        Ok(goal.is_some())
    }

    pub fn send(
        &mut self,
        prompt: String,
        cwd: Option<PathBuf>,
        config: &crate::config::Config,
        pane: u64,
        remote: bool,
        target: serde_json::Value,
    ) -> io::Result<u64> {
        if self.busy {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Agent is still responding",
            ));
        }
        let goal = self.is_goal()?;
        if self.runtime.is_none() {
            self.runtime = Some(Harness::start(cwd, remote, config, self.turn)?);
        }
        let turn = self
            .runtime
            .as_ref()
            .unwrap()
            .send(prompt, pane, goal, target)?;
        self.turn = turn;
        self.messages.push(Message {
            turn,
            role: "You".into(),
            text: std::mem::take(&mut self.input),
            id: None,
        });
        self.response = None;
        self.clear_input();
        self.scroll = 0;
        self.busy = true;
        self.trim();
        Ok(turn)
    }

    pub fn rewind(&mut self, index: usize) -> io::Result<()> {
        if self.remote {
            if !self
                .messages
                .get(index)
                .is_some_and(|message| message.role == "You")
            {
                return Err(io::Error::other("Select a user message to rewind"));
            }
            self.pending
                .push(serde_json::json!({"action": "rewind", "index": index}));
            self.busy = true;
            return Ok(());
        }
        if self.rewind.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Conversation is already rewinding",
            ));
        }
        let message = self
            .messages
            .get(index)
            .filter(|message| message.role == "You")
            .ok_or_else(|| io::Error::other("Select a user message to rewind"))?;
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            io::Error::other("This conversation is no longer available in model context")
        })?;
        runtime.rewind(message.turn)?;
        self.rewind = Some((message.turn, message.text.clone()));
        self.busy = true;
        self.response = None;
        Ok(())
    }

    pub fn abort(&mut self) -> io::Result<()> {
        if self.remote {
            self.pending.push(serde_json::json!({"action": "abort"}));
            return Ok(());
        }
        if let Some(runtime) = &self.runtime {
            runtime.abort()?;
        }
        Ok(())
    }

    pub fn poll(&mut self, client: &mut Client) -> io::Result<bool> {
        let Some(runtime) = &self.runtime else {
            return Ok(false);
        };
        let events = runtime.poll()?;
        let changed = !events.is_empty();
        for event in events {
            match event {
                Event::Text(text) => {
                    let index = *self.response.get_or_insert_with(|| {
                        self.messages.push(Message {
                            turn: self.turn,
                            role: "Agent".into(),
                            text: String::new(),
                            id: None,
                        });
                        self.messages.len() - 1
                    });
                    self.messages[index].text.push_str(&text);
                }
                Event::Tool {
                    id,
                    name,
                    text,
                    done,
                    is_error,
                } => {
                    self.response = None;
                    let role = format!(
                        "{} · {}",
                        name,
                        if is_error {
                            "error"
                        } else if done {
                            "done"
                        } else {
                            "running"
                        }
                    );
                    if let Some(message) = self
                        .messages
                        .iter_mut()
                        .find(|m| m.id.as_ref() == Some(&id))
                    {
                        if message.text != text {
                            self.revision += 1;
                        }
                        message.role = role;
                        message.text = text;
                    } else {
                        self.messages.push(Message {
                            turn: self.turn,
                            role,
                            text,
                            id: Some(id),
                        });
                    }
                }
                Event::Terminal(request) => {
                    if request.name == "monitor_terminal" {
                        match request.input["block"].as_u64() {
                            Some(block) => self.monitors.push((block, request)),
                            None => self.runtime.as_ref().unwrap().respond(
                                request,
                                Err(io::Error::other(
                                    "Missing shell block ID; inspect command_blocks first",
                                )),
                            )?,
                        }
                        continue;
                    }
                    let result = terminal_request(client, &request);
                    self.runtime.as_ref().unwrap().respond(request, result)?;
                }
                Event::Notice(text) => {
                    self.messages.push(Message {
                        turn: self.turn,
                        role: "Context".into(),
                        text,
                        id: None,
                    });
                }
                Event::Error(error) => {
                    self.messages.push(Message {
                        turn: self.turn,
                        role: "Error".into(),
                        text: error,
                        id: None,
                    });
                }
                Event::Rewound { turn, error } => {
                    let (_, text) = self.rewind.take().unwrap();
                    self.busy = false;
                    self.response = None;
                    if let Some(error) = error {
                        self.messages.push(Message {
                            turn: self.turn,
                            role: "Error".into(),
                            text: error,
                            id: None,
                        });
                    } else {
                        self.messages.retain(|message| message.turn < turn);
                        self.input = text;
                        self.cursor = self.input.len();
                        self.anchor = None;
                        self.scroll = 0;
                        self.revision += 1;
                        self.rewound = Some(turn);
                    }
                }
                Event::Stopped => {
                    self.runtime = None;
                    self.rewind = None;
                    self.busy = false;
                    self.response = None;
                }
                Event::Settled => {
                    self.busy = self.rewind.is_some();
                    self.response = None;
                }
            }
            self.trim();
        }
        self.poll_monitors(client)?;
        Ok(changed)
    }

    pub fn poll_monitors(&mut self, client: &Client) -> io::Result<()> {
        let mut pending = Vec::new();
        for (block, request) in self.monitors.drain(..) {
            let result = if request.canceled.load(Ordering::Acquire) {
                Some(Err(io::ErrorKind::Interrupted.into()))
            } else {
                let marker = client
                    .snapshot()
                    .and_then(|s| {
                        s.panes
                            .iter()
                            .find(|p| Some(p.id) == request.input["pane"].as_u64())
                    })
                    .and_then(|p| p.command_blocks.iter().find(|b| b.id == block));
                match marker {
                    Some(marker) if !marker.has_output => Some(Err(io::Error::other("This block is a prompt, not an executing shell command"))),
                    Some(marker) if marker.status.is_none() => None,
                    Some(marker) => Some(terminal_context(client,request.input["pane"].as_u64().unwrap(), false).map(|mut context| {
                        context["command_finished"]=serde_json::json!({"block":block,"exit_code":marker.status,"source":"OSC133","output_incomplete":!marker.complete});
                        context.to_string()
                    })),
                    None => Some(Err(io::Error::other("Shell block is no longer observable; cannot determine its exit status"))),
                }
            };
            if let Some(result) = result {
                if let Some(runtime) = &self.runtime {
                    runtime.respond(request, result)?;
                }
            } else {
                pending.push((block, request));
            }
        }
        self.monitors = pending;
        Ok(())
    }

    fn trim(&mut self) {
        let mut bytes = self.messages.iter().map(|m| m.text.len()).sum::<usize>();
        while self.messages.len() > 1 && (bytes > TRANSCRIPT_LIMIT || self.messages.len() > 128) {
            // Keep the active objective and streamed reply during long tool loops.
            let Some(index) = self
                .messages
                .iter()
                .enumerate()
                .position(|(index, message)| {
                    self.response != Some(index)
                        && !(message.turn == self.turn && message.role == "You")
                })
            else {
                break;
            };
            let message = self.messages.remove(index);
            self.revision += 1;
            bytes -= message.text.len();
            if let Some(response) = &mut self.response
                && *response > index
            {
                *response -= 1;
            }
        }
        if bytes > TRANSCRIPT_LIMIT {
            let index = self
                .messages
                .iter()
                .position(|m| m.role != "You")
                .unwrap_or(0);
            let message = &mut self.messages[index];
            let mut start = bytes - TRANSCRIPT_LIMIT;
            while !message.text.is_char_boundary(start) {
                start += 1;
            }
            message.text.drain(..start);
            self.revision += 1;
        }
    }
}

pub(crate) fn terminal_context(
    client: &Client,
    pane: u64,
    html: bool,
) -> io::Result<serde_json::Value> {
    let snapshot = client
        .snapshot()
        .ok_or_else(|| io::Error::other("No workspace"))?;
    let view = snapshot
        .panes
        .iter()
        .find(|view| view.id == pane)
        .ok_or_else(|| io::Error::other("Pane no longer exists in this tab"))?;
    let screen = client
        .screens()
        .get(&pane)
        .ok_or_else(|| io::Error::other("No screen for pane"))?;
    let capture = crate::context::snapshot(screen, None);
    Ok(serde_json::json!({
        "target": {
            "workspace": snapshot.name,
            "pane": pane,
            "tab": snapshot.active_tab,
            "connection": if client.is_remote() { "ssh" } else { "local" },
            "ssh_target": client.remote_target(),
            "writable": client.can_agent_input(),
            "title": view.title,
            "cwd": view.cwd,
            "process": {
                "launch_argv": view.process.launch_argv,
                "child_pid": view.process.child_pid,
                "foreground_pgid": view.process.foreground_pgid,
                "foreground_pid": view.process.foreground_pid,
                "foreground_scope": "pty_process_group_leader",
                "foreground_executable": view.process.foreground_executable,
                "foreground_argv": view.process.foreground_argv,
                "foreground_cwd": view.process.foreground_cwd,
                "sampled_at_ms": view.process.sampled_at_ms,
                "unavailable": view.process.unavailable,
            },
            "symbols": {"availability": "unknown"},
        },
        "screen": if html {
            crate::agent_context::html(screen)
        } else {
            crate::agent_context::shape(screen)
        },
        "command_blocks": view.command_blocks.iter().rev().map(|b| serde_json::json!({
            "id":b.id,"has_output":b.has_output,"exit_code":b.status,
            "output_incomplete":!b.complete,
        })).collect::<Vec<_>>(),
        "raw_text": capture.text,
        "incomplete": capture.incomplete,
    }))
}

fn terminal_request(client: &mut Client, request: &TerminalRequest) -> io::Result<String> {
    if request.canceled.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let pane = request.input["pane"]
        .as_u64()
        .ok_or_else(|| io::Error::other("Missing pane ID"))?;
    match request.name.as_str() {
        "read_terminal" => {
            Ok(terminal_context(client, pane, request.input["view"] == "html")?.to_string())
        }
        "send_terminal" => {
            let snapshot = client
                .snapshot()
                .ok_or_else(|| io::Error::other("No workspace"))?;
            snapshot
                .panes
                .iter()
                .find(|p| p.id == pane)
                .ok_or_else(|| io::Error::other("Pane no longer exists in this tab"))?;
            if !client.can_agent_input() {
                return Err(io::Error::other("Workspace is read-only"));
            }
            let text = request.input["text"]
                .as_str()
                .ok_or_else(|| io::Error::other("Missing terminal input text"))?;
            let mut text = text.to_owned();
            if request.input["submit"].as_bool() == Some(true) && !text.ends_with(['\r', '\n']) {
                text.push('\r');
            }
            if text.len() > INPUT_LIMIT {
                return Err(io::Error::other("Terminal input exceeds 64 KiB"));
            }
            client.send(Action::Input {
                pane,
                data: text.as_bytes().to_vec(),
            })?;
            Ok(format!(
                "Queued {} input bytes to pane {pane}; read_terminal to inspect the result",
                text.len()
            ))
        }
        _ => Err(io::Error::other("Unknown terminal tool")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_restores_canonical_messages_without_overwriting_next_draft() {
        let mut owner = Agent::default();
        owner.messages.push(Message {
            role: "You".into(),
            text: "first request".into(),
            turn: 3,
            id: None,
        });
        owner.busy = true;
        owner.turn = 3;
        owner.revision = 9;
        let mut mirror = Agent::mirror();
        mirror.insert("下一条🙂").unwrap();
        mirror.anchor = Some(3);
        mirror.scroll = 7;
        let cursor = mirror.cursor;
        assert_eq!(mirror.restore(owner.snapshot()).unwrap(), Some(0));
        assert_eq!(mirror.input, "下一条🙂");
        assert_eq!(mirror.cursor, cursor);
        assert_eq!(mirror.anchor, Some(3));
        assert_eq!(mirror.scroll, 7);
        assert_eq!(mirror.messages[0].turn, 3);
        assert_eq!(mirror.turn, 3);
        assert_eq!(mirror.revision, 9);
        assert!(mirror.busy);
        mirror.abort().unwrap();
        mirror.rewind(0).unwrap();
        assert_eq!(
            mirror.pending,
            vec![
                serde_json::json!({"action":"abort"}),
                serde_json::json!({"action":"rewind","index":0})
            ]
        );
        assert_eq!(mirror.fd(), -1);
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
    #[test]
    fn long_tool_loops_keep_the_current_user_message_and_reply() {
        let mut agent = Agent {
            turn: 2,
            ..Agent::default()
        };
        agent.messages.push(Message {
            turn: 1,
            role: "You".into(),
            text: "old".into(),
            id: None,
        });
        agent.messages.push(Message {
            turn: 2,
            role: "You".into(),
            text: "/goal inspect the debugger".into(),
            id: None,
        });
        agent.messages.push(Message {
            turn: 2,
            role: "Agent".into(),
            text: "working".into(),
            id: None,
        });
        agent.response = Some(2);
        for index in 0..160 {
            agent.messages.push(Message {
                turn: 2,
                role: "read_terminal".into(),
                text: "x".repeat(4096),
                id: Some(index.to_string()),
            });
            agent.trim();
        }
        assert_eq!(agent.messages[0].text, "/goal inspect the debugger");
        assert_eq!(agent.messages[agent.response.unwrap()].text, "working");
        assert!(agent.messages.iter().map(|m| m.text.len()).sum::<usize>() <= TRANSCRIPT_LIMIT);
        assert!(agent.messages.len() <= 128);
    }

    #[test]
    fn rejected_send_keeps_the_draft_without_starting_a_worker() {
        let config = crate::config::Config::default();
        for (input, diagnostic) in [
            ("/goal  ", "followed by the task"),
            ("keep 中文", "Set model"),
        ] {
            let mut agent = Agent::default();
            agent.insert(input).unwrap();
            let cursor = agent.cursor;
            let error = agent
                .send(
                    input.into(),
                    None,
                    &config,
                    1,
                    false,
                    serde_json::Value::Null,
                )
                .unwrap_err();
            assert!(error.to_string().contains(diagnostic));
            assert_eq!(agent.input, input);
            assert_eq!(agent.cursor, cursor);
            assert!(agent.runtime.is_none());
            assert!(!agent.busy);
        }
    }
}
