pub(crate) mod context;
pub(crate) mod harness;
pub(crate) mod http;
pub(crate) mod markdown;
pub mod owner;
pub(crate) mod panel;

use crate::agent::harness::{Event, Harness, MAX_READ_WAIT, TOOL_OUTPUT_LIMIT, Tool, ToolRequest};
use crate::input::keysym;
use crate::workspace::{
    Action, AgentMessage, AgentModelPatch, Client, ClientEvent, FileOperation, FileResult, PaneView,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{io, ops::Range};

const INPUT_LIMIT: usize = 64 * 1024;
const TRANSCRIPT_LIMIT: usize = 256 * 1024;
const INPUT_OBSERVE: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct AgentView {
    pub input: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub busy: bool,
    pub messages: Vec<AgentMessage>,
    pub scroll: usize,
    pub revision: u64,
    pub pending: Vec<serde_json::Value>,
}

impl AgentView {
    pub fn restore_patch(&mut self, patch: &AgentModelPatch) -> io::Result<bool> {
        let start = patch.start as usize;
        let busy = patch.busy
            || self
                .pending
                .iter()
                .any(|request| request["action"] == "send");
        let redraw = self.busy != busy
            || self.messages.len() != start + patch.messages.len()
            || patch.messages.iter().enumerate().any(|(index, new)| {
                self.messages.get(start + index).is_none_or(|old| {
                    old.turn != new.turn
                        || old.role != new.role
                        || old.id != new.id
                        || if index == 0 && patch.prefix_bytes != 0 {
                            new.role != "Agent"
                                || new.id.is_some()
                                || patch.prefix_bytes as usize != old.text.len()
                        } else {
                            old.text != new.text
                                && (new.role != "Agent"
                                    || new.id.is_some()
                                    || !new.text.starts_with(&old.text))
                        }
                })
            });
        patch.apply_messages(&mut self.messages)?;
        self.busy = busy;
        self.revision = patch.revision;
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
        self.set_cursor(0, false);
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
        self.set_cursor(range.start + text.len(), false);
        self.input.replace_range(range, text);
        Ok(())
    }

    pub fn key(&mut self, symbol: u32, shift: bool) {
        let before = || {
            self.input
                .floor_char_boundary(self.cursor.saturating_sub(1))
        };
        let after = || self.input.ceil_char_boundary(self.cursor + 1);
        let selection = self.selection();
        let offset = match symbol {
            keysym::LEFT => {
                if !shift && let Some(range) = selection {
                    range.start
                } else {
                    before()
                }
            }
            keysym::RIGHT => {
                if !shift && let Some(range) = selection {
                    range.end
                } else {
                    after()
                }
            }
            keysym::HOME => self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1),
            keysym::END => self.input[self.cursor..]
                .find('\n')
                .map_or(self.input.len(), |offset| self.cursor + offset),
            keysym::BACKSPACE | keysym::DELETE => {
                let range = selection.unwrap_or_else(|| {
                    if symbol == keysym::BACKSPACE {
                        before()..self.cursor
                    } else {
                        self.cursor..after()
                    }
                });
                self.set_cursor(range.start, false);
                self.input.replace_range(range, "");
                return;
            }
            _ => return,
        };
        self.set_cursor(offset, shift);
    }
}

#[derive(Default)]
pub(crate) struct Agent {
    pub busy: bool,
    pub messages: Vec<AgentMessage>,
    pub revision: u64,
    pub rewound: Option<(u64, String)>,
    pub(crate) turn: u64,
    rewind: Option<String>,
    runtime: Option<Harness>,
    response: Option<usize>,
    operation: Option<Operation>,
    observed_input: BTreeMap<u64, u64>,
    observed_activity: BTreeMap<u64, u64>,
}

impl Agent {
    pub fn fd(&self) -> i32 {
        self.runtime.as_ref().map_or(-1, Harness::fd)
    }

    pub fn send(
        &mut self,
        input: &mut String,
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
        if let Some(generation) = generation {
            self.observed_input.insert(pane, generation);
        } else {
            self.observed_input.remove(&pane);
        }
        self.observed_activity.insert(pane, activity);
        self.message("You", std::mem::take(input));
        self.response = None;
        self.busy = true;
        self.trim();
        Ok(turn)
    }

