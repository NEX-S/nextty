#[cfg(feature = "files")]
use crate::files::FileNav;
#[cfg(feature = "monitor")]
use crate::monitor::Monitor;
use crate::search::Search;
#[cfg(feature = "agent")]
use crate::{
    agent::Agent,
    config::Config,
    context::{self, Object, Snapshot as ContextSnapshot},
    core::Mode,
    width::scalar_width,
    workspace::{Client, PaneId},
};
#[cfg(feature = "agent")]
use std::{
    collections::HashSet,
    ops::Range,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{Arc, Condvar, Mutex},
};
use std::{
    io,
    time::{Duration, Instant},
};

pub(crate) struct Ui {
    pub tab_deadline: Option<Instant>,
    tab_phase: u8,
    pub status: Option<(String, Instant)>,
    pub focus: Focus,
    pub close_tab: Option<crate::workspace::TabId>,
    pub close_confirm: bool,
    pub help: bool,
    pub help_scroll: usize,
    pub search: Search,
    #[cfg(feature = "agent")]
    pub agent: AgentPanel,
    #[cfg(feature = "monitor")]
    pub resources: Monitor,
    #[cfg(feature = "files")]
    pub files: Option<FileNav>,
    #[cfg(feature = "files")]
    pub files_open: bool,
}

impl Ui {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            tab_deadline: None,
            tab_phase: 0,
            status: None,
            focus: Focus::Terminal,
            close_tab: None,
            close_confirm: false,
            help: false,
            help_scroll: 0,
            search: Search::default(),
            #[cfg(feature = "agent")]
            agent: AgentPanel::default(),
            #[cfg(feature = "monitor")]
            resources: Monitor::new()?,
            #[cfg(feature = "files")]
            files: None,
            #[cfg(feature = "files")]
            files_open: false,
        })
    }
    pub fn update_tabs(
        &mut self,
        snapshot: &crate::workspace::Snapshot,
        columns: u16,
        label: &str,
        now: Instant,
    ) -> bool {
        use crate::workspace::TabActivity;
        let running = snapshot
            .tabs
            .iter()
            .any(|tab| tab.activity == TabActivity::Running)
            && crate::workspace::tab_chrome(snapshot, columns, label)
                .0
                .iter()
                .any(|(index, _, _)| snapshot.tabs[*index].activity == TabActivity::Running);
        if !running {
            self.tab_deadline = None;
            return false;
        }
        let changed = self.tab_deadline.is_some_and(|deadline| now >= deadline);
        if self.tab_deadline.is_none() || changed {
            self.tab_phase = if changed { (self.tab_phase + 1) % 8 } else { 0 };
            self.tab_deadline = Some(now + Duration::from_millis(250));
        }
        changed
    }

    pub fn tab_color(
        &self,
        tab: &crate::workspace::TabView,
        foreground: u32,
        background: u32,
    ) -> u32 {
        use crate::workspace::TabActivity;
        match tab.activity {
            TabActivity::Finished => return 0xfe9b30,
            TabActivity::Failed => return 0xe87575,
            _ => (),
        }
        let wave = [0, 8, 16, 24, 32, 24, 16, 8][usize::from(self.tab_phase)];
        let weight = if tab.activity == TabActivity::Running {
            80 + wave
        } else {
            64
        };
        [0, 8, 16].into_iter().fold(0, |rgb, shift| {
            rgb | (((foreground >> shift & 255) * weight
                + (background >> shift & 255) * (255 - weight))
                / 255
                << shift)
        })
    }

    pub fn request_close_tab(&mut self, tab: &crate::workspace::TabView) -> bool {
        use crate::workspace::TabActivity;
        if matches!(tab.activity, TabActivity::Running | TabActivity::Unknown) {
            self.close_tab = Some(tab.id);
            self.close_confirm = false;
            false
        } else {
            true
        }
    }

    pub fn message(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now() + Duration::from_secs(4)));
    }
    #[cfg(any(feature = "monitor", feature = "gui"))]
    pub fn footer(&self, snapshot: &crate::workspace::Snapshot, remote: bool) -> String {
        if let Some((message, _)) = &self.status {
            return message.clone();
        }
        let location = if remote { "SSH" } else { "Local" };
        let focus = if !snapshot.controller {
            "Read-only"
        } else {
            match self.focus {
                Focus::Terminal => "Terminal",
                #[cfg(feature = "agent")]
                Focus::Editor => "Agent input",
                #[cfg(feature = "agent")]
                Focus::Output => "Agent output",
                #[cfg(feature = "agent")]
                Focus::Controls => "Agent controls",
                #[cfg(feature = "monitor")]
                Focus::Monitor => "Monitor",
                #[cfg(feature = "files")]
                Focus::Files => "Files",
            }
        };
        format!("{} · {location} · {focus}", snapshot.name)
    }

    pub fn expire_status(&mut self) -> bool {
        if self
            .status
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.status = None;
            true
        } else {
            false
        }
    }
    pub fn cycle_focus(&mut self, reverse: bool) {
        #[cfg(feature = "files")]
        if self.files_open {
            self.files_open = false;
            self.focus = Focus::Terminal;
            return;
        }
        let targets = [
            Some(Focus::Terminal),
            #[cfg(feature = "agent")]
            self.agent.open.then_some(Focus::Editor),
            #[cfg(feature = "agent")]
            self.agent.open.then_some(Focus::Output),
            #[cfg(feature = "monitor")]
            self.resources.open.then_some(Focus::Monitor),
        ];
        let count = targets.iter().flatten().count();
        let index = targets
            .iter()
            .flatten()
            .position(|target| *target == self.focus)
            .unwrap_or(0);
        self.focus = *targets
            .iter()
            .flatten()
            .nth((index + if reverse { count - 1 } else { 1 }) % count)
            .unwrap();
    }
    #[cfg(any(feature = "agent", feature = "monitor", feature = "files"))]
    fn has_other_focus(&self) -> bool {
        #[cfg(feature = "agent")]
        if self.agent.open {
            return true;
        }
        #[cfg(feature = "monitor")]
        if self.resources.open {
            return true;
        }
        #[cfg(feature = "files")]
        if self.files_open {
            return true;
        }
        false
    }
    pub fn help_lines(&self) -> Vec<&'static str> {
        let mut lines = Vec::new();
        lines.extend([
            "Alt+v / Alt+s  Split left-right / top-bottom",
            "Alt+h/j/k/l    Focus pane",
            "Alt+, / Alt+.  Previous / next tab",
            "Alt+Enter      New tab",
            "Alt+p / Alt+n  Previous / next command",
            "Alt+c          Copy command output",
            "Alt+/          Find in this pane",
            "Alt+q          Detach; keep workspace running",
            "Alt+?          Help; Esc closes this view",
        ]);
        #[cfg(any(feature = "agent", feature = "monitor", feature = "files"))]
        if self.has_other_focus() {
            lines.push("Alt+o          Cycle input focus");
        }
        #[cfg(feature = "agent")]
        {
            lines.push("Alt+a          Agent input");
            if matches!(self.focus, Focus::Editor | Focus::Output | Focus::Controls) {
                lines.extend([
                    "Tab/Shift+Tab  Agent input / output / controls",
                    "Enter / Space  Activate selected control",
                    "Ctrl+Space     Attach selection / visible screen",
                    "Enter          Send draft; Ctrl+J inserts newline",
                    "Ctrl+C         Copy selection; otherwise stop while busy",
                    "Esc            Select previous user message; repeat for older",
                    "Enter          Confirm selected rewind; click input to cancel",
                ]);
            }
        }
        #[cfg(feature = "monitor")]
        {
            lines.push("Alt+m          System monitor");
            if self.focus == Focus::Monitor {
                lines.extend([
                    "Tab/Shift+Tab  Monitor controls",
                    "Arrows / hjkl  Navigate; Enter activates",
                    "/              Filter processes",
                    "Esc            Leave filter, then close monitor",
                ]);
            }
        }
        #[cfg(feature = "files")]
        {
            lines.push("Alt+f          Files");
            if self.focus == Focus::Files {
                lines.extend([
                    "Tab            File list / preview",
                    "/              Filter filenames",
                    "Enter          Open directory / insert quoted path",
                    "Esc            Preview → list → close",
                ]);
            }
        }
        lines
    }
    #[cfg(feature = "agent")]
    pub fn agent_tab(&mut self, reverse: bool) {
        let actions = self.agent.controls();
        let current = match self.focus {
            Focus::Editor => AgentAction::Editor,
            Focus::Output => AgentAction::Output,
            _ => self.agent.keyboard,
        };
        let index = actions
            .iter()
            .position(|action| *action == current)
            .unwrap_or(0);
        self.agent.keyboard =
            actions[(index + if reverse { actions.len() - 1 } else { 1 }) % actions.len()];
        self.focus = match self.agent.keyboard {
            AgentAction::Editor => Focus::Editor,
            AgentAction::Output => Focus::Output,
            _ => Focus::Controls,
        };
        if let AgentAction::Tool(index) | AgentAction::Copy(index) | AgentAction::Rewind(index) =
            self.agent.keyboard
            && let Some(line) = self.agent.sessions[self.agent.current]
                .lines
                .iter()
                .rposition(|line| match line {
                    AgentLine::Role(i) | AgentLine::Text { message: i, .. } => *i as usize == index,
                    AgentLine::Blank => false,
                })
        {
            self.agent.sessions[self.agent.current].model.scroll = self.agent.sessions
                [self.agent.current]
                .lines
                .len()
                .saturating_sub(line + 1);
        }
        let hint = match self.agent.keyboard {
            AgentAction::Editor => "Agent input · Enter sends · Ctrl+J newline",
            AgentAction::Output => "Agent output · Ctrl+C copies selection / stops",
            AgentAction::Stop => "Stop generation · Enter",
            AgentAction::ContextMenu => "Attach terminal context · Enter",
            AgentAction::Selection => "Attach visible selection · Enter",
            AgentAction::Screen => "Attach visible screen · Enter",
            AgentAction::Inspect(_) => "Inspect attachment · Enter",
            AgentAction::Remove(_) => "Remove attachment · Enter or Delete",
            AgentAction::Tool(_) => "Expand/collapse tool · Enter",
            AgentAction::Copy(_) => "Copy message · Enter",
            AgentAction::CopyPreview => "Copy snapshot · Enter",
            AgentAction::ClosePreview => "Close snapshot · Enter or Esc",
            AgentAction::NewSession => "New conversation · Enter",
            AgentAction::Session(_) => "Switch conversation · Enter",
            AgentAction::CloseSession(_) => "Close conversation · Enter",
            AgentAction::ClosePanel => "Close Agent panel · Enter",
            AgentAction::Rewind(_) => "Selected rewind target · Esc older · Enter confirms",
        };
        self.message(hint);
    }
    #[cfg(feature = "agent")]
    pub fn agent_key(&mut self, symbol: u32, shift: bool, control: bool) -> Option<AgentAction> {
        if matches!(symbol, 0xff09 | 0xfe20) {
            self.agent_tab(shift || symbol == 0xfe20);
            return None;
        }
        if control && symbol == 32 {
            return Some(AgentAction::ContextMenu);
        }
        if self.focus != Focus::Controls {
            return None;
        }
        match symbol {
            0xff1b => {
                if self.agent.sessions[self.agent.current].context.menu {
                    self.agent.sessions[self.agent.current].context.menu = false;
                    self.agent.keyboard = AgentAction::ContextMenu;
                } else {
                    self.focus = Focus::Editor;
                }
                None
            }
            0xff52 | 0xff54 => {
                self.agent_tab(symbol == 0xff52);
                None
            }
            0xff0d | 0xff8d | 32 => Some(self.agent.keyboard),
            0xffff => match self.agent.keyboard {
                AgentAction::Inspect(index) | AgentAction::Remove(index) => {
                    Some(AgentAction::Remove(index))
                }
                _ => None,
            },
            _ => None,
        }
    }
    #[cfg(feature = "agent")]
    pub fn activate_agent(
        &mut self,
        action: AgentAction,
        client: &Client,
    ) -> io::Result<Option<String>> {
        match action {
            AgentAction::ClosePanel => {
                if self.agent.open {
                    self.toggle_agent();
                }
            }
            AgentAction::NewSession => {
                self.agent.new_session();
                self.focus = Focus::Editor;
            }
            AgentAction::Session(index) => {
                self.agent.switch(index);
                self.focus = Focus::Editor;
            }
            AgentAction::CloseSession(index) => {
                self.agent.close(index);
                self.focus = Focus::Editor;
            }
            AgentAction::Rewind(index) => self.select_agent_rewind(index),
            AgentAction::Editor => {
                self.agent.active_mut().rewind_target = None;
                self.focus = Focus::Editor;
            }
            AgentAction::Output => self.focus = Focus::Output,
            AgentAction::Stop => self.agent.sessions[self.agent.current].model.abort()?,
            AgentAction::ContextMenu => {
                self.agent.sessions[self.agent.current].context.menu =
                    !self.agent.sessions[self.agent.current].context.menu;
                self.focus = Focus::Controls;
                self.agent.keyboard = if self.agent.sessions[self.agent.current].context.menu {
                    AgentAction::Selection
                } else {
                    AgentAction::ContextMenu
                };
            }
            AgentAction::Selection | AgentAction::Screen => {
                if let Some(pane) = client.snapshot().map(|s| s.focused) {
                    self.agent
                        .active_mut()
                        .attach_screen(client, pane, action == AgentAction::Selection)
                        .map_err(io::Error::other)?;
                }
                self.agent.sessions[self.agent.current].context.menu = false;
                self.focus = Focus::Editor;
            }
            AgentAction::Inspect(index) => {
                if let Some(item) = self.agent.sessions[self.agent.current]
                    .context
                    .basket
                    .get(index)
                {
                    self.agent.sessions[self.agent.current].context.preview =
                        Some(ContextPreview::new(item.clone(), 0));
                    self.agent.keyboard = AgentAction::CopyPreview;
                    self.focus = Focus::Controls;
                }
            }
            AgentAction::Remove(index) => {
                if index < self.agent.sessions[self.agent.current].context.basket.len() {
                    self.agent.sessions[self.agent.current]
                        .context
                        .basket
                        .remove(index);
                }
                self.agent.keyboard = AgentAction::ContextMenu;
            }
            AgentAction::Tool(index) => self.agent.active_mut().toggle_tool(index),
            AgentAction::Copy(index) => {
                return Ok(self.agent.sessions[self.agent.current]
                    .model
                    .messages
                    .get(index)
                    .map(|message| message.text.clone()));
            }
            AgentAction::CopyPreview => {
                return Ok(self.agent.sessions[self.agent.current]
                    .context
                    .preview
                    .as_ref()
                    .map(|preview| preview.item.snapshot.text.clone()));
            }
            AgentAction::ClosePreview => {
                self.agent.sessions[self.agent.current].context.preview = None;
                self.focus = Focus::Editor;
            }
        }
        Ok(None)
    }
    #[cfg(feature = "agent")]
    fn select_agent_rewind(&mut self, index: usize) {
        let session = self.agent.active_mut();
        let Some(message) = session
            .model
            .messages
            .get(index)
            .filter(|message| message.role == "You")
        else {
            return;
        };
        session.rewind_target = Some(message.turn);
        session.clear_selection();
        if let Some(line) = session.lines.iter().position(
            |line| matches!(line, AgentLine::Text { message, .. } if *message as usize == index),
        ) {
            session.model.scroll = session.lines.len().saturating_sub(line + 1);
        }
        self.agent.keyboard = AgentAction::Rewind(index);
        self.focus = Focus::Controls;
    }

    #[cfg(feature = "agent")]
    fn previous_agent_rewind(&mut self) {
        let session = self.agent.active();
        let before = session
            .rewind_index()
            .unwrap_or(session.model.messages.len());
        if let Some(index) = session.model.messages[..before]
            .iter()
            .rposition(|message| message.role == "You")
        {
            self.select_agent_rewind(index);
        }
    }

    #[cfg(feature = "agent")]
    pub fn poll_agent(&mut self, client: &mut Client, config: &Config) -> io::Result<(bool, bool)> {
        self.agent.poll_remote(client, config, &mut self.focus)
    }

    #[cfg(feature = "agent")]
    fn confirm_agent_rewind(&mut self) {
        let index = self.agent.active().rewind_index();
        self.agent.active_mut().rewind_target = None;
        self.agent.keyboard = AgentAction::Editor;
        self.focus = Focus::Editor;
        if let Some(index) = index
            && let Err(error) = self.agent.active_mut().model.rewind(index)
        {
            self.message(error.to_string());
        }
    }

    #[cfg(feature = "agent")]
    pub fn agent_rewind_key(&mut self, symbol: u32) -> bool {
        if !self.focus.is_agent() {
            self.agent.active_mut().rewind_target = None;
            return false;
        }
        if !self.agent.open
            || self.agent.active().context.menu
            || self.agent.active().context.preview.is_some()
        {
            return false;
        }
        if symbol == 0xff1b
            && (self.focus == Focus::Editor || self.agent.active().rewind_target.is_some())
        {
            self.previous_agent_rewind();
            return true;
        }
        if self.agent.active().rewind_target.is_some() {
            if matches!(symbol, 0xff0d | 0xff8d) {
                self.confirm_agent_rewind();
                return true;
            }
            self.agent.active_mut().rewind_target = None;
            self.agent.keyboard = AgentAction::Editor;
            self.focus = Focus::Editor;
        }
        false
    }

    #[cfg(feature = "monitor")]
    pub fn toggle_monitor(&mut self) {
        self.resources.toggle();
        self.focus = if self.resources.open {
            Focus::Monitor
        } else {
            Focus::Terminal
        };
    }

    #[cfg(feature = "agent")]
    pub fn toggle_agent(&mut self) {
        self.agent.active_mut().rewind_target = None;
        self.agent.open = !self.agent.open;
        self.agent.sessions[self.agent.current].context.menu = false;
        self.agent.sessions[self.agent.current].context.preview = None;
        self.focus = if self.agent.open {
            Focus::Editor
        } else {
            Focus::Terminal
        };
        #[cfg(feature = "files")]
        if self.agent.open {
            self.files_open = false;
        }
    }
}

