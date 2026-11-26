//! A workspace owns its PTYs independently of any particular frontend.
pub use crate::agent_files::{AgentChange, FileOperation, FileResult};
use crate::context::ContextItem;
pub use crate::core::{BlockMarker, CommandBlock};
use crate::core::{Core, Event, Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::history::{HistoryBudget, HistoryCopy};
pub use crate::history::{HistoryCursor, HistoryPage};
use crate::screen::{FrameState, Screen, Update};
use crate::search::SearchResult;
use crate::terminal::{INPUT_CAPACITY, Shell, Terminal};
use crate::wire::Peer;
pub use crate::wire::Response;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Actions fail with a message for the requesting frontend; nothing matches on the cause.
type Failure = Box<dyn std::error::Error>;

pub type PaneId = u64;
pub type TabId = u64;
// Requests can outlive their workspace, so a new pane must not reuse its identity.
static NEXT_PANE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    LeftRight,
    TopBottom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}
impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.cols)
            && y < self.y.saturating_add(self.rows)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateOptions {
    pub size: WindowSize,
    pub foreground: u32,
    pub background: u32,
    #[serde(with = "crate::wire::path::option")]
    pub directory: Option<PathBuf>,
    pub program: Option<Shell>,
    pub history_capacity: u64,
}
impl Default for CreateOptions {
    fn default() -> Self {
        let config = crate::config::Config::default();
        Self {
            size: WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 0,
                cell_height: 0,
            },
            foreground: config.foreground,
            background: config.background,
            directory: None,
            program: None,
            history_capacity: config.history_limit_bytes,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub turn: u64,
    pub role: String,
    pub text: String,
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDraft {
    pub id: u64,
    pub input: String,
    pub basket: Vec<ContextItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDrafts {
    pub next_id: u64,
    pub sessions: Vec<AgentDraft>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSend {
    pub session: u64,
    pub pane: PaneId,
    pub input: String,
    pub basket: Vec<ContextItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRequest {
    Send(AgentSend),
    Abort { session: u64 },
    Rewind { session: u64, turn: u64 },
}

/// Sent once when a rewind or a send did not happen: by the owner, or by the daemon when the
/// owner never started. A rewind replaces the editor and context; a failed send is merged into
/// what was typed since.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecovery {
    pub session: u64,
    pub rewind: bool,
    pub input: String,
    pub basket: Vec<ContextItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelPatch {
    pub session: u64,
    pub busy: bool,
    pub start: u32,
    pub prefix_bytes: u32,
    pub messages: Vec<AgentMessage>,
    pub changes: Option<Arc<Vec<AgentChange>>>,
    pub changes_omitted: u32,
}
impl AgentModelPatch {
    pub(crate) fn apply_messages(&self, messages: &mut Vec<AgentMessage>) -> io::Result<()> {
        let start = self.start as usize;
        let prefix = self.prefix_bytes as usize;
        if start > messages.len()
            || (prefix != 0
                && (self.messages.is_empty()
                    || messages
                        .get(start)
                        .is_none_or(|message| !message.text.is_char_boundary(prefix))))
        {
            return Err(io::Error::other("invalid Agent message patch"));
        }
        if prefix != 0 {
            let update = &self.messages[0];
            let message = &mut messages[start];
            message.text.truncate(prefix);
            message.text.push_str(&update.text);
            message.role.clone_from(&update.role);
            message.id.clone_from(&update.id);
            message.turn = update.turn;
        }
        let skip = usize::from(prefix != 0);
        messages.truncate(start + skip);
        messages.extend_from_slice(&self.messages[skip..]);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEvent {
    Draft(AgentDrafts),
    Model(AgentModelPatch),
    Recovery(AgentRecovery),
    Request(AgentRequest),
    Start,
    Ready,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Action {
    ScreenRequest {
        request: u64,
        wait: bool,
    },
    AgentOpen,
    AgentView(AgentDrafts),
    AgentRequest(AgentRequest),
    AgentAttach {
        name: String,
    },
    AgentUpdate(AgentModelPatch),
    AgentRecover(AgentRecovery),
    AgentRemove(u64),
    Create {
        /// An empty name requests the lowest available nonnegative integer.
        name: String,
        options: CreateOptions,
    },
    Attach {
        name: String,
    },
    List,
    Input {
        pane: PaneId,
        #[serde(with = "crate::wire::bytes")]
        data: Vec<u8>,
    },
    AgentInput {
        pane: PaneId,
        request: u64,
        generation: u64,
        prompt: Option<u64>,
        #[serde(with = "crate::wire::bytes")]
        data: Vec<u8>,
    },
    AgentFile {
        pane: PaneId,
        request: u64,
        #[serde(with = "crate::wire::path")]
        path: PathBuf,
        operation: FileOperation,
    },
    AgentNewTab {
        pane: PaneId,
        request: u64,
    },
    Resize(WindowSize),
    Split {
        axis: Axis,
    },
    ResizeSplit {
        first: PaneId,
        second: PaneId,
        position: u16,
    },
    NewTab {
        pane: PaneId,
    },
    Focus(PaneId),
    Navigate(Direction),
    Tab(i32),
    ClosePane(PaneId),
    CloseTab(TabId),
    Scroll {
        pane: PaneId,
        scroll: Scroll,
    },
    SelectionStart {
        pane: PaneId,
        point: Point,
        side: Side,
        kind: SelectionType,
    },
    SelectionUpdate {
        pane: PaneId,
        point: Point,
        side: Side,
    },
    CopySelection {
        pane: PaneId,
        primary: bool,
    },
    PromptJump {
        pane: PaneId,
        previous: bool,
    },
    CopyBlock {
        pane: PaneId,
        block: u64,
    },
    ReadHistory {
        pane: PaneId,
        request: u64,
        block: Option<u64>,
        cursor: Option<HistoryCursor>,
        query: Option<String>,
    },
    Cancel {
        request: u64,
    },
    Search {
        pane: PaneId,
        request: u64,
        query: String,
    },
    SearchSelect {
        pane: PaneId,
        request: u64,
        index: u32,
    },
    SearchCancel {
        pane: PaneId,
        request: u64,
    },
    Terminate,
    TerminateName(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub tabs: usize,
    pub panes: usize,
    pub attached: usize,
}
/// Ordered by what a tab shows when its panes disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TabActivity {
    #[default]
    Idle,
    Finished,
    Failed,
    Unknown,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabView {
    pub id: TabId,
    pub title: String,
    pub activity: TabActivity,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneView {
    pub id: PaneId,
    pub tab: TabId,
    pub rect: Rect,
    pub title: String,
    #[serde(with = "crate::wire::path::option")]
    pub cwd: Option<PathBuf>,
    pub process: Arc<crate::terminal::ProcessMetadata>,
    pub blocks: Vec<BlockMarker>,
    pub command_blocks: Vec<CommandBlock>,
    pub input_generation: u64,
    pub shell_prompt: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub controller: bool,
    pub focused: PaneId,
    pub active_tab: TabId,
    pub tabs: Vec<TabView>,
    pub panes: Vec<PaneView>,
}

#[derive(Clone, Copy, Debug)]
pub struct SplitDrag {
    pub first: PaneId,
    pub second: PaneId,
    pub axis: Axis,
}
impl SplitDrag {
    pub fn action(self, column: u16, row: u16) -> Action {
        Action::ResizeSplit {
            first: self.first,
            second: self.second,
            position: match self.axis {
                Axis::LeftRight => column,
                Axis::TopBottom => row,
            },
        }
    }
}
impl Snapshot {
    pub(crate) fn pane(&self, id: PaneId) -> Option<&PaneView> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub(crate) fn pane_at(&self, column: u16, row: u16) -> Option<&PaneView> {
        self.panes
            .iter()
            .find(|pane| pane.rect.contains(column, row))
    }

    pub(crate) fn focused_pane(&self) -> Option<&PaneView> {
        self.pane(self.focused)
    }

    pub fn divider_at(&self, column: u16, row: u16) -> Option<SplitDrag> {
        for first in &self.panes {
            for second in &self.panes {
                let a = first.rect;
                let b = second.rect;
                let axis = if column == a.x + a.cols
                    && b.x == column + 1
                    && row >= a.y.max(b.y)
                    && row < (a.y + a.rows).min(b.y + b.rows)
                {
                    Axis::LeftRight
                } else if row == a.y + a.rows
                    && b.y == row + 1
                    && column >= a.x.max(b.x)
                    && column < (a.x + a.cols).min(b.x + b.cols)
                {
                    Axis::TopBottom
                } else {
                    continue;
                };
                return Some(SplitDrag {
                    first: first.id,
                    second: second.id,
                    axis,
                });
            }
        }
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ClientEndpoint {
    Local(#[serde(with = "crate::wire::path")] PathBuf),
    Remote {
        #[serde(with = "crate::wire::bytes")]
        program: Vec<u8>,
        args: Vec<Vec<u8>>,
        target: String,
    },
}
pub struct Client {
    screen_pending: bool,
    screen_request: u64,
    screen_dirty: bool,
    endpoint: ClientEndpoint,
    agent_events: VecDeque<AgentEvent>,
    agent_delegate: bool,
    peer: Peer,
    snapshot: Option<Snapshot>,
    screens: HashMap<PaneId, Screen>,
    ssh: Option<Child>,
}
impl Client {
    fn new(peer: Peer, ssh: Option<Child>, endpoint: ClientEndpoint) -> Self {
        Self {
            peer,
            ssh,
            endpoint,
            screen_pending: false,
            screen_request: 0,
            screen_dirty: true,
            agent_events: VecDeque::new(),
            agent_delegate: false,
            snapshot: None,
            screens: HashMap::new(),
        }
    }

    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self::new(
            Peer::new(stream)?,
            None,
            ClientEndpoint::Local(path.to_path_buf()),
        ))
    }
    pub fn connect_remote(mut command: Command, target: String) -> io::Result<Self> {
        let endpoint = ClientEndpoint::Remote {
            program: command.get_program().as_bytes().to_vec(),
            args: command
                .get_args()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            target,
        };
        let (local, ssh) = UnixStream::pair()?;
        let peer = Peer::new(local)?;
        command.stdin(OwnedFd::from(ssh.try_clone()?));
        command.stdout(OwnedFd::from(ssh));
        let child = command.spawn()?;
        // Closes this process's copies of the child's ends, so the child's exit ends the stream.
        drop(command);
        let mut client = Self::new(peer, Some(child), endpoint);
        loop {
            match client.peer.hello() {
                Ok(Some(crate::wire::VERSION)) => return Ok(client),
                Ok(Some(version)) => {
                    return Err(io::Error::other(format!(
                        "remote SSH bridge version {version} differs from {}",
                        crate::wire::VERSION
                    )));
                }
                Ok(None) => (),
                Err(error) => return Err(client.ssh_error(error)),
            }
            crate::sys::poll(&mut [crate::sys::watch(client.fd(), libc::POLLIN)], -1)?;
        }
    }
    /// Names SSH's exit status when the transport died, which is the useful part of a read error.
    fn ssh_error(&mut self, error: io::Error) -> io::Error {
        if let Some(child) = &mut self.ssh
            && let Ok(Some(status)) = child.try_wait()
            && !status.success()
        {
            return io::Error::new(error.kind(), format!("SSH exited with {status}: {error}"));
        }
        error
    }
    pub fn is_remote(&self) -> bool {
        matches!(self.endpoint, ClientEndpoint::Remote { .. })
    }
    pub fn remote_target(&self) -> Option<&str> {
        match &self.endpoint {
            ClientEndpoint::Local(_) => None,
            ClientEndpoint::Remote { target, .. } => Some(target),
        }
    }
    pub(crate) fn fd(&self) -> RawFd {
        self.peer.fd()
    }
    pub fn send(&mut self, action: Action) -> io::Result<()> {
        let screen = !matches!(
            action,
            Action::ScreenRequest { .. }
                | Action::AgentOpen
                | Action::AgentView(_)
                | Action::AgentRequest(_)
                | Action::AgentUpdate(_)
                | Action::AgentRecover(_)
                | Action::AgentRemove(_)
                | Action::ReadHistory { .. }
                | Action::Cancel { .. }
                | Action::AgentFile { .. }
                | Action::AgentNewTab { .. }
                | Action::List
                | Action::CopyBlock { .. }
                | Action::CopySelection { .. }
        );
        match action {
            Action::Input { pane, data } => return self.send_input(pane, &data),
            // A rejected owner gets an error and exits, so it can count itself attached now.
            Action::AgentAttach { .. } => {
                self.agent_delegate = true;
                self.peer.send_action(&action)?;
            }
            action => self.peer.send_action(&action)?,
        }
        if screen {
            self.request_screen()?;
        }
        Ok(())
    }
    pub(crate) fn send_input(&mut self, pane: PaneId, data: &[u8]) -> io::Result<()> {
        // An older response may already be in flight; keep a fresh request
        // ahead of input, including chunks that PTY backpressure may block.
        self.send_screen_request(true)?;
        // Each chunk fits the pane's PTY input queue, which takes it whole or later.
        for chunk in data.chunks(INPUT_CAPACITY / 4) {
            let data = chunk.to_vec();
            self.peer.send_action(&Action::Input { pane, data })?;
        }
        Ok(())
    }
    fn send_screen_request(&mut self, wait: bool) -> io::Result<()> {
        self.screen_pending = true;
        self.screen_request += 1;
        self.peer.send_action(&Action::ScreenRequest {
            request: self.screen_request,
            wait,
        })
    }
    pub fn request_screen(&mut self) -> io::Result<()> {
        if !self.screen_pending {
            self.send_screen_request(true)?;
        }
        Ok(())
    }
    pub fn refresh_screen(&mut self) -> io::Result<()> {
        self.screen_dirty = true;
        self.send_screen_request(false)
    }

    pub fn scroll_to(&mut self, pane: PaneId, offset: usize) -> io::Result<()> {
        // Live output must not skew the delta between these actions.
        self.peer.send_action(&Action::Scroll {
            pane,
            scroll: Scroll::Bottom,
        })?;
        if offset > 0 {
            self.peer.send_action(&Action::Scroll {
                pane,
                scroll: Scroll::Delta(offset as i32),
            })?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        self.peer.flush()
    }
    pub fn pollfd(&self) -> libc::pollfd {
        let writing = self.peer.pending();
        let events = libc::POLLIN | if writing { libc::POLLOUT } else { 0 };
        crate::sys::watch(self.fd(), events)
    }
    pub fn needs_poll(&self) -> bool {
        self.peer.buffered_action()
    }
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
    pub(crate) fn focused(&self) -> Option<PaneId> {
        self.snapshot.as_ref().map(|snapshot| snapshot.focused)
    }
    pub(crate) fn controller(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.controller)
    }
    pub(crate) fn mode(&self, pane: PaneId) -> Mode {
        self.screens
            .get(&pane)
            .map_or(Mode::default(), Screen::mode)
    }
    pub fn screens(&self) -> &HashMap<PaneId, Screen> {
        &self.screens
    }
    pub fn snapshot_and_screens_mut(
        &mut self,
    ) -> Option<(&Snapshot, &mut HashMap<PaneId, Screen>)> {
        Some((self.snapshot.as_ref()?, &mut self.screens))
    }

    pub fn poll(&mut self) -> io::Result<Vec<Response>> {
        let responses = self.peer.flush().and_then(|()| self.peer.responses());
        let responses = responses.map_err(|error| self.ssh_error(error))?;
        let mut events = Vec::new();
        for response in responses {
            let event = match response {
                Response::State(snapshot, updates, request) => {
                    for (id, update) in updates {
                        let screen = self.screens.entry(id).or_default();
                        if self.agent_delegate && screen.changes_activity(&update) {
                            screen.activity += 1;
                            screen.output_activity += 1;
                        }
                        screen.apply(update);
                    }
                    if let Some(snapshot) = snapshot {
                        self.screens.retain(|id, _| snapshot.pane(*id).is_some());
                        for pane in &snapshot.panes {
                            let Some(screen) = self.screens.get_mut(&pane.id) else {
                                continue;
                            };
                            let previous = self.snapshot.as_ref().and_then(|old| old.pane(pane.id));
                            if self.agent_delegate
                                && previous.is_none_or(|old| {
                                    old.input_generation != pane.input_generation
                                        || old.shell_prompt != pane.shell_prompt
                                        || old.command_blocks != pane.command_blocks
                                })
                            {
                                screen.activity += 1;
                            }
                            if let Some(old) = previous
                                && old.blocks != pane.blocks
                            {
                                for row in 0..screen.screen_lines() {
                                    screen.dirty[row] |= screen.output_changed(old, pane, row);
                                }
                            }
                        }
                        self.snapshot = Some(snapshot);
                    }
                    if request == self.screen_request {
                        self.screen_pending = false;
                        self.screen_dirty = false;
                    }
                    Response::ScreenDirty
                }
                Response::ScreenDirty => {
                    self.screen_dirty = true;
                    Response::ScreenDirty
                }
                Response::Agent(event) => {
                    self.agent_events.push_back(event);
                    continue;
                }
                Response::Detached => {
                    self.peer.cancel_output();
                    self.snapshot = None;
                    self.agent_delegate = false;
                    self.screens.clear();
                    Response::Detached
                }
                response => response,
            };
            if !matches!(event, Response::ScreenDirty)
                || !matches!(events.last(), Some(Response::ScreenDirty))
            {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub(crate) fn screen_stale(&self) -> bool {
        self.screen_dirty
    }

    pub(crate) fn endpoint(&self) -> &ClientEndpoint {
        &self.endpoint
    }

    pub(crate) fn next_agent_event(&mut self) -> Option<AgentEvent> {
        self.agent_events.pop_front()
    }

    pub(crate) fn can_agent_input(&self) -> bool {
        self.agent_delegate || self.controller()
    }

    pub(crate) fn terminal_revision(&self, pane: PaneId) -> io::Result<u64> {
        self.pane_screen(pane).map(|screen| screen.activity)
    }

    pub(crate) fn screen_revision(&self, pane: PaneId) -> io::Result<u64> {
        self.pane_screen(pane).map(|screen| screen.output_activity)
    }

    fn pane_screen(&self, pane: PaneId) -> io::Result<&Screen> {
        (self.screens.get(&pane))
            .ok_or_else(|| io::Error::other("Target terminal no longer exists"))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(child) = &mut self.ssh {
            // Dropping a frontend ends its SSH bridge, never the detached workspace server.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn socket_path() -> io::Result<PathBuf> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    private_directory(&root)?;
    let directory = root.join("nextty");
    private_directory(&directory)?;
    Ok(directory.join(format!("server-v{}.sock", crate::wire::VERSION)))
}
fn private_directory(path: &Path) -> io::Result<()> {
    let context =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(context(error)),
        _ => (),
    }
    let metadata = fs::symlink_metadata(path).map_err(context)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(format!(
            "{} must be an owned private directory",
            path.display()
        )));
    }
    Ok(())
}
/// Opens a private file for appending.
fn private_file(path: PathBuf) -> io::Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
/// Starts a process that outlives the frontend: in its own session, logging to `log`, and from
/// `/` so it does not pin the directory the frontend started in.
pub(crate) fn spawn_detached(command: &mut Command, log: PathBuf) -> io::Result<Child> {
    command
        .current_dir("/")
        .stdout(Stdio::null())
        .stderr(private_file(log)?);
    unsafe { command.pre_exec(|| crate::sys::check(libc::setsid()).map(drop)) };
    command.spawn()
}
pub fn ensure_server(executable: &Path, socket: &Path) -> io::Result<()> {
    // Concurrent starts wait here, so only the first one starts a server.
    let lock = private_file(socket.with_extension("start.lock"))?;
    lock.lock()?;
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let log_path = socket.with_extension("log");
    let mut command = Command::new(executable);
    command.arg("--server").arg(socket).stdin(Stdio::null());
    let mut child = spawn_detached(&mut command, log_path.clone())?;
    while UnixStream::connect(socket).is_err() {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "workspace server exited with {status}; see {}",
                log_path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

enum Node {
    Pane(PaneId),
    Split {
        axis: Axis,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}
impl Node {
    fn layout(&self, rect: Rect, output: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Pane(id) => output.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) =
                    Self::partition(rect, *axis, *ratio, first.minimum(), second.minimum());
                first.layout(a, output);
                second.layout(b, output);
            }
        }
    }
    fn partition(
        rect: Rect,
        axis: Axis,
        ratio: f64,
        min_a: (u16, u16),
        min_b: (u16, u16),
    ) -> (Rect, Rect) {
        let mut a = rect;
        let mut b = rect;
        match axis {
            Axis::LeftRight => {
                let available = rect.cols - 1;
                a.cols =
                    ((f64::from(available) * ratio) as u16).clamp(min_a.0, available - min_b.0);
                b.cols = available - a.cols;
                b.x = rect.x + a.cols + 1;
            }
            Axis::TopBottom => {
                let available = rect.rows - 1;
                a.rows =
                    ((f64::from(available) * ratio) as u16).clamp(min_a.1, available - min_b.1);
                b.rows = available - a.rows;
                b.y = rect.y + a.rows + 1;
            }
        }
        (a, b)
    }
    fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Pane(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }
    fn resize_split(&mut self, rect: Rect, left: PaneId, right: PaneId, position: u16) -> bool {
        let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let min_a = first.minimum();
        let min_b = second.minimum();
        let (a, b) = Self::partition(rect, *axis, *ratio, min_a, min_b);
        let in_first = first.contains(left);
        if in_first && second.contains(right) {
            let (origin, available, low, high, current) = match axis {
                Axis::LeftRight => (rect.x, rect.cols - 1, min_a.0, min_b.0, a.cols),
                Axis::TopBottom => (rect.y, rect.rows - 1, min_a.1, min_b.1, a.rows),
            };
            let size = position.saturating_sub(origin).clamp(low, available - high);
            if size == current {
                return false;
            }
            // Bias inside the cell so floating point rounding keeps the requested boundary.
            *ratio = (f64::from(size) + 0.5) / f64::from(available);
            true
        } else if in_first {
            first.resize_split(a, left, right, position)
        } else {
            second.resize_split(b, left, right, position)
        }
    }
    fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            }
        }
    }
    fn remove_child(&mut self, target: PaneId) -> bool {
        let Self::Split { first, second, .. } = self else {
            return false;
        };
        if matches!(**first, Self::Pane(id) if id == target) {
            // Move out the sibling without allocating a replacement box.
            *self = std::mem::replace(&mut **second, Self::Pane(0));
            true
        } else if matches!(**second, Self::Pane(id) if id == target) {
            *self = std::mem::replace(&mut **first, Self::Pane(0));
            true
        } else {
            first.remove_child(target) || second.remove_child(target)
        }
    }
    fn first(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first(),
        }
    }
    /// The smallest columns and rows the subtree can be laid out in.
    fn minimum(&self) -> (u16, u16) {
        match self {
            Self::Pane(_) => (2, 1),
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (a, b) = (first.minimum(), second.minimum());
                match axis {
                    Axis::LeftRight => (a.0 + b.0 + 1, a.1.max(b.1)),
                    Axis::TopBottom => (a.0.max(b.0), a.1 + b.1 + 1),
                }
            }
        }
    }
}
struct Tab {
    id: TabId,
    root: Node,
    focused: PaneId,
}
struct Pane {
    terminal: Terminal,
    title: String,
}
#[derive(Default)]
struct AgentState {
    /// When a frontend was asked to start an owner.
    starting: Option<Instant>,
    pending: Vec<AgentRequest>,
    view: Option<AgentDrafts>,
    models: HashMap<u64, AgentModelPatch>,
}
struct Workspace {
    agent: AgentState,
    name: String,
    foreground: u32,
    background: u32,
    size: WindowSize,
    tabs: Vec<Tab>,
    active: usize,
    panes: HashMap<PaneId, Pane>,
    controller: Option<u64>,
    budget: Arc<HistoryBudget>,
    /// The pane last told it has focus (mode 1004).
    reported_focus: PaneId,
    /// Viewers need a new frame; cleared when the broadcast runs.
    dirty: bool,
}
impl Workspace {
    fn focused(&self) -> PaneId {
        self.tabs[self.active].focused
    }
    fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 1,
            cols: self.size.num_cols,
            rows: self.size.num_lines.saturating_sub(1),
        }
    }
    fn layout(&self) -> Vec<(PaneId, Rect)> {
        let mut result = Vec::new();
        self.tabs[self.active].root.layout(self.area(), &mut result);
        result
    }
    fn resize(&mut self, size: WindowSize) -> io::Result<()> {
        self.dirty = true;
        let minimum = self
            .tabs
            .iter()
            .map(|tab| tab.root.minimum())
            .fold((2, 1), |a, b| (a.0.max(b.0), a.1.max(b.1)));
        self.size = WindowSize {
            num_cols: size.num_cols.max(minimum.0),
            num_lines: size.num_lines.max(minimum.1 + 1),
            ..size
        };
        let mut layouts = Vec::new();
        for tab in &self.tabs {
            tab.root.layout(self.area(), &mut layouts);
        }
        for (id, rect) in layouts {
            self.panes
                .get_mut(&id)
                .unwrap()
                .terminal
                .resize(WindowSize {
                    num_cols: rect.cols,
                    num_lines: rect.rows,
                    ..self.size
                })?;
        }
        Ok(())
    }
    fn new_pane(
        &mut self,
        directory: Option<PathBuf>,
        program: Option<Shell>,
    ) -> io::Result<PaneId> {
        let id = NEXT_PANE.fetch_add(1, Ordering::Relaxed);
        let terminal = Terminal::new(
            program,
            directory,
            WindowSize {
                num_cols: self.size.num_cols,
                num_lines: self.size.num_lines - 1,
                ..self.size
            },
            self.foreground,
            self.background,
            self.budget.clone(),
        )?;
        self.panes.insert(
            id,
            Pane {
                terminal,
                title: "shell".into(),
            },
        );
        Ok(id)
    }
    fn new_tab(
        &mut self,
        directory: Option<PathBuf>,
        program: Option<Shell>,
    ) -> io::Result<PaneId> {
        let id = self.new_pane(directory, program)?;
        self.tabs.push(Tab {
            id,
            root: Node::Pane(id),
            focused: id,
        });
        Ok(id)
    }
    /// Ends the panes' terminals and removes them, and their tabs when they were the tabs' last
    /// panes; the survivors are laid out again.
    fn close_panes(
        &mut self,
        ids: &[PaneId],
        clients: &mut [Connection],
        closing: &mut Vec<Terminal>,
    ) -> io::Result<()> {
        for &id in ids {
            for client in clients.iter_mut() {
                client.cancel_sources(|source| source.pane == id);
                client.search.take_if(|search| search.pane == id);
            }
            let active_tab = self.tabs[self.active].id;
            closing.extend(
                self.panes
                    .remove(&id)
                    .and_then(|pane| pane.terminal.shutdown()),
            );
            self.tabs.retain_mut(|tab| {
                if matches!(tab.root, Node::Pane(pane) if pane == id) {
                    return false;
                }
                if tab.root.remove_child(id) && tab.focused == id {
                    tab.focused = tab.root.first();
                }
                true
            });
            self.active = self
                .tabs
                .iter()
                .position(|tab| tab.id == active_tab)
                .unwrap_or(self.active)
                .min(self.tabs.len().saturating_sub(1));
        }
        if self.tabs.is_empty() {
            Ok(())
        } else {
            self.resize(self.size)
        }
    }
    /// Tells applications that track focus which pane lost it and which gained it.
    fn report_focus(&mut self) -> io::Result<()> {
        if self.reported_focus == self.focused() {
            return Ok(());
        }
        self.dirty = true;
        let focused = self.focused();
        let previous = std::mem::replace(&mut self.reported_focus, focused);
        for (id, bytes) in [(previous, b"\x1b[O"), (focused, b"\x1b[I")] {
            if let Some(pane) = self.panes.get_mut(&id)
                && pane.terminal.core.mode().contains(Mode::FOCUS_IN_OUT)
            {
                pane.terminal.write(bytes)?;
            }
        }
        Ok(())
    }
    fn navigate(&mut self, direction: Direction) {
        let layout = self.layout();
        let focused = self.focused();
        let current = layout.iter().find(|(id, _)| *id == focused).unwrap().1;
        let center = |r: Rect| {
            (
                i32::from(r.x) * 2 + i32::from(r.cols),
                i32::from(r.y) * 2 + i32::from(r.rows),
            )
        };
        let (cx, cy) = center(current);
        if let Some((id, _)) = layout
            .iter()
            .filter(|(_, rect)| match direction {
                Direction::Left => rect.x + rect.cols <= current.x,
                Direction::Right => rect.x >= current.x + current.cols,
                Direction::Up => rect.y + rect.rows <= current.y,
                Direction::Down => rect.y >= current.y + current.rows,
            })
            .min_by_key(|(_, rect)| {
                let (x, y) = center(*rect);
                match direction {
                    Direction::Left | Direction::Right => (y - cy).abs() * 4 + (x - cx).abs(),
                    _ => (x - cx).abs() * 4 + (y - cy).abs(),
                }
            })
        {
            self.tabs[self.active].focused = *id;
        }
    }
}
/// Bounds the grids and retained rows a client can make the daemon allocate.
fn validate_size(size: WindowSize) -> Result<(), &'static str> {
    let (columns, lines) = (usize::from(size.num_cols), usize::from(size.num_lines));
    if !(2..=4096).contains(&columns)
        || !(2..=2048).contains(&lines)
        || columns * lines > 1 << 20
        || size.num_cols.checked_mul(size.cell_width).is_none()
        || size.num_lines.checked_mul(size.cell_height).is_none()
    {
        return Err("workspace size must be 2..4096 columns, 2..2048 rows, at most 1048576 cells");
    }
    Ok(())
}

/// Tells a worker thread to stop once the request that owns it is gone.
#[derive(Default)]
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct PaneSearch {
    pane: PaneId,
    request: u64,
    origin: Option<crate::core::SearchAnchor>,
    state: SearchState,
}
enum SearchState {
    Pending {
        receiver: mpsc::Receiver<io::Result<SearchResult>>,
        _cancel: Cancel,
    },
    Ready(SearchResult),
}
struct PendingRead {
    source: RequestSource,
    receiver: mpsc::Receiver<io::Result<HistoryPage>>,
}

struct RequestSource {
    request: u64,
    pane: PaneId,
    cancelled: Cancel,
}
struct FileJob {
    source: RequestSource,
    path: PathBuf,
    operation: FileOperation,
}
/// Results of one-shot file tasks; one eventfd wakes the reactor for all of them.
struct FileResults {
    sender: mpsc::Sender<(u64, Response)>,
    receiver: mpsc::Receiver<(u64, Response)>,
    wake: Arc<OwnedFd>,
}

struct Connection {
    id: u64,
    peer: Peer,
    workspace: Option<String>,
    /// The outstanding frame request, answered once `screen_dirty`.
    screen_request: Option<u64>,
    screen_dirty: bool,
    /// The Agent owner process.
    agent: bool,
    /// A frontend showing the Agent panel.
    agent_open: bool,
    size: WindowSize,
    frames: HashMap<PaneId, FrameState>,
    last_snapshot: Option<Snapshot>,
    failed: bool,
    copies: Vec<(bool, mpsc::Receiver<io::Result<HistoryCopy>>)>,
    /// An action held back by PTY input backpressure.
    deferred: Option<Action>,
    /// Told `Detached`; waiting for the peer to close.
    ending: bool,
    reads: Vec<PendingRead>,
    search: Option<PaneSearch>,
    // Jobs run one at a time per connection, in request order.
    files: VecDeque<FileJob>,
    file: Option<RequestSource>,
}

impl Connection {
    fn new(id: u64, stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            id,
            peer: Peer::new(stream)?,
            workspace: None,
            screen_request: None,
            screen_dirty: true,
            agent: false,
            agent_open: false,
            size: CreateOptions::default().size,
            frames: HashMap::new(),
            last_snapshot: None,
            failed: false,
            copies: Vec::new(),
            deferred: None,
            ending: false,
            reads: Vec::new(),
            search: None,
            files: VecDeque::new(),
            file: None,
        })
    }

    fn viewing(&self, workspace: &str) -> bool {
        self.workspace.as_deref() == Some(workspace)
    }

    fn workspace_of<'a>(
        &self,
        workspaces: &'a mut HashMap<String, Workspace>,
    ) -> Option<&'a mut Workspace> {
        workspaces.get_mut(self.workspace.as_ref()?)
    }

    /// Whether input would overflow the pane's PTY queue; it waits instead of failing.
    fn action_blocked(&self, action: &Action, workspaces: &HashMap<String, Workspace>) -> bool {
        let Action::Input { pane, data } = action else {
            return false;
        };
        self.workspace
            .as_ref()
            .and_then(|name| workspaces.get(name))
            .filter(|workspace| workspace.controller == Some(self.id))
            .and_then(|workspace| workspace.panes.get(pane))
            .is_some_and(|pane| pane.terminal.input_pending_bytes() + data.len() > INPUT_CAPACITY)
    }

    fn finish(&mut self) {
        self.workspace = None;
        self.ending = true;
        self.deferred = None;
        self.reads.clear();
        self.copies.clear();
        self.search = None;
        self.cancel_sources(|_| true);
        self.files.clear();
        self.respond(Response::Detached);
    }

    fn fail(&mut self, error: io::Error) {
        eprintln!("workspace client {}: {error}", self.id);
        self.failed = true;
    }

    fn respond(&mut self, response: Response) {
        if let Err(error) = self.peer.send_response(response) {
            self.fail(error);
        }
    }

    fn cancel_sources(&self, matches: impl Fn(&RequestSource) -> bool) {
        for source in self
            .file
            .iter()
            .chain(self.files.iter().map(|job| &job.source))
            .chain(self.reads.iter().map(|read| &read.source))
            .filter(|source| matches(source))
        {
            source.cancelled.0.store(true, Ordering::Relaxed);
        }
    }
}

/// Run in the hidden server process. No GUI or font libraries are needed here.
pub fn run_server(socket: &Path) -> io::Result<()> {
    let history_directory = crate::config::cache_directory()
        .ok_or_else(|| io::Error::other("HOME or XDG_CACHE_HOME is required for history"))?
        .join("history");
    let lock = private_file(socket.with_extension("lock"))?;
    lock.try_lock()?;
    // The lock shows that no server uses a socket left behind.
    if fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    fs::create_dir_all(&history_directory)?;
    fs::set_permissions(&history_directory, fs::Permissions::from_mode(0o700))?;
    let result = server_loop(listener, &history_directory);
    // Terminated panes drop their archives on the writer thread; let that finish before exit.
    crate::history::flush();
    result.and(fs::remove_file(socket))
}

fn terminate_workspace(
    workspace: Workspace,
    clients: &mut [Connection],
    closing: &mut Vec<Terminal>,
) {
    for client in clients {
        if client.viewing(&workspace.name) {
            client.finish();
        } else {
            client.cancel_sources(|source| workspace.panes.contains_key(&source.pane));
        }
    }
    closing.extend(
        workspace
            .panes
            .into_values()
            .filter_map(|pane| pane.terminal.shutdown()),
    );
}

fn agent_subscribers<'a>(
    clients: &'a mut [Connection],
    workspace: &'a str,
) -> impl Iterator<Item = &'a mut Connection> {
    clients
        .iter_mut()
        .filter(move |client| client.agent_open && client.viewing(workspace))
}

