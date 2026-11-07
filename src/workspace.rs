//! A workspace owns its PTYs independently of any particular frontend.
pub use crate::agent_files::{AgentChange, FileMatch, FileOperation, FileResult};
pub use crate::core::{BlockMarker, CommandBlock};
use crate::core::{Core, Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::history::{HistoryBudget, HistoryConfig, HistoryCopy};
pub use crate::history::{HistoryCursor, HistoryPage};
use crate::screen::{Screen, Update};
use crate::terminal::{Event, INPUT_CAPACITY, Shell, Terminal};
use crate::wire::{Peer, Response};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

#[cfg(any(feature = "gui", feature = "tui"))]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type PaneId = u64;
pub type TabId = u64;
// Requests can outlive their workspace, so a new pane must not reuse its identity.
static NEXT_PANE: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_COLUMNS: usize = 4096;
pub(crate) const MAX_LINES: usize = 2048;
pub(crate) const MAX_CELLS: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Axis {
    LeftRight = 0,
    TopBottom = 1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    Left = 0,
    Down = 1,
    Up = 2,
    Right = 3,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub size: WindowSize,
    pub foreground: u32,
    pub background: u32,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMessage {
    pub turn: u64,
    pub role: String,
    pub text: String,
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentModelPatch {
    pub session: u64,
    pub busy: bool,
    pub revision: u64,
    pub turn: u64,
    pub start: u32,
    pub prefix_bytes: u32,
    pub messages: Vec<AgentMessage>,
    pub recovery: Vec<u8>,
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Agent message patch",
            ));
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    Draft(Vec<u8>),
    Model(AgentModelPatch),
    Request(Vec<u8>),
    Start(Vec<u8>),
    Attached,
    Ready,
}

#[derive(Clone, Debug)]
pub enum Action {
    ScreenRequest {
        request: u64,
        wait: bool,
    },
    AgentOpen,
    AgentView(Vec<u8>),
    AgentRequest(Vec<u8>),
    AgentAttach {
        name: String,
        token: Vec<u8>,
    },
    AgentUpdate(AgentModelPatch),
    AgentRemove(u64),
    Create {
        /// An empty name requests the lowest available nonnegative integer.
        name: String,
        options: CreateOptions,
    },
    Attach {
        name: String,
        size: WindowSize,
    },
    List,
    Input {
        pane: PaneId,
        data: Vec<u8>,
    },
    AgentInput {
        pane: PaneId,
        request: u64,
        generation: u64,
        prompt: Option<u64>,
        data: Vec<u8>,
    },
    AgentFile {
        pane: PaneId,
        request: u64,
        path: PathBuf,
        operation: FileOperation,
    },
    CancelAgentFile {
        request: u64,
    },
    AgentNewTab {
        pane: PaneId,
        request: u64,
    },
    Resize(WindowSize),
    Split {
        /// Zero selects the focused pane when the server executes the action.
        pane: PaneId,
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
    SelectionClear {
        pane: PaneId,
    },
    CopySelection {
        pane: PaneId,
        request: u64,
    },
    PromptJump {
        pane: PaneId,
        previous: bool,
    },
    CopyBlock {
        pane: PaneId,
        block: u64,
        request: u64,
    },
    ReadHistory {
        pane: PaneId,
        request: u64,
        block: Option<u64>,
        cursor: Option<HistoryCursor>,
        query: Option<String>,
    },
    CancelHistory {
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
    Detach,
}
#[derive(Clone, Debug)]
pub struct WorkspaceInfo {
    pub name: String,
    pub tabs: usize,
    pub panes: usize,
    pub attached: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum TabActivity {
    #[default]
    Idle,
    Running,
    Finished,
    Failed,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct TabView {
    pub id: TabId,
    pub title: String,
    pub activity: TabActivity,
}
#[derive(Clone, Debug)]
pub struct PaneView {
    pub id: PaneId,
    pub tab: TabId,
    pub rect: Rect,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub process: Arc<crate::terminal::ProcessMetadata>,
    pub blocks: Vec<BlockMarker>,
    pub command_blocks: Vec<CommandBlock>,
    pub input_generation: u64,
    pub shell_prompt: Option<u64>,
}
#[derive(Clone, Debug)]
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

    pub(crate) fn matching_pane(&self, index: usize, id: PaneId) -> Option<&PaneView> {
        self.panes
            .get(index)
            .filter(|pane| pane.id == id)
            .or_else(|| self.pane(id))
    }

    #[cfg(any(feature = "gui", feature = "tui"))]
    pub(crate) fn pane_at(&self, column: u16, row: u16) -> Option<&PaneView> {
        self.panes
            .iter()
            .find(|pane| pane.rect.contains(column, row))
    }

    #[cfg(any(feature = "gui", feature = "tui"))]
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

pub type TabLayout = Vec<(usize, u16, u16)>;

pub fn tab_layout(snapshot: &Snapshot, columns: u16, right_label: &str) -> TabLayout {
    let text_width = |text: &str, limit: u16| {
        let prefix = crate::width::printable_ascii_prefix(
            &text.as_bytes()[..text.len().min(usize::from(limit))],
        );
        let mut width = prefix as u16;
        for ch in text[prefix..].chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            width = width.saturating_add(u16::from(crate::width::scalar_width(ch).min(2)));
            if width >= limit {
                return limit;
            }
        }
        width
    };
    let active = snapshot
        .tabs
        .iter()
        .position(|tab| tab.id == snapshot.active_tab)
        .unwrap_or(0);
    let active_width = snapshot
        .tabs
        .get(active)
        .map(|tab| text_width(&tab.title, 22) + 8);
    let minimum = if snapshot.controller { 0 } else { 12 };
    let reserve = text_width(right_label, columns)
        .saturating_add(3)
        .min(columns.saturating_sub(5 + active_width.unwrap_or(9)))
        .max(minimum.min(columns.saturating_sub(5 + 9)));
    let available = columns.saturating_sub(5 + reserve);
    let tab_width = |title: &str, available| (text_width(title, 22) + 8).min(available);
    let mut start = active;
    let mut used = active_width.unwrap_or(0).min(available);
    while start > 0 {
        let width = tab_width(&snapshot.tabs[start - 1].title, available);
        if used.saturating_add(width + 1) > available {
            break;
        }
        used += width + 1;
        start -= 1;
    }
    let mut x = 3;
    let mut layout = Vec::new();
    for (index, tab) in snapshot.tabs.iter().enumerate().skip(start) {
        let remaining = (3 + available).saturating_sub(x);
        let width = if index == active {
            active_width.unwrap_or(0).min(remaining)
        } else {
            tab_width(&tab.title, remaining)
        };
        if width < 9 {
            break;
        }
        layout.push((index, x, width));
        x += width + 1;
    }
    layout
}
pub fn tab_chrome<'a>(
    snapshot: &crate::workspace::Snapshot,
    columns: u16,
    base: &'a str,
) -> (TabLayout, Cow<'a, str>, usize, usize) {
    let mut tabs = tab_layout(snapshot, columns, base);
    let counts = |tabs: &[(usize, u16, u16)]| {
        (
            tabs.first().map_or(0, |tab| tab.0),
            tabs.last()
                .map_or(0, |tab| snapshot.tabs.len().saturating_sub(tab.0 + 1)),
        )
    };
    let label = |before, after| {
        let mut label = match (before, after) {
            (0, 0) => return Cow::Borrowed(base),
            (0, _) => format!("{after}›"),
            (_, 0) => format!("‹{before}"),
            _ => format!("‹{before} {after}›"),
        };
        if !base.is_empty() {
            label.push(' ');
            label.push_str(base);
        }
        Cow::Owned(label)
    };
    let initial = counts(&tabs);
    let text = label(initial.0, initial.1);
    if initial != (0, 0) {
        tabs = tab_layout(snapshot, columns, &text);
    }
    let (before, after) = counts(&tabs);
    let text = if (before, after) == initial {
        text
    } else {
        drop(text);
        label(before, after)
    };
    (tabs, text, before, after)
}

pub enum ClientEvent {
    Updated,
    AgentInput {
        request: u64,
        result: Result<Option<u64>, String>,
    },
    AgentFile {
        request: u64,
        result: Result<FileResult, String>,
    },
    AgentNewTab {
        request: u64,
        result: Result<PaneId, String>,
    },
    List(Vec<WorkspaceInfo>),
    Clipboard {
        request: u64,
        file: File,
        incomplete: bool,
    },
    CopyError {
        request: u64,
        error: String,
    },
    History {
        request: u64,
        result: Result<HistoryPage, String>,
    },
    ClipboardStore {
        primary: bool,
        text: String,
    },
    Search {
        pane: PaneId,
        request: u64,
        count: usize,
        truncated: bool,
    },
    Error(String),
    Detached,
}

#[cfg(any(feature = "gui", feature = "tui"))]
pub(crate) enum ClientEndpoint {
    Local(PathBuf),
    Remote {
        program: OsString,
        args: Vec<OsString>,
        env: Vec<(OsString, Option<OsString>)>,
        directory: Option<PathBuf>,
    },
}
pub struct Client {
    screen_pending: bool,
    screen_request: u64,
    screen_dirty: bool,

    #[cfg(any(feature = "gui", feature = "tui"))]
    endpoint: ClientEndpoint,
    agent_events: VecDeque<AgentEvent>,
    agent_delegate: bool,
    peer: Peer,
    snapshot: Option<Snapshot>,
    screens: HashMap<PaneId, Screen>,
    write_error: Option<io::Error>,
    ssh: Option<(Child, String)>,
}
impl Client {
    fn new(
        peer: Peer,
        ssh: Option<(Child, String)>,
        #[cfg(any(feature = "gui", feature = "tui"))] endpoint: ClientEndpoint,
    ) -> Self {
        Self {
            peer,
            ssh,

            #[cfg(any(feature = "gui", feature = "tui"))]
            endpoint,
            screen_pending: false,
            screen_request: 0,
            screen_dirty: true,
            agent_events: VecDeque::new(),
            agent_delegate: false,
            snapshot: None,
            screens: HashMap::new(),
            write_error: None,
        }
    }

    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        check_peer(stream.as_raw_fd())?;
        Ok(Self::new(
            Peer::new(stream)?,
            None,
            #[cfg(any(feature = "gui", feature = "tui"))]
            ClientEndpoint::Local(path.to_path_buf()),
        ))
    }
    pub fn connect_remote(mut command: Command, target: String) -> io::Result<Self> {
        #[cfg(any(feature = "gui", feature = "tui"))]
        let endpoint = ClientEndpoint::Remote {
            program: command.get_program().to_owned(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            env: command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
                .collect(),
            directory: command.get_current_dir().map(Path::to_path_buf),
        };
        let (local, ssh) = UnixStream::pair()?;
        let peer = Peer::remote(local)?;
        command.stdin(OwnedFd::from(ssh.try_clone()?));
        command.stdout(OwnedFd::from(ssh));
        let child = command.spawn()?;
        drop(command);
        let mut client = Self::new(
            peer,
            Some((child, target)),
            #[cfg(any(feature = "gui", feature = "tui"))]
            endpoint,
        );
        loop {
            match client.peer.hello() {
                Ok(Some(BRIDGE_VERSION)) => return Ok(client),
                Ok(Some(version)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "remote SSH bridge version {version} differs from {BRIDGE_VERSION}"
                        ),
                    ));
                }
                Ok(None) => (),
                Err(error) => return Err(client.ssh_error(error)?),
            }
            let mut fd = libc::pollfd {
                fd: client.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            crate::poll::poll(std::slice::from_mut(&mut fd), -1)?;
        }
    }
    fn ssh_error(&mut self, error: io::Error) -> io::Result<io::Error> {
        if let Some((child, _)) = &mut self.ssh
            && let Some(status) = child.try_wait()?
            && !status.success()
        {
            return Ok(io::Error::new(
                error.kind(),
                format!("SSH exited with {status}: {error}"),
            ));
        }
        Ok(error)
    }
    pub fn is_remote(&self) -> bool {
        self.ssh.is_some()
    }
    pub fn remote_target(&self) -> Option<&str> {
        self.ssh.as_ref().map(|(_, target)| target.as_str())
    }
    pub fn fd(&self) -> RawFd {
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
                | Action::AgentRemove(_)
                | Action::ReadHistory { .. }
                | Action::CancelHistory { .. }
                | Action::AgentFile { .. }
                | Action::CancelAgentFile { .. }
                | Action::List
                | Action::CopyBlock { .. }
                | Action::CopySelection { .. }
        );
        match action {
            Action::Input { pane, data } => return self.send_input(pane, &data),
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
        self.peer.send_input(pane, data)
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

    pub fn scroll_to(&mut self, pane: PaneId, mut offset: usize) -> io::Result<()> {
        // Live output must not skew the delta between these actions.
        self.peer.send_action(&Action::Scroll {
            pane,
            scroll: Scroll::Bottom,
        })?;
        while offset > 0 {
            let lines = offset.min(i32::MAX as usize);
            self.peer.send_action(&Action::Scroll {
                pane,
                scroll: Scroll::Delta(lines as i32),
            })?;
            offset -= lines;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        if self.write_error.is_none()
            && let Err(error) = self.peer.flush()
        {
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
            ) {
                // The peer may have sent the final Detached message before closing its input.
                self.write_error = Some(error);
            } else {
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn receiver_fd(&self) -> RawFd {
        self.peer.receiver_fd()
    }
    pub fn pollfd(&self) -> libc::pollfd {
        let events = if self.peer.wants_read() {
            libc::POLLIN
        } else {
            0
        } | if self.wants_write() { libc::POLLOUT } else { 0 };
        libc::pollfd {
            fd: if events == 0 { -1 } else { self.fd() },
            events,
            revents: 0,
        }
    }
    pub fn needs_poll(&self) -> bool {
        self.peer.needs_poll()
    }
    fn wants_write(&self) -> bool {
        self.write_error.is_none() && self.peer.pending()
    }
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }
    pub fn screens(&self) -> &HashMap<PaneId, Screen> {
        &self.screens
    }
    pub fn snapshot_and_screens_mut(
        &mut self,
    ) -> Option<(&Snapshot, &mut HashMap<PaneId, Screen>)> {
        Some((self.snapshot.as_ref()?, &mut self.screens))
    }
    pub fn screen_mut(&mut self, pane: PaneId) -> Option<&mut Screen> {
        self.screens.get_mut(&pane)
    }

    pub fn poll(&mut self) -> io::Result<Vec<ClientEvent>> {
        self.flush()?;
        let responses = match self.peer.responses(self.snapshot.as_ref()) {
            Ok(responses) => responses,
            Err(error) => {
                let error = self.write_error.take().unwrap_or(error);
                return Err(self.ssh_error(error)?);
            }
        };
        let capacity = responses.len().min(4);
        let mut events = Vec::new();
        for response in responses {
            let event = match response {
                Response::State(snapshot, updates, request) => {
                    self.screens.retain(|id, _| snapshot.pane(*id).is_some());
                    for (id, update) in updates {
                        let screen = self.screens.entry(id).or_default();
                        if self.agent_delegate && screen.changes_activity(&update) {
                            screen.activity += 1;
                            screen.output_activity += 1;
                        }
                        screen.apply(update)?;
                    }
                    for (index, pane) in snapshot.panes.iter().enumerate() {
                        let Some(screen) = self.screens.get_mut(&pane.id) else {
                            continue;
                        };
                        let previous = self
                            .snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.matching_pane(index, pane.id));
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
                                if !screen.dirty[row] && screen.output_changed(old, pane, row) {
                                    screen.dirty[row] = true;
                                }
                            }
                        }
                    }
                    self.snapshot = Some(snapshot);
                    if request == self.screen_request {
                        self.screen_pending = false;
                        self.screen_dirty = false;
                    }
                    ClientEvent::Updated
                }
                Response::ScreenDirty => {
                    self.screen_dirty = true;
                    ClientEvent::Updated
                }
                Response::Agent(event) => {
                    if event == AgentEvent::Attached {
                        self.agent_delegate = true;
                    }
                    self.agent_events.push_back(event);
                    continue;
                }
                Response::List(list) => ClientEvent::List(list),
                Response::AgentInput { request, result } => {
                    ClientEvent::AgentInput { request, result }
                }
                Response::AgentFile { request, result } => {
                    ClientEvent::AgentFile { request, result }
                }
                Response::AgentNewTab { request, result } => {
                    ClientEvent::AgentNewTab { request, result }
                }
                Response::Clipboard {
                    request,
                    file,
                    incomplete,
                } => ClientEvent::Clipboard {
                    request,
                    file,
                    incomplete,
                },
                Response::CopyError { request, error } => ClientEvent::CopyError { request, error },
                Response::History { request, result } => ClientEvent::History { request, result },
                Response::ClipboardStore { primary, text } => {
                    ClientEvent::ClipboardStore { primary, text }
                }
                Response::Search {
                    pane,
                    request,
                    count,
                    truncated,
                } => ClientEvent::Search {
                    pane,
                    request,
                    count: count as usize,
                    truncated,
                },
                Response::Error(error) => {
                    if self.snapshot.is_none() {
                        self.screen_pending = false;
                    }
                    ClientEvent::Error(error)
                }
                Response::Detached => {
                    self.peer.cancel_output();
                    self.write_error = None;
                    self.snapshot = None;
                    self.agent_delegate = false;
                    self.screens.clear();
                    ClientEvent::Detached
                }
            };
            if !matches!(event, ClientEvent::Updated)
                || !matches!(events.last(), Some(ClientEvent::Updated))
            {
                if events.is_empty() {
                    events = Vec::with_capacity(capacity);
                }
                events.push(event);
            }
        }
        Ok(events)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some((child, _)) = &mut self.ssh {
            // Dropping a frontend ends its SSH bridge, never the detached workspace server.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub use crate::bridge::{BRIDGE_VERSION, run_bridge};

pub fn socket_path() -> io::Result<PathBuf> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    private_directory(&root, false)?;
    let directory = root.join("nextty");
    private_directory(&directory, true)?;
    Ok(directory.join(format!("server-v{}.sock", crate::wire::VERSION)))
}
fn private_directory(path: &Path, create: bool) -> io::Result<()> {
    if create
        && let Err(error) = fs::DirBuilder::new().mode(0o700).create(path)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be an owned private directory", path.display()),
        ));
    }
    Ok(())
}
pub(crate) fn check_peer(fd: RawFd) -> io::Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace peer has another user ID",
        ));
    }
    Ok(())
}
pub fn ensure_server(executable: &Path, socket: &Path) -> io::Result<()> {
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no directory"))?;
    private_directory(parent, true)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(socket.with_extension("start.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    let log_path = socket.with_extension("log");
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log_path)?;
    let mut command = Command::new(executable);
    command
        .arg("--server")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "workspace server exited with {status}; see {}",
                log_path.display()
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "workspace server did not open its socket",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

enum Node {
    Pane(PaneId),
    Split {
        axis: Axis,
        ratio: f64,
        // Keep layout linear even for deeply nested splits.
        minimum: (u16, u16),
        first: Box<Node>,
        second: Box<Node>,
    },
}
impl Node {
    fn for_each_pane(&self, visit: &mut impl FnMut(PaneId) -> io::Result<()>) -> io::Result<()> {
        match self {
            Self::Pane(id) => visit(*id),
            Self::Split { first, second, .. } => {
                first.for_each_pane(visit)?;
                second.for_each_pane(visit)
            }
        }
    }
    fn layout(&self, rect: Rect, output: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Pane(id) => output.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
                ..
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
            ..
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
                    minimum: Self::split_minimum(axis, (2, 1), (2, 1)),
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split {
                axis: split_axis,
                minimum,
                first,
                second,
                ..
            } => {
                let changed = first.split(target, new, axis) || second.split(target, new, axis);
                if changed {
                    *minimum = Self::split_minimum(*split_axis, first.minimum(), second.minimum());
                }
                changed
            }
        }
    }
    fn remove_child(&mut self, target: PaneId) -> bool {
        let Self::Split {
            axis,
            minimum,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        if matches!(**first, Self::Pane(id) if id == target) {
            // Move out the sibling without allocating a replacement box.
            *self = std::mem::replace(&mut **second, Self::Pane(0));
            true
        } else if matches!(**second, Self::Pane(id) if id == target) {
            *self = std::mem::replace(&mut **first, Self::Pane(0));
            true
        } else if first.remove_child(target) || second.remove_child(target) {
            *minimum = Self::split_minimum(*axis, first.minimum(), second.minimum());
            true
        } else {
            false
        }
    }
    fn first(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first(),
        }
    }
    fn minimum(&self) -> (u16, u16) {
        match self {
            Self::Pane(_) => (2, 1),
            Self::Split { minimum, .. } => *minimum,
        }
    }
    fn split_minimum(axis: Axis, a: (u16, u16), b: (u16, u16)) -> (u16, u16) {
        match axis {
            Axis::LeftRight => (a.0 + b.0 + 1, a.1.max(b.1)),
            Axis::TopBottom => (a.0.max(b.0), a.1 + b.1 + 1),
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
    owner: Option<u64>,
    token: Option<Vec<u8>>,
    pending: Vec<Vec<u8>>,
    view: Option<Vec<u8>>,
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
    fn all_layouts(&self) -> Vec<(PaneId, Rect)> {
        let mut result = Vec::with_capacity(self.panes.len());
        for tab in &self.tabs {
            tab.root.layout(self.area(), &mut result);
        }
        result
    }
    fn resize(&mut self, size: WindowSize) -> io::Result<()> {
        validate_size(size)?;
        let minimum = self
            .tabs
            .iter()
            .map(|tab| tab.root.minimum())
            .fold((2, 1), |a, b| (a.0.max(b.0), a.1.max(b.1)));
        let next_size = WindowSize {
            num_cols: size.num_cols.max(minimum.0),
            num_lines: size.num_lines.max(minimum.1 + 1),
            ..size
        };
        validate_size(next_size)?;
        self.size = next_size;
        for (id, rect) in self.all_layouts() {
            self.panes[&id].terminal.resize(WindowSize {
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
        if self.panes.len() >= 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace has reached its 1024-pane limit",
            ));
        }
        let id = NEXT_PANE.fetch_add(1, Ordering::Relaxed);
        let terminal = Terminal::new_with_history(
            program,
            directory,
            WindowSize {
                num_cols: self.size.num_cols,
                num_lines: self.size.num_lines - 1,
                ..self.size
            },
            self.foreground,
            self.background,
            HistoryConfig::new(self.budget.clone()),
        )
        .map_err(io::Error::other)?;
        self.panes.insert(
            id,
            Pane {
                terminal,
                title: "shell".into(),
            },
        );
        Ok(id)
    }
    fn remove_pane(&mut self, id: PaneId) -> Option<Terminal> {
        let active_tab = self.tabs[self.active].id;
        let terminal = self.panes.remove(&id).map(|pane| pane.terminal);
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
        terminal
    }
    fn snapshot(&self, panes: Vec<PaneView>) -> io::Result<Snapshot> {
        Ok(Snapshot {
            name: self.name.clone(),
            controller: false,
            focused: self.focused(),
            active_tab: self.tabs[self.active].id,
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    let pane = &self.panes[&tab.focused];
                    let title = |directory: &Path| {
                        directory
                            .file_name()
                            .unwrap_or(directory.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    };
                    let title = match panes.iter().find(|pane| pane.id == tab.focused) {
                        Some(pane) => pane.cwd.as_deref().map(title),
                        None => pane_directory(&pane.terminal)?.as_deref().map(title),
                    }
                    .unwrap_or_else(|| pane.title.clone());
                    Ok(TabView {
                        id: tab.id,
                        title,
                        activity: self.tab_activity(&tab.root)?,
                    })
                })
                .collect::<io::Result<Vec<_>>>()?,
            panes,
        })
    }
    fn tab_activity(&self, node: &Node) -> io::Result<TabActivity> {
        let mut activity = TabActivity::Idle;
        node.for_each_pane(&mut |id| {
            if activity == TabActivity::Running {
                return Ok(());
            }
            let terminal = &self.panes[&id].terminal;
            let process = terminal.process_metadata();
            if process
                .foreground_pgid
                .is_some_and(|pgid| pgid != process.child_pid)
            {
                activity = TabActivity::Running;
                return Ok(());
            }
            let core = terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let latest = core.latest_command_status();
            let state =
                if latest.is_none() && process.has_children == Some(true) || latest == Some(None) {
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
                };
            if state == TabActivity::Running
                || state == TabActivity::Unknown
                || activity == TabActivity::Idle
                || state == TabActivity::Failed && activity == TabActivity::Finished
            {
                activity = state;
            }
            Ok(())
        })?;
        Ok(activity)
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
fn validate_size(size: WindowSize) -> io::Result<()> {
    let columns = usize::from(size.num_cols);
    let lines = usize::from(size.num_lines);
    if !(2..=MAX_COLUMNS).contains(&columns)
        || !(2..=MAX_LINES).contains(&lines)
        || columns * lines > MAX_CELLS
        || size.num_cols.checked_mul(size.cell_width).is_none()
        || size.num_lines.checked_mul(size.cell_height).is_none()
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace size must be 2..{MAX_COLUMNS} columns, 2..{MAX_LINES} rows, at most {MAX_CELLS} cells"
            ),
        ))
    } else {
        Ok(())
    }
}
struct PaneSearch {
    pane: PaneId,
    request: u64,
    query: String,
    core: Arc<Mutex<Core>>,
    origin: Option<crate::core::SearchAnchor>,
    cancelled: Arc<AtomicBool>,
    state: SearchState,
}
enum SearchState {
    Queued,
    Pending(mpsc::Receiver<io::Result<crate::search::SearchResult>>),
    Ready(crate::search::SearchResult),
}
impl Drop for PaneSearch {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(origin) = self.origin {
            self.core.lock().unwrap().restore_search(origin);
        }
    }
}
struct CopyRequest {
    pane: PaneId,
    request: u64,
    block: Option<u64>,
}