#[cfg(feature = "agent")]
#[derive(Clone)]
pub(crate) struct ContextItem {
    pub label: String,
    pub source: String,
    pub kind: String,
    pub snapshot: ContextSnapshot,
}

#[cfg(feature = "agent")]
type ContextView = (String, String, String, String, bool);

#[cfg(feature = "agent")]
impl ContextItem {
    fn view(&self) -> (&str, &str, &str, &str, bool) {
        (
            &self.label,
            &self.source,
            &self.kind,
            &self.snapshot.text,
            self.snapshot.incomplete,
        )
    }

    fn from_view((label, source, kind, text, incomplete): ContextView) -> Self {
        Self {
            label,
            source,
            kind,
            snapshot: ContextSnapshot { text, incomplete },
        }
    }
}

#[cfg(feature = "agent")]
pub(crate) struct ContextPreview {
    pub item: ContextItem,
    pub scroll: usize,
    columns: usize,
    pub lines: Vec<Range<usize>>,
}

#[cfg(feature = "agent")]
impl ContextPreview {
    pub fn new(item: ContextItem, scroll: usize) -> Self {
        Self {
            item,
            scroll,
            columns: 0,
            lines: Vec::new(),
        }
    }

    pub fn prepare(&mut self, columns: usize) {
        let columns = columns.max(1);
        if self.columns != columns {
            self.lines = wrap_context(&self.item.snapshot.text, columns);
            self.columns = columns;
        }
    }
}

#[cfg(feature = "agent")]
#[derive(Default)]
pub(crate) struct ChatContext {
    pub basket: Vec<ContextItem>,
    pub menu: bool,
    pub preview: Option<ContextPreview>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Terminal,
    #[cfg(feature = "agent")]
    Editor,
    #[cfg(feature = "agent")]
    Output,
    #[cfg(feature = "agent")]
    Controls,
    #[cfg(feature = "monitor")]
    Monitor,
    #[cfg(feature = "files")]
    Files,
}
#[cfg(feature = "agent")]
impl Focus {
    fn view(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Editor => "editor",
            Self::Output => "output",
            Self::Controls => "controls",
            #[cfg(feature = "files")]
            Self::Files => "files",
            #[cfg(feature = "monitor")]
            Self::Monitor => "monitor",
        }
    }

    fn from_view(value: &str) -> Option<Self> {
        Some(match value {
            "terminal" => Self::Terminal,
            "editor" => Self::Editor,
            "output" => Self::Output,
            "controls" => Self::Controls,
            #[cfg(feature = "files")]
            "files" => Self::Files,
            #[cfg(feature = "monitor")]
            "monitor" => Self::Monitor,
            _ => return None,
        })
    }

    pub fn is_agent(self) -> bool {
        matches!(self, Self::Editor | Self::Output | Self::Controls)
    }
}
#[cfg(feature = "agent")]
pub(crate) const REWIND_LABEL: &str = " ← ";