/// The Agent owner never attached: hand the queued requests back to the frontends.
fn expire_agent(workspace: &mut Workspace, clients: &mut [Connection]) {
    workspace.agent.starting = None;
    for request in std::mem::take(&mut workspace.agent.pending) {
        let (session, rewind, input, basket) = match request {
            AgentRequest::Send(send) => (send.session, false, send.input, send.basket),
            AgentRequest::Rewind { session, turn } => {
                let draft = (workspace.agent.view.as_ref())
                    .and_then(|view| view.sessions.iter().find(|draft| draft.id == session));
                let you = |message: &&AgentMessage| message.turn == turn && message.role == "You";
                let input = (workspace.agent.models.get(&session))
                    .and_then(|model| model.messages.iter().find(you))
                    .map(|message| message.text.clone())
                    .or_else(|| draft.map(|draft| draft.input.clone()))
                    .unwrap_or_default();
                let basket = draft.map(|draft| draft.basket.clone());
                (session, true, input, basket.unwrap_or_default())
            }
            AgentRequest::Abort { session } => (session, false, String::new(), Vec::new()),
        };
        let recovery = AgentRecovery {
            session,
            rewind,
            input,
            basket,
        };
        for client in agent_subscribers(clients, &workspace.name) {
            client.respond(Response::Agent(AgentEvent::Recovery(recovery.clone())));
        }
    }
    for client in agent_subscribers(clients, &workspace.name) {
        client.respond(Response::Error(
            "Agent owner did not start; queued requests were returned".into(),
        ));
    }
}