struct ReadRequest {
    pane: PaneId,
    request: u64,
    block: Option<u64>,
    cursor: Option<HistoryCursor>,
    query: Option<String>,
}

struct PendingRead {
    request: u64,
    workspace: String,
    pane: PaneId,
    receiver: mpsc::Receiver<io::Result<HistoryPage>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for PendingRead {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct FileSource {
    request: u64,
    pane: PaneId,
    cancelled: Arc<AtomicBool>,
}
struct FileJob {
    source: FileSource,
    path: PathBuf,
    operation: FileOperation,
}
struct FileWorker {
    worker: crate::worker::Worker<FileJob, io::Result<FileResult>>,
    queued: VecDeque<FileJob>,
    active: Option<FileSource>,
}
impl FileWorker {
    fn new() -> io::Result<Self> {
        Ok(Self {
            worker: crate::worker::Worker::new(
                "agent-files",
                Arc::new(crate::eventfd::new()?),
                |job: FileJob, publish| {
                    publish(crate::agent_files::execute(
                        job.path,
                        job.operation,
                        &job.source.cancelled,
                    ));
                },
            )?,
            queued: VecDeque::new(),
            active: None,
        })
    }

    fn take(&mut self) -> Option<Response> {
        self.active.as_ref()?;
        let result = self.worker.take()?;
        let request = self.active.take().unwrap().request;
        Some(Response::AgentFile {
            request,
            result: result.map_err(|error| error.to_string()),
        })
    }

    fn cancel(&mut self, matches: impl Fn(&FileSource) -> bool) {
        if let Some(source) = self.active.as_ref().filter(|source| matches(source)) {
            source.cancelled.store(true, Ordering::Relaxed);
        }
        for job in self.queued.iter_mut().filter(|job| matches(&job.source)) {
            job.source.cancelled.store(true, Ordering::Relaxed);
            // Retain its FIFO error reply without keeping the unused payload.
            job.path = PathBuf::new();
            job.operation = FileOperation::Read { offset: 0 };
        }
    }
}
impl Drop for FileWorker {
    fn drop(&mut self) {
        self.cancel(|_| true);
    }
}

struct Connection {
    screen_request: Option<u64>,
    screen_dirty: bool,
    agent: bool,
    agent_open: bool,
    id: u64,
    peer: Peer,
    workspace: Option<String>,
    size: WindowSize,
    full: bool,
    failed: bool,
    known: HashMap<PaneId, Arc<crate::terminal::ProcessMetadata>>,
    copy: Option<(u64, mpsc::Receiver<io::Result<HistoryCopy>>)>,
    deferred: Option<Action>,
    ending: bool,
    copy_requests: VecDeque<(String, CopyRequest)>,
    reads: Vec<PendingRead>,
    read_requests: VecDeque<(String, ReadRequest)>,
    search: Option<PaneSearch>,
    files: Option<FileWorker>,
}

impl Connection {
    fn screen_eligible(&self) -> bool {
        self.screen_request.is_some() && self.screen_dirty && !self.failed && !self.peer.pending()
    }

    fn new(id: u64, stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            id,
            screen_request: None,
            screen_dirty: true,
            agent: false,
            agent_open: false,
            peer: Peer::new(stream)?,
            workspace: None,
            size: CreateOptions::default().size,
            full: true,
            failed: false,
            known: HashMap::new(),
            copy: None,
            deferred: None,
            ending: false,
            copy_requests: VecDeque::new(),
            reads: Vec::new(),
            read_requests: VecDeque::new(),
            search: None,
            files: None,
        })
    }