#[cfg(feature = "agent")]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentAction {
    #[default]
    Editor,
    Output,
    Stop,
    Tool(usize),
    Copy(usize),
    ContextMenu,
    Selection,
    Screen,
    Inspect(usize),
    Remove(usize),
    ClosePreview,
    CopyPreview,
    NewSession,
    Session(usize),
    CloseSession(usize),
    ClosePanel,
    Rewind(usize),
}

#[cfg(feature = "agent")]
impl AgentAction {
    fn view(self) -> (u8, Option<usize>) {
        match self {
            Self::Editor => (0, None),
            Self::Output => (1, None),
            Self::Stop => (2, None),
            Self::Tool(index) => (3, Some(index)),
            Self::Copy(index) => (4, Some(index)),
            Self::ContextMenu => (5, None),
            Self::Selection => (6, None),
            Self::Screen => (7, None),
            Self::Inspect(index) => (8, Some(index)),
            Self::Remove(index) => (9, Some(index)),
            Self::ClosePreview => (10, None),
            Self::CopyPreview => (11, None),
            Self::NewSession => (12, None),
            Self::Session(index) => (13, Some(index)),
            Self::CloseSession(index) => (14, Some(index)),
            Self::ClosePanel => (15, None),
            Self::Rewind(index) => (16, Some(index)),
        }
    }

    fn from_view(value: (u8, Option<usize>)) -> io::Result<Self> {
        Ok(match value {
            (0, None) => Self::Editor,
            (1, None) => Self::Output,
            (2, None) => Self::Stop,
            (3, Some(index)) => Self::Tool(index),
            (4, Some(index)) => Self::Copy(index),
            (5, None) => Self::ContextMenu,
            (6, None) => Self::Selection,
            (7, None) => Self::Screen,
            (8, Some(index)) => Self::Inspect(index),
            (9, Some(index)) => Self::Remove(index),
            (10, None) => Self::ClosePreview,
            (11, None) => Self::CopyPreview,
            (12, None) => Self::NewSession,
            (13, Some(index)) => Self::Session(index),
            (14, Some(index)) => Self::CloseSession(index),
            (15, None) => Self::ClosePanel,
            (16, Some(index)) => Self::Rewind(index),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unknown Agent control",
                ));
            }
        })
    }
}

// Agent bounds retained text at 256 KiB, so cached offsets fit in u32.
#[cfg(feature = "agent")]
pub(crate) enum AgentLine {
    Role(u32),
    Text { message: u32, range: Range<u32> },
    Blank,
}

#[cfg(feature = "agent")]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptPoint {
    message: usize,
    part: u8,
    offset: usize,
}

#[cfg(feature = "agent")]
struct ChatSelection {
    anchor: TranscriptPoint,
    head: TranscriptPoint,
    dragging: bool,
}

#[cfg(feature = "agent")]
struct RenderedMarkdown {
    turn: u64,
    source: String,
    document: crate::markdown::Markdown,
}

#[cfg(feature = "agent")]
struct MarkdownJob {
    session: u64,
    request: u64,
    messages: Vec<(usize, u64, String)>,
}

#[cfg(feature = "agent")]
struct MarkdownResult {
    session: u64,
    request: u64,
    messages: Vec<(usize, RenderedMarkdown)>,
}

#[cfg(feature = "agent")]
#[derive(Default)]
struct MarkdownWork {
    pending: Option<MarkdownJob>,
    result: Option<MarkdownResult>,
    closed: bool,
}

#[cfg(feature = "agent")]
struct MarkdownWorker {
    shared: Arc<(Mutex<MarkdownWork>, Condvar)>,
    notification: Arc<OwnedFd>,
}

#[cfg(feature = "agent")]
impl MarkdownWorker {
    fn new() -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(MarkdownWork::default()), Condvar::new()));
        let notification = Arc::new(crate::eventfd::new()?);
        let worker = Arc::clone(&shared);
        let wake = Arc::clone(&notification);
        std::thread::Builder::new()
            .name("nextty-markdown".into())
            .spawn(move || {
                loop {
                    let (lock, changed) = &*worker;
                    let mut state = lock.lock().unwrap();
                    while state.pending.is_none() && !state.closed {
                        state = changed.wait(state).unwrap();
                    }
                    if state.closed {
                        return;
                    }
                    let job = state.pending.take().unwrap();
                    drop(state);
                    let messages = job
                        .messages
                        .into_iter()
                        .map(|(index, turn, source)| {
                            let document = crate::markdown::Markdown::render(&source);
                            (
                                index,
                                RenderedMarkdown {
                                    turn,
                                    source,
                                    document,
                                },
                            )
                        })
                        .collect();
                    let previous = lock.lock().unwrap().result.replace(MarkdownResult {
                        session: job.session,
                        request: job.request,
                        messages,
                    });
                    drop(previous);
                    crate::eventfd::notify(wake.as_raw_fd()).unwrap();
                }
            })?;
        Ok(Self {
            shared,
            notification,
        })
    }

    fn submit(&self, job: MarkdownJob) {
        let (lock, changed) = &*self.shared;
        let previous = lock.lock().unwrap().pending.replace(job);
        if previous.is_none() {
            changed.notify_one();
        }
        drop(previous);
    }

    fn take(&self) -> Option<MarkdownResult> {
        self.shared.0.lock().unwrap().result.take()
    }
}

#[cfg(feature = "agent")]
impl Drop for MarkdownWorker {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        state.pending = None;
        changed.notify_one();
    }
}

#[cfg(feature = "agent")]
pub(crate) struct AgentPanel {
    pub open: bool,
    pub width: Option<f32>,
    pub sessions: Vec<AgentSession>,
    pub current: usize,
    pub keyboard: AgentAction,
    next_id: u64,
    remote_opened: bool,
    remote_ready: bool,
    pub view_dirty: bool,
    last_view: Option<serde_json::Value>,
    markdown_worker: Option<MarkdownWorker>,
    next_render: u64,
}

#[cfg(feature = "agent")]
impl Default for AgentPanel {
    fn default() -> Self {
        Self {
            open: false,
            width: None,
            sessions: vec![AgentSession {
                model: Agent::mirror(),
                ..AgentSession::default()
            }],
            current: 0,
            keyboard: AgentAction::Editor,
            next_id: 1,
            remote_opened: false,
            remote_ready: false,
            view_dirty: true,
            last_view: None,
            markdown_worker: None,
            next_render: 0,
        }
    }
}

#[cfg(feature = "agent")]
impl AgentPanel {
    #[cfg(test)]
    fn set_local(&mut self) {
        for session in &mut self.sessions {
            session.model.remote = false;
        }
    }

    #[cfg(test)]
    fn model_snapshot(&self) -> Vec<(u64, serde_json::Value)> {
        self.sessions
            .iter()
            .map(|session| (session.id, session.model.snapshot()))
            .collect()
    }