const AGENT_START_TIMEOUT: Duration = Duration::from_secs(10);

fn server_loop(listener: UnixListener, history_directory: &Path) -> io::Result<()> {
    let mut closing: Vec<Terminal> = Vec::new();
    let mut workspaces: HashMap<String, Workspace> = HashMap::new();
    let mut clients: Vec<Connection> = Vec::new();
    let mut next_client = 1u64;
    // Connections that only probe for the socket do not count: the first real client follows them.
    let mut served = false;
    let (sender, receiver) = mpsc::channel();
    let files = FileResults {
        sender,
        receiver,
        wake: Arc::new(crate::sys::new()?),
    };
    let mut fds = Vec::new();
    let mut retry_accept = false;
    loop {
        if served && workspaces.is_empty() && clients.is_empty() {
            return Ok(());
        }
        // The poll set: listener, clients, the file wakeup, then per workspace its history
        // wakeup followed by each pane's PTY and process.
        let mut timeout = if retry_accept { 100 } else { -1 };
        if clients.iter().any(|client| {
            client.failed
                || !client.ending
                    && match &client.deferred {
                        Some(action) => !client.action_blocked(action, &workspaces),
                        None => client.peer.buffered_action(),
                    }
        }) {
            timeout = 0;
        }
        fds.clear();
        fds.push(crate::sys::watch(
            listener.as_raw_fd(),
            if retry_accept { 0 } else { libc::POLLIN },
        ));
        // Even without events, a client's hangup must be seen.
        fds.extend(clients.iter().map(|client| libc::pollfd {
            fd: client.peer.fd(),
            events: if client.deferred.is_none() && !client.ending {
                libc::POLLIN
            } else {
                0
            } | if client.peer.pending() {
                libc::POLLOUT
            } else {
                0
            },
            revents: 0,
        }));
        fds.push(crate::sys::watch(files.wake.as_raw_fd(), libc::POLLIN));
        for workspace in workspaces.values() {
            fds.push(crate::sys::watch(workspace.budget.fd(), libc::POLLIN));
            if let Some(started) = workspace.agent.starting {
                timeout = crate::sys::timeout_until(timeout, started + AGENT_START_TIMEOUT);
            }
            for pane in workspace.panes.values() {
                let terminal = &pane.terminal;
                fds.extend(terminal.pollfds());
                if let Some(deadline) = terminal.deadline() {
                    timeout = crate::sys::timeout_until(timeout, deadline);
                }
            }
        }
        for terminal in &closing {
            if let Some(deadline) = terminal.deadline() {
                timeout = crate::sys::timeout_until(timeout, deadline);
            }
        }
        crate::sys::poll(&mut fds, timeout)?;
        retry_accept = false;

        let client_count = clients.len();
        if fds[1 + client_count].revents != 0 {
            crate::sys::consume_notification(files.wake.as_raw_fd())?;
        }
        // Panes, in the order they were polled.
        let mut slot = 2 + client_count;
        for (name, workspace) in &mut workspaces {
            let history = fds[slot];
            slot += 1;
            // Drained before any pane looks for replies, so a later reply raises it again.
            if history.revents != 0
                && let Err(error) = crate::sys::consume_notification(history.fd)
            {
                eprintln!("workspace {name}: history notification: {error}");
            }
            let mut exited = Vec::new();
            for (id, pane) in &mut workspace.panes {
                let (pty, pidfd) = (fds[slot].revents, fds[slot + 1].revents);
                slot += 2;
                if pty == 0
                    && pidfd == 0
                    && history.revents == 0
                    && pane
                        .terminal
                        .deadline()
                        .is_none_or(|deadline| deadline > Instant::now())
                {
                    continue;
                }
                let (mut changed, mut exit) = match pane.terminal.step(pty, pidfd) {
                    Ok(step) => (step.changed, step.done),
                    Err(error) => {
                        eprintln!("workspace {name}: terminal step failed: {error}");
                        (true, true)
                    }
                };
                for event in pane.terminal.core.take_events() {
                    match event {
                        // Replies were written back while parsing.
                        Event::Write(_) => unreachable!(),
                        Event::Title(title) => {
                            pane.title = title.unwrap_or_else(|| "shell".into());
                            changed = true;
                        }
                        Event::Exit(_) => exit = true,
                        Event::Error(error) => {
                            eprintln!("workspace {name}: {error}");
                            exit = true;
                        }
                        Event::HistoryError(error) => {
                            eprintln!("workspace {name}: {error}");
                            for client in clients
                                .iter_mut()
                                .filter(|client| !client.agent && client.viewing(name))
                            {
                                client.respond(Response::Error(error.to_string()));
                            }
                            changed = true;
                        }
                        Event::ClipboardStore { primary, text } => {
                            let Some(controller) = clients
                                .iter_mut()
                                .find(|client| Some(client.id) == workspace.controller)
                            else {
                                continue;
                            };
                            // Streamed like a copy, so it may exceed a frame.
                            let file = crate::sys::memfd(c"nextty-osc52").and_then(|mut file| {
                                file.write_all(text.as_bytes())?;
                                file.rewind().map(|()| file)
                            });
                            match file {
                                Ok(file) => controller.respond(Response::Clipboard {
                                    primary,
                                    file,
                                    incomplete: false,
                                }),
                                Err(error) => eprintln!("workspace {name}: OSC 52: {error}"),
                            }
                        }
                    }
                }
                if exit {
                    exited.push(*id);
                }
                workspace.dirty |= changed || exit;
            }
            if !exited.is_empty()
                && let Err(error) = workspace.close_panes(&exited, &mut clients, &mut closing)
            {
                eprintln!("workspace {name}: resize failed: {error}");
            }
        }

        for index in 0..client_count {
            let revents = fds[1 + index].revents;
            let client = &mut clients[index];
            if client.failed {
                continue;
            }
            if revents & (libc::POLLERR | libc::POLLNVAL) != 0
                || revents & libc::POLLHUP != 0 && (client.deferred.is_some() || client.ending)
            {
                client.failed = true;
                continue;
            }
            if client.ending
                || revents & (libc::POLLIN | libc::POLLHUP) == 0
                    && client.deferred.is_none()
                    && !client.peer.buffered_action()
            {
                continue;
            }
            for _ in 0..256 {
                let client = &mut clients[index];
                let was_deferred = client.deferred.is_some();
                let action = match client.deferred.take() {
                    Some(action) => action,
                    None => match client.peer.next_action() {
                        Ok(Some(action)) => action,
                        Ok(None) => break,
                        Err(error) => {
                            client.fail(error);
                            break;
                        }
                    },
                };
                served = true;
                if client.action_blocked(&action, &workspaces) {
                    // Applied input changes the input generation, which viewers must see even
                    // while the rest waits for the PTY.
                    if !was_deferred && let Some(workspace) = client.workspace_of(&mut workspaces) {
                        workspace.dirty = true;
                    }
                    client.deferred = Some(action);
                    break;
                }
                if let Err(error) = apply_action(
                    index,
                    action,
                    &mut clients,
                    &mut workspaces,
                    history_directory,
                    &mut closing,
                ) {
                    clients[index].respond(Response::Error(error.to_string()));
                }
                if clients[index].failed || !clients[index].peer.buffered_action() {
                    break;
                }
            }
        }
        // A workspace that lost its last tab is gone, whether a pane exited or a client closed it.
        for (_, workspace) in workspaces.extract_if(|_, workspace| workspace.tabs.is_empty()) {
            terminate_workspace(workspace, &mut clients, &mut closing);
        }
        complete_requests(&mut clients, &files);
        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => match Connection::new(next_client, stream) {
                        Ok(client) => {
                            clients.push(client);
                            next_client += 1;
                        }
                        Err(error) => eprintln!("workspace client setup failed: {error}"),
                    },
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        eprintln!("workspace accept failed: {error}");
                        retry_accept = true;
                        break;
                    }
                }
            }
        }

        // Finishes the terminals that have exited since they were told to close.
        closing.retain_mut(|terminal| match terminal.step(0, 0) {
            Ok(step) => !step.done,
            Err(error) => {
                eprintln!("terminal close: {error}");
                false
            }
        });
        for client in clients.iter_mut().filter(|client| client.failed) {
            let workspace = client.workspace_of(&mut workspaces);
            close_search(client, workspace);
        }
        clients.retain(|client| !client.failed);
        // Frames wait for a client whose earlier replies are still unsent.
        flush_clients(&mut clients);
        for (name, workspace) in &mut workspaces {
            if (workspace.agent.starting)
                .is_some_and(|started| started.elapsed() >= AGENT_START_TIMEOUT)
            {
                expire_agent(workspace, &mut clients);
            }
            if workspace.controller.is_some_and(|id| {
                !clients
                    .iter()
                    .any(|client| client.id == id && client.viewing(name))
            }) {
                let controller = clients
                    .iter()
                    .rev()
                    .find(|client| !client.agent && client.viewing(name));
                workspace.controller = controller.map(|client| client.id);
                if let Some(controller) = controller
                    && let Err(error) = workspace.resize(controller.size)
                {
                    eprintln!("workspace {name}: resize failed: {error}");
                }
                workspace.dirty = true;
            }
            if let Err(error) = workspace.report_focus() {
                eprintln!("workspace {name}: focus report failed: {error}");
            }
            let changed = std::mem::take(&mut workspace.dirty);
            broadcast(workspace, &mut clients, changed);
        }
        flush_clients(&mut clients);
    }
}