    fn action_blocked(
        &self,
        action: &Action,
        workspaces: &HashMap<String, Workspace>,
    ) -> io::Result<bool> {
        let pane = match action {
            Action::Input { pane, .. }
            | Action::SelectionStart { pane, .. }
            | Action::SelectionUpdate { pane, .. } => self
                .workspace
                .as_ref()
                .and_then(|name| workspaces.get(name))
                .filter(|workspace| workspace.controller == Some(self.id) || self.agent)
                .and_then(|workspace| workspace.panes.get(pane)),
            _ => None,
        };
        match (action, pane) {
            (Action::Input { data, .. }, Some(pane)) => {
                Ok(pane.terminal.input_pending_bytes() + data.len() > INPUT_CAPACITY)
            }
            (_, Some(pane)) => pane
                .terminal
                .core
                .lock()
                .map(|core| core.resize_pending())
                .map_err(|error| io::Error::other(error.to_string())),
            (_, None) => Ok(false),
        }
    }

    fn finish(&mut self) {
        self.workspace = None;
        self.ending = true;
        self.deferred = None;
        self.copy_requests.clear();
        self.read_requests.clear();
        self.reads.clear();
        self.copy = None;
        self.search = None;
        if let Some(files) = &mut self.files
            && let Some(response) = files.take()
        {
            self.respond(response);
        }
        self.files = None;
        self.respond(Response::Detached);
    }