    pub fn rewind(&mut self, turn: u64) -> io::Result<()> {
        let message = self
            .messages
            .iter()
            .find(|message| message.turn == turn && message.role == "You")
            .ok_or_else(|| io::Error::other("Select a user message to rewind"))?;
        if self.rewind.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Conversation is already rewinding",
            ));
        }
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            io::Error::other("This conversation is no longer available in model context")
        })?;
        runtime.rewind(turn)?;
        self.rewind = Some(message.text.clone());
        self.busy = true;
        self.response = None;
        Ok(())
    }

    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        let cancel = if let Some(operation) = self.operation.take() {
            let (command, cancel) = match operation.stage {
                Stage::Command { command, .. } => (Some(command), None),
                Stage::Read { id, command, .. } => {
                    (command, Some(Action::CancelHistory { request: id }))
                }
                Stage::File { id } => (None, Some(Action::CancelAgentFile { request: id })),
                _ => (None, None),
            };
            let result = match command {
                Some(command) if operation.request.tool == Tool::Shell => {
                    Ok(json!({"input_queued":true,"command":command}).to_string())
                }
                _ if matches!(operation.stage, Stage::InputOutput { .. }) => {
                    Ok(queued_input(Err(io::ErrorKind::Interrupted.into()), None))
                }
                _ if matches!(operation.stage, Stage::NewTerminalOutput { .. }) => {
                    Ok(created_terminal(
                        operation.request.pane,
                        Err(io::ErrorKind::Interrupted.into()),
                    ))
                }
                _ => Err(io::ErrorKind::Interrupted.into()),
            };
            // Queue known execution facts before abort publishes cancellation and wakes the worker.
            let _ = operation.request.reply.send(result);
            cancel
        } else {
            None
        };
        if let Some(runtime) = &self.runtime {
            runtime.abort()?;
        }
        if let Some(action) = cancel {
            client.send(action)?;
        }
        Ok(())
    }

    pub fn poll(&mut self, client: &mut Client) -> io::Result<bool> {
        self.advance(client)?;
        let Some(runtime) = &self.runtime else {
            return Ok(false);
        };
        let events = runtime.poll()?;
        let mut changed = false;
        for event in events {
            changed |= !matches!(event, Event::Request(_));
            match event {
                Event::ResetText => {
                    if let Some(index) = self.response.take() {
                        self.messages.remove(index);
                        self.revision += 1;
                    }
                }
                Event::Text(text) => {
                    if let Some(index) = self.response {
                        self.messages[index].text.push_str(&text);
                    } else {
                        self.response = Some(self.messages.len());
                        self.message("Agent", text);
                    }
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
                        self.messages.push(AgentMessage {
                            turn: self.turn,
                            role,
                            text,
                            id: Some(id),
                        });
                    }
                }
                Event::Request(request) => self.begin(request, client)?,
                Event::Notice(text) => {
                    self.response = None;
                    self.message("Context", text);
                }
                Event::Blocked(reason) => {
                    self.response = None;
                    self.message("Blocked", reason);
                }
                Event::Error(error) => self.message("Error", error),
                Event::Rewound { turn, error } => {
                    let text = self.rewind.take().unwrap();
                    self.busy = false;
                    self.response = None;
                    if let Some(error) = error {
                        self.message("Error", error);
                    } else {
                        self.messages.retain(|message| message.turn < turn);
                        self.revision += 1;
                        self.rewound = Some((turn, text));
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

    fn message(&mut self, role: &str, text: String) {
        self.messages.push(AgentMessage {
            turn: self.turn,
            role: role.into(),
            text,
            id: None,
        });
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
            // Leave room for new deltas so trimming does not reset the whole transcript every time.
            let start = message
                .text
                .ceil_char_boundary((bytes - TRANSCRIPT_LIMIT).max(TRANSCRIPT_LIMIT / 8));
            message.text.drain(..start);
            self.revision += 1;
        }
    }
}

pub(crate) fn terminal_context(client: &Client, pane: u64) -> io::Result<Value> {
    let mut context = terminal_observation(client, pane, None)?;
    let snapshot = client.snapshot().unwrap();
    let view = snapshot.pane(pane).unwrap();
    context["target"] = json!({
        "workspace": snapshot.name,
        "pane": pane,
        "tab": view.tab,
        "connection": if client.is_remote() { "ssh" } else { "local" },
        "ssh_target": client.remote_target(),
        "writable": client.can_agent_input(),
        "title": view.title,
        "cwd": view.cwd,
        "launch_argv": view.process.launch_argv,
        "child_pid": view.process.child_pid,
    });
    context["workspace"] = workspace_context(client)?;
    Ok(context)
}

fn terminal_observation(client: &Client, pane: u64, query: Option<&str>) -> io::Result<Value> {
    let snapshot = client
        .snapshot()
        .ok_or_else(|| io::Error::other("No workspace"))?;
    let view = snapshot
        .pane(pane)
        .ok_or_else(|| io::Error::other("Pane no longer exists in this tab"))?;
    let screen = client
        .screens()
        .get(&pane)
        .ok_or_else(|| io::Error::other("No screen for pane"))?;
    let mut observation = pane_context(view);
    observation["history_rows"] = json!(screen.history);
    observation["screen"] = crate::agent::context::shape(screen, query);
    Ok(observation)
}

fn pane_context(view: &PaneView) -> Value {
    json!({
        "pane":view.id,"tab":view.tab,"title":view.title,"cwd":view.cwd,
        "input_generation":view.input_generation,"shell_prompt":view.shell_prompt,
        "process":{
            "foreground_pgid":view.process.foreground_pgid,
            "foreground_pid":view.process.foreground_pid,
            "foreground_scope":"pty_process_group_leader",
            "foreground_executable":view.process.foreground_executable,
            "foreground_argv":view.process.foreground_argv,
            "foreground_cwd":view.process.foreground_cwd,
            "sampled_at_ms":view.process.sampled_at_ms,
            "unavailable":view.process.unavailable,
        },
        "command_blocks":view.command_blocks.iter().rev().map(|b| json!({
            "id":b.id,"has_output":b.has_output,"exit_code":b.status,"output_incomplete":!b.complete,
        })).collect::<Vec<_>>(),
    })
}

fn workspace_context(client: &Client) -> io::Result<Value> {
    let snapshot = client
        .snapshot()
        .ok_or_else(|| io::Error::other("No workspace"))?;
    Ok(json!({
        "workspace":snapshot.name,"active_tab":snapshot.active_tab,
        "connection":if client.is_remote() {"ssh"} else {"local"},
        "ssh_target":client.remote_target(),
        "panes":snapshot.panes.iter().map(pane_context).collect::<Vec<_>>(),
    }))
}

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

struct Operation {
    request: ToolRequest,
    stage: Stage,
}

enum Stage {
    Refresh,
    File {
        id: u64,
    },
    NewTerminal {
        id: u64,
    },
    NewTerminalOutput {
        until: Instant,
    },
    Input {
        id: u64,
        generation: u64,
        before: u64,
    },
    InputOutput {
        before: u64,
        until: Instant,
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
    pub fn needs_screen(&self) -> bool {
        self.operation.as_ref().is_some_and(|operation| {
            !matches!(
                operation.stage,
                Stage::File { .. } | Stage::NewTerminal { .. }
            )
        })
    }

    pub fn deadline(&self, client: &Client) -> Option<Instant> {
        self.operation
            .as_ref()
            .filter(|_| !client.screen_stale())
            .and_then(|operation| match operation.stage {
                Stage::Command { until, .. }
                | Stage::Change { until, .. }
                | Stage::InputOutput { until, .. }
                | Stage::NewTerminalOutput { until } => Some(until),
                _ => None,
            })
    }

    fn begin(&mut self, mut request: ToolRequest, client: &mut Client) -> io::Result<()> {
        if request.canceled.load(Ordering::Acquire) {
            return request.respond(Err(io::ErrorKind::Interrupted.into()));
        }
        let result = if matches!(
            request.tool,
            Tool::ReadFile | Tool::SearchFiles | Tool::EditFile | Tool::OpenTerminal
        ) {
            (|| {
                let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                if request.tool == Tool::OpenTerminal {
                    client.send(Action::AgentNewTab {
                        pane: request.pane,
                        request: id,
                    })?;
                    return Ok(Stage::NewTerminal { id });
                }
                let path = serde_json::from_value(request.input["path"].take())?;
                let operation = match request.tool {
                    Tool::ReadFile => FileOperation::Read {
                        offset: request
                            .input
                            .get_mut("offset")
                            .map(|value| serde_json::from_value(value.take()))
                            .transpose()?
                            .unwrap_or(0),
                    },
                    Tool::SearchFiles => FileOperation::Search {
                        query: serde_json::from_value(request.input["query"].take())?,
                    },
                    Tool::EditFile => FileOperation::Edit {
                        old_text: serde_json::from_value(request.input["old_text"].take())?,
                        new_text: serde_json::from_value(request.input["new_text"].take())?,
                    },
                    _ => unreachable!(),
                };
                client.send(Action::AgentFile {
                    pane: request.pane,
                    request: id,
                    path,
                    operation,
                })?;
                Ok(Stage::File { id })
            })()
        } else {
            client.refresh_screen().map(|()| Stage::Refresh)
        };
        match result {
            Ok(stage) => self.operation = Some(Operation { request, stage }),
            Err(error) => request.respond(Err(error))?,
        }
        Ok(())
    }

    fn begin_ready(&mut self, request: ToolRequest, client: &mut Client) -> io::Result<()> {
        if request.tool == Tool::Read && request.input["source"] == "workspace" {
            let result = if request.canceled.load(Ordering::Acquire) {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                workspace_context(client).map(|context| context.to_string())
            };
            return request.respond(result);
        }
        let result = (|| {
            if request.canceled.load(Ordering::Acquire) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let pane = request.pane;
            match request.tool {
                Tool::Shell | Tool::Input => {
                    let shell = request.tool == Tool::Shell;
                    let view = client
                        .snapshot()
                        .and_then(|snapshot| snapshot.pane(pane))
                        .ok_or_else(|| io::Error::other("Target terminal no longer exists"))?;
                    let field = if shell { "command" } else { "text" };
                    let text = request.input[field]
                        .as_str()
                        .ok_or_else(|| io::Error::other(format!("Missing string {field}")))?;
                    let prompt = if shell {
                        Some(view.shell_prompt.ok_or_else(|| io::Error::other("This pane has no untouched integrated shell prompt. Use input only to interact with its current foreground program. For independent shell commands, use open_terminal and the returned new pane ID."))?)
                    } else {
                        None
                    };
                    let data = if shell
                        && client.screens().get(&pane).is_some_and(|screen| {
                            screen.mode.contains(crate::core::Mode::BRACKETED_PASTE)
                        }) {
                        [
                            b"\x1b[200~".as_slice(),
                            text.as_bytes(),
                            b"\x1b[201~\r".as_slice(),
                        ]
                        .concat()
                    } else {
                        if shell && text.chars().any(char::is_control) {
                            return Err(io::Error::other(
                                "Shell editing controls require bracketed paste; use a single printable command",
                            ));
                        }
                        let mut data = text.as_bytes().to_vec();
                        if shell {
                            data.push(b'\r');
                        }
                        data
                    };
                    let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                    let generation = self.observed_input.get(&pane).copied().ok_or_else(|| {
                        io::Error::other("Read the current terminal state before sending input")
                    })?;
                    let before = client.screen_revision(pane)?;
                    client.send(Action::AgentInput {
                        pane,
                        request: id,
                        generation,
                        prompt,
                        data,
                    })?;
                    Ok(Stage::Input {
                        id,
                        generation,
                        before,
                    })
                }
                Tool::Read => {
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
                _ => unreachable!(),
            }
        })();
        match result {
            Ok(stage) => self.operation = Some(Operation { request, stage }),
            Err(error) => request.respond(Err(error))?,
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: &mut ClientEvent, client: &mut Client) -> io::Result<()> {
        let id = match event {
            ClientEvent::AgentInput { request, .. }
            | ClientEvent::History { request, .. }
            | ClientEvent::AgentFile { request, .. }
            | ClientEvent::AgentNewTab { request, .. } => *request,
            _ => return Ok(()),
        };
        if !self.operation.as_ref().is_some_and(|operation| matches!(operation.stage, Stage::Input {id:pending,..} | Stage::Read {id:pending,..} | Stage::File {id:pending} | Stage::NewTerminal {id:pending} if pending == id)) {
            return Ok(());
        }
        let mut operation = self.operation.take().unwrap();
        let event = std::mem::replace(event, ClientEvent::Updated);
        match event {
            ClientEvent::AgentFile { result, .. } => {
                operation.request.respond(result.map_err(io::Error::other).map(|result| {
                    match result {
                        FileResult::Read { path, offset, text, next_offset, has_more } => json!({"path":path,"offset":offset,"text":text,"next_offset":next_offset,"has_more":has_more}),
                        FileResult::Search { path, matches, truncated } => json!({"path":path,"matches":matches.into_iter().map(|found|json!({"path":found.path,"line":found.line,"text":found.text})).collect::<Vec<_>>(),"truncated":truncated}),
                        FileResult::Edited { path } => json!({"path":path,"written":true}),
                    }.to_string()
                }))?;
            }
            ClientEvent::AgentNewTab { result, .. } => match result {
                Ok(pane) => {
                    operation.request.pane = pane;
                    operation.stage = Stage::NewTerminalOutput {
                        until: Instant::now() + Duration::from_secs(1),
                    };
                    let refreshed = if operation.request.canceled.load(Ordering::Acquire) {
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        client.refresh_screen()
                    };
                    match refreshed {
                        Ok(()) => self.operation = Some(operation),
                        Err(error) => operation
                            .request
                            .respond(Ok(created_terminal(pane, Err(error))))?,
                    }
                }
                Err(error) => operation.request.respond(Err(io::Error::other(error)))?,
            },
            ClientEvent::AgentInput { result, .. } => {
                let Stage::Input {
                    generation, before, ..
                } = operation.stage
                else {
                    unreachable!()
                };
                if result.is_ok() {
                    self.observed_input
                        .insert(operation.request.pane, generation + 1);
                }
                match result {
                    Ok(None) => {
                        operation.stage = Stage::InputOutput {
                            before,
                            until: Instant::now() + INPUT_OBSERVE,
                        };
                        if operation.request.canceled.load(Ordering::Acquire) {
                            return operation.request.respond(Ok(queued_input(
                                Err(io::ErrorKind::Interrupted.into()),
                                None,
                            )));
                        }
                        match client.refresh_screen() {
                            Ok(()) => self.operation = Some(operation),
                            Err(error) => operation
                                .request
                                .respond(Ok(queued_input(Err(error), None)))?,
                        }
                    }
                    Ok(Some(command)) if !operation.request.canceled.load(Ordering::Acquire) => {
                        operation.stage = Stage::Command {
                            command,
                            until: Instant::now() + Duration::from_secs(1),
                        };
                        self.operation = Some(operation);
                    }
                    Ok(Some(command)) => {
                        let pane = operation.request.pane;
                        operation.request.respond(Ok(queued_input(
                            terminal_observation(client, pane, None).inspect(|_| {
                                self.observed_activity
                                    .insert(pane, client.terminal_revision(pane).unwrap());
                            }),
                            Some(command),
                        )))?;
                    }
                    Err(error) => operation.request.respond(Err(io::Error::other(error)))?,
                }
            }
            ClientEvent::History { result, .. } => {
                let Stage::Read {
                    command,
                    before,
                    outcome,
                    ..
                } = operation.stage
                else {
                    unreachable!()
                };
                if result.is_ok() {
                    self.observed_activity
                        .insert(operation.request.pane, before);
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
                        let pane = operation.request.pane;
                        let mut context = terminal_observation(client, pane, None)?;
                        self.observed_input.insert(pane, context["input_generation"].as_u64().unwrap());
                        result["screen"] = context["screen"].take();
                        result["shell_prompt"] = context["shell_prompt"].take();
                        result["input_generation"] = context["input_generation"].take();
                        result["process"] = context["process"].take();
                    }
                    Ok(result)
                });
                let result = if operation.request.tool == Tool::Shell {
                    Ok(queued_input(result, command))
                } else {
                    result.map(|result| result.to_string())
                };
                operation.request.respond(result)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn advance(&mut self, client: &mut Client) -> io::Result<()> {
        let Some(mut operation) = self.operation.take() else {
            return Ok(());
        };
        if matches!(operation.stage, Stage::Refresh)
            && !operation.request.canceled.load(Ordering::Acquire)
            && !client.screen_stale()
        {
            return self.begin_ready(operation.request, client);
        }
        let result = (|| -> io::Result<Option<String>> {
            let pane = operation.request.pane;
            if operation.request.canceled.load(Ordering::Acquire) {
                if let Stage::Read { id, .. } = operation.stage {
                    client.send(Action::CancelHistory { request: id })?;
                } else if let Stage::File { id } = operation.stage {
                    client.send(Action::CancelAgentFile { request: id })?;
                }
                return Err(io::ErrorKind::Interrupted.into());
            }
            if matches!(
                operation.stage,
                Stage::Command { .. }
                    | Stage::Change { .. }
                    | Stage::InputOutput { .. }
                    | Stage::NewTerminalOutput { .. }
            ) && client.screen_stale()
            {
                client.request_screen()?;
                return Ok(None);
            }
            match &operation.stage {
                Stage::NewTerminalOutput { until } => {
                    let view = client
                        .snapshot()
                        .and_then(|snapshot| snapshot.pane(pane))
                        .ok_or_else(|| io::Error::other("Target terminal no longer exists"))?;
                    if view.shell_prompt.is_some() || Instant::now() >= *until {
                        let mut context = terminal_observation(client, pane, None)?;
                        self.observed_input
                            .insert(pane, context["input_generation"].as_u64().unwrap());
                        self.observed_activity
                            .insert(pane, client.terminal_revision(pane)?);
                        context["outcome"] = json!(if view.shell_prompt.is_some() {
                            "current"
                        } else {
                            "timeout"
                        });
                        return Ok(Some(created_terminal(pane, Ok(context))));
                    }
                }
                Stage::InputOutput { before, until } => {
                    let activity = client.screen_revision(pane)?;
                    let changed = activity != *before;
                    if changed || Instant::now() >= *until {
                        let mut context = terminal_observation(client, pane, None)?;
                        self.observed_input
                            .insert(pane, context["input_generation"].as_u64().unwrap());
                        self.observed_activity
                            .insert(pane, client.terminal_revision(pane)?);
                        context["outcome"] = json!(if changed { "changed" } else { "timeout" });
                        return Ok(Some(queued_input(Ok(context), None)));
                    }
                }
                Stage::Command { command, until } => {
                    let view = client
                        .snapshot()
                        .and_then(|snapshot| snapshot.pane(pane))
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
                    let changed = self.observed_activity.get(&pane) != Some(&activity);
                    if changed || Instant::now() >= *until {
                        let outcome = if operation.request.input["wait"].as_u64().unwrap_or(0) == 0
                        {
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
                            self.observed_input
                                .insert(pane, context["input_generation"].as_u64().unwrap());
                            self.observed_activity.insert(pane, activity);
                            context["outcome"] = json!(outcome);
                            return Ok(Some(context.to_string()));
                        }
                        operation.stage = read_output(client, &operation.request, None, outcome)?;
                    }
                }
                _ => (),
            }
            Ok(None)
        })();
        match result {
            Ok(Some(text)) => operation.request.respond(Ok(text))?,
            Ok(None) => self.operation = Some(operation),
            Err(error) => {
                let command = match operation.stage {
                    Stage::Command { command, .. }
                    | Stage::Read {
                        command: Some(command),
                        ..
                    } if operation.request.tool == Tool::Shell => Some(command),
                    _ => None,
                };
                let result = if matches!(operation.stage, Stage::NewTerminalOutput { .. }) {
                    Ok(created_terminal(operation.request.pane, Err(error)))
                } else if command.is_some() || matches!(operation.stage, Stage::InputOutput { .. })
                {
                    Ok(queued_input(Err(error), command))
                } else {
                    Err(error)
                };
                operation.request.respond(result)?;
            }
        }
        Ok(())
    }
}

fn created_terminal(pane: u64, observation: io::Result<Value>) -> String {
    let mut result =
        observation.unwrap_or_else(|error| json!({"observation_error":error.to_string()}));
    result["created"] = json!(true);
    result["pane"] = json!(pane);
    let text = result.to_string();
    if text.len() <= TOOL_OUTPUT_LIMIT {
        text
    } else {
        json!({
            "created":true,"pane":pane,
            "observation_error":"Terminal observation exceeds 64 KiB; use read with a focused query",
        }).to_string()
    }
}

fn queued_input(observation: io::Result<Value>, command: Option<u64>) -> String {
    let mut result =
        observation.unwrap_or_else(|error| json!({"observation_error":error.to_string()}));
    result["input_queued"] = json!(true);
    if let Some(command) = command {
        result["command"] = json!(command);
    }
    let text = result.to_string();
    if text.len() <= TOOL_OUTPUT_LIMIT {
        return text;
    }
    let mut facts = json!({
        "input_queued": true,
        "observation_error": "Terminal observation exceeds 64 KiB; use read with a focused query",
    });
    if let Some(command) = command {
        facts["command"] = json!(command);
    }
    facts.to_string()
}

fn read_output(
    client: &mut Client,
    request: &ToolRequest,
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
    let pane = request.pane;
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
    fn failed_or_oversized_first_observation_preserves_created_terminal() {
        for observation in [
            Err(io::ErrorKind::Interrupted.into()),
            Ok(json!({"screen":{"rows":[[0, "终".repeat(TOOL_OUTPUT_LIMIT / 3)]]}})),
        ] {
            let text = created_terminal(37, observation);
            assert!(text.len() <= TOOL_OUTPUT_LIMIT);
            let result: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(result["created"], true);
            assert_eq!(result["pane"], 37);
            assert!(result["observation_error"].is_string());
            assert!(result.get("screen").is_none());
        }
    }

    #[test]
    fn oversized_observation_preserves_accepted_input_and_command() {
        for command in [None, Some(37)] {
            let text = queued_input(
                Ok(json!({"screen":{"rows":[[0, "终".repeat(TOOL_OUTPUT_LIMIT / 3)]]}})),
                command,
            );
            assert!(text.len() <= TOOL_OUTPUT_LIMIT);
            let result: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(result["input_queued"], true);
            assert!(
                result["observation_error"]
                    .as_str()
                    .unwrap()
                    .contains("64 KiB")
            );
            assert_eq!(result.get("command").and_then(Value::as_u64), command);
            assert!(result.get("screen").is_none());
        }
    }

    #[test]
    fn mirror_restores_canonical_messages_without_overwriting_next_draft() {
        let patch = AgentModelPatch {
            busy: true,
            revision: 9,
            turn: 3,
            messages: vec![AgentMessage {
                role: "You".into(),
                text: "first request".into(),
                turn: 3,
                id: None,
            }],
            ..AgentModelPatch::default()
        };
        let mut mirror = AgentView::default();
        mirror.insert("下一条🙂").unwrap();
        mirror.anchor = Some(3);
        mirror.scroll = 7;
        let cursor = mirror.cursor;
        assert!(mirror.restore_patch(&patch).unwrap());
        assert_eq!(mirror.input, "下一条🙂");
        assert_eq!(mirror.cursor, cursor);
        assert_eq!(mirror.anchor, Some(3));
        assert_eq!(mirror.scroll, 7);
        assert_eq!(mirror.messages[0].turn, 3);
        assert_eq!(mirror.revision, 9);
        assert!(mirror.busy);
    }

    #[test]
    fn rewind_identity_survives_independent_transcript_trimming() {
        let mut owner = Agent::default();
        for turn in [2, 7] {
            owner.messages.push(AgentMessage {
                turn,
                role: "You".into(),
                text: format!("turn {turn}"),
                id: None,
            });
        }
        owner.messages.remove(0);
        assert!(
            owner
                .rewind(7)
                .unwrap_err()
                .to_string()
                .contains("model context")
        );
        assert!(
            owner
                .rewind(2)
                .unwrap_err()
                .to_string()
                .contains("Select a user message")
        );
    }

    #[test]
    fn selection_replacement_and_deletion_preserve_unicode_boundaries() {
        let mut agent = AgentView::default();
        agent.insert("a中文z").unwrap();
        agent.set_cursor(1, false);
        agent.set_cursor(7, true);
        assert_eq!(&agent.input[agent.selection().unwrap()], "中文");
        agent.insert("新").unwrap();
        assert_eq!(agent.input, "a新z");
        assert_eq!(agent.cursor, 4);
        assert_eq!(agent.selection(), None);
        agent.key(keysym::BACKSPACE, false);
        assert_eq!(agent.input, "az");
        agent.select_all();
        agent.key(keysym::DELETE, false);
        assert!(agent.input.is_empty());
        assert_eq!(agent.cursor, 0);
        assert_eq!(agent.anchor, None);
    }

    #[test]
    fn shift_navigation_extends_from_anchor_and_arrows_collapse_selection() {
        let mut agent = AgentView::default();
        agent.insert("ab\n中文").unwrap();
        agent.key(keysym::LEFT, true);
        agent.key(keysym::LEFT, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(keysym::RIGHT, false);
        assert_eq!(agent.cursor, 9);
        assert_eq!(agent.selection(), None);
        agent.key(keysym::HOME, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(keysym::LEFT, false);
        assert_eq!(agent.cursor, 3);
        agent.key(keysym::END, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.clear_input();
        assert!(agent.input.is_empty());
        assert_eq!(agent.anchor, None);
    }

    #[test]
    fn replacing_selected_text_can_reuse_the_input_budget() {
        let mut agent = AgentView::default();
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
        agent.messages.push(AgentMessage {
            turn: 1,
            role: "You".into(),
            text: "old".into(),
            id: None,
        });
        agent.messages.push(AgentMessage {
            turn: 2,
            role: "You".into(),
            text: "inspect the debugger".into(),
            id: None,
        });
        agent.messages.push(AgentMessage {
            turn: 2,
            role: "Agent".into(),
            text: "working".into(),
            id: None,
        });
        agent.response = Some(2);
        for index in 0..160 {
            agent.messages.push(AgentMessage {
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

        agent.messages[agent.response.unwrap()]
            .text
            .push_str(&"🙂中文".repeat(TRANSCRIPT_LIMIT / 10 + 1));
        agent.trim();
        assert_eq!(agent.messages[0].text, "inspect the debugger");
        let response = agent.response.unwrap();
        assert_eq!(response, 1);
        let revision = agent.revision;
        for _ in 0..100 {
            agent.messages[response].text.push_str("增量🙂");
            agent.trim();
        }
        assert_eq!(agent.revision, revision);
        agent.messages[response]
            .text
            .push_str(&"尾".repeat(TRANSCRIPT_LIMIT / 3));
        agent.trim();
        assert!(agent.messages.iter().map(|m| m.text.len()).sum::<usize>() <= TRANSCRIPT_LIMIT);
        assert_eq!(agent.messages[0].text, "inspect the debugger");
        assert_eq!(agent.response, Some(response));
    }

    #[test]
    fn rejected_send_does_not_start_a_worker_or_publish_a_message() {
        let config = crate::config::Config::default();
        for (input, diagnostic) in [
            ("inspect current shell", "Set model"),
            ("keep 中文", "Set model"),
        ] {
            let mut agent = Agent::default();
            let mut draft = input.to_owned();
            let error = agent
                .send(
                    &mut draft,
                    input.into(),
                    &config,
                    1,
                    serde_json::Value::Null,
                    0,
                )
                .unwrap_err();
            assert!(error.to_string().contains(diagnostic));
            assert_eq!(draft, input);
            assert!(agent.messages.is_empty());
            assert!(agent.runtime.is_none());
            assert!(!agent.busy);
        }
    }
}
