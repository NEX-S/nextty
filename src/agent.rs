use crate::harness::{Event, Harness, MAX_READ_WAIT, TerminalRequest};
use crate::workspace::{Action, Client, ClientEvent};
use serde_json::{Value, json};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{io, ops::Range};

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
    rewind: Option<String>,
    runtime: Option<Harness>,
    response: Option<usize>,
    operation: Option<Operation>,
    observed_input: Option<(u64, u64)>,
    observed_activity: Option<(u64, u64)>,
}

impl Agent {
    pub fn mirror() -> Self {
        Self {
            remote: true,
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let mut snapshot = serde_json::json!({
            "busy": self.busy,
            "revision": self.revision,
            "turn": self.turn,
        });
        snapshot["messages"] = serde_json::Value::Array(
            self.messages
                .iter()
                .map(|m| serde_json::json!((&m.role, &m.text, &m.id, m.turn)))
                .collect(),
        );
        snapshot
    }

    pub fn restore(&mut self, mut value: serde_json::Value) -> io::Result<bool> {
        let messages: Vec<(String, String, Option<String>, u64)> =
            serde_json::from_value(value["messages"].take())?;
        let busy = serde_json::from_value(value["busy"].take())?;
        let revision = serde_json::from_value(value["revision"].take())?;
        let turn = serde_json::from_value(value["turn"].take())?;
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
        self.turn = turn;
        Ok(redraw)
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

    pub fn send(
        &mut self,
        prompt: String,
        config: &crate::config::Config,
        pane: u64,
        target: serde_json::Value,
        activity: u64,
    ) -> io::Result<u64> {
        if self.busy {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Agent is still responding",
            ));
        }
        if self.runtime.is_none() {
            self.runtime = Some(Harness::start(config, self.turn)?);
        }
        let generation = target["input_generation"].as_u64();
        let turn = self.runtime.as_ref().unwrap().send(prompt, pane, target)?;
        self.turn = turn;
        self.observed_input = generation.map(|generation| (pane, generation));
        self.observed_activity = Some((pane, activity));
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
        self.rewind = Some(message.text.clone());
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

    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        let history = if let Some(operation) = self.operation.take() {
            let (command, history) = match operation.stage {
                Stage::Command { command, .. } => (Some(command), None),
                Stage::Read { id, command, .. } => (command, Some(id)),
                _ => (None, None),
            };
            let result = match command {
                Some(command) if operation.request.name == "shell" => {
                    Ok(json!({"input_queued":true,"command":command}).to_string())
                }
                _ => Err(io::ErrorKind::Interrupted.into()),
            };
            // Queue known execution facts before abort publishes cancellation and wakes the worker.
            let _ = operation.request.reply.send(result);
            history
        } else {
            None
        };
        self.abort()?;
        if let Some(request) = history {
            client.send(Action::CancelHistory { request })?;
        }
        Ok(())
    }