    fn respond(&mut self, response: Response) {
        if let Err(error) = self.peer.send_response(response) {
            eprintln!("workspace client {}: {error}", self.id);
            self.failed = true;
        }
    }
}

/// Run in the hidden server process. No GUI or font libraries are needed here.
pub fn run_server(socket: &Path) -> io::Result<()> {
    let history_directory = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("nextty/history");
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no directory"))?;
    private_directory(parent, true)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Ok(metadata) = fs::symlink_metadata(socket) {
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to replace an unowned workspace socket",
            ));
        }
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    fs::create_dir_all(&history_directory)?;
    fs::set_permissions(&history_directory, fs::Permissions::from_mode(0o700))?;
    private_directory(&history_directory, false)?;
    let result = server_loop(listener, &history_directory);
    let cleanup = fs::remove_file(socket);
    result.and(cleanup)
}
#[derive(Default)]
struct ClosingTerminals(Vec<JoinHandle<()>>);
impl ClosingTerminals {
    fn reap(&mut self, wait: bool) {
        for thread in self.0.extract_if(.., |thread| wait || thread.is_finished()) {
            if thread.join().is_err() {
                eprintln!("terminal IO thread panicked");
            }
        }
    }
}
impl Drop for ClosingTerminals {
    fn drop(&mut self) {
        self.reap(true);
    }
}

fn terminate_workspace(
    workspace: Workspace,
    clients: &mut [Connection],
    closing: &mut ClosingTerminals,
) {
    closing.0.extend(
        workspace
            .panes
            .into_values()
            .map(|pane| pane.terminal.shutdown()),
    );
    for client in clients {
        if client.workspace.as_deref() == Some(&workspace.name) {
            client.finish();
        }
    }
}