    pub fn restore_models(&mut self, values: Vec<(u64, serde_json::Value)>) -> io::Result<bool> {
        let mut redraw = false;
        for (id, mut value) in values {
            if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                let selection = session.selection_checkpoint();
                let rewind: Option<(u64, String, Vec<ContextView>)> = serde_json::from_value(
                    value
                        .get_mut("rewind_restore")
                        .map_or(serde_json::Value::Null, serde_json::Value::take),
                )
                .map_err(io::Error::other)?;
                let model_redraw = session.model.restore(value)?;
                redraw |= model_redraw;
                if model_redraw {
                    session.columns = 0;
                    session.sync_rendered();
                }
                if session.model.remote
                    && let Some((turn, input, items)) = rewind
                    && session.rewind_applied != Some(turn)
                {
                    session.model.input = input;
                    session.model.cursor = session.model.input.len();
                    session.model.anchor = None;
                    session.model.scroll = 0;
                    session.context.basket =
                        items.into_iter().map(ContextItem::from_view).collect();
                    session.context.preview = None;
                    session.context.menu = false;
                    session.editor_scroll = None;
                    session.rewind_target = None;
                    session.rewind_applied = Some(turn);
                    self.view_dirty = true;
                    redraw = true;
                }
                if selection.is_some() && !session.selection_matches(selection.as_ref()) {
                    session.clear_selection();
                    self.view_dirty = true;
                    redraw = true;
                }
            }
        }
        self.request_markdown()?;
        Ok(redraw)
    }

    pub fn poll_remote(
        &mut self,
        client: &mut Client,
        config: &Config,
        focus: &mut Focus,
    ) -> io::Result<(bool, bool)> {
        if client.snapshot().is_none() {
            return Ok((false, false));
        }
        if !self.remote_opened {
            client.send(crate::workspace::Action::AgentOpen)?;
            self.remote_opened = true;
        }
        let mut changed = false;
        let mut messages = false;
        for (kind, payload) in client.take_agent_events() {
            match kind {
                0 => {
                    if let Some(restored) = self.restore_view_state(
                        serde_json::from_slice(&payload).map_err(io::Error::other)?,
                    )? {
                        *focus = restored;
                    }
                    self.last_view = Some(self.view_state(*focus));
                    self.view_dirty = false;
                    changed = true;
                }
                1 => {
                    let redraw = self.restore_models(
                        serde_json::from_slice(&payload).map_err(io::Error::other)?,
                    )?;
                    changed |= redraw;
                    messages |= redraw;
                }
                3 => crate::agent_owner::spawn(client, &payload, config)?,
                5 => self.remote_ready = true,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected Agent frontend event",
                    ));
                }
            }
        }
        if self.remote_ready && client.can_agent_input() {
            if self.view_dirty {
                let view = self.view_state(*focus);
                if self.last_view.as_ref() != Some(&view) {
                    client.send(crate::workspace::Action::AgentView(
                        serde_json::to_vec(&view).map_err(io::Error::other)?,
                    ))?;
                    self.last_view = Some(view);
                }
                self.view_dirty = false;
            }
            for session in &mut self.sessions {
                for mut request in std::mem::take(&mut session.model.pending) {
                    request["session"] = serde_json::json!(session.id);
                    client.send(crate::workspace::Action::AgentRequest(
                        serde_json::to_vec(&request).map_err(io::Error::other)?,
                    ))?;
                }
            }
        } else if self.remote_ready {
            for session in &mut self.sessions {
                session.model.pending.clear();
            }
        }
        Ok((changed, messages))
    }

    pub fn view_state(&self, focus: Focus) -> serde_json::Value {
        use serde_json::json;
        let sessions: Vec<_> = self.sessions.iter().map(|session| {
            let point = |point: TranscriptPoint| (point.message, point.part, point.offset);
            let mut expanded_tools: Vec<_> = session.expanded_tools.iter().collect();
            expanded_tools.sort_unstable();
            json!({
                "id": session.id,
                "input": session.model.input,
                "cursor": session.model.cursor,
                "anchor": session.model.anchor,
                "scroll": session.model.scroll,
                "basket": session.context.basket.iter().map(ContextItem::view).collect::<Vec<_>>(),
                "menu": session.context.menu,
                "preview": session.context.preview.as_ref().map(|preview| (preview.item.view(), preview.scroll)),
                "expanded_tools": expanded_tools,
                "editor_scroll": session.editor_scroll,
                "rewind_target": session.rewind_target,
                "rewind_applied": session.rewind_applied,
                "selection": session.selection.as_ref().map(|selection| (point(selection.anchor), point(selection.head))),
            })
        }).collect();
        let mut view = json!({
            "open": self.open,
            "focus": focus.view(),
            "width": self.width,
            "current": self.current,
            "keyboard": self.keyboard.view(),
            "next_id": self.next_id,
        });
        view["sessions"] = serde_json::Value::Array(sessions);
        view
    }

    pub fn restore_view_state(
        &mut self,
        mut value: serde_json::Value,
    ) -> io::Result<Option<Focus>> {
        macro_rules! read {
            ($value:expr) => {
                serde_json::from_value($value.take()).map_err(io::Error::other)?
            };
        }
        let open = read!(value["open"]);
        let focus: Option<String> = read!(value["focus"]);
        let width = read!(value["width"]);
        let current: usize = read!(value["current"]);
        let keyboard = AgentAction::from_view(read!(value["keyboard"]))?;
        let next_id: u64 = read!(value["next_id"]);
        let values: Vec<serde_json::Value> = read!(value["sessions"]);
        let mut sessions = Vec::with_capacity(values.len());
        let mut ids = HashSet::new();
        for mut value in values {
            let mut session = AgentSession {
                id: read!(value["id"]),
                model: Agent::mirror(),
                ..AgentSession::default()
            };
            session.model.input = read!(value["input"]);
            session.model.cursor = read!(value["cursor"]);
            session.model.anchor = read!(value["anchor"]);
            session.model.scroll = read!(value["scroll"]);
            if !ids.insert(session.id)
                || session.id >= next_id
                || !session.model.input.is_char_boundary(session.model.cursor)
                || session
                    .model
                    .anchor
                    .is_some_and(|anchor| !session.model.input.is_char_boundary(anchor))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid Agent view identity or cursor",
                ));
            }
            let basket: Vec<ContextView> = read!(value["basket"]);
            session.context.basket = basket.into_iter().map(ContextItem::from_view).collect();
            session.context.menu = read!(value["menu"]);
            let preview: Option<(ContextView, usize)> = read!(value["preview"]);
            session.context.preview = preview
                .map(|(item, scroll)| ContextPreview::new(ContextItem::from_view(item), scroll));
            session.expanded_tools = read!(value["expanded_tools"]);
            session.editor_scroll = read!(value["editor_scroll"]);
            session.rewind_target = read!(value["rewind_target"]);
            session.rewind_applied = read!(value["rewind_applied"]);
            type PointView = (usize, u8, usize);
            let selection: Option<(PointView, PointView)> = read!(value["selection"]);
            session.selection = selection.map(|(anchor, head)| {
                let point = |(message, part, offset)| TranscriptPoint {
                    message,
                    part,
                    offset,
                };
                ChatSelection {
                    anchor: point(anchor),
                    head: point(head),
                    dragging: false,
                }
            });
            sessions.push(session);
        }
        if current >= sessions.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Agent view has no current conversation",
            ));
        }
        let mut previous = std::mem::take(&mut self.sessions);
        for session in &mut sessions {
            if let Some(index) = previous.iter().position(|old| old.id == session.id) {
                let old = previous.swap_remove(index);
                let mut view = std::mem::replace(&mut session.model, old.model);
                session.model.input = std::mem::take(&mut view.input);
                session.model.cursor = view.cursor;
                session.model.anchor = view.anchor;
                session.model.scroll = view.scroll;
            }
        }
        self.open = open;
        self.width = width;
        self.current = current;
        self.keyboard = keyboard;
        self.next_id = next_id;
        self.sessions = sessions;
        Ok(focus.as_deref().and_then(Focus::from_view))
    }

    pub fn active(&self) -> &AgentSession {
        &self.sessions[self.current]
    }
    pub fn active_mut(&mut self) -> &mut AgentSession {
        &mut self.sessions[self.current]
    }

    pub fn prepare_layout(&mut self, columns: usize) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        // Commit with wrapping during draw so input keeps the displayed frame's geometry.
        self.apply_markdown();
        self.request_markdown()?;
        let previous = (
            self.active().model.scroll,
            self.active().expanded_tools.len(),
        );
        self.active_mut().layout(columns);
        self.view_dirty |= previous
            != (
                self.active().model.scroll,
                self.active().expanded_tools.len(),
            );
        Ok(())
    }

    fn request_markdown(&mut self) -> io::Result<()> {
        if !self.open || self.active().render_request.is_some() {
            return Ok(());
        }
        let session = &mut self.sessions[self.current];
        let selection = session.selection_checkpoint();
        session.sync_rendered();
        if selection.is_some() && !session.selection_matches(selection.as_ref()) {
            session.clear_selection();
            self.view_dirty = true;
        }
        // sync_rendered kept only matching prefixes; equal lengths imply equal text.
        let messages: Vec<_> = session
            .model
            .messages
            .iter()
            .enumerate()
            .filter(|&(index, message)| {
                message.role == "Agent"
                    && message.id.is_none()
                    && session.rendered[index]
                        .as_ref()
                        .is_none_or(|rendered| rendered.source.len() != message.text.len())
            })
            .map(|(index, message)| (index, message.turn, message.text.clone()))
            .collect();
        if !messages.is_empty() {
            if self.markdown_worker.is_none() {
                self.markdown_worker = Some(MarkdownWorker::new()?);
            }
            for session in &mut self.sessions {
                session.render_request = None;
            }
            self.next_render += 1;
            let job = MarkdownJob {
                session: self.active().id,
                request: self.next_render,
                messages,
            };
            self.active_mut().render_request = Some(job.request);
            self.markdown_worker.as_ref().unwrap().submit(job);
        }
        Ok(())
    }

    pub fn layout_fd(&self) -> RawFd {
        self.markdown_worker
            .as_ref()
            .map_or(-1, |worker| worker.notification.as_raw_fd())
    }

    pub fn poll_layout(&mut self) -> io::Result<bool> {
        let Some(worker) = &self.markdown_worker else {
            return Ok(false);
        };
        Ok(
            crate::eventfd::consume_notification(worker.notification.as_raw_fd())?
                && self.open
                && worker.shared.0.lock().unwrap().result.is_some(),
        )
    }

    fn apply_markdown(&mut self) {
        let Some(result) = self.markdown_worker.as_ref().and_then(MarkdownWorker::take) else {
            return;
        };
        let Some(session) = self.sessions.iter_mut().find(|session| {
            session.id == result.session && session.render_request == Some(result.request)
        }) else {
            return;
        };
        session.render_request = None;
        // New deltas can arrive between parsing and drawing the next frame.
        if !result.messages.iter().all(|(index, rendered)| {
            session.model.messages.get(*index).is_some_and(|message| {
                message.role == "Agent"
                    && message.id.is_none()
                    && message.turn == rendered.turn
                    && message.text.starts_with(&rendered.source)
            })
        }) {
            return;
        }
        let selection = session.selection_checkpoint();
        for (index, rendered) in result.messages {
            session.rendered[index] = Some(rendered);
        }
        if session.selection.is_some() && !session.selection_matches(selection.as_ref()) {
            session.clear_selection();
            self.view_dirty = true;
        }
        session.columns = 0;
    }

    #[cfg(test)]
    pub(crate) fn wait_layout(&mut self, columns: usize) {
        self.prepare_layout(columns).unwrap();
        while self.active().render_request.is_some() {
            let mut fd = libc::pollfd {
                fd: self.layout_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
            self.poll_layout().unwrap();
            self.prepare_layout(columns).unwrap();
        }
    }

    pub fn new_session(&mut self) {
        self.active_mut().end_selection();
        self.active_mut().rewind_target = None;
        self.sessions.push(AgentSession {
            id: self.next_id,
            model: Agent::mirror(),
            ..AgentSession::default()
        });
        self.next_id += 1;
        self.current = self.sessions.len() - 1;
        self.keyboard = AgentAction::Editor;
    }

    pub fn switch(&mut self, index: usize) {
        if index < self.sessions.len() {
            self.active_mut().end_selection();
            self.active_mut().rewind_target = None;
            self.current = index;
            self.keyboard = AgentAction::Editor;
        }
    }

    pub fn close(&mut self, index: usize) {
        if index >= self.sessions.len() {
            return;
        }
        self.active_mut().end_selection();
        self.active_mut().rewind_target = None;
        self.sessions.remove(index);
        if self.sessions.is_empty() {
            self.sessions.push(AgentSession {
                id: self.next_id,
                model: Agent::mirror(),
                ..AgentSession::default()
            });
            self.next_id += 1;
        }
        self.current = if index < self.current {
            self.current - 1
        } else {
            self.current.min(self.sessions.len() - 1)
        };
        self.keyboard = AgentAction::Editor;
    }

    pub fn paste(&mut self, id: u64, text: &str) -> Result<(), String> {
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or_else(|| "Conversation was closed; paste discarded".to_string())?;
        session.model.insert(text)?;
        session.editor_scroll = None;
        Ok(())
    }

    pub fn controls(&self) -> Vec<AgentAction> {
        let session = self.active();
        if session.context.preview.is_some() {
            return vec![AgentAction::CopyPreview, AgentAction::ClosePreview];
        }
        if session.context.menu {
            return vec![
                AgentAction::Selection,
                AgentAction::Screen,
                AgentAction::ContextMenu,
            ];
        }
        let capacity = (6
            + self.sessions.len()
            + usize::from(session.model.busy)
            + session.context.basket.len() * 2
            + session.model.messages.len())
        .next_power_of_two();
        let mut actions = Vec::with_capacity(capacity);
        actions.extend([AgentAction::Editor, AgentAction::Output]);
        if session.model.busy {
            actions.push(AgentAction::Stop);
        }
        actions.push(AgentAction::ContextMenu);
        for index in 0..session.context.basket.len() {
            actions.extend([AgentAction::Inspect(index), AgentAction::Remove(index)]);
        }
        for (index, message) in session.model.messages.iter().enumerate().rev() {
            if message.role == "You" {
                actions.push(AgentAction::Rewind(index));
            }
            actions.push(if message.id.is_some() {
                AgentAction::Tool(index)
            } else {
                AgentAction::Copy(index)
            });
        }
        actions.push(AgentAction::NewSession);
        actions.extend((0..self.sessions.len()).map(AgentAction::Session));
        actions.extend([
            AgentAction::CloseSession(self.current),
            AgentAction::ClosePanel,
        ]);
        actions
    }

    pub fn tabs(&self, columns: usize) -> Vec<(usize, usize, usize)> {
        let available = columns.saturating_sub(6);
        if available == 0 {
            return Vec::new();
        }
        let count = self.sessions.len().min((available / 8).max(1));
        let start = self
            .current
            .saturating_sub(count - 1)
            .min(self.sessions.len() - count);
        // Reuse the column slot for desired widths while fitting the tabs.
        let mut tabs: Vec<_> = (start..start + count)
            .map(|index| {
                (
                    index,
                    (crate::width::text_width(self.sessions[index].title()) + 4).max(8),
                    0,
                )
            })
            .collect();
        let mut remaining = available;
        loop {
            let pending = tabs.iter().filter(|(_, _, width)| *width == 0).count();
            if pending == 0 {
                break;
            }
            let share = remaining / pending;
            let mut fitted = false;
            for (_, desired, width) in &mut tabs {
                if *width == 0 && *desired <= share {
                    *width = *desired;
                    remaining -= *desired;
                    fitted = true;
                }
            }
            if !fitted {
                let mut extra = remaining % pending;
                for (_, _, width) in &mut tabs {
                    if *width == 0 {
                        *width = share + usize::from(extra > 0);
                        extra = extra.saturating_sub(1);
                    }
                }
                break;
            }
        }
        let mut x = 0;
        for (_, column, width) in &mut tabs {
            *column = x;
            x += *width;
        }
        tabs
    }
}