fn flush_clients(clients: &mut [Connection]) {
    for client in clients.iter_mut().filter(|client| !client.failed) {
        if let Err(error) = client.peer.flush() {
            client.fail(error);
        }
    }
}

/// Answers the requests whose background work has finished and starts each connection's next
/// file job.
fn complete_requests(clients: &mut [Connection], files: &FileResults) {
    while let Ok((id, response)) = files.receiver.try_recv() {
        if let Some(client) = clients.iter_mut().find(|client| client.id == id) {
            client.file = None;
            if !client.ending {
                client.respond(response);
            }
        }
    }
    let mut responses = Vec::new();
    for client in clients.iter_mut().filter(|client| !client.failed) {
        if client.file.is_none()
            && let Some(FileJob {
                source,
                path,
                operation,
            }) = client.files.pop_front()
        {
            let (id, request, cancelled) = (client.id, source.request, source.cancelled.0.clone());
            let (sender, wake) = (files.sender.clone(), files.wake.clone());
            client.file = Some(source);
            let started = crate::sys::task(move || {
                let result = crate::agent_files::execute(path, operation, &cancelled)
                    .map_err(|error| error.to_string());
                if sender
                    .send((id, Response::AgentFile { request, result }))
                    .is_ok()
                {
                    crate::sys::notify(wake.as_raw_fd());
                }
            });
            if let Err(error) = started {
                client.file = None;
                let result = Err(error.to_string());
                client.respond(Response::AgentFile { request, result });
            }
        }
        client.copies.retain(|(primary, receiver)| {
            let Some(result) = worker_result(receiver, "history copy worker disconnected") else {
                return true;
            };
            responses.push(match result {
                Ok(copy) => Response::Clipboard {
                    primary: *primary,
                    file: copy.file,
                    incomplete: copy.incomplete,
                },
                Err(error) => Response::Error(error.to_string()),
            });
            false
        });
        client.reads.retain(|read| {
            let result = if read.source.cancelled.0.load(Ordering::Relaxed) {
                Err("history source pane no longer exists".into())
            } else if let Some(result) =
                worker_result(&read.receiver, "history reader disconnected")
            {
                result.map_err(|error| error.to_string())
            } else {
                return true;
            };
            responses.push(Response::History {
                request: read.source.request,
                result,
            });
            false
        });
        for response in responses.drain(..) {
            client.respond(response);
        }
        if let Some(search) = &mut client.search
            && let SearchState::Pending { receiver, .. } = &search.state
            && let Some(result) = worker_result(receiver, "pane search worker stopped")
        {
            let (pane, request) = (search.pane, search.request);
            let response = |count, truncated| Response::Search {
                pane,
                request,
                count,
                truncated,
            };
            match result {
                Ok(result) => {
                    let response = response(result.matches.len() as u32, result.truncated);
                    search.state = SearchState::Ready(result);
                    client.respond(response);
                }
                Err(error) => {
                    client.search = None;
                    client.respond(response(0, true));
                    client.respond(Response::Error(error.to_string()));
                }
            }
        }
    }
}