fn server_loop(listener: UnixListener, history_directory: &Path) -> io::Result<()> {
    let mut closing = ClosingTerminals::default();
    let mut workspaces: HashMap<String, Workspace> = HashMap::new();
    let mut clients: Vec<Connection> = Vec::new();
    let mut next_client = 1u64;
    let mut had_workspace = false;
    let mut fds = Vec::new();
    let mut terminals: Vec<(String, PaneId)> = Vec::new();
    let mut dirty = HashSet::new();
    loop {
        closing.reap(false);
        had_workspace |= !workspaces.is_empty();
        clients.retain(|client| !client.failed);
        for index in 0..clients.len() {
            if clients[index].screen_eligible()
                && let Some(workspace) = clients[index]
                    .workspace
                    .as_ref()
                    .and_then(|name| workspaces.get_mut(name))
            {
                broadcast(workspace, &mut clients, false)?;
            }
        }
        for (name, workspace) in &mut workspaces {
            if workspace
                .agent
                .owner
                .is_some_and(|id| !clients.iter().any(|client| client.id == id))
            {
                workspace.agent.owner = None;
            }
            if workspace.controller.is_some_and(|id| {
                !clients
                    .iter()
                    .any(|client| client.id == id && client.workspace.as_ref() == Some(name))
            }) {
                let controller = clients
                    .iter_mut()
                    .rev()
                    .find(|client| !client.agent && client.workspace.as_ref() == Some(name));
                workspace.controller = controller.as_ref().map(|client| client.id);
                if let Some(controller) = controller {
                    workspace.resize(controller.size)?;
                    controller.full = true;
                }
                dirty.insert(name.clone());
            }
        }
        for name in dirty.drain() {
            if let Some(workspace) = workspaces.get_mut(&name) {
                broadcast(workspace, &mut clients, true)?;
            }
        }
        fds.clear();
        fds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for client in &clients {
            fds.push(libc::pollfd {
                fd: client.peer.fd(),
                events: (if client.deferred.is_none() && !client.ending {
                    libc::POLLIN
                } else {
                    0
                }) | if client.peer.pending() {
                    libc::POLLOUT
                } else {
                    0
                },
                revents: 0,
            });
        }
        let mut terminal_count = 0;
        for (name, workspace) in &workspaces {
            for (id, pane) in &workspace.panes {
                fds.push(libc::pollfd {
                    fd: pane.terminal.event_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                if let Some((previous_name, previous_id)) = terminals.get_mut(terminal_count) {
                    // Pane IDs are global and never change workspace.
                    if *previous_id != *id {
                        previous_name.clone_from(name);
                        *previous_id = *id;
                    }
                } else {
                    terminals.push((name.clone(), *id));
                }
                terminal_count += 1;
            }
        }
        terminals.truncate(terminal_count);
        let file_start = fds.len();
        // A late notification may outlive the last result; idle workers need no polling.
        for files in clients
            .iter()
            .filter_map(|client| client.files.as_ref())
            .filter(|files| files.active.is_some())
        {
            fds.push(libc::pollfd {
                fd: files.worker.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // Failures and completed resizes may have no further socket or terminal wakeup.
        let timeout = if clients.iter().any(|client| {
            client.failed
                || !client.ending
                    && match &client.deferred {
                        Some(action) => {
                            !matches!(client.action_blocked(action, &workspaces), Ok(true))
                        }
                        None => client.peer.buffered_action(),
                    }
        }) {
            0
        } else {
            -1
        };
        crate::poll::poll(&mut fds, timeout)?;
        let file_clients = clients.iter_mut().filter(|client| {
            client
                .files
                .as_ref()
                .is_some_and(|files| files.active.is_some())
        });
        for (fd, client) in fds[file_start..].iter().zip(file_clients) {
            if fd.revents != 0
                && let Err(error) = crate::eventfd::consume_notification(fd.fd)
            {
                eprintln!("agent file worker: {error}");
                client.failed = true;
            }
        }
        // Both client passes use this poll's order; remove failed clients next iteration.
        let client_count = clients.len();
        for index in 0..client_count {
            if clients[index].failed {
                continue;
            }
            let revents = fds[1 + index].revents;
            if revents & (libc::POLLERR | libc::POLLNVAL) != 0
                || revents & libc::POLLHUP != 0
                    && (clients[index].deferred.is_some() || clients[index].ending)
            {
                clients[index].failed = true;
                continue;
            }
            if revents & libc::POLLOUT != 0 && clients[index].peer.flush().is_err() {
                clients[index].failed = true;
                continue;
            }
            if !clients[index].ending
                && (revents & (libc::POLLIN | libc::POLLHUP) != 0
                    || clients[index].deferred.is_some()
                    || clients[index].peer.buffered_action())
            {
                for _ in 0..256 {
                    let was_deferred = clients[index].deferred.is_some();
                    let action = match clients[index].deferred.take() {
                        Some(action) => Some(action),
                        None => match clients[index].peer.next_action() {
                            Ok(action) => action,
                            Err(error) => {
                                eprintln!("workspace client {}: {error}", clients[index].id);
                                clients[index].failed = true;
                                break;
                            }
                        },
                    };
                    let Some(action) = action else {
                        break;
                    };
                    if matches!(action, Action::Detach) {
                        clients[index].failed = true;
                        break;
                    }
                    let result = match clients[index].action_blocked(&action, &workspaces) {
                        Ok(true) => {
                            // Accepted input generations must remain observable during backpressure.
                            if !was_deferred
                                && matches!(action, Action::Input { .. })
                                && let Some(name) = &clients[index].workspace
                            {
                                dirty.insert(name.clone());
                            }
                            clients[index].deferred = Some(action);
                            break;
                        }
                        Ok(false) => apply_action(
                            index,
                            action,
                            &mut clients,
                            &mut workspaces,
                            &mut dirty,
                            history_directory,
                            &mut closing,
                        ),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        clients[index].respond(Response::Error(error.to_string()));
                        if clients[index].failed {
                            break;
                        }
                    }
                    if !clients[index].peer.buffered_action() {
                        break;
                    }
                }
            }
        }
        let mut marked_workspace = None;
        for (index, (name, id)) in terminals.iter().enumerate() {
            if fds[1 + client_count + index].revents == 0 {
                continue;
            }
            let Some(workspace) = workspaces.get_mut(name) else {
                continue;
            };
            let Some(pane) = workspace.panes.get_mut(id) else {
                continue;
            };
            let mut changed = pane.terminal.drain_notification()?;
            let mut exit = false;
            for event in pane.terminal.events.try_iter() {
                match event {
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
                        for client in clients.iter_mut().filter(|client| {
                            !client.agent && client.workspace.as_ref() == Some(name)
                        }) {
                            client.respond(Response::Error(error.to_string()));
                        }
                        changed = true;
                    }
                    Event::ClipboardStore { primary, text } => {
                        if let Some(controller) = clients
                            .iter_mut()
                            .find(|client| Some(client.id) == workspace.controller)
                        {
                            controller.respond(Response::ClipboardStore { primary, text });
                        }
                    }
                }
            }
            if !changed && !exit {
                // A concurrent state wakeup must stay pending until its state is observed.
                continue;
            }
            pane.terminal.acknowledge_wakeup();
            if exit {
                cancel_pane_files(&mut clients, *id);
                closing
                    .0
                    .extend(workspace.remove_pane(*id).map(Terminal::shutdown));
                if !workspace.tabs.is_empty() {
                    workspace.resize(workspace.size)?;
                }
            }
            if marked_workspace != Some(name) {
                dirty.insert(name.clone());
                marked_workspace = Some(name);
            }
        }
        for (_, workspace) in workspaces.extract_if(|_, workspace| workspace.tabs.is_empty()) {
            terminate_workspace(workspace, &mut clients, &mut closing);
        }
        for name in dirty.drain() {
            if let Some(workspace) = workspaces.get_mut(&name) {
                broadcast(workspace, &mut clients, true)?;
            }
        }
        complete_requests(&mut clients, &workspaces, &mut dirty);
        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if check_peer(stream.as_raw_fd()).is_err() {
                            continue;
                        }
                        clients.push(Connection::new(next_client, stream)?);
                        next_client += 1;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
        }
        for (client, fd) in clients.iter_mut().zip(&fds[1..1 + client_count]) {
            debug_assert_eq!(client.peer.fd(), fd.fd);
            if client.failed || fd.events & libc::POLLOUT != 0 && fd.revents & libc::POLLOUT == 0 {
                continue;
            }
            if let Err(error) = client.peer.flush() {
                eprintln!("workspace client {}: {error}", client.id);
                client.failed = true;
            }
        }
        if had_workspace
            && workspaces.is_empty()
            && !clients
                .iter()
                .any(|client| !client.failed && client.peer.pending())
        {
            return Ok(());
        }
    }
}
fn complete_requests(
    clients: &mut [Connection],
    workspaces: &HashMap<String, Workspace>,
    dirty: &mut HashSet<String>,
) {
    for client in clients.iter_mut() {
        if client.failed {
            client.files = None;
            continue;
        }
        if let Some(response) = client.files.as_mut().and_then(FileWorker::take) {
            client.respond(response);
        }
        if client.failed {
            client.files = None;
            continue;
        }
        if let Some(files) = &mut client.files
            && files.active.is_none()
            && let Some(job) = files.queued.pop_front()
        {
            // Worker replaces pending work and results, so submit only after taking the result.
            files.active = Some(job.source.clone());
            files.worker.submit(|_| job);
        }
        if let Some(result) = client
            .copy
            .as_ref()
            .and_then(|(_, receiver)| worker_result(receiver, "history copy worker disconnected"))
        {
            let (request, _) = client.copy.take().unwrap();
            match result {
                Ok(copy) => client.respond(Response::Clipboard {
                    request,
                    file: copy.file,
                    incomplete: copy.incomplete,
                }),
                Err(error) => client.respond(Response::CopyError {
                    request,
                    error: error.to_string(),
                }),
            }
        }
        while client.copy.is_none()
            && let Some((name, copy)) = client.copy_requests.pop_front()
        {
            let Some(pane) = workspaces
                .get(&name)
                .and_then(|workspace| workspace.panes.get(&copy.pane))
            else {
                client.respond(Response::CopyError {
                    request: copy.request,
                    error: "copy source pane no longer exists".into(),
                });
                continue;
            };
            if !queue_copy(client, Cow::Owned(name), &pane.terminal, copy) {
                break;
            }
        }
        for index in (0..client.reads.len()).rev() {
            let read = &client.reads[index];
            let ready = if workspaces
                .get(&read.workspace)
                .is_none_or(|workspace| !workspace.panes.contains_key(&read.pane))
            {
                Some(Err("history source pane no longer exists".into()))
            } else {
                worker_result(&read.receiver, "history reader disconnected")
                    .map(|result| result.map_err(|error| error.to_string()))
            };
            if let Some(result) = ready {
                let read = client.reads.swap_remove(index);
                client.respond(Response::History {
                    request: read.request,
                    result,
                });
            }
        }
        for _ in 0..client.read_requests.len() {
            let (name, read) = client.read_requests.pop_front().unwrap();
            match workspaces
                .get(&name)
                .and_then(|workspace| workspace.panes.get(&read.pane))
            {
                Some(pane) => queue_read(client, Cow::Owned(name), &pane.terminal, read),
                None => client.respond(Response::History {
                    request: read.request,
                    result: Err("history source pane no longer exists".into()),
                }),
            }
        }
        let Some(search) = client.search.as_mut() else {
            continue;
        };
        let workspace = client
            .workspace
            .as_ref()
            .and_then(|name| workspaces.get(name));
        let Some(terminal) = workspace
            .and_then(|workspace| workspace.panes.get(&search.pane))
            .map(|pane| &pane.terminal)
        else {
            client.search = None;
            if let Some(name) = &client.workspace {
                dirty.insert(name.clone());
            }
            continue;
        };
        let submission_error = if matches!(search.state, SearchState::Queued) {
            try_search(search, terminal).err()
        } else {
            None
        };
        let ready = submission_error.map(Err).or_else(|| match &search.state {
            SearchState::Pending(receiver) => worker_result(receiver, "pane search worker stopped"),
            _ => None,
        });
        if let Some(result) = ready {
            let (pane, request) = (search.pane, search.request);
            match result {
                Ok(result) => {
                    let response = Response::Search {
                        pane,
                        request,
                        count: result.matches.len() as u32,
                        truncated: result.truncated,
                    };
                    search.state = SearchState::Ready(result);
                    client.respond(response);
                }
                Err(error) => {
                    client.respond(Response::Search {
                        pane,
                        request,
                        count: 0,
                        truncated: true,
                    });
                    client.respond(Response::Error(error.to_string()));
                    client.search = None;
                    if let Some(name) = &client.workspace {
                        dirty.insert(name.clone());
                    }
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
        Err(mpsc::TryRecvError::Disconnected) => {
            Some(Err(io::Error::new(io::ErrorKind::BrokenPipe, disconnected)))
        }
    }
}

fn cancel_pane_files(clients: &mut [Connection], pane: PaneId) {
    for files in clients
        .iter_mut()
        .filter_map(|client| client.files.as_mut())
    {
        files.cancel(|source| source.pane == pane);
    }
}

fn broadcast(
    workspace: &mut Workspace,
    clients: &mut [Connection],
    changed: bool,
) -> io::Result<()> {
    for client in clients
        .iter_mut()
        .filter(|client| client.workspace.as_deref() == Some(&workspace.name) && !client.failed)
    {
        if changed && !client.screen_dirty {
            client.screen_dirty = true;
            if client.screen_request.is_none() {
                client.respond(Response::ScreenDirty);
            }
        }
    }
    if !clients.iter().any(|client| {
        client.workspace.as_deref() == Some(&workspace.name) && client.screen_eligible()
    }) {
        return Ok(());
    }
    let has_agent = clients.iter().any(|client| {
        client.agent
            && client.screen_eligible()
            && client.workspace.as_deref() == Some(&workspace.name)
    });
    let all_tabs = has_agent && workspace.tabs.len() > 1;
    let tabs = if all_tabs {
        workspace.tabs.as_slice()
    } else {
        std::slice::from_ref(&workspace.tabs[workspace.active])
    };
    let mut visible = Vec::new();
    tabs[0].root.layout(workspace.area(), &mut visible);
    let active = &workspace.tabs[workspace.active];
    let capacity = if all_tabs {
        workspace.panes.len()
    } else {
        visible.len()
    };
    let mut frames = Vec::with_capacity(capacity);
    let mut panes = Vec::with_capacity(capacity);
    for (index, tab) in tabs.iter().enumerate() {
        if index != 0 {
            visible.clear();
            tab.root.layout(workspace.area(), &mut visible);
        }
        for &(id, rect) in &visible {
            let pane = &workspace.panes[&id];
            let process = pane.terminal.process_metadata();
            let mut core = pane
                .terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let blocks = core.visible_blocks();
            let command_blocks = core.recent_command_blocks();
            let input_generation = core.input_generation();
            let shell_prompt = core.shell_prompt();
            let cwd = core.current_directory().map(Path::to_path_buf);
            let synchronized = core.mode().contains(Mode::SYNC_UPDATE);
            if !synchronized || has_agent {
                let offset = core.display_offset();
                let mut needs_full = false;
                let mut needs_delta = false;
                for client in clients.iter().filter(|client| {
                    client.workspace.as_deref() == Some(&workspace.name)
                        && client.screen_eligible()
                        && (client.agent || !all_tabs || tab.id == active.id)
                        && (!client.agent || offset == 0)
                }) {
                    let full = client.full || !client.known.contains_key(&id);
                    needs_full |= full;
                    needs_delta |= !full;
                    if needs_full && needs_delta {
                        break;
                    }
                }
                let full = needs_full.then(|| Update::capture(&core, true));
                let delta = (needs_delta
                    && (full.is_none() || core.dirty_rows().count() != core.screen_lines()))
                .then(|| Update::capture(&core, false));
                let live = (offset != 0 && has_agent).then(|| Update::capture_live(&core));
                let hidden = synchronized || tab.id != active.id;
                frames.push((id, full, delta, live, hidden));
                if !synchronized {
                    core.reset_damage();
                }
            }
            drop(core);
            panes.push(PaneView {
                id,
                tab: tab.id,
                rect,
                title: pane.title.clone(),
                cwd: match cwd {
                    Some(directory) => Some(directory),
                    None => pane_directory(&pane.terminal)?,
                },
                process,
                blocks,
                command_blocks,
                input_generation,
                shell_prompt,
            });
        }
    }
    let all_frames = frames.len() == panes.len();
    let mut all_snapshot = workspace.snapshot(panes)?;
    let mut active_snapshot = None;
    let mut updates = Vec::with_capacity(frames.len());
    for client in clients {
        if client.workspace.as_deref() != Some(&workspace.name) || client.failed {
            continue;
        }
        if !client.screen_eligible() {
            client.full = true;
            continue;
        }
        let snapshot = if client.agent || !all_tabs {
            &mut all_snapshot
        } else {
            active_snapshot.get_or_insert_with(|| Snapshot {
                name: all_snapshot.name.clone(),
                controller: all_snapshot.controller,
                focused: all_snapshot.focused,
                active_tab: all_snapshot.active_tab,
                tabs: all_snapshot.tabs.clone(),
                panes: all_snapshot
                    .panes
                    .iter()
                    .filter(|pane| pane.tab == active.id)
                    .cloned()
                    .collect(),
            })
        };
        updates.clear();
        updates.extend(frames.iter().filter_map(|(id, full, delta, live, hidden)| {
            if !client.agent && *hidden {
                return None;
            }
            let update = if client.agent
                && let Some(live) = live
            {
                live
            } else if !client.known.contains_key(id) || client.full {
                full.as_ref()?
            } else {
                delta.as_ref().or(full.as_ref())?
            };
            Some((*id, update))
        }));
        snapshot.controller = workspace.controller == Some(client.id);
        match client.peer.send_state(
            snapshot,
            &updates,
            &client.known,
            client.screen_request.unwrap(),
        ) {
            Err(error) => {
                eprintln!("workspace client {}: {error}", client.id);
                client.failed = true;
            }
            Ok(metadata_changed) if !all_frames || metadata_changed => {
                if all_frames {
                    client.known.clear();
                } else {
                    client.known.retain(|id, _| snapshot.pane(*id).is_some());
                }
                client.known.extend(
                    snapshot
                        .panes
                        .iter()
                        .filter(|pane| {
                            all_frames || frames.iter().any(|(id, _, _, _, _)| *id == pane.id)
                        })
                        .map(|pane| (pane.id, Arc::clone(&pane.process))),
                );
            }
            Ok(_) => (),
        }
        client.full = client.full && updates.len() < snapshot.panes.len();
        client.screen_request = None;
        client.screen_dirty = false;
    }
    Ok(())
}
fn apply_action(
    index: usize,
    action: Action,
    clients: &mut [Connection],
    workspaces: &mut HashMap<String, Workspace>,
    dirty: &mut HashSet<String>,
    history_root: &Path,
    closing: &mut ClosingTerminals,
) -> io::Result<()> {
    let client_id = clients[index].id;
    let authorized = match action {
        Action::AgentFile { .. } | Action::CancelAgentFile { .. } | Action::AgentNewTab { .. } => {
            clients[index].agent
        }
        Action::AgentInput { .. }
        | Action::CopyBlock { .. }
        | Action::ReadHistory { .. }
        | Action::CancelHistory { .. }
        | Action::AgentUpdate(_)
        | Action::AgentRemove(_)
        | Action::Detach
        | Action::ScreenRequest { .. } => true,
        _ => !clients[index].agent,
    };
    if !authorized {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent owner action is not authorized",
        ));
    }
    let Some(action) = apply_session_action(
        index,
        action,
        clients,
        workspaces,
        dirty,
        history_root,
        closing,
    )?
    else {
        return Ok(());
    };
    let name = clients[index].workspace.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "attach to a workspace first")
    })?;
    let workspace = workspaces
        .get_mut(name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace does not exist"))?;
    if matches!(
        action,
        Action::AgentOpen
            | Action::AgentView(_)
            | Action::AgentRequest(_)
            | Action::AgentUpdate(_)
            | Action::AgentRemove(_)
    ) {
        return apply_agent_action(index, action, clients, workspace);
    }
    if let Action::Resize(size) = action {
        validate_size(size)?;
        clients[index].size = size;
        if workspace.controller != Some(client_id) {
            return Ok(());
        }
    }
    if workspace.controller != Some(client_id)
        && !(clients[index].agent
            && matches!(
                action,
                Action::AgentInput { .. } | Action::AgentFile { .. } | Action::AgentNewTab { .. }
            ))
        && !matches!(
            action,
            Action::CopySelection { .. }
                | Action::CopyBlock { .. }
                | Action::ReadHistory { .. }
                | Action::Search { .. }
                | Action::SearchCancel { .. }
                | Action::Terminate
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "this frontend is read-only; attach again to take control",
        ));
    }
    let previous_focus = workspace.focused();
    let pane_ref = |id: PaneId| {
        workspace
            .panes
            .get(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane does not exist"))
    };
    if let Action::AgentFile {
        pane,
        request,
        path,
        operation,
    } = action
    {
        let result: io::Result<()> = (|| {
            let terminal = &pane_ref(pane)?.terminal;
            let path = if path.is_absolute() {
                path
            } else {
                terminal.current_directory()?.join(path)
            };
            let client = &mut clients[index];
            if client.files.is_none() {
                client.files = Some(FileWorker::new()?);
            }
            client.files.as_mut().unwrap().queued.push_back(FileJob {
                source: FileSource {
                    request,
                    pane,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
                path,
                operation,
            });
            Ok(())
        })();
        if let Err(error) = result {
            clients[index].respond(Response::AgentFile {
                request,
                result: Err(error.to_string()),
            });
        }
        return Ok(());
    }
    if let Action::AgentNewTab { pane, request } = action {
        let result = pane_ref(pane)
            .and_then(|pane| pane.terminal.current_directory())
            .and_then(|directory| {
                let new = workspace.new_pane(Some(directory), None)?;
                workspace.tabs.push(Tab {
                    id: new,
                    root: Node::Pane(new),
                    focused: new,
                });
                Ok(new)
            })
            .map_err(|error| error.to_string());
        if result.is_ok() {
            dirty.insert(workspace.name.clone());
        }
        clients[index].respond(Response::AgentNewTab { request, result });
        return Ok(());
    }
    if let Action::AgentInput {
        pane,
        request,
        generation,
        prompt,
        data,
    } = action
    {
        let result = pane_ref(pane)
            .and_then(|pane| agent_input(&pane.terminal, generation, prompt, data))
            .map_err(|error| error.to_string());
        clients[index].respond(Response::AgentInput { request, result });
        dirty.insert(workspace.name.clone());
        return Ok(());
    }
    if let Action::Input { pane, data } = action {
        let terminal = &pane_ref(pane)?.terminal;
        let focus_report = data == b"\x1b[I" || data == b"\x1b[O";
        let mut core = terminal
            .core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        terminal.try_write(data)?;
        if !focus_report {
            let changed = core.selection_range().is_some() || core.display_offset() != 0;
            core.note_input();
            core.clear_selection();
            core.scroll_display(Scroll::Bottom);
            if changed {
                dirty.insert(workspace.name.clone());
            }
        }
        return Ok(());
    }
    let name = workspace.name.clone();
    match action {
        Action::Resize(size) => {
            if size == workspace.size {
                return Ok(());
            }
            workspace.resize(size)?;
            mark_full(clients, &name);
        }
        Action::ResizeSplit {
            first,
            second,
            position,
        } => {
            let rect = workspace.area();
            if !workspace.tabs[workspace.active]
                .root
                .resize_split(rect, first, second, position)
            {
                return Ok(());
            }
            workspace.resize(workspace.size)?;
            mark_full(clients, &name);
        }
        Action::Split { pane, axis } => {
            let pane = if pane == 0 { workspace.focused() } else { pane };
            let rect = workspace
                .layout()
                .into_iter()
                .find(|(id, _)| *id == pane)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane is not visible"))?
                .1;
            if match axis {
                Axis::LeftRight => rect.cols < 5,
                Axis::TopBottom => rect.rows < 3,
            } {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pane is too small to split",
                ));
            }
            let directory = pane_directory(&workspace.panes[&pane].terminal)?;
            let new = workspace.new_pane(directory, None)?;
            workspace.tabs[workspace.active].root.split(pane, new, axis);
            workspace.tabs[workspace.active].focused = new;
            workspace.resize(workspace.size)?;
            mark_full(clients, &name);
        }
        Action::NewTab { pane } => {
            pane_ref(pane)?;
            let directory = pane_directory(&workspace.panes[&pane].terminal)?;
            let new = workspace.new_pane(directory, None)?;
            workspace.tabs.push(Tab {
                id: new,
                root: Node::Pane(new),
                focused: new,
            });
            workspace.active = workspace.tabs.len() - 1;
            mark_full(clients, &name);
        }
        Action::Focus(pane) => {
            if pane == previous_focus {
                return Ok(());
            }
            if !workspace.tabs[workspace.active].root.contains(pane) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "pane is not visible",
                ));
            }
            workspace.tabs[workspace.active].focused = pane;
        }
        Action::Navigate(direction) => {
            workspace.navigate(direction);
            if workspace.focused() == previous_focus {
                return Ok(());
            }
        }
        Action::Tab(delta) => {
            let active = (workspace.active as i64 + i64::from(delta))
                .rem_euclid(workspace.tabs.len() as i64) as usize;
            if active == workspace.active {
                return Ok(());
            }
            workspace.active = active;
            mark_full(clients, &name);
        }
        Action::ClosePane(pane) => {
            pane_ref(pane)?;
            cancel_pane_files(clients, pane);
            closing
                .0
                .extend(workspace.remove_pane(pane).map(Terminal::shutdown));
            if !workspace.tabs.is_empty() {
                workspace.resize(workspace.size)?;
            }
        }
        Action::CloseTab(id) => {
            let index = workspace
                .tabs
                .iter()
                .position(|tab| tab.id == id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tab no longer exists"))?;
            let tab = workspace.tabs.remove(index);
            tab.root.for_each_pane(&mut |pane| {
                cancel_pane_files(clients, pane);
                closing
                    .0
                    .push(workspace.panes.remove(&pane).unwrap().terminal.shutdown());
                Ok(())
            })?;
            if index < workspace.active {
                workspace.active -= 1;
            }
            workspace.active = workspace.active.min(workspace.tabs.len().saturating_sub(1));
            if !workspace.tabs.is_empty() {
                workspace.resize(workspace.size)?;
            }
        }
        Action::Terminate => {
            let workspace = workspaces.remove(&name).unwrap();
            terminate_workspace(workspace, clients, closing);
            return Ok(());
        }
        action => {
            if !apply_pane_action(index, action, clients, workspace, &name)? {
                return Ok(());
            }
        }
    }
    if !workspace.tabs.is_empty() && previous_focus != workspace.focused() {
        for (id, bytes) in [
            (previous_focus, b"\x1b[O".as_slice()),
            (workspace.focused(), b"\x1b[I".as_slice()),
        ] {
            if let Some(pane) = workspace.panes.get(&id)
                && pane
                    .terminal
                    .core
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?
                    .mode()
                    .contains(Mode::FOCUS_IN_OUT)
            {
                pane.terminal.write(bytes)?;
            }
        }
    }
    dirty.insert(name);
    Ok(())
}