#[cfg(feature = "agent")]
pub(crate) fn wrap_context(text: &str, columns: usize) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    wrap_context_into(text, columns, |line| result.push(line));
    result
}

#[cfg(feature = "agent")]
fn wrap_context_into(text: &str, columns: usize, mut push: impl FnMut(Range<usize>)) {
    let mut offset = 0;
    let columns = columns.max(1);
    let mut ends = memchr::memchr_iter(b'\n', text.as_bytes());
    while offset < text.len() {
        let end = ends.next().map_or(text.len(), |index| index + 1);
        let part = &text[offset..end];
        let line = part
            .strip_suffix("\r\n")
            .or_else(|| part.strip_suffix('\n'))
            .unwrap_or(part);
        let mut start = offset;
        if line.len() <= columns || line.is_ascii() {
            while offset + line.len() - start > columns {
                push(start..start + columns);
                start += columns;
            }
        } else {
            let mut width = 0;
            for (index, c) in line.char_indices() {
                let index = offset + index;
                let c = if c.is_control() { ' ' } else { c };
                let next = usize::from(scalar_width(c).min(2));
                if width + next > columns && index > start {
                    push(start..index);
                    start = index;
                    width = 0;
                }
                width += next;
            }
        }
        push(start..offset + line.len());
        offset += part.len();
    }
    if text.ends_with('\n') {
        push(text.len()..text.len());
    }
}

#[cfg(feature = "agent")]
pub(crate) fn column_offset(text: &str, range: Range<usize>, column: f32) -> usize {
    let mut used = 0.;
    for (index, c) in text[range.clone()].char_indices() {
        let width = f32::from(scalar_width(if c.is_control() { ' ' } else { c }).min(2));
        if column < used + width * 0.5 {
            return range.start + index;
        }
        used += width;
    }
    range.end
}

#[cfg(feature = "agent")]
pub(crate) struct EditorLines {
    pub lines: Vec<Range<usize>>,
    pub first: usize,
    pub caret_row: usize,
    pub caret_column: usize,
}

#[cfg(feature = "agent")]
#[derive(Default)]
pub(crate) struct AgentSession {
    pub id: u64,
    pub model: Agent,
    pub context: ChatContext,
    pub expanded_tools: HashSet<String>,
    pub lines: Vec<AgentLine>,
    pub columns: usize,
    rendered: Vec<Option<RenderedMarkdown>>,
    render_request: Option<u64>,
    pub editor_scroll: Option<usize>,
    pub rewind_target: Option<u64>,
    selection: Option<ChatSelection>,
    rewind_applied: Option<u64>,
}

#[cfg(feature = "agent")]
impl AgentSession {
    pub fn rewind_index(&self) -> Option<usize> {
        let turn = self.rewind_target?;
        self.model
            .messages
            .iter()
            .position(|message| message.turn == turn && message.role == "You")
    }