fn worker_result<T>(
    receiver: &mpsc::Receiver<io::Result<T>>,
    disconnected: &'static str,
) -> Option<io::Result<T>> {
    match receiver.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err(io::Error::other(disconnected))),
    }
}

/// How a pane's command state shows on its tab.
fn pane_activity(core: &Core, process: &crate::terminal::ProcessMetadata) -> TabActivity {
    let foreground_running = process
        .foreground_pgid
        .is_some_and(|pgid| pgid != process.child_pid);
    let latest = core.latest_command_status();
    if foreground_running
        || latest.is_none() && process.has_children == Some(true)
        || latest == Some(None)
    {
        TabActivity::Running
    } else if matches!(latest, Some(Some(status)) if status != 0) {
        TabActivity::Failed
    } else if latest.is_some() {
        TabActivity::Finished
    } else if process.has_children.is_none()
        || core.shell_prompt().is_none() && !core.shell_editing()
    {
        TabActivity::Unknown
    } else {
        TabActivity::Idle
    }
}

/// Sends every viewer with a pending request the panes that changed since its last frame. The
/// Agent owner sees all tabs; frontends see the active one.
fn broadcast(workspace: &mut Workspace, clients: &mut [Connection], changed: bool) {
    let viewing = |client: &Connection| client.viewing(&workspace.name) && !client.failed;
    if changed {
        for client in clients
            .iter_mut()
            .filter(|client| viewing(client) && !client.screen_dirty)
        {
            client.screen_dirty = true;
            if client.screen_request.is_none() {
                client.respond(Response::ScreenDirty);
            }
        }
    }
    let due = |client: &Connection| {
        viewing(client)
            && client.screen_request.is_some()
            && client.screen_dirty
            && client.peer.frame_ready()
    };
    if !clients.iter().any(due) {
        return;
    }
    let all_tabs = clients.iter().any(|client| client.agent && due(client));
    let mut panes = Vec::new();
    let mut tabs = Vec::new();
    let mut layout = Vec::new();
    for (index, tab) in workspace.tabs.iter().enumerate() {
        layout.clear();
        tab.root.layout(workspace.area(), &mut layout);
        let mut activity = TabActivity::Idle;
        let mut title = String::new();
        for &(id, rect) in &layout {
            let pane = &workspace.panes[&id];
            let core = &pane.terminal.core;
            let process = pane.terminal.process_metadata();
            activity = activity.max(pane_activity(core, &process));
            let cwd = pane.terminal.current_directory();
            if id == tab.focused {
                title = match &cwd {
                    Some(directory) => (directory.file_name())
                        .unwrap_or(directory.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    None => pane.title.clone(),
                };
            }
            if all_tabs || index == workspace.active {
                panes.push(PaneView {
                    id,
                    tab: tab.id,
                    rect,
                    title: pane.title.clone(),
                    cwd,
                    process,
                    blocks: core.visible_blocks(),
                    command_blocks: core.recent_command_blocks(),
                    input_generation: core.input_generation(),
                    shell_prompt: core.shell_prompt(),
                });
            }
        }
        tabs.push(TabView {
            id: tab.id,
            title,
            activity,
        });
    }
    let active_tab = workspace.tabs[workspace.active].id;
    let mut all = Snapshot {
        name: workspace.name.clone(),
        controller: false,
        focused: workspace.focused(),
        active_tab,
        tabs,
        panes,
    };
    let mut active = None;
    for client in clients.iter_mut().filter(|client| due(client)) {
        let snapshot = if client.agent || !all_tabs {
            &mut all
        } else {
            active.get_or_insert_with(|| Snapshot {
                panes: (all.panes.iter())
                    .filter(|pane| pane.tab == active_tab)
                    .cloned()
                    .collect(),
                tabs: all.tabs.clone(),
                name: all.name.clone(),
                ..all
            })
        };
        snapshot.controller = workspace.controller == Some(client.id);
        client.frames.retain(|id, _| snapshot.pane(*id).is_some());
        let mut updates = Vec::with_capacity(snapshot.panes.len());
        for pane in &snapshot.panes {
            let core = &workspace.panes[&pane.id].terminal.core;
            if !client.agent && core.mode().contains(Mode::SYNC_UPDATE) {
                continue;
            }
            let update = if client.agent && core.display_offset() != 0 {
                Update::capture_live(core)
            } else {
                Update::capture_since(core, client.frames.get(&pane.id))
            };
            updates.push((pane.id, update));
            client.frames.insert(pane.id, FrameState::new(core));
        }
        let send = client.last_snapshot.as_ref() != Some(&*snapshot);
        if send {
            client.last_snapshot = Some(snapshot.clone());
        }
        let request = client.screen_request.take().unwrap();
        if let Err(error) = client
            .peer
            .send_state(send.then_some(&*snapshot), &updates, request)
        {
            client.fail(error);
        }
        client.screen_dirty = false;
    }
    // The owner also reads panes mid-update, so every broadcast starts a new damage revision.
    for pane in workspace.panes.values_mut() {
        pane.terminal.core.reset_damage();
    }
}

const NOT_ATTACHED: &str = "attach to a workspace first";
const NO_WORKSPACE: &str = "workspace does not exist";

fn apply_action(
    index: usize,
    action: Action,
    clients: &mut [Connection],
    workspaces: &mut HashMap<String, Workspace>,
    history_root: &Path,
    closing: &mut Vec<Terminal>,
) -> Result<(), Failure> {
    let client_id = clients[index].id;
    match action {
        Action::ScreenRequest { request, wait } => {
            let client = &mut clients[index];
            client.screen_request = Some(request);
            client.screen_dirty |= !wait;
        }
        Action::Cancel { request } => {
            let client = &mut clients[index];
            client.reads.retain(|read| read.source.request != request);
            client.cancel_sources(|source| source.request == request);
        }
        Action::AgentAttach { name } => {
            let workspace = workspaces.get_mut(&name).ok_or(NO_WORKSPACE)?;
            if clients
                .iter()
                .any(|client| client.agent && client.viewing(&name))
            {
                return Err("the workspace already has an Agent owner".into());
            }
            workspace.agent.starting = None;
            workspace.dirty = true;
            let client = &mut clients[index];
            client.agent = true;
            client.workspace = Some(name);
            if let Some(payload) = &workspace.agent.view {
                client.respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
            for request in workspace.agent.pending.drain(..) {
                client.respond(Response::Agent(AgentEvent::Request(request)));
            }
        }
        Action::List => {
            let mut list: Vec<_> = workspaces
                .values()
                .map(|workspace| WorkspaceInfo {
                    name: workspace.name.clone(),
                    tabs: workspace.tabs.len(),
                    panes: workspace.panes.len(),
                    attached: clients
                        .iter()
                        .filter(|client| !client.agent && client.viewing(&workspace.name))
                        .count(),
                })
                .collect();
            list.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            clients[index].respond(Response::List(list));
        }
        Action::Create { mut name, options } => {
            if name.is_empty() {
                name = (0..)
                    .map(|number: usize| number.to_string())
                    .find(|number| !workspaces.contains_key(number))
                    .unwrap();
            }
            validate_size(options.size)?;
            if workspaces.contains_key(&name) {
                return Err("workspace already exists".into());
            }
            // Relative directories, and none, start from the server's HOME: the remote one over SSH.
            let directory = match options.directory {
                Some(path) if path.is_absolute() => path,
                relative => {
                    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
                    home.join(relative.unwrap_or_default())
                }
            };
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let budget = HistoryBudget::new(
                history_root.join(format!("workspace-{}-{timestamp}", std::process::id())),
                options.history_capacity,
            )?;
            let mut workspace = Workspace {
                agent: AgentState::default(),
                name: name.clone(),
                size: options.size,
                foreground: options.foreground,
                background: options.background,
                tabs: Vec::new(),
                active: 0,
                panes: HashMap::new(),
                controller: Some(client_id),
                budget,
                reported_focus: 0,
                dirty: true,
            };
            workspace.new_tab(Some(directory), options.program)?;
            workspaces.insert(name.clone(), workspace);
            let client = &mut clients[index];
            client.workspace = Some(name);
            client.agent_open = false;
            client.size = options.size;
        }
        Action::Attach { name } => {
            let workspace = workspaces.get_mut(&name).ok_or(NO_WORKSPACE)?;
            close_search(&mut clients[index], Some(workspace));
            // Searches by the previous controller moved its view; it returns to the view first.
            for client in clients.iter_mut().filter(|client| {
                client.viewing(&name)
                    && (client.search.as_ref()).is_some_and(|search| search.origin.is_some())
            }) {
                close_search(client, Some(workspace));
            }
            workspace.controller = Some(client_id);
            workspace.dirty = true;
            clients[index].workspace = Some(name);
            clients[index].agent_open = false;
        }
        action @ (Action::Terminate | Action::TerminateName(_)) => {
            let notify_requester = clients[index].workspace.is_none();
            let name = match action {
                Action::TerminateName(name) => name,
                _ => clients[index].workspace.clone().ok_or(NOT_ATTACHED)?,
            };
            let workspace = workspaces.remove(&name).ok_or(NO_WORKSPACE)?;
            terminate_workspace(workspace, clients, closing);
            if notify_requester {
                clients[index].respond(Response::Detached);
            }
        }
        action => {
            // A workspace that lost its last tab earlier in this round is removed at the end of it.
            let workspace = (clients[index].workspace_of(workspaces))
                .filter(|workspace| !workspace.tabs.is_empty())
                .ok_or(NOT_ATTACHED)?;
            workspace_action(index, action, workspace, clients, closing)?;
        }
    }
    Ok(())
}

/// An action on the workspace that `clients[index]` views. The latest frontend controls the
/// workspace; earlier ones only view it and copy from it. The Agent owner may only use the Agent
/// actions. Whatever a guard rejects reaches the last arm.
fn workspace_action(
    index: usize,
    action: Action,
    workspace: &mut Workspace,
    clients: &mut [Connection],
    closing: &mut Vec<Terminal>,
) -> Result<(), Failure> {
    let controller = workspace.controller == Some(clients[index].id);
    let agent = controller || clients[index].agent;
    match action {
        Action::Resize(size) => {
            validate_size(size)?;
            clients[index].size = size;
            if controller && size != workspace.size {
                workspace.resize(size)?;
            }
        }
        Action::Input { pane, data } if controller => {
            let terminal = &mut pane_ref(workspace, pane)?.terminal;
            let focus_report = data == b"\x1b[I" || data == b"\x1b[O";
            if !focus_report {
                terminal.core.note_input();
            }
            terminal.try_write(data)?;
            if !focus_report {
                let core = &mut terminal.core;
                workspace.dirty |= core.clear_selection() | core.scroll_display(Scroll::Bottom);
            }
        }
        Action::AgentInput {
            pane,
            request,
            generation,
            prompt,
            data,
        } if agent => {
            let result = agent_input(workspace, pane, generation, prompt, data)
                .map_err(|error| error.to_string());
            clients[index].respond(Response::AgentInput { request, result });
            workspace.dirty = true;
        }
        Action::AgentFile {
            pane,
            request,
            path,
            operation,
        } if agent => {
            let path = pane_ref(workspace, pane).and_then(|pane| {
                if path.is_absolute() {
                    return Ok(path);
                }
                let directory = pane.terminal.current_directory();
                Ok(directory.ok_or("pane directory is unknown")?.join(path))
            });
            match path {
                Ok(path) => clients[index].files.push_back(FileJob {
                    source: RequestSource {
                        request,
                        pane,
                        cancelled: Cancel::default(),
                    },
                    path,
                    operation,
                }),
                Err(error) => {
                    let result = Err(error.to_string());
                    clients[index].respond(Response::AgentFile { request, result });
                }
            }
        }
        Action::AgentNewTab { pane, request } if agent => {
            let result = pane_ref(workspace, pane)
                .map_err(Failure::from)
                .map(|pane| pane.terminal.current_directory())
                .and_then(|directory| Ok(workspace.new_tab(directory, None)?))
                .map_err(|error| error.to_string());
            workspace.dirty |= result.is_ok();
            clients[index].respond(Response::AgentNewTab { request, result });
        }
        Action::ResizeSplit {
            first,
            second,
            position,
        } if controller => {
            let rect = workspace.area();
            let root = &mut workspace.tabs[workspace.active].root;
            if root.resize_split(rect, first, second, position) {
                workspace.resize(workspace.size)?;
            }
        }
        Action::Split { axis } if controller => {
            let previous = workspace.focused();
            let layout = workspace.layout();
            let rect = layout.iter().find(|(id, _)| *id == previous).unwrap().1;
            if match axis {
                Axis::LeftRight => rect.cols < 5,
                Axis::TopBottom => rect.rows < 3,
            } {
                return Err("pane is too small to split".into());
            }
            let directory = workspace.panes[&previous].terminal.current_directory();
            let new = workspace.new_pane(directory, None)?;
            let tab = &mut workspace.tabs[workspace.active];
            tab.root.split(previous, new, axis);
            tab.focused = new;
            workspace.resize(workspace.size)?;
        }
        Action::NewTab { pane } if controller => {
            let directory = pane_ref(workspace, pane)?.terminal.current_directory();
            workspace.new_tab(directory, None)?;
            workspace.active = workspace.tabs.len() - 1;
        }
        Action::Focus(pane) if controller => {
            let tab = &mut workspace.tabs[workspace.active];
            if !tab.root.contains(pane) {
                return Err("pane is not visible".into());
            }
            tab.focused = pane;
        }
        Action::Navigate(direction) if controller => workspace.navigate(direction),
        Action::Tab(delta) if controller => {
            workspace.active = (workspace.active as i64 + i64::from(delta))
                .rem_euclid(workspace.tabs.len() as i64) as usize;
        }
        Action::ClosePane(pane) if controller => {
            pane_ref(workspace, pane)?;
            workspace.close_panes(&[pane], clients, closing)?;
        }
        Action::CloseTab(id) if controller => {
            let tab = workspace.tabs.iter().find(|tab| tab.id == id);
            let mut layout = Vec::new();
            tab.ok_or("tab no longer exists")?
                .root
                .layout(workspace.area(), &mut layout);
            let panes: Vec<_> = layout.into_iter().map(|(pane, _)| pane).collect();
            workspace.close_panes(&panes, clients, closing)?;
        }
        Action::Scroll { pane, scroll } if controller => {
            let core = &mut pane_ref(workspace, pane)?.terminal.core;
            workspace.dirty |= core.scroll_display(scroll);
        }
        Action::SelectionStart {
            pane,
            point,
            side,
            kind,
        } if controller => {
            validate_point(&pane_ref(workspace, pane)?.terminal.core, point)?;
            for (id, other) in &mut workspace.panes {
                let core = &mut other.terminal.core;
                if *id == pane {
                    core.selection_start(point, side, kind);
                } else {
                    core.clear_selection();
                }
            }
            workspace.dirty = true;
        }
        Action::SelectionUpdate { pane, point, side } if controller => {
            let core = &mut pane_ref(workspace, pane)?.terminal.core;
            validate_point(core, point)?;
            workspace.dirty |= core.selection_update(point, side);
        }
        Action::PromptJump { pane, previous } if controller => {
            let core = &mut pane_ref(workspace, pane)?.terminal.core;
            workspace.dirty |= core.prompt_jump(previous);
        }
        Action::CopySelection { pane, primary } => {
            let receiver = pane_ref(workspace, pane)?.terminal.core.copy_selection();
            clients[index].copies.push((primary, receiver));
        }
        Action::CopyBlock { pane, block } => {
            let receiver = pane_ref(workspace, pane)?.terminal.core.copy_block(block);
            clients[index].copies.push((false, receiver));
        }
        Action::ReadHistory {
            pane,
            request,
            block,
            cursor,
            query,
        } => match pane_ref(workspace, pane) {
            Ok(source) => {
                let core = &mut source.terminal.core;
                let (receiver, cancelled) = core.read_history(block, cursor, query.as_deref());
                clients[index].reads.push(PendingRead {
                    source: RequestSource {
                        request,
                        pane,
                        cancelled: Cancel(cancelled),
                    },
                    receiver,
                });
            }
            Err(error) => clients[index].respond(Response::History {
                request,
                result: Err(error.into()),
            }),
        },
        Action::Search {
            pane,
            request,
            query,
        } => {
            clients[index].search.take_if(|search| search.pane != pane);
            let core = &mut pane_ref(workspace, pane)?.terminal.core;
            let search = clients[index].search.get_or_insert_with(|| PaneSearch {
                pane,
                request,
                origin: controller.then(|| core.search_anchor()),
                state: SearchState::Ready(SearchResult::default()),
            });
            search.request = request;
            if query.is_empty() {
                if let Some(origin) = search.origin {
                    core.restore_search(origin);
                }
                search.state = SearchState::Ready(SearchResult::default());
                clients[index].respond(Response::Search {
                    pane,
                    request,
                    count: 0,
                    truncated: false,
                });
            } else {
                // Replacing a pending search cancels it.
                let cancel = Cancel::default();
                let receiver = core.search(&query, &cancel.0);
                search.state = SearchState::Pending {
                    receiver,
                    _cancel: cancel,
                };
            }
            workspace.dirty = true;
        }
        Action::SearchSelect {
            pane,
            request,
            index: found,
        } if controller => {
            if let Some(search) = &clients[index].search
                && (search.pane, search.request) == (pane, request)
                && let SearchState::Ready(result) = &search.state
            {
                let core = &mut pane_ref(workspace, pane)?.terminal.core;
                core.search_select(result, found as usize)?;
            }
            workspace.dirty = true;
        }
        Action::SearchCancel { pane, request } => {
            if (clients[index].search.as_ref())
                .is_some_and(|search| (search.pane, search.request) == (pane, request))
            {
                close_search(&mut clients[index], Some(workspace));
            }
            workspace.dirty = true;
        }
        Action::AgentOpen => {
            let client = &mut clients[index];
            client.agent_open = true;
            if let Some(payload) = &workspace.agent.view {
                client.respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
            for model in workspace.agent.models.values() {
                client.respond(Response::Agent(AgentEvent::Model(model.clone())));
            }
            client.respond(Response::Agent(AgentEvent::Ready));
        }
        Action::AgentView(payload) if agent => {
            let view = workspace.agent.view.get_or_insert_default();
            if payload.complete {
                view.clone_from(&payload);
            } else {
                view.complete = true;
                view.next_id = view.next_id.max(payload.next_id);
                for draft in &payload.sessions {
                    match view
                        .sessions
                        .iter_mut()
                        .find(|session| session.id == draft.id)
                    {
                        Some(existing) => existing.clone_from(draft),
                        None => view.sessions.push(draft.clone()),
                    }
                }
            }
            // The owner only tracks the session list, which complete snapshots carry.
            let id = clients[index].id;
            for client in clients.iter_mut().filter(|client| {
                client.id != id
                    && client.viewing(&workspace.name)
                    && (client.agent_open || client.agent && payload.complete)
            }) {
                client.respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
        }
        Action::AgentRequest(payload) if agent => {
            let owner = clients
                .iter_mut()
                .find(|client| client.agent && !client.failed && client.viewing(&workspace.name));
            if let Some(owner) = owner {
                owner.respond(Response::Agent(AgentEvent::Request(payload)));
            } else {
                workspace.agent.pending.push(payload);
                if workspace.agent.starting.is_none() {
                    workspace.agent.starting = Some(Instant::now());
                    clients[index].respond(Response::Agent(AgentEvent::Start));
                }
            }
        }
        Action::AgentUpdate(patch) if agent => {
            let model = workspace.agent.models.entry(patch.session).or_default();
            patch.apply_messages(&mut model.messages)?;
            if patch.changes.is_some() {
                model.changes.clone_from(&patch.changes);
            }
            model.session = patch.session;
            model.busy = patch.busy;
            model.changes_omitted = patch.changes_omitted;
            for client in agent_subscribers(clients, &workspace.name) {
                client.respond(Response::Agent(AgentEvent::Model(patch.clone())));
            }
        }
        Action::AgentRecover(recovery) if agent => {
            for client in agent_subscribers(clients, &workspace.name) {
                client.respond(Response::Agent(AgentEvent::Recovery(recovery.clone())));
            }
        }
        Action::AgentRemove(session) if agent => {
            workspace.agent.models.remove(&session);
        }
        _ => return Err("this frontend is read-only; attach again to take control".into()),
    }
    Ok(())
}

fn pane_ref(workspace: &mut Workspace, id: PaneId) -> Result<&mut Pane, &'static str> {
    workspace.panes.get_mut(&id).ok_or("pane does not exist")
}

/// Writes Agent input only while the terminal is still in the state the Agent observed.
fn agent_input(
    workspace: &mut Workspace,
    pane: PaneId,
    generation: u64,
    prompt: Option<u64>,
    data: Vec<u8>,
) -> Result<Option<u64>, Failure> {
    let terminal = &mut pane_ref(workspace, pane)?.terminal;
    let core = &mut terminal.core;
    if core.input_generation() != generation {
        return Err("Terminal input changed; observe it again before sending input".into());
    }
    if core.shell_editing() {
        return Err("The shell already has pending input".into());
    }
    if prompt.is_some_and(|prompt| core.shell_prompt() != Some(prompt)) {
        return Err("The shell is not at an untouched prompt".into());
    }
    core.note_input();
    terminal.try_write(data)?;
    Ok(prompt)
}

/// Ends the client's search, returning a view that it moved to where it was.
fn close_search(client: &mut Connection, workspace: Option<&mut Workspace>) {
    if let Some(search) = client.search.take()
        && let Some(origin) = search.origin
        && let Some(pane) = workspace.and_then(|workspace| workspace.panes.get_mut(&search.pane))
    {
        pane.terminal.core.restore_search(origin);
    }
}

fn validate_point(core: &Core, point: Point) -> Result<(), &'static str> {
    if point.column >= core.columns()
        || i64::from(point.line) < -(core.history_size() as i64)
        || point.line >= core.screen_lines() as i32
    {
        Err("selection point is outside the pane")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