/// Writes Agent input only while the terminal is still in the state the Agent observed.
fn agent_input(
    terminal: &Terminal,
    generation: u64,
    prompt: Option<u64>,
    data: Vec<u8>,
) -> io::Result<Option<u64>> {
    let mut core = terminal
        .core
        .lock()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if core.input_generation() != generation {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Terminal input changed; observe it again before sending input",
        ));
    }
    if core.shell_editing() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The shell already has pending input",
        ));
    }
    if let Some(prompt) = prompt
        && core.shell_prompt() != Some(prompt)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The shell is not at an untouched prompt",
        ));
    }
    terminal.try_write(data)?;
    core.note_input();
    Ok(prompt)
}

/// Pane view actions; returns false when nothing changed for other clients.
fn apply_pane_action(
    index: usize,
    action: Action,
    clients: &mut [Connection],
    workspace: &mut Workspace,
    name: &str,
) -> io::Result<bool> {
    let client_id = clients[index].id;
    let pane_ref = |id: PaneId| {
        workspace
            .panes
            .get(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane does not exist"))
    };
    match action {
        Action::Scroll { pane, scroll } => {
            if !pane_ref(pane)?
                .terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?
                .scroll_display(scroll)
            {
                return Ok(false);
            }
        }
        Action::SelectionStart {
            pane,
            point,
            side,
            kind,
        } => {
            {
                let mut core = pane_ref(pane)?
                    .terminal
                    .core
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                validate_point(&core, point)?;
                core.selection_start(point, side, kind);
            }
            for (id, other) in &workspace.panes {
                if *id != pane {
                    other
                        .terminal
                        .core
                        .lock()
                        .map_err(|error| io::Error::other(error.to_string()))?
                        .clear_selection();
                }
            }
        }
        Action::SelectionUpdate { pane, point, side } => {
            let mut core = pane_ref(pane)?
                .terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            validate_point(&core, point)?;
            if !core.selection_update(point, side) {
                return Ok(false);
            }
        }
        Action::SelectionClear { pane } => {
            if !pane_ref(pane)?
                .terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?
                .clear_selection()
            {
                return Ok(false);
            }
        }
        Action::PromptJump { pane, previous } => {
            if !pane_ref(pane)?
                .terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?
                .prompt_jump(previous)
            {
                return Ok(false);
            }
        }
        action @ (Action::CopySelection { pane, request }
        | Action::CopyBlock { pane, request, .. }) => {
            let block = if let Action::CopyBlock { block, .. } = action {
                Some(block)
            } else {
                None
            };
            let copy = CopyRequest {
                pane,
                request,
                block,
            };
            if let Err(error) = pane_ref(pane).map(|pane| {
                queue_copy(
                    &mut clients[index],
                    Cow::Borrowed(name),
                    &pane.terminal,
                    copy,
                )
            }) {
                clients[index].respond(Response::CopyError {
                    request,
                    error: error.to_string(),
                });
            }
        }
        Action::ReadHistory {
            pane,
            request,
            block,
            cursor,
            query,
        } => match pane_ref(pane) {
            Ok(source) => queue_read(
                &mut clients[index],
                Cow::Borrowed(name),
                &source.terminal,
                ReadRequest {
                    pane,
                    request,
                    block,
                    cursor,
                    query,
                },
            ),
            Err(error) => clients[index].respond(Response::History {
                request,
                result: Err(error.to_string()),
            }),
        },
        Action::Search {
            pane,
            request,
            query,
        } => {
            let terminal = &pane_ref(pane)?.terminal;
            clients[index].search.take_if(|search| search.pane != pane);
            let search = clients[index].search.get_or_insert_with(|| {
                let origin = (workspace.controller == Some(client_id))
                    .then(|| terminal.core.lock().unwrap().search_anchor());
                PaneSearch {
                    pane,
                    request,
                    query: String::new(),
                    core: Arc::clone(&terminal.core),
                    origin,
                    cancelled: Default::default(),
                    state: SearchState::Queued,
                }
            });
            search.cancelled.store(true, Ordering::Relaxed);
            search.request = request;
            search.query = query;
            search.state = SearchState::Queued;
            if search.query.is_empty() {
                if let Some(origin) = search.origin {
                    search.core.lock().unwrap().restore_search(origin);
                }
                search.state = SearchState::Ready(crate::search::SearchResult {
                    matches: Box::new([]),
                    truncated: false,
                    columns: 0,
                    alternate: false,
                });
                clients[index].respond(Response::Search {
                    pane,
                    request,
                    count: 0,
                    truncated: false,
                });
            } else {
                search.cancelled = Default::default();
                try_search(search, terminal)?;
            }
        }
        Action::SearchSelect {
            pane,
            request,
            index: found,
        } => {
            if let Some(search) = clients[index]
                .search
                .as_mut()
                .filter(|search| search.pane == pane && search.request == request)
                && let SearchState::Ready(result) = &search.state
            {
                search
                    .core
                    .lock()
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .search_select(result, found as usize)?;
            }
        }
        Action::SearchCancel { pane, request } => {
            clients[index]
                .search
                .take_if(|search| search.pane == pane && search.request == request);
        }
        _ => unreachable!(),
    }
    Ok(true)
}

/// Handles actions that need no attached workspace; returns every other action.
fn apply_session_action(
    index: usize,
    action: Action,
    clients: &mut [Connection],
    workspaces: &mut HashMap<String, Workspace>,
    dirty: &mut HashSet<String>,
    history_root: &Path,
    closing: &mut ClosingTerminals,
) -> io::Result<Option<Action>> {
    let client_id = clients[index].id;
    match action {
        Action::CancelAgentFile { request } => {
            if let Some(files) = &mut clients[index].files {
                files.cancel(|source| source.request == request);
            }
            Ok(None)
        }
        Action::CancelHistory { request } => {
            let client = &mut clients[index];
            client
                .read_requests
                .retain(|(_, read)| read.request != request);
            if let Some(position) = client.reads.iter().position(|read| read.request == request) {
                client.reads.swap_remove(position);
            }
            Ok(None)
        }
        Action::ScreenRequest { request, wait } => {
            let client = &mut clients[index];
            client.screen_request = Some(request);
            client.screen_dirty |= !wait;
            Ok(None)
        }
        Action::AgentAttach { name, token } => {
            let workspace = workspaces.get_mut(&name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "workspace does not exist")
            })?;
            if workspace.agent.owner.is_some() || workspace.agent.token.as_ref() != Some(&token) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "invalid agent owner token",
                ));
            }
            workspace.agent.token = None;
            workspace.agent.owner = Some(client_id);
            let client = &mut clients[index];
            client.agent = true;
            client.workspace = Some(name.clone());
            client.full = true;
            client.respond(Response::Agent(AgentEvent::Attached));
            if let Some(payload) = &workspace.agent.view {
                client.respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
            for payload in workspace.agent.pending.drain(..) {
                client.respond(Response::Agent(AgentEvent::Request(payload)));
            }
            dirty.insert(name);
            Ok(None)
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
                        .filter(|client| {
                            !client.agent && client.workspace.as_deref() == Some(&workspace.name)
                        })
                        .count(),
                })
                .collect();
            list.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            clients[index].respond(Response::List(list));
            Ok(None)
        }
        Action::Create { mut name, options } => {
            if name.is_empty() {
                let mut number = 0usize;
                while workspaces.contains_key(&number.to_string()) {
                    number += 1;
                }
                name = number.to_string();
            }
            validate_name(&name)?;
            validate_size(options.size)?;
            if options.foreground > 0xffffff
                || options.background > 0xffffff
                || options
                    .directory
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workspace colors must be 24-bit RGB and its directory must be absolute",
                ));
            }
            if workspaces.contains_key(&name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "workspace already exists",
                ));
            }
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
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
            };
            let pane = workspace.new_pane(options.directory, options.program)?;
            workspace.tabs.push(Tab {
                id: pane,
                root: Node::Pane(pane),
                focused: pane,
            });
            workspaces.insert(name.clone(), workspace);
            clients[index].workspace = Some(name.clone());
            clients[index].agent_open = false;
            clients[index].size = options.size;
            clients[index].full = true;
            dirty.insert(name);
            Ok(None)
        }
        Action::Attach { name, size } => {
            validate_size(size)?;
            clients[index].search = None;
            for client in clients.iter_mut().filter(|client| {
                client.workspace.as_deref() == Some(&name)
                    && client
                        .search
                        .as_ref()
                        .is_some_and(|search| search.origin.is_some())
            }) {
                client.search = None;
            }

            let workspace = workspaces.get_mut(&name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "workspace does not exist")
            })?;
            workspace.controller = Some(client_id);
            workspace.resize(size)?;
            clients[index].workspace = Some(name.clone());
            clients[index].agent_open = false;
            clients[index].size = size;
            mark_full(clients, &name);
            dirty.insert(name);
            Ok(None)
        }
        Action::TerminateName(name) => {
            let notify_requester = clients[index].workspace.is_none();
            let workspace = workspaces.remove(&name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "workspace does not exist")
            })?;
            terminate_workspace(workspace, clients, closing);
            if notify_requester {
                clients[index].respond(Response::Detached);
            }
            Ok(None)
        }
        action => Ok(Some(action)),
    }
}
fn apply_agent_action(
    index: usize,
    action: Action,
    clients: &mut [Connection],
    workspace: &mut Workspace,
) -> io::Result<()> {
    let client_id = clients[index].id;
    match action {
        Action::AgentOpen => {
            clients[index].agent_open = true;
            if let Some(payload) = &workspace.agent.view {
                clients[index].respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
            for model in workspace.agent.models.values() {
                clients[index].respond(Response::Agent(AgentEvent::Model(model.clone())));
            }
            clients[index].respond(Response::Agent(AgentEvent::Ready));
            Ok(())
        }
        Action::AgentView(payload) if workspace.controller == Some(client_id) => {
            clients[index].agent_open = true;
            for client in clients.iter_mut().filter(|client| {
                client.id != client_id
                    && (client.agent || client.agent_open)
                    && client.workspace.as_deref() == Some(&workspace.name)
            }) {
                client.respond(Response::Agent(AgentEvent::Draft(payload.clone())));
            }
            workspace.agent.view = Some(payload);
            Ok(())
        }
        Action::AgentRequest(payload) if workspace.controller == Some(client_id) => {
            clients[index].agent_open = true;
            if let Some(owner) = workspace.agent.owner.and_then(|id| {
                clients
                    .iter_mut()
                    .find(|client| client.id == id && !client.failed)
            }) {
                owner.respond(Response::Agent(AgentEvent::Request(payload)));
            } else {
                let bytes: usize = workspace
                    .agent
                    .pending
                    .iter()
                    .map(|payload| payload.len() + 6)
                    .sum();
                if bytes + payload.len() + 6 > crate::wire::MAX_FRAME {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "agent command queue is full",
                    ));
                }
                workspace.agent.pending.push(payload);
                if workspace.agent.token.is_none() {
                    use std::io::Read;
                    let mut token = vec![0; 32];
                    File::open("/dev/urandom")?.read_exact(&mut token)?;
                    workspace.agent.token = Some(token.clone());
                    clients[index].respond(Response::Agent(AgentEvent::Start(token)));
                }
            }
            Ok(())
        }
        Action::AgentUpdate(patch) if clients[index].agent => {
            let model = workspace.agent.models.entry(patch.session).or_default();
            patch.apply_messages(&mut model.messages)?;
            if patch.changes.is_some() {
                model.changes.clone_from(&patch.changes);
            }
            model.session = patch.session;
            model.busy = patch.busy;
            model.revision = patch.revision;
            model.turn = patch.turn;
            model.recovery.clone_from(&patch.recovery);
            model.changes_omitted = patch.changes_omitted;
            let mut subscribers = clients.iter_mut().filter(|client| {
                client.agent_open && client.workspace.as_deref() == Some(&workspace.name)
            });
            if let Some(last) = subscribers.next_back() {
                for client in subscribers {
                    client.respond(Response::Agent(AgentEvent::Model(patch.clone())));
                }
                last.respond(Response::Agent(AgentEvent::Model(patch)));
            }
            Ok(())
        }
        Action::AgentRemove(session) if clients[index].agent => {
            workspace.agent.models.remove(&session);
            Ok(())
        }
        Action::AgentView(_)
        | Action::AgentRequest(_)
        | Action::AgentUpdate(_)
        | Action::AgentRemove(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent action is not authorized",
        )),
        _ => unreachable!(),
    }
}