    pub fn title(&self) -> &str {
        self.model
            .messages
            .iter()
            .find(|message| message.role == "You")
            .and_then(|message| message.text.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("New chat")
    }

    pub fn message_text(&self, index: usize) -> &str {
        let message = &self.model.messages[index];
        if message.role == "Agent" && message.id.is_none() {
            self.markdown(index).map_or("", |document| &document.text)
        } else {
            &message.text
        }
    }

    pub fn markdown(&self, index: usize) -> Option<&crate::markdown::Markdown> {
        self.rendered
            .get(index)
            .and_then(Option::as_ref)
            .map(|rendered| &rendered.document)
    }

    fn sync_rendered(&mut self) {
        self.rendered
            .resize_with(self.model.messages.len(), || None);
        for (message, rendered) in self.model.messages.iter().zip(&mut self.rendered) {
            if rendered.as_ref().is_some_and(|old| {
                message.role != "Agent"
                    || message.id.is_some()
                    || message.turn != old.turn
                    || !message.text.starts_with(&old.source)
            }) {
                *rendered = None;
                self.columns = 0;
            }
        }
    }

    fn line_text(&self, index: usize) -> Option<(TranscriptPoint, Range<usize>, &str)> {
        match self.lines.get(index)? {
            AgentLine::Role(message) => {
                let text = &self.model.messages.get(*message as usize)?.role;
                Some((
                    TranscriptPoint {
                        message: *message as usize,
                        part: 0,
                        offset: 0,
                    },
                    0..text.len(),
                    text,
                ))
            }
            AgentLine::Text { message, range } => {
                self.model.messages.get(*message as usize)?;
                let text = self.message_text(*message as usize);
                let range = range.start as usize..range.end as usize;
                text.get(range.clone())?;
                Some((
                    TranscriptPoint {
                        message: *message as usize,
                        part: 1,
                        offset: 0,
                    },
                    range,
                    text,
                ))
            }
            AgentLine::Blank => None,
        }
    }

    pub fn point(&self, line: usize, column: usize) -> Option<TranscriptPoint> {
        let (mut point, range, text) = self.line_text(line)?;
        point.offset = column_offset(text, range, column as f32);
        Some(point)
    }

    pub fn begin_selection(&mut self, point: TranscriptPoint, extend: bool) {
        self.rewind_target = None;
        let anchor = if extend {
            self.selection
                .as_ref()
                .map_or(point, |selection| selection.anchor)
        } else {
            point
        };
        self.selection = Some(ChatSelection {
            anchor,
            head: point,
            dragging: true,
        });
    }

    pub fn extend_selection(&mut self, point: TranscriptPoint) {
        if let Some(selection) = &mut self.selection {
            selection.head = point;
        }
    }

    pub fn end_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.dragging = false;
        }
    }

    pub fn selecting(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
    }
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn selection_checkpoint(&self) -> Option<[(u64, Option<String>, String); 2]> {
        self.selection_prefixes().map(|prefixes| {
            prefixes.map(|(turn, id, text)| (turn, id.map(str::to_owned), text.to_owned()))
        })
    }

    fn selection_matches(&self, previous: Option<&[(u64, Option<String>, String); 2]>) -> bool {
        self.selection_prefixes().is_some_and(|current| {
            previous.is_none_or(|previous| {
                current
                    == previous
                        .each_ref()
                        .map(|(turn, id, text)| (*turn, id.as_deref(), text.as_str()))
            })
        })
    }

    fn selection_prefixes(&self) -> Option<[(u64, Option<&str>, &str); 2]> {
        let selection = self.selection.as_ref()?;
        let checkpoint = |point: TranscriptPoint| {
            let message = self.model.messages.get(point.message)?;
            let text = if point.part == 0 {
                &message.role
            } else {
                self.message_text(point.message)
            };
            Some((
                message.turn,
                message.id.as_deref(),
                text.get(..point.offset)?,
            ))
        };
        Some([checkpoint(selection.anchor)?, checkpoint(selection.head)?])
    }

    fn selected_range(&self, line: usize) -> Option<Range<usize>> {
        let selection = self.selection.as_ref()?;
        let (start, end) = (
            selection.anchor.min(selection.head),
            selection.anchor.max(selection.head),
        );
        let (mut first, range, _) = self.line_text(line)?;
        first.offset = range.start;
        let mut last = first;
        last.offset = range.end;
        if start >= last || end <= first {
            return None;
        }
        let from = if (start.message, start.part) == (first.message, first.part) {
            start.offset.max(range.start)
        } else {
            range.start
        };
        let to = if (end.message, end.part) == (first.message, first.part) {
            end.offset.min(range.end)
        } else {
            range.end
        };
        (from < to).then_some(from..to)
    }

    pub fn selection_columns(&self, line: usize) -> Option<Range<usize>> {
        let selected = self.selected_range(line)?;
        let (_, range, text) = self.line_text(line)?;
        Some(
            crate::width::text_width(&text[range.start..selected.start])
                ..crate::width::text_width(&text[range.start..selected.end]),
        )
    }

    pub fn selected_text(&self) -> Option<String> {
        let mut output = String::new();
        let mut previous: Option<TranscriptPoint> = None;
        for index in 0..self.lines.len() {
            let Some(range) = self.selected_range(index) else {
                continue;
            };
            let (mut point, _, text) = self.line_text(index).unwrap();
            if let Some(last) = previous {
                if (last.message, last.part) == (point.message, point.part) {
                    output.push_str(&text[last.offset..range.start]);
                } else {
                    output.push_str(if last.message == point.message {
                        "\n"
                    } else {
                        "\n\n"
                    });
                }
            }
            output.push_str(&text[range.clone()]);
            point.offset = range.end;
            previous = Some(point);
        }
        (!output.is_empty()).then_some(output)
    }

    pub fn editor_layout(&self, columns: usize, rows: usize) -> EditorLines {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let prefix = &self.model.input[..self.model.cursor];
        let mut lines = wrap_context(&self.model.input, columns);
        let prefix_lines;
        let caret = if self.model.cursor == self.model.input.len() {
            &lines
        } else {
            prefix_lines = wrap_context(prefix, columns);
            &prefix_lines
        };
        let width = caret.last().map_or(0, |range| {
            let text = &prefix[range.clone()];
            if text.is_ascii() {
                text.len()
            } else {
                text.chars()
                    .map(|c| usize::from(scalar_width(if c.is_control() { ' ' } else { c }).min(2)))
                    .sum::<usize>()
            }
        });
        let caret_row = caret.len().saturating_sub(1) + usize::from(width >= columns);
        if lines.is_empty() {
            lines.push(0..0);
        }
        if caret_row >= lines.len() {
            lines.push(self.model.input.len()..self.model.input.len());
        }
        let first = self
            .editor_scroll
            .unwrap_or_else(|| caret_row.saturating_sub(rows - 1))
            .min(lines.len().saturating_sub(rows));
        EditorLines {
            lines,
            first,
            caret_row,
            caret_column: width % columns,
        }
    }
    pub fn move_vertical(&mut self, layout: EditorLines, up: bool, extend: bool) {
        let row = if up {
            layout.caret_row.saturating_sub(1)
        } else {
            (layout.caret_row + 1).min(layout.lines.len() - 1)
        };
        let cursor = column_offset(
            &self.model.input,
            layout.lines[row].clone(),
            layout.caret_column as f32,
        );
        self.model.set_cursor(cursor, extend);
    }
    pub fn add_context(&mut self, item: ContextItem) -> Result<(), &'static str> {
        if self.context.basket.iter().any(|old| {
            old.source == item.source && old.label == item.label && old.snapshot == item.snapshot
        }) {
            return Err("Already attached");
        }
        if self.context.basket.len() >= 3
            || self
                .context
                .basket
                .iter()
                .map(|i| i.snapshot.text.len())
                .sum::<usize>()
                + item.snapshot.text.len()
                > 160 * 1024
        {
            return Err("Context is full; remove an attachment first");
        }
        self.context.basket.push(item);
        Ok(())
    }
    pub fn toggle_tool(&mut self, index: usize) {
        self.clear_selection();
        if let Some(id) = self
            .model
            .messages
            .get(index)
            .and_then(|message| message.id.clone())
        {
            if !self.expanded_tools.remove(&id) {
                self.expanded_tools.insert(id);
            }
            self.columns = 0;
        }
    }
    pub fn send(&mut self, client: &Client) -> io::Result<bool> {
        if self.model.busy || self.model.input.trim().is_empty() {
            return Ok(false);
        }
        let pane = client
            .snapshot()
            .ok_or_else(|| io::Error::other("No workspace"))?
            .focused;
        if !client.can_agent_input() {
            return Err(io::Error::other("Workspace is read-only"));
        }
        let mut request = serde_json::json!({
            "action": "send",
            "pane": pane,
            "basket": self.context.basket.iter().map(ContextItem::view).collect::<Vec<_>>(),
        });
        request["input"] = serde_json::Value::String(std::mem::take(&mut self.model.input));
        self.model.pending.push(request);
        self.model.clear_input();
        self.model.busy = true;
        self.model.scroll = 0;
        self.clear_selection();
        self.context.basket.clear();
        self.context.preview = None;
        self.context.menu = false;
        self.columns = 0;
        self.editor_scroll = None;
        Ok(true)
    }

    pub fn attach_object(
        &mut self,
        client: &Client,
        pane: PaneId,
        object: Object,
    ) -> Result<(), &'static str> {
        let screen = client.screens().get(&pane).ok_or("No active pane")?;
        self.add_context(ContextItem {
            label: object.text,
            source: context_source(client, pane),
            kind: object.kind.into(),
            snapshot: context::region_context(screen, object.range.start.line)
                .unwrap_or_else(|| context::object_context(screen, object.range)),
        })
    }
    pub fn attach_screen(
        &mut self,
        client: &Client,
        pane: PaneId,
        selection: bool,
    ) -> Result<(), &'static str> {
        let screen = client.screens().get(&pane).ok_or("No active pane")?;
        let range = if selection {
            Some(
                screen
                    .selection_range()
                    .ok_or("Select text in the terminal first")?,
            )
        } else {
            None
        };
        self.add_context(ContextItem {
            label: if selection {
                "Visible selection"
            } else {
                "Visible screen"
            }
            .into(),
            kind: if selection { "Selection" } else { "Screen" }.into(),
            source: context_source(client, pane),
            snapshot: context::snapshot(screen, range),
        })
    }
    pub fn layout(&mut self, columns: usize) {
        if self.columns == columns {
            return;
        }
        let previous_lines = self.lines.len();
        let anchor = (self.model.scroll > 0)
            .then(|| {
                let last = self.lines.len().saturating_sub(self.model.scroll + 1);
                (0..=last).rev().find_map(|index| {
                    self.line_text(index).map(|(mut point, range, _)| {
                        point.offset = range.start;
                        (point, last - index)
                    })
                })
            })
            .flatten();
        let mut lines = std::mem::take(&mut self.lines);
        lines.clear();
        self.expanded_tools.retain(|id| {
            self.model
                .messages
                .iter()
                .any(|message| message.id.as_ref() == Some(id))
        });
        for (index, message) in self.model.messages.iter().enumerate() {
            let tool = message.id.is_some();
            if tool {
                lines.push(AgentLine::Role(index as u32));
            }
            if !tool
                || message
                    .id
                    .as_ref()
                    .is_some_and(|id| self.expanded_tools.contains(id))
            {
                let reserved = if message.role == "You"
                    && columns >= crate::width::text_width(REWIND_LABEL) + 5
                {
                    1 + crate::width::text_width(REWIND_LABEL)
                } else if tool || message.role == "You" {
                    2
                } else {
                    0
                };
                wrap_context_into(
                    self.message_text(index),
                    columns.saturating_sub(reserved),
                    |range| {
                        lines.push(AgentLine::Text {
                            message: index as u32,
                            range: range.start as u32..range.end as u32,
                        });
                    },
                );
            }
            lines.push(AgentLine::Blank);
        }
        self.lines = lines;
        if let Some((anchor, gap)) = anchor {
            if let Some(index) = self.lines.iter().position(|line| match line {
                AgentLine::Role(message) => *message as usize == anchor.message && anchor.part == 0,
                AgentLine::Text { message, range } => {
                    *message as usize == anchor.message
                        && anchor.part == 1
                        && (range.contains(&(anchor.offset as u32))
                            || range.is_empty() && range.start as usize == anchor.offset)
                }
                AgentLine::Blank => false,
            }) {
                self.model.scroll = self.lines.len().saturating_sub(index + gap + 1);
            }
        } else if self.model.scroll > 0 {
            self.model.scroll = self
                .model
                .scroll
                .saturating_add(self.lines.len().saturating_sub(previous_lines));
        }
        self.columns = columns;
    }
}

#[cfg(feature = "agent")]
fn context_source(client: &Client, pane: PaneId) -> String {
    let name = client.snapshot().map_or("", |s| s.name.as_str());
    let alternate = client
        .screens()
        .get(&pane)
        .is_some_and(|screen| screen.mode().contains(Mode::ALT_SCREEN));
    format!(
        "{name} · pane {pane} · {}{}",
        if client.is_remote() { "SSH" } else { "local" },
        if alternate {
            " · TUI snapshot"
        } else {
            " · visible snapshot"
        }
    )
}

#[cfg(all(test, feature = "agent"))]
mod tests {
    use super::*;

