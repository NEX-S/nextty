//! A workspace owns its PTYs independently of any particular frontend.
pub use crate::core::{BlockMarker, CommandBlock};
use crate::core::{Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::history::{HistoryBudget, HistoryConfig, HistoryCopy};
use crate::screen::{Screen, Update};
pub use crate::search::SearchMatch;
use crate::terminal::{Event, Shell, Terminal};
use crate::wire::{Peer, Response};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
#[cfg(feature = "agent")]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type PaneId = u64;
pub type TabId = u64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    LeftRight,
    TopBottom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
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
        Self {
            size: WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 0,
                cell_height: 0,
            },
            foreground: 0xcccccc,
            background: 0x202020,
            directory: None,
            program: None,
            history_capacity: 1 << 30,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Action {
    AgentOpen,
    AgentView(Vec<u8>),
    AgentRequest(Vec<u8>),
    AgentAttach {
        name: String,
        token: Vec<u8>,
    },
    AgentUpdate {
        kind: u8,
        payload: Vec<u8>,
    },
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
#[derive(Clone, Debug)]
pub struct TabView {
    pub id: TabId,
    pub title: String,
}
#[derive(Clone, Debug)]
pub struct PaneView {
    pub id: PaneId,
    pub rect: Rect,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub process: Arc<crate::terminal::ProcessMetadata>,
    pub blocks: Vec<BlockMarker>,
    pub command_blocks: Vec<CommandBlock>,
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
        let mut width = 0u16;
        for ch in text.chars() {
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
        .map_or(9, |tab| text_width(&tab.title, 22) + 8);
    let minimum = if snapshot.controller { 0 } else { 12 };
    let reserve = text_width(right_label, columns)
        .saturating_add(3)
        .min(columns.saturating_sub(5 + active_width))
        .max(minimum.min(columns.saturating_sub(5 + 9)));
    let available = columns.saturating_sub(5 + reserve);
    let tab_width = |title: &str, available| (text_width(title, 22) + 8).min(available);
    let mut start = active;
    let mut used = snapshot
        .tabs
        .get(active)
        .map_or(0, |tab| tab_width(&tab.title, available));
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
        let width = tab_width(&tab.title, (3 + available).saturating_sub(x));
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
        if before == 0 && after == 0 {
            return Cow::Borrowed(base);
        }
        let mut parts = Vec::new();
        if before > 0 {
            parts.push(format!("‹{before}"));
        }
        if after > 0 {
            parts.push(format!("{after}›"));
        }
        if !base.is_empty() {
            parts.push(base.to_owned());
        }
        parts.join(" ").into()
    };
    let (before, after) = counts(&tabs);
    if before > 0 || after > 0 {
        tabs = tab_layout(snapshot, columns, &label(before, after));
    }
    let (before, after) = counts(&tabs);
    (tabs, label(before, after), before, after)
}

pub enum ClientEvent {
    Updated,
    List(Vec<WorkspaceInfo>),
    Clipboard {
        request: u64,
        file: File,
        incomplete: bool,
    },
    ClipboardStore {
        primary: bool,
        text: String,
    },
    Search {
        pane: PaneId,
        request: u64,
        matches: Vec<SearchMatch>,
        truncated: bool,
    },
    Error(String),
    Detached,
}
#[cfg(feature = "agent")]
#[derive(Clone, Debug)]
pub(crate) enum ClientEndpoint {
    Local(PathBuf),
    Remote {
        program: OsString,
        args: Vec<OsString>,
        env: Vec<(OsString, Option<OsString>)>,
        directory: Option<PathBuf>,
        target: String,
    },
}
pub struct Client {
    #[cfg(feature = "agent")]
    endpoint: ClientEndpoint,
    agent_events: Vec<(u8, Vec<u8>)>,
    agent_delegate: bool,
    peer: Peer,
    snapshot: Option<Snapshot>,
    screens: HashMap<PaneId, Screen>,
    write_error: Option<io::Error>,
    ssh: Option<Child>,
    remote_target: Option<String>,
}
impl Client {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        check_peer(stream.as_raw_fd())?;
        Ok(Self {
            peer: Peer::new(stream)?,
            #[cfg(feature = "agent")]
            endpoint: ClientEndpoint::Local(path.to_path_buf()),
            agent_events: Vec::new(),
            agent_delegate: false,
            snapshot: None,
            screens: HashMap::new(),
            write_error: None,
            ssh: None,
            remote_target: None,
        })
    }
    pub fn connect_remote(mut command: Command, target: String) -> io::Result<Self> {
        #[cfg(feature = "agent")]
        let endpoint = ClientEndpoint::Remote {
            program: command.get_program().to_owned(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            env: command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
                .collect(),
            directory: command.get_current_dir().map(Path::to_path_buf),
            target: target.clone(),
        };
        let (local, ssh) = UnixStream::pair()?;
        let peer = Peer::remote(local)?;
        command.stdin(Stdio::from(OwnedFd::from(ssh.try_clone()?)));
        command.stdout(Stdio::from(OwnedFd::from(ssh)));
        let child = command.spawn()?;
        drop(command);
        let mut client = Self {
            peer,
            #[cfg(feature = "agent")]
            endpoint,
            agent_events: Vec::new(),
            agent_delegate: false,
            snapshot: None,
            screens: HashMap::new(),
            write_error: None,
            ssh: Some(child),
            remote_target: Some(target),
        };
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
            if unsafe { libc::poll(&mut fd, 1, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    fn ssh_error(&mut self, error: io::Error) -> io::Result<io::Error> {
        if let Some(child) = &mut self.ssh
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
        self.remote_target.as_deref()
    }
    pub fn fd(&self) -> RawFd {
        self.peer.fd()
    }
    pub fn send(&mut self, action: Action) -> io::Result<()> {
        match action {
            Action::Input { pane, data } => self.peer.send_input(pane, &data)?,
            action => self.peer.send_action(&action)?,
        }
        self.flush()
    }
    pub fn scroll_to(&mut self, pane: PaneId, mut offset: usize) -> io::Result<()> {
        // Queue both actions before flushing: live output must not skew a drag delta.
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
        self.flush()
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
    pub fn wants_read(&self) -> bool {
        self.peer.wants_read()
    }
    pub fn needs_poll(&self) -> bool {
        self.peer.needs_poll()
    }
    pub fn wants_write(&self) -> bool {
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
    #[cfg(feature = "agent")]
    pub(crate) fn endpoint(&self) -> &ClientEndpoint {
        &self.endpoint
    }
    #[cfg(any(test, feature = "agent"))]
    pub(crate) fn take_agent_events(&mut self) -> Vec<(u8, Vec<u8>)> {
        std::mem::take(&mut self.agent_events)
    }
    #[cfg(any(test, feature = "agent"))]
    pub(crate) fn can_agent_input(&self) -> bool {
        self.agent_delegate
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.controller)
    }
    pub fn poll(&mut self) -> io::Result<Vec<ClientEvent>> {
        let responses = match self.peer.responses(self.snapshot.as_ref()) {
            Ok(responses) => responses,
            Err(error) => {
                let error = self.write_error.take().unwrap_or(error);
                return Err(self.ssh_error(error)?);
            }
        };
        let mut events = Vec::with_capacity(responses.len().min(4));
        for response in responses {
            match response {
                Response::State(snapshot, updates) => {
                    self.screens
                        .retain(|id, _| snapshot.panes.iter().any(|pane| pane.id == *id));
                    for (id, update) in updates {
                        self.screens.entry(id).or_default().apply(update)?;
                    }
                    if let Some(previous) = &self.snapshot {
                        for pane in &snapshot.panes {
                            if let Some(old) = previous.panes.iter().find(|old| old.id == pane.id)
                                && old.blocks != pane.blocks
                                && let Some(screen) = self.screens.get_mut(&pane.id)
                            {
                                for row in 0..screen.screen_lines() {
                                    if !screen.dirty[row] && screen.output_changed(old, pane, row) {
                                        screen.dirty[row] = true;
                                    }
                                }
                            }
                        }
                    }
                    self.snapshot = Some(snapshot);
                    if !matches!(events.last(), Some(ClientEvent::Updated)) {
                        events.push(ClientEvent::Updated);
                    }
                }
                Response::Agent { kind, payload } => {
                    if kind == 4 {
                        self.agent_delegate = true;
                    }
                    self.agent_events.push((kind, payload));
                    if !matches!(events.last(), Some(ClientEvent::Updated)) {
                        events.push(ClientEvent::Updated);
                    }
                }
                Response::List(list) => events.push(ClientEvent::List(list)),
                Response::Clipboard {
                    request,
                    file,
                    incomplete,
                } => events.push(ClientEvent::Clipboard {
                    request,
                    file,
                    incomplete,
                }),
                Response::ClipboardStore { primary, text } => {
                    events.push(ClientEvent::ClipboardStore { primary, text })
                }
                Response::Search {
                    pane,
                    request,
                    matches,
                    truncated,
                } => events.push(ClientEvent::Search {
                    pane,
                    request,
                    matches: matches.into_owned(),
                    truncated,
                }),
                Response::Error(error) => events.push(ClientEvent::Error(error)),
                Response::Detached => {
                    self.peer.cancel_output();
                    self.write_error = None;
                    self.snapshot = None;
                    self.agent_delegate = false;
                    self.screens.clear();
                    events.push(ClientEvent::Detached);
                }
            }
        }
        Ok(events)
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

pub use crate::bridge::{BRIDGE_VERSION, run_bridge};

pub struct ServerOptions {
    pub history_directory: PathBuf,
}
impl Default for ServerOptions {
    fn default() -> Self {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache")))
            .unwrap_or_else(|| PathBuf::from(".cache"));
        Self {
            history_directory: root.join("nextty/history"),
        }
    }
}

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
    if create {
        match fs::create_dir(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
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
        .stderr(Stdio::from(log));
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

#[derive(Clone, Debug)]
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
        if first.contains(left) && second.contains(right) {
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
        } else {
            first.resize_split(a, left, right, position)
                || second.resize_split(b, left, right, position)
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
    fn remove(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (one, None) | (None, one) => one,
            },
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
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let a = first.minimum();
                let b = second.minimum();
                match axis {
                    Axis::LeftRight => (a.0.saturating_add(b.0).saturating_add(1), a.1.max(b.1)),
                    Axis::TopBottom => (a.0.max(b.0), a.1.saturating_add(b.1).saturating_add(1)),
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
    owner: Option<u64>,
    token: Option<Vec<u8>>,
    pending: Vec<Vec<u8>>,
    view: Option<Vec<u8>>,
    models: Option<Vec<u8>>,
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
    next_id: u64,
    budget: Arc<HistoryBudget>,
}
impl Workspace {
    fn focused(&self) -> PaneId {
        self.tabs[self.active].focused
    }
    fn layout(&self) -> Vec<(PaneId, Rect)> {
        let mut result = Vec::new();
        self.tabs[self.active].root.layout(
            Rect {
                x: 0,
                y: 1,
                cols: self.size.num_cols,
                rows: self.size.num_lines.saturating_sub(1),
            },
            &mut result,
        );
        result
    }
    fn all_layouts(&self) -> Vec<(PaneId, Rect)> {
        let mut result = Vec::new();
        for tab in &self.tabs {
            tab.root.layout(
                Rect {
                    x: 0,
                    y: 1,
                    cols: self.size.num_cols,
                    rows: self.size.num_lines.saturating_sub(1),
                },
                &mut result,
            );
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
        let id = self.next_id;
        self.next_id += 1;
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
        let terminal = self.panes.remove(&id).map(|pane| pane.terminal);
        let old = std::mem::take(&mut self.tabs);
        self.tabs = old
            .into_iter()
            .filter_map(|tab| {
                tab.root.remove(id).map(|root| Tab {
                    focused: if tab.focused == id {
                        root.first()
                    } else {
                        tab.focused
                    },
                    root,
                    id: tab.id,
                })
            })
            .collect();
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
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
                    Ok(TabView { id: tab.id, title })
                })
                .collect::<io::Result<Vec<_>>>()?,
            panes,
        })
    }
    fn navigate(&mut self, direction: Direction) {
        let layout = self.layout();
        let current = layout
            .iter()
            .find(|(id, _)| *id == self.focused())
            .unwrap()
            .1;
        let center = |r: Rect| {
            (
                i32::from(r.x) * 2 + i32::from(r.cols),
                i32::from(r.y) * 2 + i32::from(r.rows),
            )
        };
        let (cx, cy) = center(current);
        if let Some((id, _)) = layout
            .iter()
            .filter(|(id, rect)| {
                if *id == self.focused() {
                    return false;
                }
                match direction {
                    Direction::Left => rect.x + rect.cols <= current.x,
                    Direction::Right => rect.x >= current.x + current.cols,
                    Direction::Up => rect.y + rect.rows <= current.y,
                    Direction::Down => rect.y >= current.y + current.rows,
                }
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
    if size.num_cols < 2
        || size.num_lines < 2
        || size.num_cols > 4096
        || size.num_lines > 2048
        || usize::from(size.num_cols) * usize::from(size.num_lines) > 1_048_576
        || size.num_cols.checked_mul(size.cell_width).is_none()
        || size.num_lines.checked_mul(size.cell_height).is_none()
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace size must be 2..4096 columns, 2..2048 rows, at most 1048576 cells",
        ))
    } else {
        Ok(())
    }
}
struct PaneSearch {
    pane: PaneId,
    request: u64,
    query: String,
    core: std::sync::Arc<std::sync::Mutex<crate::core::Core>>,
    origin: Option<crate::core::SearchAnchor>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    receiver: Option<std::sync::mpsc::Receiver<io::Result<crate::search::SearchResult>>>,
    result: Option<crate::search::SearchResult>,
}
impl Drop for PaneSearch {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(origin) = self.origin {
            self.core.lock().unwrap().restore_search(origin);
        }
    }
}
struct Connection {
    agent: bool,
    id: u64,
    peer: Peer,
    workspace: Option<String>,
    size: WindowSize,
    full: bool,
    failed: bool,
    known: HashMap<PaneId, Arc<crate::terminal::ProcessMetadata>>,
    copy: Option<(u64, std::sync::mpsc::Receiver<io::Result<HistoryCopy>>)>,
    deferred: Option<Action>,
    ending: bool,
    copy_request: Option<(String, Action)>,
    search: Option<PaneSearch>,
}

impl Connection {
    fn new(id: u64, stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            id,
            agent: false,
            peer: Peer::new(stream)?,
            workspace: None,
            size: CreateOptions::default().size,
            full: true,
            failed: false,
            known: HashMap::new(),
            copy: None,
            deferred: None,
            ending: false,
            copy_request: None,
            search: None,
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
                Ok(pane.terminal.input_pending_bytes() + data.len() > 256 * 1024)
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
        self.copy_request = None;
        self.copy = None;
        self.search = None;
        self.respond(Response::Detached);
    }

    fn respond(&mut self, response: Response<'_>) {
        if let Err(error) = self.peer.send_response(response) {
            eprintln!("workspace client {}: {error}", self.id);
            self.failed = true;
        }
    }
}

/// Run in the hidden server process. No GUI or font libraries are needed here.
pub fn run_server(socket: &Path, options: ServerOptions) -> io::Result<()> {
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
    fs::create_dir_all(&options.history_directory)?;
    fs::set_permissions(
        &options.history_directory,
        fs::Permissions::from_mode(0o700),
    )?;
    private_directory(&options.history_directory, false)?;
    let result = server_loop(listener, options);
    let cleanup = fs::remove_file(socket);
    result.and(cleanup)
}
#[derive(Default)]
struct ClosingTerminals(Vec<JoinHandle<()>>);
impl ClosingTerminals {
    fn reap(&mut self) {
        for thread in self.0.extract_if(.., |thread| thread.is_finished()) {
            if thread.join().is_err() {
                eprintln!("terminal IO thread panicked");
            }
        }
    }
}
impl Drop for ClosingTerminals {
    fn drop(&mut self) {
        for thread in self.0.drain(..) {
            if thread.join().is_err() {
                eprintln!("terminal IO thread panicked");
            }
        }
    }
}

fn server_loop(listener: UnixListener, options: ServerOptions) -> io::Result<()> {
    let mut closing = ClosingTerminals::default();
    let mut workspaces: HashMap<String, Workspace> = HashMap::new();
    let mut clients: Vec<Connection> = Vec::new();
    let mut next_client = 1u64;
    let mut had_workspace = false;
    let mut fds = Vec::new();
    let mut terminals: Vec<(String, PaneId)> = Vec::new();
    let mut dirty = HashSet::new();
    loop {
        closing.reap();
        had_workspace |= !workspaces.is_empty();
        clients.retain(|client| !client.failed);
        dirty.extend(
            clients
                .iter()
                .filter(|client| client.full && !client.peer.pending())
                .filter_map(|client| client.workspace.clone()),
        );
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
                broadcast(workspace, &mut clients)?;
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
                    previous_name.clone_from(name);
                    *previous_id = *id;
                } else {
                    terminals.push((name.clone(), *id));
                }
                terminal_count += 1;
            }
        }
        terminals.truncate(terminal_count);
        // Failures and completed resizes may have no further socket or terminal wakeup.
        let timeout = if !dirty.is_empty()
            || clients.iter().any(|client| {
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
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
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
            if revents & libc::POLLOUT != 0 {
                if clients[index].peer.flush().is_err() {
                    clients[index].failed = true;
                    continue;
                }
                if clients[index].full
                    && !clients[index].peer.pending()
                    && let Some(name) = &clients[index].workspace
                {
                    dirty.insert(name.clone());
                }
            }
            if !clients[index].ending
                && (revents & (libc::POLLIN | libc::POLLHUP) != 0
                    || clients[index].deferred.is_some()
                    || clients[index].peer.buffered_action())
            {
                for _ in 0..256 {
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
                    let result = match clients[index].action_blocked(&action, &workspaces) {
                        Ok(true) => {
                            clients[index].deferred = Some(action);
                            break;
                        }
                        Ok(false) => apply_action(
                            index,
                            action,
                            &mut clients,
                            &mut workspaces,
                            &mut dirty,
                            &options.history_directory,
                            &mut closing,
                        ),
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(detach) => {
                            if detach {
                                clients[index].failed = true;
                                break;
                            }
                        }
                        Err(error) => {
                            clients[index].respond(Response::Error(error.to_string()));
                            if clients[index].failed {
                                break;
                            }
                        }
                    }
                    if !clients[index].peer.buffered_action() {
                        break;
                    }
                }
            }
        }
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
            pane.terminal.drain_notification()?;
            pane.terminal.acknowledge_wakeup();
            let mut exit = false;
            for event in pane.terminal.events.try_iter() {
                match event {
                    Event::Title(title) => pane.title = title.unwrap_or_else(|| "shell".into()),
                    Event::Exit | Event::ChildExit(_) => exit = true,
                    Event::Error(error) => {
                        eprintln!("workspace {name}: {error}");
                        exit = true;
                    }
                    Event::ClipboardStore { primary, text } => {
                        if let Some(controller) = clients
                            .iter_mut()
                            .find(|client| Some(client.id) == workspace.controller)
                        {
                            controller.respond(Response::ClipboardStore { primary, text });
                        }
                    }
                    Event::Wakeup | Event::HistoryCleared => (),
                }
            }
            if exit {
                closing
                    .0
                    .extend(workspace.remove_pane(*id).map(Terminal::shutdown));
                if !workspace.tabs.is_empty() {
                    workspace.resize(workspace.size)?;
                }
            }
            dirty.insert(name.clone());
        }
        clients.retain(|client| !client.failed);
        for (name, workspace) in workspaces.extract_if(|_, workspace| workspace.tabs.is_empty()) {
            drop(workspace);
            for client in &mut clients {
                if client.workspace.as_ref() == Some(&name) {
                    client.finish();
                }
            }
        }
        for name in dirty.drain() {
            if let Some(workspace) = workspaces.get_mut(&name) {
                broadcast(workspace, &mut clients)?;
            }
        }
        for client in &mut clients {
            if let Some((name, action)) = client.copy_request.take() {
                let pane = match action {
                    Action::CopySelection { pane, .. } | Action::CopyBlock { pane, .. } => pane,
                    _ => unreachable!(),
                };
                let result = workspaces
                    .get(&name)
                    .and_then(|workspace| workspace.panes.get(&pane))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "copy source pane no longer exists")
                    })
                    .and_then(|pane| queue_copy(client, &name, &pane.terminal, action));
                if let Err(error) = result {
                    client.respond(Response::Error(error.to_string()));
                }
            }
            let ready = client
                .copy
                .as_ref()
                .and_then(|(_, receiver)| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "history copy worker disconnected",
                    ))),
                });
            if let Some(result) = ready {
                let (request, _) = client.copy.take().unwrap();
                match result {
                    Ok(copy) => client.respond(Response::Clipboard {
                        request,
                        file: copy.file,
                        incomplete: copy.incomplete,
                    }),
                    Err(error) => client.respond(Response::Error(error.to_string())),
                }
            }
        }
        for client in &mut clients {
            let Some(search) = client.search.as_mut() else {
                continue;
            };
            let Some(terminal) = client
                .workspace
                .as_ref()
                .and_then(|name| workspaces.get(name))
                .and_then(|workspace| workspace.panes.get(&search.pane))
                .map(|pane| &pane.terminal)
            else {
                client.search = None;
                if let Some(name) = &client.workspace {
                    dirty.insert(name.clone());
                }
                continue;
            };
            if search.receiver.is_none()
                && search.result.is_none()
                && !search.query.is_empty()
                && let Err(error) = try_search(search, terminal)
            {
                let (pane, request) = (search.pane, search.request);
                client.respond(Response::Search {
                    pane,
                    request,
                    matches: Default::default(),
                    truncated: true,
                });
                client.respond(Response::Error(error.to_string()));
                client.search = None;
                if let Some(name) = &client.workspace {
                    dirty.insert(name.clone());
                }
                continue;
            }
            let ready = search
                .receiver
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "pane search worker stopped",
                    ))),
                });
            if let Some(result) = ready {
                search.receiver = None;
                let (pane, request) = (search.pane, search.request);
                match result {
                    Ok(result) => {
                        client.respond(Response::Search {
                            pane,
                            request,
                            matches: result.matches.as_slice().into(),
                            truncated: result.truncated,
                        });
                        client.search.as_mut().unwrap().result = Some(result);
                    }
                    Err(error) => {
                        client.respond(Response::Search {
                            pane,
                            request,
                            matches: Default::default(),
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
            if fd.events & libc::POLLOUT != 0 && fd.revents & libc::POLLOUT == 0 {
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
fn broadcast(workspace: &mut Workspace, clients: &mut [Connection]) -> io::Result<()> {
    if !clients.iter().any(|client| {
        client.workspace.as_deref() == Some(&workspace.name)
            && !client.peer.pending()
            && !client.failed
    }) {
        for client in clients {
            if client.workspace.as_deref() == Some(&workspace.name) {
                client.full = true;
            }
        }
        return Ok(());
    }
    let active = workspace.layout();
    let visible = if clients.iter().any(|client| {
        client.agent && client.workspace.as_deref() == Some(&workspace.name) && !client.failed
    }) {
        workspace.all_layouts()
    } else {
        active.clone()
    };
    let mut frames = Vec::with_capacity(visible.len());
    let mut panes = Vec::with_capacity(visible.len());
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
        let cwd = core.current_directory().map(Path::to_path_buf);
        if !core.mode().contains(Mode::SYNC_UPDATE) {
            let eligible = || {
                clients.iter().filter(|client| {
                    client.workspace.as_deref() == Some(&workspace.name)
                        && !client.peer.pending()
                        && !client.failed
                        && (client.agent || active.iter().any(|(pane, _)| *pane == id))
                })
            };
            let full = eligible()
                .any(|client| client.full || !client.known.contains_key(&id))
                .then(|| Update::capture(&core, true));
            let delta = (eligible().any(|client| !client.full && client.known.contains_key(&id))
                && (full.is_none() || core.dirty_rows().count() != core.screen_lines()))
            .then(|| Update::capture(&core, false));
            frames.push((id, full, delta, Arc::clone(&process)));
            core.reset_damage();
        }
        drop(core);
        panes.push(PaneView {
            id,
            rect,
            title: pane.title.clone(),
            cwd: match cwd {
                Some(directory) => Some(directory),
                None => pane_directory(&pane.terminal)?,
            },
            process,
            blocks,
            command_blocks,
        });
    }
    let all_snapshot = workspace.snapshot(panes)?;
    let mut updates = Vec::new();
    for client in clients {
        if client.workspace.as_deref() != Some(&workspace.name) || client.failed {
            continue;
        }
        if client.peer.pending() {
            client.full = true;
            continue;
        }
        let mut snapshot = all_snapshot.clone();
        if !client.agent {
            snapshot
                .panes
                .retain(|pane| active.iter().any(|(id, _)| *id == pane.id));
        }
        updates.clear();
        updates.extend(frames.iter().filter_map(|(id, full, delta, _)| {
            if !client.agent && !active.iter().any(|(pane, _)| pane == id) {
                return None;
            }
            let update = if !client.known.contains_key(id) || client.full {
                full.as_ref()
            } else {
                delta.as_ref().or(full.as_ref())
            }?;
            Some((*id, update))
        }));
        snapshot.controller = workspace.controller == Some(client.id);
        if let Err(error) = client.peer.send_state(&snapshot, &updates, &client.known) {
            eprintln!("workspace client {}: {error}", client.id);
            client.failed = true;
        } else {
            client
                .known
                .retain(|id, _| snapshot.panes.iter().any(|pane| *id == pane.id));
            client.known.extend(
                frames
                    .iter()
                    .filter(|(id, _, _, _)| snapshot.panes.iter().any(|pane| pane.id == *id))
                    .map(|(id, _, _, process)| (*id, Arc::clone(process))),
            );
        }
        client.full = false;
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
) -> io::Result<bool> {
    let client_id = clients[index].id;
    if clients[index].agent
        && !matches!(
            action,
            Action::Input { .. } | Action::AgentUpdate { .. } | Action::Detach
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent owner can only update agent state or send PTY input",
        ));
    }
    match action {
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
            client.respond(Response::Agent {
                kind: 4,
                payload: Vec::new(),
            });
            if let Some(payload) = &workspace.agent.view {
                client.respond(Response::Agent {
                    kind: 0,
                    payload: payload.clone(),
                });
            }
            for payload in workspace.agent.pending.drain(..) {
                client.respond(Response::Agent { kind: 2, payload });
            }
            dirty.insert(name);
            return Ok(false);
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
            list.sort_by(|a, b| a.name.cmp(&b.name));
            clients[index].respond(Response::List(list));
            return Ok(false);
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
                next_id: 1,
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
            clients[index].size = options.size;
            clients[index].full = true;
            dirty.insert(name);
            return Ok(false);
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
            clients[index].size = size;
            for client in clients {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
            dirty.insert(name);
            return Ok(false);
        }
        Action::TerminateName(name) => {
            let notify_requester = clients[index].workspace.is_none();
            let workspace = workspaces.remove(&name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "workspace does not exist")
            })?;
            closing.0.extend(
                workspace
                    .panes
                    .into_values()
                    .map(|pane| pane.terminal.shutdown()),
            );
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.finish();
                }
            }
            if notify_requester {
                clients[index].respond(Response::Detached);
            }
            return Ok(false);
        }
        Action::Detach => return Ok(true),
        _ => (),
    }
    let name = clients[index].workspace.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "attach to a workspace first")
    })?;
    let workspace = workspaces
        .get_mut(name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace does not exist"))?;
    match &action {
        Action::AgentOpen => {
            for (kind, payload) in [(0, &workspace.agent.view), (1, &workspace.agent.models)] {
                if let Some(payload) = payload {
                    clients[index].respond(Response::Agent {
                        kind,
                        payload: payload.clone(),
                    });
                }
            }
            clients[index].respond(Response::Agent {
                kind: 5,
                payload: Vec::new(),
            });
            return Ok(false);
        }
        Action::AgentView(payload) if workspace.controller == Some(client_id) => {
            workspace.agent.view = Some(payload.clone());
            for client in clients.iter_mut().filter(|client| {
                client.id != client_id && client.workspace.as_deref() == Some(&workspace.name)
            }) {
                client.respond(Response::Agent {
                    kind: 0,
                    payload: payload.clone(),
                });
            }
            return Ok(false);
        }
        Action::AgentRequest(payload) if workspace.controller == Some(client_id) => {
            if let Some(owner) = workspace.agent.owner.and_then(|id| {
                clients
                    .iter_mut()
                    .find(|client| client.id == id && !client.failed)
            }) {
                owner.respond(Response::Agent {
                    kind: 2,
                    payload: payload.clone(),
                });
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
                workspace.agent.pending.push(payload.clone());
                if workspace.agent.token.is_none() {
                    use std::io::Read;
                    let mut token = vec![0; 32];
                    File::open("/dev/urandom")?.read_exact(&mut token)?;
                    workspace.agent.token = Some(token.clone());
                    clients[index].respond(Response::Agent {
                        kind: 3,
                        payload: token,
                    });
                }
            }
            return Ok(false);
        }
        Action::AgentUpdate { kind, payload } if clients[index].agent && matches!(kind, 0 | 1) => {
            if *kind == 0 {
                workspace.agent.view = Some(payload.clone());
            } else {
                workspace.agent.models = Some(payload.clone());
            }
            for client in clients.iter_mut().filter(|client| {
                !client.agent && client.workspace.as_deref() == Some(&workspace.name)
            }) {
                client.respond(Response::Agent {
                    kind: *kind,
                    payload: payload.clone(),
                });
            }
            return Ok(false);
        }
        Action::AgentView(_) | Action::AgentRequest(_) | Action::AgentUpdate { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "agent action is not authorized",
            ));
        }
        _ => (),
    }
    if let Action::Resize(size) = action {
        validate_size(size)?;
        clients[index].size = size;
        if workspace.controller != Some(client_id) {
            return Ok(false);
        }
    }
    if workspace.controller != Some(client_id)
        && !(clients[index].agent && matches!(action, Action::Input { .. }))
        && !matches!(
            action,
            Action::CopySelection { .. }
                | Action::CopyBlock { .. }
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
    if let Action::Input { pane, data } = action {
        let terminal = &pane_ref(pane)?.terminal;
        let focus_report = data == b"\x1b[I" || data == b"\x1b[O";
        terminal.try_write(data)?;
        if !focus_report {
            let mut core = terminal
                .core
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let selected = core.clear_selection();
            let scrolled = core.display_offset() != 0;
            core.scroll_display(Scroll::Bottom);
            if selected || scrolled {
                dirty.insert(workspace.name.clone());
            }
        }
        return Ok(false);
    }
    let name = workspace.name.clone();
    match action {
        Action::Resize(size) => {
            if size == workspace.size {
                return Ok(false);
            }
            workspace.resize(size)?;
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
        }
        Action::ResizeSplit {
            first,
            second,
            position,
        } => {
            let rect = Rect {
                x: 0,
                y: 1,
                cols: workspace.size.num_cols,
                rows: workspace.size.num_lines - 1,
            };
            if !workspace.tabs[workspace.active]
                .root
                .resize_split(rect, first, second, position)
            {
                return Ok(false);
            }
            workspace.resize(workspace.size)?;
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
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
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
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
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
        }
        Action::Focus(pane) => {
            if pane == previous_focus {
                return Ok(false);
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
                return Ok(false);
            }
        }
        Action::Tab(delta) => {
            let active = (workspace.active as i64 + i64::from(delta))
                .rem_euclid(workspace.tabs.len() as i64) as usize;
            if active == workspace.active {
                return Ok(false);
            }
            workspace.active = active;
            for client in clients.iter_mut() {
                if client.workspace.as_deref() == Some(&name) {
                    client.full = true;
                }
            }
        }
        Action::ClosePane(pane) => {
            pane_ref(pane)?;
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
            let mut panes = Vec::new();
            tab.root.layout(
                Rect {
                    cols: workspace.size.num_cols,
                    rows: workspace.size.num_lines - 1,
                    ..Rect::default()
                },
                &mut panes,
            );
            for (pane, _) in panes {
                closing
                    .0
                    .push(workspace.panes.remove(&pane).unwrap().terminal.shutdown());
            }
            if index < workspace.active {
                workspace.active -= 1;
            }
            workspace.active = workspace.active.min(workspace.tabs.len().saturating_sub(1));
            if !workspace.tabs.is_empty() {
                workspace.resize(workspace.size)?;
            }
        }
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
        action @ Action::CopySelection { pane, .. } => {
            queue_copy(
                &mut clients[index],
                &name,
                &pane_ref(pane)?.terminal,
                action,
            )?;
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
        action @ Action::CopyBlock { pane, .. } => {
            queue_copy(
                &mut clients[index],
                &name,
                &pane_ref(pane)?.terminal,
                action,
            )?;
        }
        Action::Search {
            pane,
            request,
            query,
        } => {
            let terminal = &pane_ref(pane)?.terminal;
            if clients[index]
                .search
                .as_ref()
                .is_some_and(|search| search.pane != pane)
            {
                clients[index].search = None;
            }
            if clients[index].search.is_none() {
                let origin = (workspace.controller == Some(client_id))
                    .then(|| terminal.core.lock().unwrap().search_anchor());
                clients[index].search = Some(PaneSearch {
                    pane,
                    request,
                    query: String::new(),
                    core: std::sync::Arc::clone(&terminal.core),
                    origin,
                    cancelled: Default::default(),
                    receiver: None,
                    result: None,
                });
            }
            let search = clients[index].search.as_mut().unwrap();
            search
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            search.cancelled = Default::default();
            search.request = request;
            search.query = query;
            search.receiver = None;
            search.result = None;
            if search.query.is_empty() {
                if let Some(origin) = search.origin {
                    search.core.lock().unwrap().restore_search(origin);
                }
                search.result = Some(crate::search::SearchResult {
                    matches: Vec::new(),
                    truncated: false,
                    columns: 0,
                    alternate: false,
                });
                clients[index].respond(Response::Search {
                    pane,
                    request,
                    matches: Default::default(),
                    truncated: false,
                });
            } else {
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
                && let Some(result) = &search.result
            {
                search
                    .core
                    .lock()
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .search_select(result, found as usize)?;
            }
        }
        Action::SearchCancel { pane, request } => {
            if clients[index]
                .search
                .as_ref()
                .is_some_and(|search| search.pane == pane && search.request == request)
            {
                clients[index].search = None;
            }
        }
        Action::Terminate => {
            let workspace = workspaces.remove(&name).unwrap();
            closing.0.extend(
                workspace
                    .panes
                    .into_values()
                    .map(|pane| pane.terminal.shutdown()),
            );
            for client in clients {
                if client.workspace.as_deref() == Some(&name) {
                    client.finish();
                }
            }
            return Ok(false);
        }
        _ => unreachable!(),
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
    Ok(false)
}
fn try_search(search: &mut PaneSearch, terminal: &Terminal) -> io::Result<()> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::fcntl(terminal.event_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let wake = unsafe { OwnedFd::from_raw_fd(fd) };
    match search
        .core
        .lock()
        .map_err(|e| io::Error::other(e.to_string()))?
        .search(
            search.query.clone(),
            std::sync::Arc::clone(&search.cancelled),
            wake,
        ) {
        Ok(receiver) => search.receiver = Some(receiver),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
        Err(error) => return Err(error),
    }
    Ok(())
}
fn queue_copy(
    client: &mut Connection,
    name: &str,
    terminal: &Terminal,
    action: Action,
) -> io::Result<()> {
    if client.copy.is_some() || client.copy_request.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "a history copy is already in progress",
        ));
    }
    let (request, result) = match action {
        Action::CopySelection { request, .. } => (request, terminal.copy_selection()),
        Action::CopyBlock { request, block, .. } => (request, terminal.copy_block(block)),
        _ => unreachable!(),
    };
    match result {
        Ok(receiver) => client.copy = Some((request, receiver)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            client.copy_request = Some((name.to_owned(), action))
        }
        Err(error) => return Err(error),
    }
    Ok(())
}
fn validate_point(core: &crate::core::Core, point: Point) -> io::Result<()> {
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
fn pane_directory(terminal: &Terminal) -> io::Result<Option<PathBuf>> {
    match terminal.current_directory() {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