fn try_search(search: &mut PaneSearch, terminal: &Terminal) -> io::Result<()> {
    let wake = unsafe { BorrowedFd::borrow_raw(terminal.event_fd()) };
    let result = search
        .core
        .lock()
        .map_err(|e| io::Error::other(e.to_string()))?
        .search(&search.query, &search.cancelled, wake);
    match result {
        Ok(receiver) => search.state = SearchState::Pending(receiver),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
        Err(error) => return Err(error),
    }
    Ok(())
}
fn queue_copy(
    client: &mut Connection,
    name: Cow<'_, str>,
    terminal: &Terminal,
    copy: CopyRequest,
) -> bool {
    if client.copy.is_none() {
        let request = copy.request;
        let result = terminal
            .core
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))
            .and_then(|mut core| match copy.block {
                None => core.copy_selection(),
                Some(block) => core.copy_block(block),
            });
        match result {
            Ok(receiver) => {
                client.copy = Some((request, receiver));
                return true;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
            Err(error) => {
                client.respond(Response::CopyError {
                    request,
                    error: error.to_string(),
                });
                return true;
            }
        }
    }
    client.copy_requests.push_back((name.into_owned(), copy));
    false
}
fn queue_read(client: &mut Connection, name: Cow<'_, str>, terminal: &Terminal, read: ReadRequest) {
    let result = terminal
        .core
        .lock()
        .map_err(|error| io::Error::other(error.to_string()))
        .and_then(|mut core| core.read_history(read.block, read.cursor, read.query.as_deref()));
    match result {
        Ok((receiver, cancelled)) => client.reads.push(PendingRead {
            request: read.request,
            workspace: name.into_owned(),
            pane: read.pane,
            receiver,
            cancelled,
        }),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            client.read_requests.push_back((name.into_owned(), read))
        }
        Err(error) => client.respond(Response::History {
            request: read.request,
            result: Err(error.to_string()),
        }),
    }
}
fn validate_point(core: &Core, point: Point) -> io::Result<()> {
    if point.column >= core.columns()
        || i64::from(point.line) < -(core.history_size() as i64)
        || point.line >= core.screen_lines() as i32
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selection point is outside the pane",
        ))
    } else {
        Ok(())
    }
}
fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 128 || name.chars().any(|c| c.is_control() || c == '/') {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace name must contain 1..128 bytes, without controls or slashes",
        ))
    } else {
        Ok(())
    }
}
fn mark_full(clients: &mut [Connection], workspace: &str) {
    for client in clients {
        if client.workspace.as_deref() == Some(workspace) {
            client.full = true;
        }
    }
}
fn pane_directory(terminal: &Terminal) -> io::Result<Option<PathBuf>> {
    match terminal.current_directory() {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Client {
    pub(crate) fn screen_stale(&self) -> bool {
        self.screen_dirty
    }

    #[cfg(any(feature = "gui", feature = "tui"))]
    pub(crate) fn endpoint(&self) -> &ClientEndpoint {
        &self.endpoint
    }
}

impl Client {
    pub(crate) fn next_agent_event(&mut self) -> Option<AgentEvent> {
        self.agent_events.pop_front()
    }

    pub(crate) fn can_agent_input(&self) -> bool {
        self.agent_delegate
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.controller)
    }

    pub(crate) fn terminal_revision(&self, pane: PaneId) -> io::Result<u64> {
        self.screens
            .get(&pane)
            .map(|screen| screen.activity)
            .ok_or_else(|| io::Error::other("Target terminal no longer exists"))
    }

    pub(crate) fn screen_revision(&self, pane: PaneId) -> io::Result<u64> {
        self.screens
            .get(&pane)
            .map(|screen| screen.output_activity)
            .ok_or_else(|| io::Error::other("Target terminal no longer exists"))
    }
}

#[cfg(test)]
mod tests;