    #[test]
    fn trimming_an_expanded_tool_marks_the_view_without_user_input() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "shell · done".into(),
                text: "output".into(),
                id: Some("old-tool".into()),
            });
        panel.active_mut().expanded_tools.insert("old-tool".into());
        panel.prepare_layout(40).unwrap();
        panel.view_dirty = false;
        let mut models = panel.model_snapshot();
        models[0].1["messages"] = serde_json::json!([]);
        panel.restore_models(models).unwrap();
        assert!(!panel.view_dirty);
        panel.prepare_layout(40).unwrap();
        assert!(panel.active().expanded_tools.is_empty());
        assert!(panel.view_dirty);
        assert_eq!(panel.active().model.scroll, 0);
    }

    #[test]
    fn escape_selects_older_messages_without_changing_the_draft_or_history() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        ui.focus = Focus::Editor;
        ui.agent.active_mut().model.input = "untouched draft".into();
        for turn in 1..=3 {
            ui.agent
                .active_mut()
                .model
                .messages
                .push(crate::agent::Message {
                    turn,
                    role: "You".into(),
                    text: format!("message {turn}"),
                    id: None,
                });
        }
        assert!(ui.agent_rewind_key(0xff1b));
        assert_eq!(ui.agent.active().rewind_target, Some(3));
        assert!(ui.agent_rewind_key(0xff1b));
        assert_eq!(ui.agent.active().rewind_target, Some(2));
        assert!(ui.agent_rewind_key(0xff1b));
        assert_eq!(ui.agent.active().rewind_target, Some(1));
        assert!(ui.agent_rewind_key(0xff1b));
        assert_eq!(ui.agent.active().rewind_target, Some(1));
        assert_eq!(ui.agent.active().model.input, "untouched draft");
        assert_eq!(ui.agent.active().model.messages.len(), 3);
        assert!(!ui.agent.active().model.busy);
        assert!(!ui.agent_rewind_key('x' as u32));
        assert_eq!(ui.agent.active().rewind_target, None);
        assert!(ui.focus == Focus::Editor);
    }

    #[test]
    fn markdown_completion_wakes_the_final_frame_and_uses_the_current_width() {
        let mut panel = AgentPanel::default();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "Agent".into(),
                text: "**done** 中文".into(),
                id: None,
            });
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.layout_fd(), -1);
        assert!(panel.active().lines.is_empty());
        panel.open = true;
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "");
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        assert!(panel.poll_layout().unwrap());
        assert_eq!(panel.active().message_text(0), "");
        panel.prepare_layout(6).unwrap();
        assert_eq!(panel.active().message_text(0), "done 中文");
        assert_eq!(panel.active().lines.len(), 3);
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().lines.len(), 2);
        assert!(panel.active().render_request.is_none());
    }

    #[test]
    fn markdown_appends_request_work_without_redrawing_until_completion() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "Agent".into(),
                text: "**first**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        panel.wait_layout(40);
        assert!(!panel.poll_layout().unwrap());
        owner.active_mut().model.messages[0].text.push_str(" next");
        assert!(!panel.restore_models(owner.model_snapshot()).unwrap());
        assert!(panel.active().render_request.is_some());
        assert_eq!(panel.active().message_text(0), "first");
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        assert!(panel.poll_layout().unwrap());
        assert_eq!(panel.active().message_text(0), "first");
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "first next");

        owner.active_mut().model.messages[0].text = "**other** next".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        panel.wait_layout(40);
        assert_eq!(panel.active().message_text(0), "other next");

        owner.active_mut().model.busy = true;
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.busy = false;
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        let message = &mut owner.active_mut().model.messages[0];
        message.role = "run · running".into();
        message.id = Some("tool".into());
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.messages[0].text = "tool output".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.messages[0].role = "run · done".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
    }

    #[test]
    fn streaming_markdown_progress_survives_new_text_before_the_next_frame() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "Agent".into(),
                text: "**first**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.restore_models(owner.model_snapshot()).unwrap();
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        owner.active_mut().model.messages[0]
            .text
            .push_str("\n\nnext");
        panel.restore_models(owner.model_snapshot()).unwrap();
        assert!(panel.poll_layout().unwrap());
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "first");
        panel.wait_layout(40);
        assert_eq!(panel.active().message_text(0), "first\n\nnext");
    }

    #[test]
    fn markdown_results_ignore_outdated_requests_and_sources() {
        for restore in [false, true] {
            let mut owner = AgentPanel::default();
            owner
                .active_mut()
                .model
                .messages
                .push(crate::agent::Message {
                    turn: 1,
                    role: "Agent".into(),
                    text: "**old".into(),
                    id: None,
                });
            let mut panel = AgentPanel {
                open: true,
                ..AgentPanel::default()
            };
            panel.restore_models(owner.model_snapshot()).unwrap();
            let mut fd = libc::pollfd {
                fd: panel.layout_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
            if restore {
                owner.active_mut().model.messages[0].text = "**new**".into();
                panel.restore_models(owner.model_snapshot()).unwrap();
            } else {
                panel.active_mut().model.messages[0].text = "**new**".into();
            }
            assert!(panel.poll_layout().unwrap());
            panel.prepare_layout(40).unwrap();
            assert_eq!(panel.active().message_text(0), "");
            panel.wait_layout(40);
            assert_eq!(panel.active().message_text(0), "new");
        }
    }

    #[test]
    fn rewinding_discards_a_completed_markdown_result() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "Agent".into(),
                text: "**discarded reply**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.restore_models(owner.model_snapshot()).unwrap();
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        owner.active_mut().model.messages.clear();
        let mut models = owner.model_snapshot();
        models[0].1["rewind_restore"] = serde_json::json!([1, "restored draft", []]);
        panel.restore_models(models).unwrap();
        assert!(panel.poll_layout().unwrap());
        panel.prepare_layout(40).unwrap();
        assert!(panel.active().model.messages.is_empty());
        assert!(panel.active().rendered.is_empty());
        assert!(panel.active().lines.is_empty());
        assert_eq!(panel.active().model.input, "restored draft");
    }

    #[test]
    fn transcript_selection_preserves_unicode_wraps_and_visible_tool_headers() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.active_mut().model.messages = vec![
            crate::agent::Message {
                role: "You".into(),
                text: "a中🙂e\u{301}\nnext".into(),
                id: None,
                turn: 1,
            },
            crate::agent::Message {
                role: "run · done".into(),
                text: "hidden tool output".into(),
                id: Some("tool".into()),
                turn: 1,
            },
            crate::agent::Message {
                role: "Agent".into(),
                text: "answer".into(),
                id: None,
                turn: 1,
            },
        ];
        panel.wait_layout(6);
        let session = panel.active_mut();
        let start = session.point(0, 1).unwrap();
        let last = session
            .lines
            .iter()
            .rposition(|line| matches!(line, AgentLine::Text { message: 2, .. }))
            .unwrap();
        let end = session.point(last, 99).unwrap();
        session.begin_selection(start, false);
        session.extend_selection(end);
        assert_eq!(session.selection_columns(0), Some(1..3));
        let expected = "中🙂e\u{301}\nnext\n\nrun · done\n\nanswer";
        assert_eq!(session.selected_text().as_deref(), Some(expected));
        session.end_selection();
        session.layout(12);
        assert_eq!(session.selected_text().as_deref(), Some(expected));
        session.begin_selection(end, false);
        session.extend_selection(start);
        assert_eq!(session.selected_text().as_deref(), Some(expected));
    }

    #[test]
    fn streamed_models_keep_selection_when_selected_text_is_unchanged() {
        let mut owner = AgentPanel::default();
        owner.set_local();
        owner.active_mut().model.messages = vec![
            crate::agent::Message {
                turn: 1,
                role: "Agent".into(),
                text: "keep this\n\n**draft".into(),
                id: None,
            },
            crate::agent::Message {
                turn: 1,
                role: "run · running".into(),
                text: "running".into(),
                id: Some("cmd".into()),
            },
        ];
        let mut mirror = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        let session = mirror.active_mut();
        session.begin_selection(session.point(0, 0).unwrap(), false);
        session.extend_selection(session.point(0, 4).unwrap());
        assert_eq!(session.selected_text().as_deref(), Some("keep"));

        owner.active_mut().model.messages[0].text.push_str("**");
        owner.active_mut().model.messages[1].text = "done".into();
        owner.active_mut().model.revision += 1;
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        let session = mirror.active_mut();
        assert!(session.selecting());
        assert_eq!(session.selected_text().as_deref(), Some("keep"));
        session.end_selection();

        owner.active_mut().model.messages[0]
            .text
            .push_str(" and more");
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        assert_eq!(mirror.active().selected_text().as_deref(), Some("keep"));

        owner.active_mut().model.messages[0].text = "replaced".into();
        owner.active_mut().model.revision += 1;
        mirror.restore_models(owner.model_snapshot()).unwrap();
        assert!(mirror.active().selected_text().is_none());
        mirror.wait_layout(30);
        assert!(mirror.active().selected_text().is_none());
    }

    #[test]
    fn streaming_markdown_reformatting_clears_stale_selection_offsets() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                role: "Agent".into(),
                text: "**中文".into(),
                id: None,
                turn: 1,
            });
        panel.wait_layout(20);
        let session = panel.active_mut();
        session.begin_selection(session.point(0, 2).unwrap(), false);
        session.extend_selection(session.point(0, 4).unwrap());
        panel.active_mut().model.messages[0].text.push_str("**");
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "中文");
        assert_eq!(panel.active().selected_text(), None);
    }

    #[test]
    fn full_layout_invalidation_overrides_pending_stream_update() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        for text in ["**previous**", "reply"] {
            panel
                .active_mut()
                .model
                .messages
                .push(crate::agent::Message {
                    role: "Agent".into(),
                    text: text.into(),
                    id: None,
                    turn: 1,
                });
        }
        panel.wait_layout(20);
        panel.active_mut().model.messages[1].text.push_str(" next");
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "previous");
        assert_eq!(panel.active().message_text(1), "reply next");
        panel.active_mut().model.messages.remove(0);
        panel.active_mut().columns = 0;
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "reply next");
        assert_eq!(panel.active().rendered.len(), 1);
    }

    #[test]
    fn view_updates_preserve_models_and_apply_rewind_once() {
        let mut owner = AgentPanel::default();
        owner.set_local();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                role: "You".into(),
                text: "earlier".into(),
                id: None,
                turn: 4,
            });
        owner.active_mut().model.revision = 3;
        let item = ContextItem {
            label: "registers".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "rax 0x1234".into(),
                incomplete: false,
            },
        };
        let mut view = owner.view_state(Focus::Terminal);
        let canonical = owner.model_snapshot();
        view["sessions"][0]["input"] = serde_json::json!("typing");
        view["sessions"][0]["cursor"] = serde_json::json!(6);
        owner.restore_view_state(view.clone()).unwrap();
        assert_eq!(owner.model_snapshot(), canonical);
        assert!(canonical[0].1.get("sent_context").is_none());
        assert_eq!(owner.active().model.input, "typing");
        assert!(!owner.active().model.remote);
        let mut frontend = AgentPanel::default();
        frontend.active_mut().model.insert("next draft").unwrap();
        frontend.restore_models(owner.model_snapshot()).unwrap();
        assert_eq!(frontend.active().model.input, "next draft");
        let canonical = frontend.model_snapshot();
        frontend.restore_view_state(view.clone()).unwrap();
        assert_eq!(frontend.model_snapshot(), canonical);
        let mut models = owner.model_snapshot();
        models[0].1["rewind_restore"] = serde_json::json!([4, "earlier", [item.view()]]);
        frontend.restore_models(models.clone()).unwrap();
        assert_eq!(frontend.active().model.input, "earlier");
        assert_eq!(frontend.active().context.basket.len(), 1);
        frontend.active_mut().model.insert(" edited").unwrap();
        frontend.restore_models(models.clone()).unwrap();
        assert_eq!(frontend.active().model.input, "earlier edited");
        let view = frontend.view_state(Focus::Terminal);
        let mut attached = AgentPanel::default();
        attached.restore_view_state(view).unwrap();
        attached.restore_models(models).unwrap();
        assert_eq!(attached.active().model.input, "earlier edited");
        assert_eq!(attached.active().context.basket.len(), 1);
        attached.restore_models(owner.model_snapshot()).unwrap();
        assert_eq!(attached.active().model.input, "earlier edited");
        assert_eq!(attached.active().context.basket.len(), 1);
    }

    #[test]
    fn agent_view_roundtrip_preserves_drafts_and_canonical_models() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.new_session();
        panel.keyboard = AgentAction::Inspect(0);
        let item = ContextItem {
            label: "寄存器".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "rax 0x1234\n终端内容".into(),
                incomplete: true,
            },
        };
        for session in &mut panel.sessions {
            session.model.insert("中文🙂 draft").unwrap();
            session.model.cursor = "中文".len();
            session.model.anchor = Some("中".len());
            session.model.scroll = 7;
            session.context.basket.push(item.clone());
            session.context.menu = true;
            session.context.preview = Some(ContextPreview::new(item.clone(), 3));
            session.expanded_tools.insert("tool-11".into());
            session.editor_scroll = Some(2);
            session.rewind_target = Some(11);
            session.selection = Some(ChatSelection {
                anchor: TranscriptPoint {
                    message: 0,
                    part: 0,
                    offset: 0,
                },
                head: TranscriptPoint {
                    message: 0,
                    part: 0,
                    offset: 6,
                },
                dragging: true,
            });
        }
        let view = panel.view_state(Focus::Output);
        assert!(view["sessions"][0].get("messages").is_none());
        let mut restored = AgentPanel::default();
        restored.new_session();
        for session in &mut restored.sessions {
            session.model.messages.push(crate::agent::Message {
                role: "Agent".into(),
                text: format!("canonical model {}", session.id),
                id: None,
                turn: 11,
            });
            session.model.busy = true;
            session.model.revision = 99;
            session.model.rewound = Some(8);
            session.layout(40);
        }
        restored.sessions.swap(0, 1);
        assert!(restored.restore_view_state(view.clone()).unwrap() == Some(Focus::Output));
        assert_eq!(restored.view_state(Focus::Output), view);
        for session in &restored.sessions {
            assert_eq!(
                session.model.messages[0].text,
                format!("canonical model {}", session.id)
            );
            assert!(session.model.busy);
            assert_eq!(session.model.revision, 99);
            assert_eq!(session.model.rewound, Some(8));
            assert!(!session.selection.as_ref().unwrap().dragging);
            assert!(session.lines.is_empty());
            assert!(session.rendered.is_empty());
            assert_eq!(session.columns, 0);
        }
        let mut invalid = view.clone();
        invalid["sessions"][0]["cursor"] = serde_json::json!(1);
        assert!(restored.restore_view_state(invalid).is_err());
        assert_eq!(restored.view_state(Focus::Output), view);
        restored.new_session();
        assert_eq!(restored.active().id, 2);
    }

    #[test]
    fn restored_stream_updates_markdown_selection_and_trimmed_messages() {
        let mut owner = AgentPanel::default();
        for text in ["**earlier**", "**中文"] {
            owner
                .active_mut()
                .model
                .messages
                .push(crate::agent::Message {
                    role: "Agent".into(),
                    text: text.into(),
                    id: None,
                    turn: 1,
                });
        }
        let mut frontend = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        let session = frontend.active_mut();
        let line = session
            .lines
            .iter()
            .position(|line| matches!(line, AgentLine::Text { message: 1, .. }))
            .unwrap();
        session.begin_selection(session.point(line, 2).unwrap(), false);
        session.extend_selection(session.point(line, 4).unwrap());
        assert!(session.selected_text().is_some());
        owner.active_mut().model.messages[1].text.push_str("**");
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        assert_eq!(frontend.active().message_text(0), "earlier");
        assert_eq!(frontend.active().message_text(1), "中文");
        assert_eq!(frontend.active().selected_text(), None);
        owner.active_mut().model.messages.remove(0);
        owner.active_mut().model.revision += 1;
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        assert_eq!(frontend.active().message_text(0), "中文");
        assert_eq!(frontend.active().rendered.len(), 1);
    }

    #[test]
    fn conversation_tabs_keep_drafts_context_and_scroll_independent() {
        let mut panel = AgentPanel::default();
        panel.active_mut().model.insert("first draft").unwrap();
        panel.active_mut().model.scroll = 7;
        panel.active_mut().context.basket.push(ContextItem {
            label: "Screen".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "original snapshot".into(),
                incomplete: false,
            },
        });
        panel.new_session();
        panel.active_mut().model.insert("second draft").unwrap();
        assert!(panel.active().context.basket.is_empty());
        panel.switch(0);
        assert_eq!(panel.active().model.input, "first draft");
        assert_eq!(panel.active().model.scroll, 7);
        assert_eq!(
            panel.active().context.basket[0].snapshot.text,
            "original snapshot"
        );
        panel.close(1);
        assert_eq!(panel.active().model.input, "first draft");
        for _ in 0..12 {
            panel.new_session();
        }
        let tabs = panel.tabs(30);
        assert!(tabs.iter().any(|(index, _, _)| *index == panel.current));
        assert!(tabs.iter().all(|(_, x, width)| x + width <= 24));
    }

    #[test]
    fn conversation_titles_use_panel_space_and_redistribute_short_tab_width() {
        let mut panel = AgentPanel::default();
        let title = "使用 pwndbg 分析 sudo 的启动与权限检查";
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 1,
                role: "You".into(),
                text: title.into(),
                id: None,
            });
        let wide = panel.tabs(100);
        assert_eq!(wide[0].2, crate::width::text_width(title) + 4);
        assert!(wide[0].2 > 18);
        let narrow = panel.tabs(24);
        assert_eq!(narrow[0].2, 18);
        assert!(wide[0].2 > narrow[0].2);
        panel.new_session();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 2,
                role: "You".into(),
                text: "第二个长会话标题".repeat(8),
                id: None,
            });
        panel.new_session();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::agent::Message {
                turn: 3,
                role: "You".into(),
                text: "a".into(),
                id: None,
            });
        let tabs = panel.tabs(80);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[2].2, 8);
        assert!(tabs[0].2 > 18 && tabs[1].2 > 18);
        assert_eq!(tabs.iter().map(|tab| tab.2).sum::<usize>(), 74);
        let tiny = panel.tabs(12);
        assert!(tiny.iter().any(|tab| tab.0 == panel.current));
        assert!(tiny.iter().all(|tab| tab.1 + tab.2 <= 6));
    }

    #[test]
    fn delayed_paste_keeps_its_conversation_and_never_targets_a_replacement_tab() {
        let mut panel = AgentPanel::default();
        let target = panel.active().id;
        panel.new_session();
        panel.paste(target, "captured paste").unwrap();
        assert_eq!(panel.sessions[0].model.input, "captured paste");
        assert!(panel.active().model.input.is_empty());
        panel.close(0);
        panel.new_session();
        assert!(panel.paste(target, "late paste").is_err());
        assert!(
            panel
                .sessions
                .iter()
                .all(|session| session.id != target && session.model.input.is_empty())
        );
    }

    #[test]
    fn wrapped_ranges_preserve_controls_blank_lines_and_unicode_columns() {
        for (text, expected) in [
            ("ab\tcd\r\n\nx\u{7f}y\n", vec!["ab ", "cd", "", "x y", ""]),
            ("a中b\nc\u{301}🙂d", vec!["a中", "b", "c\u{301}🙂", "d"]),
            ("\r", vec![" "]),
        ] {
            let lines: Vec<String> = wrap_context(text, 3)
                .into_iter()
                .map(|range| {
                    text[range]
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect()
                })
                .collect();
            assert_eq!(lines, expected);
        }
        assert!(wrap_context("", 3).is_empty());
    }

    #[test]
    fn vertical_selection_uses_the_displayed_caret_before_a_wide_wrap() {
        let mut panel = AgentSession::default();
        panel.model.insert("ab中").unwrap();
        panel.model.set_cursor(2, false);
        let layout = panel.editor_layout(3, 3);
        assert_eq!((layout.caret_row, layout.caret_column), (0, 2));
        panel.move_vertical(layout, true, false);
        assert_eq!(panel.model.cursor, 2);
        panel.move_vertical(panel.editor_layout(3, 3), false, true);
        assert_eq!(panel.model.selection(), Some(2..5));
    }

    #[test]
    fn editor_viewport_retains_scrolled_draft_and_full_width_caret() {
        let mut panel = AgentSession::default();
        panel.model.insert("ab中").unwrap();
        let editor = panel.editor_layout(4, 3);
        assert_eq!((editor.caret_row, editor.caret_column), (1, 0));
        assert_eq!(editor.lines, vec![0..5, 5..5]);
        assert_eq!(column_offset(&panel.model.input, 0..5, 3.), 5);
        panel.model.clear_input();
        panel
            .model
            .insert(
                &(0..12)
                    .map(|n| format!("draft {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
        assert_eq!(panel.editor_layout(20, 3).first, 9);
        panel.editor_scroll = Some(3);
        let editor = panel.editor_layout(20, 3);
        assert_eq!(editor.first, 3);
        assert_eq!(editor.caret_row, 11);
    }
    #[test]
    fn history_anchor_survives_reflow_and_collapsed_tool_append() {
        let mut session = AgentSession::default();
        session.model.messages.push(crate::agent::Message {
            turn: 1,
            role: "You".into(),
            text: "0123456789".repeat(50),
            id: None,
        });
        session.layout(20);
        session.model.scroll = 10;
        let last = session.lines.len() - session.model.scroll - 1;
        let anchor = (0..=last)
            .rev()
            .find_map(|index| session.line_text(index).map(|(_, range, _)| range.start))
            .unwrap();
        session.layout(40);
        let last = session.lines.len() - session.model.scroll - 1;
        let range = session.line_text(last).unwrap().1;
        assert!(range.contains(&anchor));
        session.model.messages.push(crate::agent::Message {
            turn: 2,
            role: "shell · done".into(),
            text: "hidden output".repeat(500),
            id: Some("tool".into()),
        });
        session.columns = 0;
        session.layout(40);
        let last = session.lines.len() - session.model.scroll - 1;
        assert!(session.line_text(last).unwrap().1.contains(&anchor));
    }
}