    pub fn poll(&mut self, client: &mut Client) -> io::Result<bool> {
        self.advance(client)?;
        let Some(runtime) = &self.runtime else {
            return Ok(false);
        };
        let events = runtime.poll()?;
        let changed = events
            .iter()
            .any(|event| !matches!(event, Event::Terminal(_)));
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
                Event::Terminal(request) => self.begin(request, client)?,
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
                    let text = self.rewind.take().unwrap();
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
        self.advance(client)?;
        Ok(changed)
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

pub(crate) fn terminal_context(client: &Client, pane: u64) -> io::Result<Value> {
    let mut context = terminal_observation(client, pane, None)?;
    let snapshot = client.snapshot().unwrap();
    let view = snapshot.panes.iter().find(|view| view.id == pane).unwrap();
    context["target"] = json!({
        "workspace": snapshot.name,
        "pane": pane,
        "tab": snapshot.active_tab,
        "connection": if client.is_remote() { "ssh" } else { "local" },
        "ssh_target": client.remote_target(),
        "writable": client.can_agent_input(),
        "title": view.title,
        "cwd": view.cwd,
        "launch_argv": view.process.launch_argv,
        "child_pid": view.process.child_pid,
    });
    Ok(context)
}

fn terminal_observation(client: &Client, pane: u64, query: Option<&str>) -> io::Result<Value> {
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
    let mut observation = json!({
        "pane": pane,
        "process": {
            "foreground_pgid": view.process.foreground_pgid,
            "foreground_pid": view.process.foreground_pid,
            "foreground_scope": "pty_process_group_leader",
            "foreground_executable": view.process.foreground_executable,
            "foreground_argv": view.process.foreground_argv,
            "foreground_cwd": view.process.foreground_cwd,
            "sampled_at_ms": view.process.sampled_at_ms,
            "unavailable": view.process.unavailable,
        },
        "history_rows": screen.history,
        "input_generation": view.input_generation,
        "shell_prompt": view.shell_prompt,
    });
    observation["screen"] = crate::agent_context::shape(screen, query);
    observation["command_blocks"] = Value::Array(
        view.command_blocks
            .iter()
            .rev()
            .map(|b| {
                json!({
                    "id":b.id,"has_output":b.has_output,"exit_code":b.status,
                    "output_incomplete":!b.complete,
                })
            })
            .collect(),
    );
    Ok(observation)
}

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

struct Operation {
    request: TerminalRequest,
    stage: Stage,
}

enum Stage {
    Input {
        id: u64,
        generation: u64,
    },
    Command {
        command: u64,
        until: Instant,
    },
    Read {
        id: u64,
        command: Option<u64>,
        before: u64,
        outcome: &'static str,
    },
    Change {
        until: Instant,
    },
}

impl Agent {
    pub fn deadline(&self) -> Option<Instant> {
        self.operation
            .as_ref()
            .and_then(|operation| match operation.stage {
                Stage::Command { until, .. } | Stage::Change { until, .. } => Some(until),
                _ => None,
            })
    }

    fn begin(&mut self, request: TerminalRequest, client: &mut Client) -> io::Result<()> {
        let result = (|| {
            if request.canceled.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let pane = request.input["pane"].as_u64().unwrap();
            match request.name.as_str() {
                "shell" | "input" => {
                    let shell = request.name == "shell";
                    let view = client
                        .snapshot()
                        .and_then(|snapshot| snapshot.panes.iter().find(|view| view.id == pane))
                        .ok_or_else(|| io::Error::other("Target terminal no longer exists"))?;
                    let field = if shell { "command" } else { "text" };
                    let text = request.input[field]
                        .as_str()
                        .ok_or_else(|| io::Error::other(format!("Missing string {field}")))?;
                    let prompt = if shell {
                        Some(view.shell_prompt.ok_or_else(|| io::Error::other("The terminal is not at an untouched integrated shell prompt; read its state and use input for the current program"))?)
                    } else {
                        None
                    };
                    let mut data = text.as_bytes().to_vec();
                    if shell {
                        if client.screens().get(&pane).is_some_and(|screen| {
                            screen.mode.contains(crate::core::Mode::BRACKETED_PASTE)
                        }) {
                            data = [
                                b"\x1b[200~".as_slice(),
                                data.as_slice(),
                                b"\x1b[201~\r".as_slice(),
                            ]
                            .concat();
                        } else {
                            if text.chars().any(char::is_control) {
                                return Err(io::Error::other(
                                    "Shell editing controls require bracketed paste; use a single printable command",
                                ));
                            }
                            data.push(b'\r');
                        }
                    }
                    let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                    let generation = self
                        .observed_input
                        .filter(|(target, _)| *target == pane)
                        .map(|(_, generation)| generation)
                        .ok_or_else(|| {
                            io::Error::other("Read the current terminal state before sending input")
                        })?;
                    client.send(Action::AgentInput {
                        pane,
                        request: id,
                        generation,
                        prompt,
                        data,
                    })?;
                    Ok(Stage::Input { id, generation })
                }
                "read" => {
                    let source = request.input["source"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("Missing read source"))?;
                    let command = match source {
                        "command" => Some(
                            request.input["command"]
                                .as_u64()
                                .ok_or_else(|| io::Error::other("Missing command ID"))?,
                        ),
                        "screen" | "history" => {
                            if request.input.get("command").is_some() {
                                return Err(io::Error::other(
                                    "command is only available for source command",
                                ));
                            }
                            if source == "screen" && request.input.get("cursor").is_some() {
                                return Err(io::Error::other(
                                    "cursor is only available for history or command output",
                                ));
                            }
                            None
                        }
                        _ => return Err(io::Error::other("Unknown terminal read source")),
                    };
                    let seconds = request
                        .input
                        .get("wait")
                        .map(|value| serde_json::from_value::<u64>(value.clone()))
                        .transpose()?
                        .unwrap_or(0);
                    if seconds > MAX_READ_WAIT {
                        return Err(io::Error::other(format!(
                            "read wait exceeds {MAX_READ_WAIT} seconds"
                        )));
                    }
                    let wait = Duration::from_secs(seconds);
                    if !wait.is_zero()
                        && let Some(command) = command
                    {
                        Ok(Stage::Command {
                            command,
                            until: Instant::now() + wait,
                        })
                    } else if !wait.is_zero() || source == "screen" {
                        Ok(Stage::Change {
                            until: Instant::now() + wait,
                        })
                    } else {
                        read_output(client, &request, command, "current")
                    }
                }
                _ => Err(io::Error::other("Unknown terminal tool")),
            }
        })();
        match result {
            Ok(stage) => self.operation = Some(Operation { request, stage }),
            Err(error) => request.respond(Err(error))?,
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: &mut ClientEvent, client: &Client) -> io::Result<()> {
        let id = match event {
            ClientEvent::AgentInput { request, .. } | ClientEvent::History { request, .. } => {
                *request
            }
            _ => return Ok(()),
        };
        if !self.operation.as_ref().is_some_and(|operation| matches!(operation.stage, Stage::Input {id:pending,..} | Stage::Read {id:pending,..} if pending == id)) {
            return Ok(());
        }
        let mut operation = self.operation.take().unwrap();
        let event = std::mem::replace(event, ClientEvent::Updated);
        match event {
            ClientEvent::AgentInput { result, .. } => {
                let Stage::Input { generation, .. } = operation.stage else {
                    unreachable!()
                };
                if result.is_ok() {
                    self.observed_input = Some((
                        operation.request.input["pane"].as_u64().unwrap(),
                        generation + 1,
                    ));
                }
                match result {
                    Ok(Some(command)) if !operation.request.canceled.load(Ordering::Acquire) => {
                        operation.stage = Stage::Command {
                            command,
                            until: Instant::now() + Duration::from_secs(1),
                        };
                        self.operation = Some(operation);
                    }
                    Ok(command) => {
                        let pane = operation.request.input["pane"].as_u64().unwrap();
                        operation
                            .request
                            .respond(terminal_observation(client, pane, None).map(
                                |mut context| {
                                    self.observed_activity =
                                        Some((pane, client.terminal_revision(pane).unwrap()));
                                    context["input_queued"] = json!(true);
                                    if let Some(command) = command {
                                        context["command"] = json!(command);
                                    }
                                    context.to_string()
                                },
                            ))?;
                    }
                    Err(error) => operation.request.respond(Err(io::Error::other(error)))?,
                }
            }
            ClientEvent::History { result, .. } => {
                let Stage::Read {
                    before, outcome, ..
                } = operation.stage
                else {
                    unreachable!()
                };
                if result.is_ok() {
                    self.observed_activity =
                        Some((operation.request.input["pane"].as_u64().unwrap(), before));
                }
                let result = result.map_err(io::Error::other).and_then(|page| {
                    let marker = page.command.as_ref();
                    let outcome = if outcome == "timeout" && marker.is_some_and(|marker| marker.status.is_some()) {
                        "completed"
                    } else {
                        outcome
                    };
                    let mut result = json!({
                        "outcome": outcome,
                        "incomplete": page.incomplete,
                        "cursor": {"epoch":page.cursor.epoch,"row":page.cursor.row,"offset":page.cursor.offset},
                        "has_more": page.has_more,
                    });
                    result["text"] = Value::String(page.text);
                    if let Some(matched) = page.matched {
                        result["matched"] = json!(matched);
                    }
                    if let Some(marker) = marker {
                        result["command"] = json!(marker.id);
                        result["exit_code"] = json!(marker.status);
                        result["running"] = json!(marker.status.is_none());
                    }
                    if outcome == "timeout" && marker.is_some() {
                        let pane = operation.request.input["pane"].as_u64().unwrap();
                        let mut context = terminal_observation(client, pane, None)?;
                        self.observed_input = Some((pane, context["input_generation"].as_u64().unwrap()));
                        result["screen"] = context["screen"].take();
                        result["shell_prompt"] = context["shell_prompt"].take();
                        result["input_generation"] = context["input_generation"].take();
                        result["process"] = context["process"].take();
                    }
                    Ok(result.to_string())
                });
                operation.request.respond(result)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn advance(&mut self, client: &mut Client) -> io::Result<()> {
        if let Some(mut operation) = self.operation.take() {
            let result = (|| -> io::Result<Option<String>> {
                let pane = operation.request.input["pane"].as_u64().unwrap();
                if operation.request.canceled.load(Ordering::Acquire) {
                    if let Stage::Read { id, .. } = operation.stage {
                        client.send(Action::CancelHistory { request: id })?;
                    }
                    return Err(io::ErrorKind::Interrupted.into());
                }
                match &operation.stage {
                    Stage::Command { command, until } => {
                        let view = client
                            .snapshot()
                            .and_then(|snapshot| snapshot.panes.iter().find(|view| view.id == pane))
                            .ok_or_else(|| io::Error::other("Target terminal no longer exists"))?;
                        let marker = view
                            .command_blocks
                            .iter()
                            .find(|block| block.id == *command);
                        // Older completed commands fall outside the recent-status window.
                        let completed = marker.is_none_or(|marker| marker.status.is_some());
                        if completed || Instant::now() >= *until {
                            operation.stage = read_output(
                                client,
                                &operation.request,
                                Some(*command),
                                if completed { "completed" } else { "timeout" },
                            )?;
                        }
                    }
                    Stage::Change { until } => {
                        let activity = client.terminal_revision(pane)?;
                        let changed = self.observed_activity != Some((pane, activity));
                        if changed || Instant::now() >= *until {
                            let outcome =
                                if operation.request.input["wait"].as_u64().unwrap_or(0) == 0 {
                                    "current"
                                } else if changed {
                                    "changed"
                                } else {
                                    "timeout"
                                };
                            if operation.request.input["source"] == "screen" {
                                let mut context = terminal_observation(
                                    client,
                                    pane,
                                    operation.request.input["query"].as_str(),
                                )?;
                                self.observed_input =
                                    Some((pane, context["input_generation"].as_u64().unwrap()));
                                self.observed_activity = Some((pane, activity));
                                context["outcome"] = json!(outcome);
                                return Ok(Some(context.to_string()));
                            }
                            operation.stage =
                                read_output(client, &operation.request, None, outcome)?;
                        }
                    }
                    _ => (),
                }
                Ok(None)
            })();
            match result {
                Ok(Some(text)) => operation.request.respond(Ok(text))?,
                Ok(None) => self.operation = Some(operation),
                Err(error) => operation.request.respond(Err(error))?,
            }
        }
        Ok(())
    }
}

fn read_output(
    client: &mut Client,
    request: &TerminalRequest,
    command: Option<u64>,
    outcome: &'static str,
) -> io::Result<Stage> {
    let cursor = request
        .input
        .get("cursor")
        .filter(|cursor| !cursor.is_null())
        .map(|cursor| -> io::Result<crate::history::HistoryCursor> {
            Ok(crate::history::HistoryCursor {
                epoch: serde_json::from_value(cursor["epoch"].clone())?,
                row: serde_json::from_value(cursor["row"].clone())?,
                offset: serde_json::from_value(cursor["offset"].clone())?,
            })
        })
        .transpose()?;
    let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let pane = request.input["pane"].as_u64().unwrap();
    let before = client.terminal_revision(pane)?;
    client.send(Action::ReadHistory {
        pane,
        request: id,
        block: command,
        cursor,
        query: request.input["query"].as_str().map(str::to_owned),
    })?;
    Ok(Stage::Read {
        id,
        command,
        before,
        outcome,
    })
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
        assert!(mirror.restore(owner.snapshot()).unwrap());
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
            text: "inspect the debugger".into(),
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
                role: "read".into(),
                text: "x".repeat(4096),
                id: Some(index.to_string()),
            });
            agent.trim();
        }
        assert_eq!(agent.messages[0].text, "inspect the debugger");
        assert_eq!(agent.messages[agent.response.unwrap()].text, "working");
        assert!(agent.messages.iter().map(|m| m.text.len()).sum::<usize>() <= TRANSCRIPT_LIMIT);
        assert!(agent.messages.len() <= 128);
    }

    #[test]
    fn rejected_send_keeps_the_draft_without_starting_a_worker() {
        let config = crate::config::Config::default();
        for (input, diagnostic) in [
            ("inspect current shell", "Set model"),
            ("keep 中文", "Set model"),
        ] {
            let mut agent = Agent::default();
            agent.insert(input).unwrap();
            let cursor = agent.cursor;
            let error = agent
                .send(input.into(), &config, 1, serde_json::Value::Null, 0)
                .unwrap_err();
            assert!(error.to_string().contains(diagnostic));
            assert_eq!(agent.input, input);
            assert_eq!(agent.cursor, cursor);
            assert!(agent.runtime.is_none());
            assert!(!agent.busy);
        }
    }
}
