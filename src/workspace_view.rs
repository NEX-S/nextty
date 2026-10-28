mod render;

#[cfg(feature = "files")]
use crate::files::{FileFocus, FileNav};
use crate::{
    config::Config,
    context,
    core::{Mode, Point, Scroll, SelectionType, Side, WindowSize},
    error::{Error, Result},
    ui::{Focus, Ui},
    width::text_width,
    workspace::{Action, Axis, Client, ClientEvent, Direction, PaneId, SplitDrag, TabId},
};
#[cfg(feature = "agent")]
use crate::{
    context::Object,
    ui::{AgentAction, EditorLines, column_offset},
};
use crate::{
    input::{key_bytes, mouse_bytes, paste_bytes, symbol_char},
    native::trace::{Trace, monotonic},
    native::{self, Event, Line, Renderer, Run, Window},
    paint::{Frame, LineCache, text_columns},
};
use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    time::{Duration, Instant},
};

enum DeferredInput {
    SearchText(String),
    #[cfg(feature = "monitor")]
    MonitorText(String),
    Bytes(PaneId, Vec<u8>),
    Paste(PasteTarget, bool),
    #[cfg(feature = "agent")]
    AgentText(u64, String),
    #[cfg(feature = "files")]
    FileText(String),
}

enum PasteTarget {
    Search,
    #[cfg(feature = "monitor")]
    Monitor,
    #[cfg(feature = "agent")]
    Agent(u64),
    Pane {
        pane: PaneId,
        bracketed: bool,
    },
    #[cfg(feature = "files")]
    Files,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseChoice {
    Terminate,
    Background,
}

fn close_layout(width: f32, height: f32, cell: (f32, f32)) -> [(f32, f32, f32, f32); 3] {
    let w = (cell.0 * 62.).min(width * 0.9);
    let h = (cell.1 * 9.).min(height * 0.9);
    let x = (width - w) * 0.5;
    let y = (height - h) * 0.5;
    let padding = cell.0.min(w * 0.1);
    let button_w = (w - padding * 3.) * 0.5;
    let button_h = (cell.1 * 2.).min(h * 0.25);
    [
        (x, y, w, h),
        (x + padding, y + h * 0.5, button_w, button_h),
        (x + padding * 2. + button_w, y + h * 0.5, button_w, button_h),
    ]
}

#[cfg(feature = "files")]
struct FileLayout {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    split: f32,
    top: f32,
    rows: usize,
}

#[derive(Default)]
struct TextCache {
    lines: Vec<(String, u32, u32, Line)>,
    used: usize,
}

impl TextCache {
    fn draw(&mut self, renderer: &Renderer, text: &str, x: f32, y: f32, color: u32) -> Result<()> {
        self.draw_flags(renderer, text, x, y, color, 0)
    }

    fn draw_flags(
        &mut self,
        renderer: &Renderer,
        text: &str,
        x: f32,
        y: f32,
        color: u32,
        flags: u32,
    ) -> Result<()> {
        self.draw_at(renderer, text, (x, y, 1.), color, flags)
    }

    fn draw_at(
        &mut self,
        renderer: &Renderer,
        text: &str,
        position: (f32, f32, f32),
        color: u32,
        flags: u32,
    ) -> Result<()> {
        if self
            .lines
            .get(self.used)
            .is_none_or(|(previous, ink, previous_flags, _)| {
                previous != text || *ink != color || *previous_flags != flags
            })
        {
            let line = renderer.shape(
                text,
                &[Run {
                    start: 0,
                    end: text.len() as u32,
                    color,
                    flags,
                }],
                &text_columns(text),
            )?;
            let entry = (text.to_owned(), color, flags, line);
            if self.used < self.lines.len() {
                self.lines[self.used] = entry;
            } else {
                self.lines.push(entry);
            }
        }
        renderer.line_scaled(&self.lines[self.used].3, position.0, position.1, position.2);
        self.used += 1;
        Ok(())
    }
}

const CHROME: u32 = 0x1c2021ff;
#[cfg(feature = "agent")]
const PANEL: u32 = 0x1b1d1bff;
const BORDER: u32 = 0x343b3dff;
const INK: u32 = 0xd4d9d8ff;
const MUTED: u32 = 0x889391ff;
const ACCENT: u32 = 0xb5d2a2ff;
const PADDING: f32 = 16.;
const TERMINAL_PADDING: f32 = 4.;

#[derive(Clone, Copy)]
struct GuiLayout {
    top: f32,
    footer: f32,
    side: f32,
    width: f32,
    height: f32,
}

impl GuiLayout {
    fn new(width: f32, height: f32, cell: (f32, f32), agent_width: Option<f32>) -> Self {
        let side_width = agent_width.map_or(0., |width_px| width_px.min(width * 0.62));
        Self {
            top: cell.1,
            footer: height - cell.1 - 12.,
            side: width - side_width,
            width,
            height,
        }
    }
    fn origin(self, cell: (f32, f32)) -> (f32, f32) {
        (TERMINAL_PADDING, self.top + TERMINAL_PADDING - cell.1)
    }
    fn size(self, cell: (f32, f32)) -> WindowSize {
        WindowSize {
            num_cols: (((self.side - TERMINAL_PADDING * 2.) / cell.0).floor() as u16).max(2),
            num_lines: (((self.footer - self.top - TERMINAL_PADDING * 2.) / cell.1).floor() as u16)
                .saturating_add(1)
                .max(2),
            cell_width: cell.0.round() as u16,
            cell_height: cell.1.round() as u16,
        }
    }
    #[cfg(any(feature = "agent", test))]
    fn divider_contains(self, point: (f32, f32)) -> bool {
        self.side < self.width
            && point.0 >= self.side
            && point.0 < self.side + 8.
            && (self.top..self.footer).contains(&point.1)
    }
    fn terminal_contains(self, point: (f32, f32)) -> bool {
        point.0 >= TERMINAL_PADDING
            && point.0 < self.side - TERMINAL_PADDING
            && point.1 >= self.top + TERMINAL_PADDING
            && point.1 < self.footer - TERMINAL_PADDING
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    Pane(PaneId),
    #[cfg(feature = "agent")]
    Agent,
    #[cfg(feature = "agent")]
    Editor,
    #[cfg(feature = "agent")]
    Preview,
}

#[derive(Clone, Copy)]
struct Scrollbar {
    target: ScrollTarget,
    bounds: (f32, f32, f32, f32),
    total: usize,
    visible: usize,
    offset: usize,
}

impl Scrollbar {
    fn new(
        target: ScrollTarget,
        bounds: (f32, f32, f32, f32),
        total: usize,
        visible: usize,
        offset: usize,
    ) -> Option<Self> {
        (total > visible && bounds.3 > 0.).then_some(Self {
            target,
            bounds,
            total,
            visible,
            offset,
        })
    }
    fn maximum(self) -> usize {
        self.total - self.visible
    }
    fn thumb(self) -> (f32, f32) {
        let height = (self.bounds.3 * self.visible as f32 / self.total as f32)
            .max(20_f32.min(self.bounds.3));
        let top = self.bounds.1
            + (self.bounds.3 - height)
                * (1. - self.offset.min(self.maximum()) as f32 / self.maximum() as f32);
        (top, height)
    }
    fn contains(self, point: (f32, f32)) -> bool {
        let (x, y, w, h) = self.bounds;
        point.0 >= x - 2. && point.0 < x + w && point.1 >= y && point.1 < y + h
    }
    fn offset_at(self, y: f32, grab: f32) -> usize {
        let (_, height) = self.thumb();
        let travel = self.bounds.3 - height;
        if travel <= 0. {
            return self.offset;
        }
        ((1. - ((y - grab - self.bounds.1) / travel).clamp(0., 1.)) * self.maximum() as f32).round()
            as usize
    }
}

struct ScrollbarDrag {
    bar: Scrollbar,
    grab: f32,
    requested: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GuiAction {
    #[cfg(feature = "monitor")]
    Resources,
    #[cfg(feature = "monitor")]
    Monitor(crate::monitor::Action),
    NewTab,
    CloseTab(TabId),
    Tab(i32),
    DismissHelp,
    Minimize,
    Maximize,
    Close,
    #[cfg(feature = "agent")]
    Agent(crate::ui::AgentAction),
}

struct GuiHit {
    bounds: (f32, f32, f32, f32),
    action: GuiAction,
}

impl GuiHit {
    fn contains(&self, point: (f32, f32)) -> bool {
        let (x, y, w, h) = self.bounds;
        point.0 >= x && point.0 < x + w && point.1 >= y && point.1 < y + h
    }
}

#[cfg(feature = "agent")]
struct EditorLayout {
    x: f32,
    y: f32,
    columns: usize,
    editor: EditorLines,
}

#[cfg(feature = "agent")]
struct ChatRow {
    line: usize,
    x: f32,
    y: f32,
    columns: usize,
}

#[derive(Clone, Copy)]
struct SelectionDrag {
    pane: PaneId,
    point: Point,
    side: Side,
    kind: SelectionType,
}

pub struct WorkspaceView {
    client: Client,
    ui: Ui,
    caches: HashMap<PaneId, LineCache>,
    text: TextCache,
    renderer: Renderer,
    window: Window,
    config: Config,
    cell: (f32, f32),
    scale: f64,
    dirty: bool,
    running: bool,
    focused: bool,
    pointer: (f32, f32),
    selecting: Option<SelectionDrag>,
    split_drag: Option<SplitDrag>,
    reported_buttons: u8,
    reported_pane: Option<PaneId>,
    scroll_remainder: f32,
    last_click: Option<(Instant, PaneId, Point, u8)>,
    preedit: String,
    paste: Option<PasteTarget>,
    deferred: VecDeque<DeferredInput>,
    next_copy: u64,
    closing: Option<CloseChoice>,
    #[cfg(feature = "agent")]
    agent_width: f32,
    #[cfg(feature = "agent")]
    agent_drag: Option<f32>,
    #[cfg(feature = "agent")]
    divider_hover: bool,
    scrollbars: Vec<Scrollbar>,
    scrollbar_drag: Option<ScrollbarDrag>,
    #[cfg(feature = "agent")]
    selecting_editor: bool,
    #[cfg(feature = "agent")]
    chat_rows: Vec<ChatRow>,
    #[cfg(feature = "agent")]
    agent_tool_click: Option<usize>,
    maximized: bool,
    tab_hits: Vec<(f32, f32, usize)>,
    hovered: Option<(PaneId, crate::context::Object)>,

    hovered_block: Option<(PaneId, u64)>,
    hover_cell: Option<(PaneId, Point)>,
    hovered_control: Option<GuiAction>,
    control_hits: Vec<GuiHit>,
    #[cfg(feature = "files")]
    file_hits: Vec<(f32, f32, usize)>,
    suppressed_text: Option<String>,
    trace: Option<Trace>,
}

impl WorkspaceView {
    pub fn new(client: Client, config: Config) -> Result<Self> {
        let width = (config.font_size_px * 0.625).ceil() as i32 * 120 + 32;
        let height = (config.font_size_px * 1.35).ceil() as i32 * 32 + 80;
        let window = Window::new(width, height)?;
        let renderer = Renderer::new(&window, &config.font_family, config.font_size_px)?;
        let cell = renderer.metrics();
        let scale = window.size().2;
        let trace = Trace::from_env()?;
        let ui = Ui::new()?;
        let mut view = Self {
            client,
            ui,
            renderer,
            window,
            config,
            cell,
            scale,
            caches: HashMap::new(),
            text: TextCache::default(),
            dirty: true,
            running: true,
            focused: false,
            pointer: (0., 0.),
            selecting: None,
            split_drag: None,
            reported_buttons: 0,
            reported_pane: None,
            scroll_remainder: 0.,
            last_click: None,
            preedit: String::new(),
            paste: None,
            deferred: VecDeque::new(),
            next_copy: 1,
            closing: None,
            #[cfg(feature = "agent")]
            agent_width: cell.0 * 36. + PADDING * 2.,
            #[cfg(feature = "agent")]
            agent_drag: None,
            #[cfg(feature = "agent")]
            divider_hover: false,
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            #[cfg(feature = "agent")]
            selecting_editor: false,
            #[cfg(feature = "agent")]
            chat_rows: Vec::new(),
            #[cfg(feature = "agent")]
            agent_tool_click: None,
            maximized: false,
            tab_hits: Vec::new(),
            hovered: None,

            hovered_block: None,
            hover_cell: None,
            hovered_control: None,
            control_hits: Vec::new(),
            #[cfg(feature = "files")]
            file_hits: Vec::new(),
            suppressed_text: None,
            trace,
        };
        view.resize()?;
        Ok(view)
    }
    pub fn run(mut self) -> Result<()> {
        let mut events = Vec::new();
        let mut pollfds = Vec::new();
        while self.running {
            #[cfg(feature = "monitor")]
            if let Some(snapshot) = self.client.snapshot() {
                let directory = if self.client.is_remote() {
                    Some(std::path::Path::new("/"))
                } else {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.id == snapshot.focused)
                        .and_then(|pane| pane.cwd.as_deref())
                };
                self.dirty |= self
                    .ui
                    .resources
                    .stats
                    .refresh(directory, self.ui.resources.open)?;
            }
            let ready = self.renderer.ready()?;
            if ready && let Some(trace) = &mut self.trace {
                trace.record_swap(self.renderer.swap_time())?;
            }
            let mut timeout = if self.dirty && ready && self.window.ready() {
                0
            } else {
                -1
            };
            if let Some((_, until)) = &self.ui.status {
                let remaining = until
                    .saturating_duration_since(Instant::now())
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min(i32::MAX as u128) as i32;
                timeout = if timeout < 0 {
                    remaining
                } else {
                    timeout.min(remaining)
                };
            }
            if self.client.needs_poll() {
                timeout = 0;
            }
            let socket_events = if self.client.wants_read() {
                libc::POLLIN
            } else {
                0
            } | if self.client.wants_write() {
                libc::POLLOUT
            } else {
                0
            };
            let socket_fd = if socket_events == 0 {
                -1
            } else {
                self.client.fd()
            };
            pollfds.clear();
            pollfds.extend(
                [
                    (socket_fd, socket_events),
                    (self.renderer.fd(), libc::POLLIN),
                    (self.client.receiver_fd(), libc::POLLIN),
                    #[cfg(feature = "files")]
                    (
                        self.ui
                            .files
                            .as_ref()
                            .filter(|_| self.ui.files_open)
                            .map_or(-1, FileNav::fd),
                        libc::POLLIN,
                    ),
                    #[cfg(feature = "monitor")]
                    (self.ui.resources.stats.fd(), libc::POLLIN),
                ]
                .map(|(fd, events)| libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                }),
            );
            #[cfg(feature = "agent")]
            pollfds.extend(self.ui.agent.fds().map(|(fd, events)| libc::pollfd {
                fd,
                events,
                revents: 0,
            }));
            self.window.wait(&mut pollfds, timeout, &mut events)?;
            if pollfds[0].revents & libc::POLLOUT != 0 {
                self.client.flush()?;
            }
            for event in events.drain(..) {
                self.event(event)?;
            }
            let previous = self.client.snapshot().map(|s| (s.focused, s.controller));
            let responses = if pollfds[0].revents
                & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                != 0
                || pollfds[2].revents != 0
                || self.client.needs_poll()
            {
                self.client.poll()?
            } else {
                Vec::new()
            };
            for event in responses {
                match event {
                    ClientEvent::Updated => {
                        self.dirty = true;
                        if self.ui.search.open {
                            self.dirty |= self.ui.search.observe(&mut self.client)?;
                        }
                        let current = self.client.snapshot().map(|s| (s.focused, s.controller));
                        if previous != current {
                            for (pane, _) in previous.into_iter().chain(current) {
                                if let Some(screen) = self.client.screen_mut(pane) {
                                    screen.damage_cursor();
                                }
                            }
                            if let Some(snapshot) = self.client.snapshot() {
                                self.window.title(&format!(
                                    "nextty — {}{}{}",
                                    snapshot.name,
                                    if self.client.is_remote() {
                                        " · SSH"
                                    } else {
                                        ""
                                    },
                                    if snapshot.controller {
                                        ""
                                    } else {
                                        " · read-only"
                                    }
                                ))?;
                            }
                        }
                    }
                    ClientEvent::Clipboard {
                        request,
                        file,
                        incomplete,
                    } => {
                        let primary = request & 1 != 0;
                        self.window.selection_file(primary, file);
                        self.message(if incomplete {
                            "Copied retained output; history is incomplete"
                        } else {
                            "Copied"
                        });
                    }
                    ClientEvent::Search {
                        pane,
                        request,
                        count,
                        truncated,
                    } => {
                        self.dirty |= self.ui.search.result(
                            &mut self.client,
                            pane,
                            request,
                            count,
                            truncated,
                        )?;
                    }
                    ClientEvent::Error(error) => self.message(error),
                    ClientEvent::Detached => self.running = false,
                    ClientEvent::List(_) => (),
                    ClientEvent::ClipboardStore { primary, text } => {
                        self.window.selection(primary, &text)
                    }
                }
            }
            #[cfg(feature = "agent")]
            {
                let revision = self.ui.agent.active().model.revision;
                self.ui.agent.width = Some(self.agent_width);
                let (changed, messages_changed) =
                    self.ui.poll_agent(&mut self.client, &self.config)?;
                if let Some(width) = self.ui.agent.width {
                    self.agent_width = width;
                }
                if revision != self.ui.agent.active().model.revision {
                    self.chat_rows.clear();
                    self.agent_tool_click = None;
                }
                if messages_changed {
                    self.control_hits.retain(|hit| {
                        !matches!(
                            hit.action,
                            GuiAction::Agent(
                                AgentAction::Copy(_)
                                    | AgentAction::Tool(_)
                                    | AgentAction::Rewind(_)
                            )
                        )
                    });
                }
                self.dirty |= changed && self.ui.agent.open;
            }
            self.refresh_hover();
            #[cfg(feature = "files")]
            if self.ui.files_open
                && let Some(files) = &mut self.ui.files
            {
                if pollfds
                    .iter()
                    .any(|source| source.fd == files.fd() && source.revents != 0)
                {
                    files.drain_notification()?;
                }
                self.dirty |= files.poll();
            }
            self.dirty |= self.ui.expire_status();
            if self.running && self.dirty && self.renderer.ready()? && self.window.ready() {
                if let Some(trace) = &mut self.trace {
                    trace.record_swap(self.renderer.swap_time())?;
                }
                self.draw()?;
                self.dirty = false;
            }
        }
        #[cfg(feature = "agent")]
        if self.client.snapshot().is_some() {
            self.ui.agent.width = Some(self.agent_width);
            self.ui.poll_agent(&mut self.client, &self.config)?;
        }
        if let Some(trace) = &mut self.trace {
            trace.output.get_mut().draining = true;
        }
        self.renderer.finish()?;
        if let Some(trace) = &mut self.trace {
            trace.record_swap(self.renderer.swap_time())?;
            trace.finish()?;
        }
        Ok(())
    }

    fn message(&mut self, text: impl Into<String>) {
        self.ui.message(text);
        self.dirty = true;
    }
    fn action(&mut self, action: Action) -> Result<()> {
        if matches!(
            &action,
            Action::Split { .. }
                | Action::NewTab { .. }
                | Action::Focus(_)
                | Action::Navigate(_)
                | Action::Tab(_)
                | Action::ClosePane(_)
                | Action::CloseTab(_)
        ) {
            if self.ui.search.open {
                self.ui.search.close(&mut self.client)?;
            }
            self.ui.focus = Focus::Terminal;
            #[cfg(feature = "agent")]
            {
                self.ui.agent.active_mut().rewind_target = None;
            }
            #[cfg(feature = "files")]
            {
                self.ui.files_open = false;
            }
            self.dirty = true;
        }
        self.client.send(action)?;
        Ok(())
    }
    fn active(&self) -> Option<PaneId> {
        self.client.snapshot().map(|s| s.focused)
    }
    fn controller(&self) -> bool {
        self.client.snapshot().is_some_and(|s| s.controller)
    }
    fn mode(&self, pane: PaneId) -> Mode {
        self.client
            .screens()
            .get(&pane)
            .map_or(Mode::NONE, |s| s.mode())
    }
    fn input(&mut self, pane: PaneId, data: Vec<u8>) -> Result<()> {
        if !self.controller() {
            return Ok(());
        }
        if self.paste.is_some() {
            self.deferred.push_back(DeferredInput::Bytes(pane, data));
            return Ok(());
        }
        self.action(Action::Input { pane, data })?;
        if let Some(trace) = &mut self.trace {
            writeln!(trace.output, "write_queued {:.9}", monotonic())?;
        }
        Ok(())
    }
    fn resize(&mut self) -> Result<()> {
        if self.ui.search.open {
            self.ui.search.close(&mut self.client)?;
        }
        let (_, _, scale) = self.window.size();
        if scale != self.scale {
            self.caches.clear();
            self.text.lines.clear();
            self.scale = scale;
        }
        self.hover_cell = None;
        self.hovered = None;

        {
            self.hovered_block = None;
        }
        self.action(Action::Resize(self.layout().size(self.cell)))?;
        self.dirty = true;
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<()> {
        if matches!(event.kind, native::KEY | native::TEXT | native::PREEDIT)
            && let Some(trace) = &mut self.trace
        {
            writeln!(
                trace.output,
                "dispatch {:.9} {} {}",
                monotonic(),
                event.kind,
                event.text.len()
            )?;
        }
        match event.kind {
            native::FRAME => {
                if event.code != 0
                    && let Some(trace) = &mut self.trace
                {
                    writeln!(
                        trace.output,
                        "frame_callback {} {:.9} {:.9} {:.0}",
                        event.code,
                        event.x,
                        monotonic(),
                        event.y
                    )?;
                }
            }
            native::PRESENTED => {
                if let Some(trace) = &mut self.trace {
                    writeln!(
                        trace.output,
                        "presented {} {:.9} {} {} {} {:.0} {:.9}",
                        event.code,
                        event.x,
                        event.start,
                        event.end,
                        event.modifiers,
                        event.y,
                        monotonic()
                    )?;
                }
            }
            native::WINDOW_STATE => {
                self.maximized = event.code & 8 != 0;
                self.dirty = true;
                if let Some(trace) = &mut self.trace {
                    writeln!(trace.output, "window_state {:.9} {}", event.x, event.code)?;
                }
            }
            native::FRAME_RATE => {
                if let Some(trace) = &mut self.trace {
                    writeln!(trace.output, "frame_rate {:.9} {:.0}", monotonic(), event.x)?;
                }
            }
            native::KEY => self.key(event.code, event.modifiers, &event.text)?,
            native::TEXT => {
                #[cfg(feature = "agent")]
                if self.ui.agent.active().rewind_target.is_some() {
                    self.ui.agent_rewind_key(0);
                }
                if self.ui.help || self.closing.is_some() || self.preview_open() {
                    return Ok(());
                }
                let marked = !self.preedit.is_empty();
                self.preedit.clear();
                if marked {
                    if let Some(pane) = self.active()
                        && let Some(screen) = self.client.screen_mut(pane)
                    {
                        screen.damage_cursor();
                    }
                    self.dirty = true;
                }
                if self.suppressed_text.take().is_some_and(|s| s == event.text) {
                    return Ok(());
                }
                if self.ui.search.open {
                    if self.paste.is_some() {
                        self.deferred
                            .push_back(DeferredInput::SearchText(event.text));
                    } else {
                        self.ui.search.edit(&mut self.client, &event.text)?;
                    }
                    self.dirty = true;
                    return Ok(());
                }
                #[cfg(feature = "monitor")]
                if self.ui.resources.open && self.ui.focus == Focus::Monitor {
                    if self.ui.resources.searching {
                        if self.paste.is_some() {
                            self.deferred
                                .push_back(DeferredInput::MonitorText(event.text));
                        } else {
                            self.ui.resources.insert_query(&event.text);
                        }
                        self.dirty = true;
                    }
                    return Ok(());
                }
                #[cfg(feature = "agent")]
                if self.ui.agent.open && (self.ui.focus == Focus::Editor) {
                    self.ui.agent.active_mut().editor_scroll = None;
                    if self.paste.is_some() {
                        self.deferred.push_back(DeferredInput::AgentText(
                            self.ui.agent.active().id,
                            event.text,
                        ));
                    } else if let Err(error) = self.ui.agent.active_mut().model.insert(&event.text)
                    {
                        self.message(error);
                    }
                    self.dirty = true;
                    return Ok(());
                }
                #[cfg(feature = "agent")]
                if self.ui.agent.open && matches!(self.ui.focus, Focus::Output | Focus::Controls) {
                    return Ok(());
                }
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    if let Some(files) = &mut self.ui.files
                        && files.is_search()
                        && files.focus() == FileFocus::List
                    {
                        if self.paste.is_some() {
                            self.deferred.push_back(DeferredInput::FileText(event.text));
                        } else {
                            files.insert_query(&event.text);
                        }
                    }
                    self.dirty = true;
                    return Ok(());
                }
                if let Some(pane) = self.active() {
                    self.input(pane, event.text.into_bytes())?;
                }
            }
            native::PREEDIT => {
                #[cfg(feature = "monitor")]
                if self.ui.resources.open && self.ui.focus == Focus::Monitor {
                    self.preedit.clear();
                    return Ok(());
                }
                #[cfg(feature = "agent")]
                if self.ui.agent.open && matches!(self.ui.focus, Focus::Output | Focus::Controls) {
                    return Ok(());
                }
                if self.ui.help || self.closing.is_some() || self.preview_open() {
                    return Ok(());
                }
                let start = if event.start < 0 {
                    event.text.len()
                } else {
                    event.start as usize
                };
                let end = if event.end < 0 {
                    event.text.len()
                } else {
                    event.end as usize
                };
                if !event.text.is_char_boundary(start) || !event.text.is_char_boundary(end) {
                    return Err(Error::msg("invalid IME preedit offset"));
                }
                self.preedit = event.text;
                self.dirty = true;
                if let Some(pane) = self.active()
                    && let Some(screen) = self.client.screen_mut(pane)
                {
                    screen.damage_cursor();
                }
            }
            native::RESIZE => self.resize()?,
            native::EXPOSE => self.dirty = true,
            native::CLOSE => {
                self.open_close();
            }
            native::FOCUS => {
                self.focused = event.code != 0;
                if let Some(trace) = &mut self.trace {
                    writeln!(
                        trace.output,
                        "focus {:.9} {}",
                        monotonic(),
                        u8::from(self.focused)
                    )?;
                    // The input probe needs this before the buffered frame log is filled.
                    trace.output.flush()?;
                }
                if let Some(pane) = self.active() {
                    if self.mode(pane).contains(Mode::FOCUS_IN_OUT) {
                        self.input(
                            pane,
                            if self.focused {
                                b"\x1b[I".to_vec()
                            } else {
                                b"\x1b[O".to_vec()
                            },
                        )?;
                    }
                    if let Some(screen) = self.client.screen_mut(pane) {
                        screen.damage_cursor();
                    }
                }
                self.dirty = true;
            }
            native::MOTION => {
                self.motion((event.x as f32, event.y as f32), event.modifiers)?;
            }
            native::POINTER_LEAVE => {
                self.pointer = (-1., -1.);
                self.hover_cell = None;
                self.hovered = None;
                self.hovered_block = None;
                self.hovered_control = None;
                self.dirty = true;
            }
            native::BUTTON => {
                self.pointer = (event.x as f32, event.y as f32);
                self.button(event.code, event.start != 0, event.modifiers)?;
            }
            native::SCROLL => self.scroll(event.y as f32, event.modifiers)?,
            native::PASTE => self.pasted(event.text)?,
            _ => (),
        }
        Ok(())
    }
    fn workspace_key(&mut self, pane: PaneId, symbol: u32, modifiers: u32) -> Result<bool> {
        if modifiers & (native::ALT | native::CONTROL | native::SUPER) != native::ALT {
            return Ok(false);
        }
        if !self.preedit.is_empty() {
            self.message("Finish or cancel IME composition first");
            return Ok(true);
        }
        match symbol_char(symbol).map(|c| c.to_ascii_lowercase()) {
            Some('?') => {
                self.ui.help = true;
                self.ui.help_scroll = 0;
                self.dirty = true;
                return Ok(true);
            }
            Some('o') => {
                self.ui.cycle_focus(modifiers & native::SHIFT != 0);
                self.resize()?;
                return Ok(true);
            }
            #[cfg(feature = "monitor")]
            Some('m') => {
                if self.ui.search.open {
                    self.ui.search.close(&mut self.client)?;
                }
                self.gui_action(GuiAction::Resources)?;
                return Ok(true);
            }
            Some('/') => {
                if self.ui.search.open {
                    self.ui.search.close(&mut self.client)?;
                } else {
                    self.ui.search.begin(&mut self.client, pane)?;
                }
                self.dirty = true;
                return Ok(true);
            }
            _ => (),
        }
        let action = match symbol_char(symbol).map(|c| c.to_ascii_lowercase()) {
            Some('s') => Some(Action::Split {
                pane: 0,
                axis: Axis::TopBottom,
            }),
            Some('v') => Some(Action::Split {
                pane: 0,
                axis: Axis::LeftRight,
            }),
            Some('h') => Some(Action::Navigate(Direction::Left)),
            Some('j') => Some(Action::Navigate(Direction::Down)),
            Some('k') => Some(Action::Navigate(Direction::Up)),
            Some('l') => Some(Action::Navigate(Direction::Right)),
            Some(',') => Some(Action::Tab(-1)),
            Some('.') => Some(Action::Tab(1)),

            Some('p') => Some(Action::PromptJump {
                pane,
                previous: true,
            }),

            Some('n') => Some(Action::PromptJump {
                pane,
                previous: false,
            }),

            Some('c') => {
                self.copy_block(pane, None)?;
                return Ok(true);
            }
            #[cfg(feature = "files")]
            Some('f') => {
                if self.ui.search.open {
                    self.ui.search.close(&mut self.client)?;
                }
                self.open_files(pane)?;
                return Ok(true);
            }
            _ => {
                if symbol == 0xff0d || symbol == 0xff8d {
                    Some(Action::NewTab { pane })
                } else {
                    None
                }
            }
        };
        if let Some(action) = action {
            if self.controller() {
                self.action(action)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<()> {
        if let Some(choice) = self.closing {
            match symbol {
                0xff0d | 0xff8d => self.choose_close(choice)?,
                0xff1b => {
                    self.closing = None;
                    self.dirty = true;
                }
                0xff51 => {
                    self.closing = Some(CloseChoice::Terminate);
                    self.dirty = true;
                }
                0xff53 => {
                    self.closing = Some(CloseChoice::Background);
                    self.dirty = true;
                }
                0xff09 | 0xfe20 => {
                    self.closing = Some(match choice {
                        CloseChoice::Terminate => CloseChoice::Background,
                        CloseChoice::Background => CloseChoice::Terminate,
                    });
                    self.dirty = true;
                }
                _ => {
                    if symbol_char(symbol).is_some_and(|c| c == 'x' || c == 'X') {
                        self.choose_close(CloseChoice::Terminate)?;
                    }
                }
            }
            return Ok(());
        }
        let Some(pane) = self.active() else {
            return Ok(());
        };
        if modifiers == native::ALT && symbol_char(symbol) == Some('q') {
            self.running = false;
            return Ok(());
        }
        if self.ui.help {
            match symbol {
                0xff1b => self.ui.help = false,
                0xff52 => self.ui.help_scroll = self.ui.help_scroll.saturating_sub(1),
                0xff54 => self.ui.help_scroll = self.ui.help_scroll.saturating_add(1),
                0xff55 => self.ui.help_scroll = self.ui.help_scroll.saturating_sub(8),
                0xff56 => self.ui.help_scroll = self.ui.help_scroll.saturating_add(8),
                _ if modifiers & native::ALT != 0 && symbol_char(symbol) == Some('?') => {
                    self.ui.help = false
                }
                _ => (),
            }
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if modifiers == native::ALT && symbol_char(symbol) == Some('a') {
            if !self.preedit.is_empty() {
                self.message("Finish or cancel IME composition first");
                return Ok(());
            }
            if self.ui.search.open {
                self.ui.search.close(&mut self.client)?;
            }
            self.ui.toggle_agent();
            self.resize()?;
            return Ok(());
        }
        if self.workspace_key(pane, symbol, modifiers)? {
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.active().context.preview.is_some() {
            if let Some(action) = self.ui.agent_key(
                symbol,
                modifiers & native::SHIFT != 0,
                modifiers & native::CONTROL != 0,
            ) {
                if modifiers & native::CONTROL == 0
                    && let Some(ch) = symbol_char(symbol)
                {
                    self.suppressed_text = Some(ch.to_string());
                }
                match self.ui.activate_agent(action, &self.client) {
                    Ok(Some(copy)) => self.window.selection(false, &copy),
                    Ok(None) => (),
                    Err(error) => self.message(error.to_string()),
                }
            } else if let Some((_, scroll)) = &mut self.ui.agent.active_mut().context.preview {
                match symbol {
                    0xff1b => self.ui.agent.active_mut().context.preview = None,
                    0xff52 => *scroll = scroll.saturating_sub(1),
                    0xff54 => *scroll = scroll.saturating_add(1),
                    0xff55 => *scroll = scroll.saturating_sub(10),
                    0xff56 => *scroll = scroll.saturating_add(10),
                    _ => (),
                }
            }
            self.dirty = true;
            return Ok(());
        }
        if self.ui.search.open {
            let control = modifiers & native::CONTROL != 0;
            if (control && symbol_char(symbol).is_some_and(|c| c.eq_ignore_ascii_case(&'v')))
                || modifiers == native::SHIFT && symbol == 0xff63
            {
                self.request_paste(PasteTarget::Search, false);
            } else {
                self.ui.search.key(
                    &mut self.client,
                    symbol,
                    modifiers & native::SHIFT != 0,
                    control,
                )?;
            }
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "monitor")]
        if self.ui.resources.open && self.ui.focus == Focus::Monitor {
            let control = modifiers & native::CONTROL != 0;
            if self.ui.resources.searching
                && ((control && symbol_char(symbol).is_some_and(|c| c.eq_ignore_ascii_case(&'v')))
                    || modifiers == native::SHIFT && symbol == 0xff63)
            {
                self.request_paste(PasteTarget::Monitor, false);
                return Ok(());
            }
            let open = self.ui.resources.open;
            if !self.ui.resources.searching
                && !control
                && modifiers & (native::ALT | native::SUPER) == 0
                && let Some(ch) = symbol_char(symbol)
            {
                self.suppressed_text = Some(ch.to_string());
            }
            let letter = if control || !self.ui.resources.searching {
                symbol_char(symbol)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            self.ui.resources.key(
                if symbol == 0xff09 && modifiers & native::SHIFT != 0 {
                    0xfe20
                } else {
                    symbol
                },
                &letter,
                control,
            );
            if open != self.ui.resources.open {
                self.ui.focus = Focus::Terminal;
                self.resize()?;
            }
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent_rewind_key(symbol) {
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.open && self.ui.focus.is_agent() {
            let control = modifiers & native::CONTROL != 0;
            let shift = modifiers & native::SHIFT != 0;
            let letter = symbol_char(symbol).map(|c| c.to_ascii_lowercase());
            if matches!(symbol, 0xff09 | 0xfe20) {
                self.ui.agent_tab(shift || symbol == 0xfe20);
                self.dirty = true;
                return Ok(());
            }
            if let Some(action) = self.ui.agent_key(symbol, shift, control) {
                if !control && let Some(ch) = symbol_char(symbol) {
                    self.suppressed_text = Some(ch.to_string());
                }
                match self.ui.activate_agent(action, &self.client) {
                    Ok(Some(copy)) => self.window.selection(false, &copy),
                    Ok(None) => (),
                    Err(error) => self.message(error.to_string()),
                }
                self.dirty = true;
                return Ok(());
            }
            if self.ui.focus == Focus::Controls {
                return Ok(());
            }
            if self.ui.focus == Focus::Output {
                if control && letter == Some('c') {
                    if let Some(text) = self.ui.agent.active().selected_text() {
                        self.window.selection(false, &text);
                        self.message("Copied");
                    } else if self.ui.agent.active().model.busy {
                        self.ui.agent.active_mut().model.abort()?;
                    }
                } else if symbol == 0xff1b {
                    self.ui.agent.active_mut().clear_selection();
                    self.ui.focus = Focus::Terminal;
                } else if matches!(symbol, 0xff52 | 0xff55 | 0xff54 | 0xff56) {
                    let step = if matches!(symbol, 0xff55 | 0xff56) {
                        10
                    } else {
                        1
                    };
                    self.ui.agent.active_mut().model.scroll = if matches!(symbol, 0xff52 | 0xff55) {
                        self.ui.agent.active().model.scroll.saturating_add(step)
                    } else {
                        self.ui.agent.active().model.scroll.saturating_sub(step)
                    };
                }
            } else if self.preedit.is_empty() && self.paste.is_none() {
                if control {
                    match letter {
                        Some('a') => self.ui.agent.active_mut().model.select_all(),
                        Some('c') => {
                            if let Some(range) = self.ui.agent.active().model.selection() {
                                self.window
                                    .selection(false, &self.ui.agent.active().model.input[range]);
                            } else {
                                self.ui.agent.active_mut().model.clear_input();
                            }
                        }
                        Some('x') => {
                            if let Some(range) = self.ui.agent.active().model.selection() {
                                self.window
                                    .selection(false, &self.ui.agent.active().model.input[range]);
                                self.ui.agent.active_mut().model.insert("").unwrap();
                            }
                        }
                        Some('v') => {
                            self.request_paste(PasteTarget::Agent(self.ui.agent.active().id), false)
                        }
                        Some('j') => {
                            if let Err(error) = self.ui.agent.active_mut().model.insert("\n") {
                                self.message(error);
                            }
                        }
                        _ => (),
                    }
                } else {
                    match symbol {
                        0xff1b => {
                            if self.ui.agent.active().context.menu {
                                self.ui.agent.active_mut().context.menu = false;
                            } else {
                                self.ui.focus = Focus::Terminal;
                            }
                        }
                        0xff0d | 0xff8d => {
                            if shift {
                                if let Err(error) = self.ui.agent.active_mut().model.insert("\n") {
                                    self.message(error);
                                }
                            } else {
                                self.send_agent();
                            }
                        }
                        0xff52 | 0xff54 => self.editor_vertical(symbol == 0xff52, shift),
                        _ => {
                            self.ui.agent.active_mut().model.key(symbol, shift);
                        }
                    }
                }
                self.ui.agent.active_mut().editor_scroll = None;
            }
            self.dirty = true;
            return Ok(());
        }
        if modifiers & (native::CONTROL | native::SHIFT) == native::CONTROL | native::SHIFT {
            match symbol_char(symbol).map(|c| c.to_ascii_lowercase()) {
                Some('c') => {
                    self.copy(pane, false)?;
                    return Ok(());
                }
                Some('v') => {
                    self.paste(pane, false);
                    return Ok(());
                }
                _ => (),
            }
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            if symbol == 0xff63 && modifiers == native::SHIFT {
                self.paste(pane, false);
                return Ok(());
            }
            return self.file_key(symbol, text);
        }
        if modifiers & (native::SHIFT | native::CONTROL | native::ALT) == native::SHIFT {
            let lines = self
                .client
                .screens()
                .get(&pane)
                .map_or(24, |s| s.screen_lines()) as i32;
            let scroll = match symbol {
                0xff55 => Some(Scroll::Delta(lines - 1)),
                0xff56 => Some(Scroll::Delta(1 - lines)),
                0xff50 => Some(Scroll::Top),
                0xff57 => Some(Scroll::Bottom),
                0xff63 => {
                    self.paste(pane, false);
                    return Ok(());
                }
                _ => None,
            };
            if let Some(scroll) = scroll {
                return self.action(Action::Scroll { pane, scroll });
            }
        }
        if self.preedit.is_empty()
            && let Some(data) = key_bytes(symbol, modifiers, text, || self.mode(pane))
        {
            self.input(pane, data)?;
        }
        Ok(())
    }
    #[cfg(feature = "agent")]
    fn agent_point(&self, clamp: bool) -> Option<crate::ui::TranscriptPoint> {
        let row = if clamp {
            self.chat_rows
                .iter()
                .rev()
                .find(|row| row.y <= self.pointer.1)
                .or_else(|| self.chat_rows.first())?
        } else {
            self.chat_rows
                .iter()
                .find(|row| (row.y..row.y + self.cell.1).contains(&self.pointer.1))?
        };
        let column = ((self.pointer.0 - row.x) / self.cell.0).max(0.) as usize;
        self.ui
            .agent
            .active()
            .point(row.line, column.min(row.columns))
    }

    #[cfg(feature = "agent")]
    fn agent_editor_reserved(&self) -> usize {
        3 * usize::from(self.ui.agent.active().model.busy)
    }

    #[cfg(feature = "agent")]
    fn editor_layout(&self) -> Option<EditorLayout> {
        let hit = self
            .control_hits
            .iter()
            .find(|hit| hit.action == GuiAction::Agent(AgentAction::Editor))?;
        let columns = (((hit.bounds.2 - 12.) / self.cell.0).floor().max(1.) as usize)
            .saturating_sub(self.agent_editor_reserved())
            .max(1);
        let editor = self.ui.agent.active().editor_layout(columns, 3);
        Some(EditorLayout {
            x: hit.bounds.0 + 6.,
            y: hit.bounds.1 + 6.,
            columns,
            editor,
        })
    }
    #[cfg(feature = "agent")]
    fn editor_pointer(&mut self, extend: bool) {
        if self.paste.is_some() {
            return;
        }
        let Some(mut layout) = self.editor_layout() else {
            return;
        };
        if extend {
            if self.pointer.1 < layout.y {
                layout.editor.first = layout.editor.first.saturating_sub(1);
            } else if self.pointer.1 >= layout.y + self.cell.1 * 3. {
                layout.editor.first =
                    (layout.editor.first + 1).min(layout.editor.lines.len().saturating_sub(3));
            }
        }
        self.ui.agent.active_mut().editor_scroll = Some(layout.editor.first);
        let row = ((self.pointer.1 - layout.y) / self.cell.1)
            .floor()
            .clamp(0., 2.) as usize;
        let range = layout
            .editor
            .lines
            .get(layout.editor.first + row)
            .cloned()
            .unwrap_or(
                self.ui.agent.active().model.input.len()..self.ui.agent.active().model.input.len(),
            );
        let column = ((self.pointer.0 - layout.x) / self.cell.0).clamp(0., layout.columns as f32);
        let offset = column_offset(&self.ui.agent.active().model.input, range, column);
        self.ui.agent.active_mut().model.set_cursor(offset, extend);
        self.dirty = true;
    }
    #[cfg(feature = "agent")]
    fn editor_vertical(&mut self, up: bool, extend: bool) {
        if let Some(layout) = self.editor_layout() {
            self.ui
                .agent
                .active_mut()
                .move_vertical(layout.editor, up, extend);
        }
    }
    fn copy(&mut self, pane: PaneId, primary: bool) -> Result<()> {
        let request = self.copy_request(primary);
        self.action(Action::CopySelection { pane, request })
    }

    fn copy_block(&mut self, pane: PaneId, block: Option<u64>) -> Result<()> {
        let block = block.or_else(|| {
            self.client
                .snapshot()?
                .panes
                .iter()
                .find(|p| p.id == pane)?
                .blocks
                .iter()
                .rev()
                .find(|b| b.output.is_some())
                .map(|b| b.id)
        });
        if let Some(block) = block {
            let request = self.copy_request(false);
            self.action(Action::CopyBlock {
                pane,
                block,
                request,
            })?;
        } else {
            self.message("No command output to copy");
        }
        Ok(())
    }
    fn paste(&mut self, pane: PaneId, primary: bool) {
        if !self.controller() {
            return;
        }
        if !self.preedit.is_empty() {
            self.message("Finish or cancel IME composition first");
            return;
        }
        let target = PasteTarget::Pane {
            pane,
            bracketed: self.mode(pane).contains(Mode::BRACKETED_PASTE),
        };
        #[cfg(feature = "files")]
        let target = if self.ui.files_open {
            if let Some(files) = &mut self.ui.files {
                files.focus_list();
                if !files.is_search() {
                    files.start_search();
                }
            }
            self.dirty = true;
            PasteTarget::Files
        } else {
            target
        };
        self.request_paste(target, primary);
    }
    fn request_paste(&mut self, target: PasteTarget, primary: bool) {
        if self.paste.is_some() {
            self.deferred
                .push_back(DeferredInput::Paste(target, primary));
        } else {
            self.paste = Some(target);
            self.window.paste(primary);
        }
    }
    fn copy_request(&mut self, primary: bool) -> u64 {
        // The destination survives async replies without retaining failed requests.
        let request = self.next_copy * 2 + u64::from(primary);
        self.next_copy += 1;
        request
    }
    fn pasted(&mut self, text: String) -> Result<()> {
        let Some(target) = self.paste.take() else {
            return Ok(());
        };
        match target {
            PasteTarget::Search => {
                self.ui.search.edit(&mut self.client, &text)?;
                self.dirty = true;
            }
            #[cfg(feature = "monitor")]
            PasteTarget::Monitor => {
                self.ui.resources.insert_query(&text);
                self.dirty = true;
            }
            #[cfg(feature = "agent")]
            PasteTarget::Agent(id) => {
                if let Err(error) = self.ui.agent.paste(id, &text) {
                    self.message(error);
                }
                self.dirty = true;
            }
            PasteTarget::Pane { pane, bracketed } => self.action(Action::Input {
                pane,
                data: paste_bytes(text, bracketed),
            })?,
            #[cfg(feature = "files")]
            PasteTarget::Files => {
                if let Some(files) = &mut self.ui.files {
                    files.insert_query(&text);
                }
                self.dirty = true;
            }
        }
        while let Some(input) = self.deferred.pop_front() {
            match input {
                DeferredInput::SearchText(text) => {
                    self.ui.search.edit(&mut self.client, &text)?;
                    self.dirty = true;
                }
                #[cfg(feature = "monitor")]
                DeferredInput::MonitorText(text) => {
                    self.ui.resources.insert_query(&text);
                    self.dirty = true;
                }
                #[cfg(feature = "agent")]
                DeferredInput::AgentText(id, text) => {
                    if let Err(error) = self.ui.agent.paste(id, &text) {
                        self.message(error);
                    }
                    self.dirty = true;
                }
                DeferredInput::Bytes(pane, data) => self.action(Action::Input { pane, data })?,
                DeferredInput::Paste(target, primary) => {
                    self.request_paste(target, primary);
                    break;
                }
                #[cfg(feature = "files")]
                DeferredInput::FileText(text) => {
                    if let Some(files) = &mut self.ui.files {
                        files.insert_query(&text);
                    }
                    self.dirty = true;
                }
            }
        }
        Ok(())
    }
    fn layout(&self) -> GuiLayout {
        let (width, height, _) = self.window.size();
        #[cfg(feature = "agent")]
        let agent_width = self.ui.agent.open.then_some(self.agent_width);
        #[cfg(not(feature = "agent"))]
        let agent_width = None;
        let mut layout = GuiLayout::new(width as f32, height as f32, self.cell, agent_width);
        #[cfg(not(feature = "monitor"))]
        {
            layout.footer = height as f32;
        }
        #[cfg(feature = "monitor")]
        if self.ui.resources.open {
            let total = ((layout.height - layout.top - 12.) / self.cell.1).max(0.) as u16;
            layout.footer -= f32::from(self.ui.resources.rows(total)) * self.cell.1 + 6.;
        }
        layout
    }
    fn preview_open(&self) -> bool {
        #[cfg(feature = "agent")]
        {
            self.ui.agent.active().context.preview.is_some()
        }
        #[cfg(not(feature = "agent"))]
        {
            false
        }
    }
    fn scrollbar_visible(&self, target: ScrollTarget) -> bool {
        #[cfg(feature = "agent")]
        {
            self.preview_open() == (target == ScrollTarget::Preview)
        }
        #[cfg(not(feature = "agent"))]
        {
            let _ = target;
            true
        }
    }
    fn hit(&self) -> Option<(PaneId, Point, Side)> {
        #[cfg(feature = "agent")]
        if self.ui.agent.active().context.preview.is_some() {
            return None;
        }
        let layout = self.layout();
        if !layout.terminal_contains(self.pointer) {
            return None;
        }
        let (x, y) = self.pointer_cell(self.pointer);
        let pane = self
            .client
            .snapshot()?
            .panes
            .iter()
            .find(|p| p.rect.contains(x, y))?;
        let (point, side) = self.position_in(pane.id)?;
        Some((pane.id, point, side))
    }
    fn position_in(&self, pane: PaneId) -> Option<(Point, Side)> {
        let pane = self
            .client
            .snapshot()?
            .panes
            .iter()
            .find(|p| p.id == pane)?;
        let rect = pane.rect;
        let screen = self.client.screens().get(&pane.id)?;
        let (ox, oy) = self.layout().origin(self.cell);
        let row = ((self.pointer.1 - oy) / self.cell.1 - f32::from(rect.y))
            .floor()
            .clamp(0., f32::from(rect.rows - 1)) as usize;
        let x = (self.pointer.0 - ox) / self.cell.0 - f32::from(rect.x);
        Some((
            Point {
                line: row as i32 - screen.display_offset() as i32,
                column: x
                    .floor()
                    .clamp(0., screen.columns().saturating_sub(1) as f32)
                    as usize,
            },
            if x < screen.columns() as f32 && x.fract() < 0.5 {
                Side::Left
            } else {
                Side::Right
            },
        ))
    }
    fn refresh_hover(&mut self) {
        #[cfg(feature = "monitor")]
        {
            let layout = self.layout();
            let top = layout.footer + 6.;
            let pointer = (self.ui.resources.open
                && !self.preview_open()
                && self.closing.is_none()
                && (PADDING..layout.width - PADDING).contains(&self.pointer.0)
                && (top..layout.height - self.cell.1 - 12.).contains(&self.pointer.1))
            .then(|| {
                (
                    ((self.pointer.0 - PADDING) / self.cell.0).floor() as u16,
                    ((self.pointer.1 - top) / self.cell.1).floor() as i32,
                )
            });
            if self.ui.resources.pointer != pointer {
                self.ui.resources.pointer = pointer;
                self.dirty = true;
            }
        }
        #[cfg(feature = "monitor")]
        let monitor_divider = self.ui.resources.open
            && self.closing.is_none()
            && !self.preview_open()
            && (self.ui.resources.dragging || (self.pointer.1 - self.layout().footer).abs() <= 5.);
        #[cfg(feature = "agent")]
        {
            #[cfg(feature = "files")]
            let divider_allowed = !self.ui.files_open;
            #[cfg(not(feature = "files"))]
            let divider_allowed = true;
            let divider = divider_allowed
                && (self.agent_drag.is_some()
                    || (self.closing.is_none()
                        && self.ui.agent.active().context.preview.is_none()
                        && self.layout().divider_contains(self.pointer)));
            if divider != self.divider_hover {
                self.divider_hover = divider;
                self.dirty = true;
            }
            #[cfg(feature = "monitor")]
            if !monitor_divider {
                self.window.resize_cursor(divider);
            }
            #[cfg(not(feature = "monitor"))]
            self.window.resize_cursor(divider);
        }
        #[cfg(feature = "monitor")]
        {
            #[cfg(not(feature = "agent"))]
            self.window.vertical_resize_cursor(monitor_divider);
            #[cfg(feature = "agent")]
            if monitor_divider {
                self.window.vertical_resize_cursor(true);
            }
        }

        {
            let block = self.block_at_pointer();
            if block != self.hovered_block {
                self.hovered_block = block;
                self.dirty = true;
            }
        }
        let control = self
            .control_hits
            .iter()
            .rev()
            .find(|hit| hit.contains(self.pointer))
            .map(|hit| hit.action);
        if self.hovered_control != control {
            self.hovered_control = control;
            self.dirty = true;
        }
        let hit = self.hit().map(|(pane, point, _)| (pane, point));
        let changed_row = hit.is_some_and(|(pane, point)| {
            self.client.screens().get(&pane).is_some_and(|s| {
                let range = self
                    .hovered
                    .as_ref()
                    .filter(|(id, _)| *id == pane)
                    .map(|(_, object)| object.range);
                let start =
                    range.map_or(point.line, |range| range.start.line) + s.display_offset() as i32;
                let end =
                    range.map_or(point.line, |range| range.end.line) + s.display_offset() as i32;
                (start..=end).any(|row| s.dirty.get(row as usize).copied().unwrap_or(false))
            })
        });
        if hit == self.hover_cell && !changed_row {
            return;
        }
        self.hover_cell = hit;
        let hovered = hit.and_then(|(pane, point)| {
            context::object_at(self.client.screens().get(&pane)?, point)
                .map(|object| (pane, object))
        });
        if self.hovered != hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
    }
    #[cfg(feature = "agent")]
    fn open_object(&mut self, pane: PaneId, object: Object) -> Result<()> {
        if !self.preedit.is_empty() {
            self.message("Finish or cancel IME composition first");
            return Ok(());
        }
        if let Err(error) = self
            .ui
            .agent
            .active_mut()
            .attach_object(&self.client, pane, object)
        {
            self.message(error);
        }
        if !self.ui.agent.open {
            self.ui.agent.open = true;
            self.resize()?;
        }
        self.ui.focus = Focus::Editor;
        self.selecting = None;
        self.dirty = true;
        Ok(())
    }
    fn gui_action(&mut self, action: GuiAction) -> Result<()> {
        #[cfg(feature = "agent")]
        if !self.preedit.is_empty()
            && matches!(action, GuiAction::Agent(action) if !matches!(
                action,
                AgentAction::Stop
                    | AgentAction::Remove(_)
                    | AgentAction::ClosePreview
                    | AgentAction::CopyPreview
            ))
        {
            self.message("Finish or cancel IME composition first");
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if action != GuiAction::Agent(AgentAction::ContextMenu) {
            self.ui.agent.active_mut().context.menu = false;
        }
        match action {
            #[cfg(feature = "monitor")]
            GuiAction::Resources => {
                #[cfg(feature = "files")]
                {
                    self.ui.files_open = false;
                }
                self.ui.toggle_monitor();
                self.resize()?;
            }
            #[cfg(feature = "monitor")]
            GuiAction::Monitor(action) => {
                let open = self.ui.resources.open;
                self.ui.resources.action(action);
                self.ui.focus = if self.ui.resources.open {
                    Focus::Monitor
                } else {
                    Focus::Terminal
                };
                if open != self.ui.resources.open {
                    self.resize()?;
                }
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::ContextMenu) => {
                self.ui.agent.active_mut().rewind_target = None;
                self.ui.agent.active_mut().context.menu = !self.ui.agent.active().context.menu;
                self.ui.focus = Focus::Editor;
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Inspect(index)) => {
                if let Some(item) = self.ui.agent.active().context.basket.get(index).cloned() {
                    self.ui.agent.active_mut().context.preview = Some((item, 0));
                }
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::ClosePreview) => {
                self.ui.agent.active_mut().context.preview = None
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::CopyPreview) => {
                if let Some((item, _)) = &self.ui.agent.active().context.preview {
                    self.window.selection(false, &item.snapshot.text);
                    self.message("Copied");
                }
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Copy(index)) => {
                self.ui.focus = Focus::Output;
                if let Some(message) = self.ui.agent.active().model.messages.get(index) {
                    self.window.selection(false, &message.text);
                    self.message("Copied");
                }
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Tool(index)) => {
                self.ui.focus = Focus::Output;
                self.ui.agent.active_mut().toggle_tool(index);
            }
            GuiAction::CloseTab(tab) => {
                if !self.preedit.is_empty() {
                    self.message("Finish or cancel IME composition first");
                } else if self.controller() {
                    self.action(Action::CloseTab(tab))?;
                }
            }
            GuiAction::Tab(direction) => {
                if self.controller() {
                    self.ui.focus = Focus::Terminal;
                    self.action(Action::Tab(direction))?;
                }
            }
            GuiAction::DismissHelp => self.ui.help = false,
            GuiAction::NewTab => {
                if !self.preedit.is_empty() {
                    self.message("Finish or cancel IME composition first");
                } else if self.controller()
                    && let Some(pane) = self.active()
                {
                    self.action(Action::NewTab { pane })?;
                }
            }
            GuiAction::Minimize => self.window.minimize(),
            GuiAction::Maximize => self.window.maximize(!self.maximized),
            GuiAction::Close => self.open_close(),
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Editor) => {
                self.ui.agent.active_mut().rewind_target = None;
                self.ui.focus = Focus::Editor;
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Stop) => self.ui.agent.active_mut().model.abort()?,
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Selection | AgentAction::Screen) => {
                let Some(pane) = self.active() else {
                    return Ok(());
                };
                if let Err(error) = self.ui.agent.active_mut().attach_screen(
                    &self.client,
                    pane,
                    matches!(action, GuiAction::Agent(AgentAction::Selection)),
                ) {
                    self.message(error);
                    return Ok(());
                }
                self.ui.focus = Focus::Editor;
                if !self.ui.agent.open {
                    self.ui.agent.open = true;
                    self.resize()?;
                }
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(AgentAction::Remove(index)) => {
                self.ui.agent.active_mut().context.basket.remove(index);
            }
            #[cfg(feature = "agent")]
            GuiAction::Agent(action) => {
                match self.ui.activate_agent(action, &self.client) {
                    Ok(Some(copy)) => self.window.selection(false, &copy),
                    Ok(None) => (),
                    Err(error) => self.message(error.to_string()),
                }
                self.selecting_editor = false;
                self.agent_tool_click = None;
                self.chat_rows.clear();
            }
        }
        self.control_hits.clear();
        self.hovered_control = None;
        self.dirty = true;
        Ok(())
    }

    fn block_at_pointer(&self) -> Option<(PaneId, u64)> {
        if self.closing.is_some() {
            return None;
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            return None;
        }
        let (id, point, _) = self.hit()?;
        let pane = self
            .client
            .snapshot()?
            .panes
            .iter()
            .find(|pane| pane.id == id)?;
        let screen = self.client.screens().get(&id)?;
        let row = (point.line + screen.display_offset() as i32) as usize;
        screen.row_output(pane, row).map(|block| (id, block.id))
    }
    fn mouse(
        &mut self,
        pane: PaneId,
        point: Point,
        button: u8,
        release: bool,
        modifiers: u32,
        count: usize,
    ) -> Result<()> {
        let mode = self.mode(pane);
        let y = (point.line + self.client.screens()[&pane].display_offset() as i32).max(0) as usize;
        let code = button
            + u8::from(modifiers & native::SHIFT != 0) * 4
            + u8::from(modifiers & native::ALT != 0) * 8
            + u8::from(modifiers & native::CONTROL != 0) * 16;
        if let Some(data) = mouse_bytes(u16::from(code), point.column, y, release, mode) {
            let data = if count == 1 { data } else { data.repeat(count) };
            self.input(pane, data)?;
        }
        Ok(())
    }
    fn button(&mut self, button: u32, pressed: bool, modifiers: u32) -> Result<()> {
        if self.ui.help {
            if button == 1
                && pressed
                && self
                    .control_hits
                    .iter()
                    .any(|hit| hit.action == GuiAction::DismissHelp && hit.contains(self.pointer))
            {
                self.ui.help = false;
                self.dirty = true;
            }
            return Ok(());
        }

        if !pressed {
            #[cfg(feature = "agent")]
            if button == 1 && self.ui.agent.active().selecting() {
                if let Some(point) = self.agent_point(true) {
                    self.ui.agent.active_mut().extend_selection(point);
                }
                self.ui.agent.active_mut().end_selection();
                if let Some(text) = self.ui.agent.active().selected_text() {
                    self.window.selection(true, &text);
                } else if let Some(index) = self.agent_tool_click.take() {
                    self.ui.agent.active_mut().toggle_tool(index);
                }
                self.agent_tool_click = None;
                self.dirty = true;
                return Ok(());
            }
            #[cfg(feature = "monitor")]
            if button == 1 && std::mem::take(&mut self.ui.resources.dragging) {
                self.refresh_hover();
                return Ok(());
            }
            #[cfg(feature = "agent")]
            if button == 1 && std::mem::take(&mut self.selecting_editor) {
                if let Some(range) = self.ui.agent.active().model.selection() {
                    self.window
                        .selection(true, &self.ui.agent.active().model.input[range]);
                }
                self.dirty = true;
                return Ok(());
            }
            if button == 1 && self.scrollbar_drag.take().is_some() {
                self.dirty = true;
                return Ok(());
            }
            #[cfg(feature = "agent")]
            if button == 1 && self.agent_drag.take().is_some() {
                return Ok(());
            }
            if button == 1 && self.split_drag.take().is_some() {
                return Ok(());
            }
            if button == 1
                && let Some(drag) = self.selecting.take()
            {
                if let Some((point, side)) = self.position_in(drag.pane) {
                    self.action(Action::SelectionUpdate {
                        pane: drag.pane,
                        point,
                        side,
                    })?;
                    if drag.kind != SelectionType::Simple
                        || point != drag.point
                        || side != drag.side
                    {
                        self.copy(drag.pane, true)?;
                    }
                }
                return Ok(());
            }
            if (1..=3).contains(&button) && self.reported_buttons & (1 << (button - 1)) != 0 {
                self.reported_buttons &= !(1 << (button - 1));
                if let Some(pane) = self.reported_pane
                    && let Some((point, _)) = self.position_in(pane)
                {
                    self.mouse(pane, point, (button - 1) as u8, true, modifiers, 1)?;
                }
                if self.reported_buttons == 0 {
                    self.reported_pane = None;
                }
            }
            return Ok(());
        }
        if self.closing.is_some() {
            if button == 1 {
                let (width, height, _) = self.window.size();
                let rects = close_layout(width as f32, height as f32, self.cell);
                for (choice, &(x, y, w, h)) in [CloseChoice::Terminate, CloseChoice::Background]
                    .into_iter()
                    .zip(&rects[1..])
                {
                    if self.pointer.0 >= x
                        && self.pointer.0 < x + w
                        && self.pointer.1 >= y
                        && self.pointer.1 < y + h
                    {
                        self.choose_close(choice)?;
                        break;
                    }
                }
            }
            return Ok(());
        }
        if self.pointer.1 < self.layout().top
            && let Some(action) = self
                .control_hits
                .iter()
                .find(|hit| {
                    hit.contains(self.pointer)
                        && matches!(
                            hit.action,
                            GuiAction::Minimize | GuiAction::Maximize | GuiAction::Close
                        )
                })
                .map(|hit| hit.action)
        {
            if button == 1 {
                self.gui_action(action)?;
            }
            return Ok(());
        }
        if button == 1
            && let Some(bar) = self
                .scrollbars
                .iter()
                .copied()
                .rev()
                .find(|bar| bar.contains(self.pointer) && (self.scrollbar_visible(bar.target)))
        {
            let (top, height) = bar.thumb();
            let grab = if (top..top + height).contains(&self.pointer.1) {
                self.pointer.1 - top
            } else {
                height * 0.5
            };
            self.scrollbar_drag = Some(ScrollbarDrag {
                bar,
                grab,
                requested: None,
            });
            self.drag_scrollbar(self.pointer.1)?;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.active().context.preview.is_some() {
            if button == 1
                && let Some(action) = self
                    .control_hits
                    .iter()
                    .rev()
                    .find(|hit| {
                        hit.contains(self.pointer)
                            && matches!(
                                hit.action,
                                GuiAction::Agent(
                                    AgentAction::ClosePreview | AgentAction::CopyPreview
                                )
                            )
                    })
                    .map(|hit| hit.action)
            {
                self.gui_action(action)?;
            }
            return Ok(());
        }
        #[cfg(feature = "monitor")]
        {
            let layout = self.layout();
            if self.ui.resources.open && (self.pointer.1 - layout.footer).abs() <= 5. && button == 1
            {
                self.ui.resources.dragging = true;
                self.ui.focus = Focus::Monitor;
                self.refresh_hover();
                return Ok(());
            }
            if self.pointer.1 >= layout.footer {
                if let Some(action) = self
                    .control_hits
                    .iter()
                    .rev()
                    .find(|hit| hit.contains(self.pointer))
                    .map(|hit| hit.action)
                {
                    if button == 1 {
                        self.gui_action(action)?;
                    } else if button == 2
                        && action == GuiAction::Monitor(crate::monitor::Action::Search)
                    {
                        self.gui_action(action)?;
                        self.request_paste(PasteTarget::Monitor, true);
                    }
                }
                return Ok(());
            }
            self.ui.focus = Focus::Terminal;
            self.ui.resources.searching = false;
        }
        #[cfg(feature = "agent")]
        let layout = self.layout();
        #[cfg(feature = "agent")]
        if button == 1 && layout.divider_contains(self.pointer) {
            if self.preedit.is_empty() {
                self.agent_drag = Some(self.pointer.0 - layout.side);
            } else {
                self.message("Finish or cancel IME composition first");
            }
            return Ok(());
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            self.ui.focus = Focus::Files;
            let layout = self.file_layout();
            if button == 1 && self.pointer.1 >= layout.top && self.pointer.1 < layout.y + layout.h {
                if self.pointer.0 >= layout.split && self.pointer.0 < layout.x + layout.w {
                    if let Some(files) = &mut self.ui.files {
                        files.focus_preview();
                    }
                } else if self.pointer.0 >= layout.x
                    && self.pointer.0 < layout.split
                    && let Some(files) = &mut self.ui.files
                {
                    files.focus_list();
                    if let Some((_, _, index)) = self
                        .file_hits
                        .iter()
                        .find(|(a, b, _)| self.pointer.1 >= *a && self.pointer.1 < *b)
                        .copied()
                    {
                        files.move_selection(index as isize - files.selection() as isize);
                    }
                }
                self.dirty = true;
            }
            return Ok(());
        }
        if let Some(action) = self
            .control_hits
            .iter()
            .rev()
            .find(|hit| hit.contains(self.pointer))
            .map(|hit| hit.action)
        {
            match (button, action) {
                #[cfg(feature = "agent")]
                (3, GuiAction::Agent(AgentAction::Copy(index) | AgentAction::Tool(index))) => {
                    if let Some(text) = self.ui.agent.active().selected_text() {
                        self.window.selection(false, &text);
                        self.message("Copied");
                    } else {
                        self.gui_action(GuiAction::Agent(AgentAction::Copy(index)))?;
                    }
                }
                #[cfg(feature = "agent")]
                (3, GuiAction::Agent(AgentAction::Editor)) => {
                    self.gui_action(GuiAction::Agent(AgentAction::ContextMenu))?
                }
                #[cfg(feature = "agent")]
                (1, GuiAction::Agent(AgentAction::Copy(_) | AgentAction::Tool(_))) => {
                    if self.preedit.is_empty() {
                        self.ui.focus = Focus::Output;
                        if let Some(point) = self.agent_point(false) {
                            self.ui
                                .agent
                                .active_mut()
                                .begin_selection(point, modifiers & native::SHIFT != 0);
                            self.agent_tool_click = match action {
                                GuiAction::Agent(AgentAction::Tool(index)) => Some(index),
                                _ => None,
                            };
                        }
                        self.dirty = true;
                    } else {
                        self.message("Finish or cancel IME composition first");
                    }
                }
                #[cfg(feature = "agent")]
                (1 | 2, GuiAction::Agent(AgentAction::Editor)) => {
                    if self.preedit.is_empty() {
                        self.ui.focus = Focus::Editor;
                        self.ui.agent.active_mut().rewind_target = None;
                        self.ui.agent.active_mut().context.menu = false;
                        self.editor_pointer(modifiers & native::SHIFT != 0);
                        self.selecting_editor = button == 1;
                        if button == 2 {
                            self.request_paste(PasteTarget::Agent(self.ui.agent.active().id), true);
                        }
                    } else {
                        self.message("Finish or cancel IME composition first");
                    }
                }
                (1, action) => self.gui_action(action)?,
                _ => (),
            }
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.active().context.menu {
            self.ui.agent.active_mut().context.menu = false;
            self.dirty = true;
        }
        #[cfg(feature = "agent")]
        if self.layout().terminal_contains(self.pointer) && self.ui.focus != Focus::Terminal {
            if !self.preedit.is_empty() {
                self.message("Finish or cancel IME composition first");
                return Ok(());
            }
            self.ui.focus = Focus::Terminal;
            self.ui.agent.active_mut().rewind_target = None;
            self.dirty = true;
        }
        #[cfg(feature = "agent")]
        if button == 1
            && self.ui.agent.open
            && self.pointer.0 >= self.layout().side
            && (self.layout().top..self.layout().footer).contains(&self.pointer.1)
        {
            if self.preedit.is_empty() {
                self.ui.focus = Focus::Output;
                self.ui.agent.active_mut().clear_selection();
                self.dirty = true;
            } else {
                self.message("Finish or cancel IME composition first");
            }
        }
        if self.pointer.1 >= self.layout().footer
            || self.pointer.0 >= self.layout().side && self.pointer.1 >= self.layout().top
        {
            return Ok(());
        }
        if button == 1
            && let Some((_, _, index)) = self
                .tab_hits
                .iter()
                .find(|(a, b, _)| {
                    self.pointer.1 < self.layout().top
                        && self.pointer.0 >= *a
                        && self.pointer.0 < *b
                })
                .copied()
        {
            if !self.preedit.is_empty() {
                self.message("Finish or cancel IME composition first");
                return Ok(());
            }
            if let Some(s) = self.client.snapshot() {
                let active = s
                    .tabs
                    .iter()
                    .position(|t| t.id == s.active_tab)
                    .unwrap_or(0);
                self.action(Action::Tab(index as i32 - active as i32))?;
            }
            return Ok(());
        }
        if button == 1
            && self.layout().terminal_contains(self.pointer)
            && self.controller()
            && self.preedit.is_empty()
        {
            let (column, row) = self.pointer_cell(self.pointer);
            if let Some(drag) = self
                .client
                .snapshot()
                .and_then(|s| s.divider_at(column, row))
            {
                self.split_drag = Some(drag);
                return Ok(());
            }
        }
        if button == 3
            && (modifiers & native::SHIFT != 0
                || self
                    .hit()
                    .is_some_and(|(pane, _, _)| !self.mode(pane).intersects(Mode::MOUSE_MODE)))
        {
            if let Some(pane) = self
                .client
                .snapshot()
                .and_then(|snapshot| {
                    snapshot.panes.iter().find(|pane| {
                        self.client
                            .screens()
                            .get(&pane.id)
                            .is_some_and(|screen| screen.selection_range().is_some())
                    })
                })
                .map(|pane| pane.id)
            {
                return self.copy(pane, false);
            }
            if let Some((_, object)) = &self.hovered {
                self.window.selection(false, &object.text);
                self.message("Object copied");
                return Ok(());
            }

            if let Some((pane, block)) = self.block_at_pointer() {
                return self.copy_block(pane, Some(block));
            }
        }
        if !self.controller() {
            return Ok(());
        }
        let hit = self
            .reported_pane
            .and_then(|pane| {
                self.position_in(pane)
                    .map(|(point, side)| (pane, point, side))
            })
            .or_else(|| self.hit());
        let Some((pane, point, side)) = hit else {
            return Ok(());
        };
        if self.active() != Some(pane) {
            if !self.preedit.is_empty() {
                return Ok(());
            }
            self.action(Action::Focus(pane))?;
        }
        let mode = self.mode(pane);
        if mode.intersects(Mode::MOUSE_MODE)
            && modifiers & native::SHIFT == 0
            && (1..=3).contains(&button)
        {
            let code = (button - 1) as u8;
            self.reported_buttons |= 1 << code;
            self.reported_pane = Some(pane);
            self.mouse(pane, point, code, false, modifiers, 1)?;
            return Ok(());
        }
        if button == 2 {
            self.paste(pane, true);
        } else if button == 1 {
            let now = Instant::now();
            let count = self
                .last_click
                .filter(|(t, p, q, _)| {
                    now.duration_since(*t) < Duration::from_millis(500) && *p == pane && *q == point
                })
                .map_or(1, |(_, _, _, n)| n % 3 + 1);
            self.last_click = Some((now, pane, point, count));
            #[cfg(feature = "agent")]
            if count == 2
                && modifiers & native::CONTROL == 0
                && let Some(object) = context::object_at(&self.client.screens()[&pane], point)
            {
                return self.open_object(pane, object);
            }
            let kind = match count {
                2 if modifiers & native::CONTROL != 0 => SelectionType::Object,
                2 => SelectionType::Semantic,
                3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            self.selecting = Some(SelectionDrag {
                pane,
                point,
                side,
                kind,
            });
            self.action(Action::SelectionStart {
                pane,
                point,
                side,
                kind,
            })?;
        }
        Ok(())
    }
    fn pointer_cell(&self, pointer: (f32, f32)) -> (u16, u16) {
        let layout = self.layout();
        let (ox, oy) = layout.origin(self.cell);
        let size = layout.size(self.cell);
        (
            (((pointer.0 - ox) / self.cell.0).max(0.) as u16).min(size.num_cols - 1),
            (((pointer.1 - oy) / self.cell.1).max(0.) as u16).min(size.num_lines - 1),
        )
    }
    fn motion(&mut self, pointer: (f32, f32), modifiers: u32) -> Result<()> {
        let previous = std::mem::replace(&mut self.pointer, pointer);
        let header_tab = |point: (f32, f32)| {
            (point.1 >= 0. && point.1 < self.layout().top)
                .then(|| {
                    self.tab_hits
                        .iter()
                        .find(|(left, right, _)| point.0 >= *left && point.0 < *right)
                        .map(|(_, _, index)| *index)
                })
                .flatten()
        };
        if header_tab(previous) != header_tab(pointer) {
            self.dirty = true;
        }
        let hovered_bar = |point| self.scrollbars.iter().position(|bar| bar.contains(point));
        if hovered_bar(previous) != hovered_bar(pointer) {
            self.dirty = true;
        }
        if self.closing.is_some() {
            self.dirty |= previous != pointer;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.open
            && pointer.0 >= self.layout().side
            && (previous.1 / self.cell.1).floor() != (pointer.1 / self.cell.1).floor()
        {
            self.dirty = true;
        }
        #[cfg(feature = "monitor")]
        if self.ui.resources.dragging {
            let layout = self.layout();
            let bottom = layout.height - self.cell.1 - 12.;
            let total = ((layout.height - layout.top - 12.) / self.cell.1).max(0.) as u16;
            let rows = ((bottom - pointer.1) / self.cell.1).round().max(0.) as u16;
            let old = self.ui.resources.height;
            self.ui.resources.resize(rows, total);
            if self.ui.resources.height != old {
                self.resize()?;
            }
            self.refresh_hover();
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.active().selecting() {
            if let Some(point) = self.agent_point(true) {
                self.ui.agent.active_mut().extend_selection(point);
            }
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.selecting_editor {
            self.editor_pointer(true);
            return Ok(());
        }
        if self.scrollbar_drag.is_some() {
            self.drag_scrollbar(pointer.1)?;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.active().context.preview.is_some() {
            self.refresh_hover();
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if let Some(grab) = self.agent_drag {
            let (width, _, _) = self.window.size();
            let minimum = self.cell.0 * 24. + PADDING * 2.;
            let agent_width = (width as f32 - self.pointer.0 + grab)
                .clamp(minimum, (width as f32 * 0.62).max(minimum));
            if agent_width != self.agent_width {
                let previous = self.layout().size(self.cell);
                self.agent_width = agent_width;
                if self.layout().size(self.cell) != previous {
                    self.resize()?;
                }
                self.dirty = true;
            }
            return Ok(());
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            return Ok(());
        }
        if let Some(drag) = self.split_drag {
            let (column, row) = self.pointer_cell(pointer);
            let (old_column, old_row) = self.pointer_cell(previous);
            if match drag.axis {
                Axis::LeftRight => column != old_column,
                Axis::TopBottom => row != old_row,
            } {
                self.action(drag.action(column, row))?;
            }
            return Ok(());
        }
        self.refresh_hover();
        let captured = self.selecting.map(|drag| drag.pane).or(self.reported_pane);
        let hit = captured
            .and_then(|pane| {
                self.position_in(pane)
                    .map(|(point, side)| (pane, point, side))
            })
            .or_else(|| self.hit());
        if let Some((pane, point, side)) = hit {
            if self.selecting.is_some_and(|drag| drag.pane == pane) {
                self.action(Action::SelectionUpdate { pane, point, side })?;
            } else {
                let mode = self.mode(pane);
                if modifiers & native::SHIFT == 0
                    && (mode.contains(Mode::MOUSE_MOTION)
                        || mode.contains(Mode::MOUSE_DRAG) && self.reported_buttons != 0)
                {
                    let button = if self.reported_buttons == 0 {
                        3
                    } else {
                        self.reported_buttons.trailing_zeros() as u8
                    };
                    self.mouse(pane, point, 32 + button, false, modifiers, 1)?;
                }
            }
        }
        Ok(())
    }
    fn drag_scrollbar(&mut self, y: f32) -> Result<()> {
        let Some(drag) = &mut self.scrollbar_drag else {
            return Ok(());
        };
        if let Some(current) = self
            .scrollbars
            .iter()
            .find(|bar| bar.target == drag.bar.target)
        {
            drag.bar.total = current.total;
            drag.bar.visible = current.visible;
        }
        let offset = drag.bar.offset_at(y, drag.grab);
        let target = drag.bar.target;
        let changed = drag.requested != Some(offset);
        drag.requested = Some(offset);
        match target {
            ScrollTarget::Pane(pane) if changed => {
                if offset == drag.bar.maximum() {
                    self.action(Action::Scroll {
                        pane,
                        scroll: Scroll::Top,
                    })?;
                } else {
                    self.client.scroll_to(pane, offset)?;
                }
            }
            ScrollTarget::Pane(_) => return Ok(()),
            #[cfg(feature = "agent")]
            ScrollTarget::Agent => self.ui.agent.active_mut().model.scroll = offset,
            #[cfg(feature = "agent")]
            ScrollTarget::Editor => {
                self.ui.agent.active_mut().editor_scroll = Some(drag.bar.maximum() - offset)
            }
            #[cfg(feature = "agent")]
            ScrollTarget::Preview => {
                if let Some((_, scroll)) = &mut self.ui.agent.active_mut().context.preview {
                    *scroll = drag.bar.maximum() - offset;
                }
            }
        }
        self.dirty = true;
        Ok(())
    }
    fn scroll(&mut self, amount: f32, modifiers: u32) -> Result<()> {
        if self.ui.help {
            self.ui.help_scroll = if amount > 0. {
                self.ui.help_scroll.saturating_sub(3)
            } else {
                self.ui.help_scroll.saturating_add(3)
            };
            self.dirty = true;
            return Ok(());
        }

        if self.scrollbar_drag.is_some() {
            return Ok(());
        }
        #[cfg(feature = "agent")]
        if let Some((_, scroll)) = &mut self.ui.agent.active_mut().context.preview {
            *scroll = if amount > 0. {
                scroll.saturating_sub(3)
            } else {
                scroll.saturating_add(3)
            };
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "monitor")]
        if self.ui.resources.open && self.pointer.1 >= self.layout().footer {
            self.ui.resources.scroll = if amount > 0. {
                self.ui.resources.scroll.saturating_sub(3)
            } else {
                self.ui.resources.scroll.saturating_add(3)
            };
            self.dirty = true;
            return Ok(());
        }
        #[cfg(feature = "agent")]
        let layout = self.layout();
        #[cfg(feature = "agent")]
        if self.ui.agent.open && self.pointer.0 >= layout.side {
            if self.control_hits.iter().any(|hit| {
                hit.action == GuiAction::Agent(AgentAction::Editor) && hit.contains(self.pointer)
            }) {
                if let Some(layout) = self.editor_layout() {
                    let editor = layout.editor;
                    self.ui.agent.active_mut().editor_scroll = Some(if amount > 0. {
                        editor.first.saturating_sub(3)
                    } else {
                        editor
                            .first
                            .saturating_add(3)
                            .min(editor.lines.len().saturating_sub(3))
                    });
                }
                self.dirty = true;
                return Ok(());
            }
            self.ui.agent.active_mut().model.scroll = if amount > 0. {
                self.ui.agent.active().model.scroll.saturating_add(3)
            } else {
                self.ui.agent.active().model.scroll.saturating_sub(3)
            };
            self.dirty = true;
            return Ok(());
        }
        if self.closing.is_some() {
            return Ok(());
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            let layout = self.file_layout();
            if let Some(files) = &mut self.ui.files {
                if self.pointer.0 >= layout.split && self.pointer.0 < layout.x + layout.w {
                    files.focus_preview();
                    files.scroll_preview(-amount.round() as isize, layout.rows);
                } else {
                    files.focus_list();
                    files.move_selection(-amount.round() as isize);
                }
            }
            self.dirty = true;
            return Ok(());
        }
        let Some((pane, point, _)) = self.hit() else {
            return Ok(());
        };
        self.scroll_remainder += amount;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return Ok(());
        }
        let mode = self.mode(pane);
        if mode.intersects(Mode::MOUSE_MODE) && modifiers & native::SHIFT == 0 {
            self.mouse(
                pane,
                point,
                if lines > 0 { 64 } else { 65 },
                false,
                modifiers,
                lines.unsigned_abs() as usize,
            )?;
        } else if mode.contains(Mode::ALT_SCREEN)
            && mode.contains(Mode::ALTERNATE_SCROLL)
            && modifiers & native::SHIFT == 0
        {
            let bytes = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            self.input(pane, bytes.repeat(lines.unsigned_abs() as usize * 3))?;
        } else {
            self.action(Action::Scroll {
                pane,
                scroll: Scroll::Delta(lines.saturating_mul(3)),
            })?;
        }
        Ok(())
    }
    fn draw(&mut self) -> Result<()> {
        self.text.used = 0;
        let begin = self.trace.as_ref().map(|_| monotonic());
        let (width, height, scale) = self.window.size();
        if scale != self.scale {
            self.scale = scale;
            self.caches.clear();
            self.text.lines.clear();
        }
        let background =
            (self.config.background << 8) | (self.config.background_opacity * 255.).round() as u32;
        self.renderer.begin(background)?;
        let layout = self.layout();
        let (grid_x, grid_y) = layout.origin(self.cell);
        self.control_hits.clear();
        self.scrollbars.clear();
        self.chrome(layout)?;
        // The tab scissor rounds outward at fractional scale; repaint the content edge.
        self.renderer
            .clip(0., layout.top, layout.side, layout.footer - layout.top)?;
        self.renderer.clear(background)?;
        self.renderer.clip(0., 0., 0., 0.)?;
        let terminal_input = self.closing.is_none()
            && !self.ui.help
            && !self.ui.search.open
            && self.ui.focus == Focus::Terminal;
        if let Some((snapshot, screens)) = self.client.snapshot_and_screens_mut() {
            let right = snapshot
                .panes
                .iter()
                .map(|pane| pane.rect.x + pane.rect.cols)
                .max()
                .unwrap_or(0);
            let bottom = snapshot
                .panes
                .iter()
                .map(|pane| pane.rect.y + pane.rect.rows)
                .max()
                .unwrap_or(0);
            self.caches
                .retain(|id, _| snapshot.panes.iter().any(|pane| pane.id == *id));
            for pane in &snapshot.panes {
                let origin = (
                    grid_x + f32::from(pane.rect.x) * self.cell.0,
                    grid_y + f32::from(pane.rect.y) * self.cell.1,
                );
                let w = if pane.rect.x + pane.rect.cols == right {
                    layout.side - TERMINAL_PADDING - origin.0
                } else {
                    f32::from(pane.rect.cols) * self.cell.0
                };
                let h = if pane.rect.y + pane.rect.rows == bottom {
                    layout.footer - TERMINAL_PADDING - origin.1
                } else {
                    f32::from(pane.rect.rows) * self.cell.1
                };
                if w <= 0. || h <= 0. {
                    continue;
                }
                self.renderer.clip(origin.0, origin.1, w, h)?;
                let active = pane.id == snapshot.focused && self.focused && snapshot.controller;
                let cache = self.caches.entry(pane.id).or_default();
                if let Some(screen) = screens.get_mut(&pane.id) {
                    let (cursor, cursor_shape) = screen.cursor();
                    let offset = screen.display_offset();
                    if !screen.mode().contains(Mode::ALT_SCREEN)
                        && let Some(bar) = Scrollbar::new(
                            ScrollTarget::Pane(pane.id),
                            (
                                if pane.rect.x + pane.rect.cols == right {
                                    layout.side - 3.
                                } else {
                                    origin.0 + w - 3.
                                },
                                origin.1,
                                3.,
                                h,
                            ),
                            screen.history_size() + screen.screen_lines(),
                            screen.screen_lines(),
                            screen.display_offset(),
                        )
                    {
                        self.scrollbars.push(bar);
                    }
                    let frame = Frame::new(
                        screen,
                        cache,
                        active,
                        pane.id == snapshot.focused
                            && (!terminal_input || !self.preedit.is_empty()),
                    );
                    if frame.background >> 8 != self.config.background {
                        self.renderer.clear(
                            (frame.background & !255)
                                | (self.config.background_opacity * 255.).round() as u32,
                        )?;
                    }

                    if self.hovered_block.is_some_and(|(id, _)| id == pane.id) {
                        for row in 0..screen.screen_lines() {
                            if screen.row_output(pane, row).is_some_and(|block| {
                                self.hovered_block == Some((pane.id, block.id))
                            }) {
                                self.renderer.rect(
                                    origin.0,
                                    origin.1 + row as f32 * self.cell.1,
                                    w,
                                    self.cell.1,
                                    0x262626ff,
                                    false,
                                );
                            }
                        }
                    }
                    frame.paint(self.cell, active, cache, &self.renderer, origin, (w, h))?;
                    if let Some((id, object)) = &self.hovered
                        && *id == pane.id
                    {
                        let range = object.range;
                        for line in range.start.line..=range.end.line {
                            let row = line + offset as i32;
                            if !(0..screen.screen_lines() as i32).contains(&row) {
                                continue;
                            }
                            let first = if line == range.start.line {
                                range.start.column
                            } else {
                                0
                            };
                            let last = if line == range.end.line {
                                range.end.column + 1
                            } else {
                                screen.columns()
                            };
                            let x = origin.0 + first as f32 * self.cell.0;
                            let y = origin.1 + row as f32 * self.cell.1;
                            let w = last.saturating_sub(first) as f32 * self.cell.0;
                            self.renderer.rect(x, y, w, self.cell.1, 0x45605266, false);
                            self.renderer
                                .rect(x, y + self.cell.1 - 1., w, 1., ACCENT, false);
                        }
                    }
                    if pane.id == snapshot.focused && terminal_input {
                        let row = (cursor.line + offset as i32)
                            .clamp(0, i32::from(pane.rect.rows) - 1)
                            as usize;
                        let x = origin.0 + cursor.column as f32 * self.cell.0;
                        let y = origin.1 + row as f32 * self.cell.1;
                        if !self.preedit.is_empty() {
                            let count = text_width(&self.preedit) as f32;
                            self.renderer.rect(
                                x,
                                y,
                                self.cell.0 * count,
                                self.cell.1,
                                (self.config.background << 8) | 255,
                                false,
                            );
                            self.text.draw(
                                &self.renderer,
                                &self.preedit,
                                x,
                                y,
                                (self.config.foreground << 8) | 255,
                            )?;
                            self.renderer.rect(
                                x,
                                y + self.cell.1 - 2.,
                                self.cell.0 * count,
                                1.,
                                0x9c8fdcff,
                                false,
                            );
                        }
                        // Anchor to the insertion point while the IME replaces its preedit text.
                        // Hidden TUI cursors visit drawing cells, not input positions.
                        if cursor_shape != crate::vt::CursorShape::Hidden {
                            self.window.cursor_rect(
                                x as i32,
                                y as i32,
                                self.cell.0 as i32,
                                self.cell.1 as i32,
                            );
                        }
                    }
                }
                self.renderer.clip(0., 0., 0., 0.)?;
            }
            // Meet at gutter centers so nested splits share continuous dividers.
            for pane in &snapshot.panes {
                let rect = pane.rect;
                let left = f32::from(rect.x) - if rect.x > 0 { 0.5 } else { 0. };
                let top = f32::from(rect.y) - if rect.y > 1 { 0.5 } else { 0. };
                let end_x = f32::from(rect.x + rect.cols)
                    + if rect.x + rect.cols < right { 0.5 } else { 0. };
                let end_y = f32::from(rect.y + rect.rows)
                    + if rect.y + rect.rows < bottom { 0.5 } else { 0. };
                if pane.rect.x > 0 {
                    self.renderer.rect(
                        grid_x + left * self.cell.0,
                        grid_y + top * self.cell.1,
                        1.,
                        (end_y - top) * self.cell.1,
                        0x303030ff,
                        false,
                    );
                }
                if pane.rect.y > 1 {
                    self.renderer.rect(
                        grid_x + left * self.cell.0,
                        grid_y + top * self.cell.1,
                        (end_x - left) * self.cell.0,
                        1.,
                        0x303030ff,
                        false,
                    );
                }
            }
        }
        self.draw_window_controls(layout)?;
        #[cfg(feature = "agent")]
        self.draw_agent(layout)?;
        self.footer(layout)?;
        #[cfg(feature = "files")]
        if self.ui.files_open {
            self.draw_files()?;
        }
        #[cfg(feature = "agent")]
        self.draw_context_preview(layout)?;
        self.draw_scrollbars()?;
        if self.ui.search.open {
            self.draw_search(layout)?;
        }
        if self.ui.help {
            self.draw_help(layout)?;
        }
        if self.closing.is_some() {
            self.draw_close(width as f32, height as f32)?;
        }
        self.text.lines.truncate(self.text.used);
        let prepared = begin.map(|_| monotonic());
        self.renderer.present()?;
        if let Some(trace) = &mut self.trace {
            let marker = self
                .client
                .snapshot()
                .and_then(|snapshot| self.client.screens().get(&snapshot.focused))
                .map(|screen| {
                    let cell = screen.row(0).cells[0];
                    let color = screen.color(cell.bg);
                    (
                        cell.c as u32,
                        (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b),
                    )
                })
                .unwrap_or((0, 0));
            trace.frame = trace.frame.wrapping_add(1);
            trace.pending = Some((
                format!(
                    "frame {} {:.9} {:.9} {:.9}",
                    trace.frame,
                    begin.unwrap(),
                    begin.unwrap(),
                    prepared.unwrap()
                ),
                format!(
                    " {} {} {} {} {} {:.9} {}",
                    marker.0,
                    marker.1,
                    u8::from(!self.preedit.is_empty()),
                    width,
                    height,
                    scale,
                    u8::from(self.focused)
                ),
            ));
        }
        Ok(())
    }
    #[cfg(feature = "agent")]
    fn send_agent(&mut self) {
        if self.paste.is_some() {
            self.message("Wait for paste to finish");
            return;
        }
        if !self.preedit.is_empty() {
            self.message("Finish or cancel IME composition first");
            return;
        }
        match self.ui.agent.active_mut().send(&self.client) {
            Ok(true) => {
                self.control_hits.clear();
                self.hovered_control = None;
            }
            Ok(false) => (),
            Err(error) => self.message(format!("Agent: {error}")),
        }
        self.dirty = true;
    }
    fn open_close(&mut self) {
        self.preedit.clear();
        self.closing = Some(CloseChoice::Background);
        self.dirty = true;
    }
    fn choose_close(&mut self, choice: CloseChoice) -> Result<()> {
        match choice {
            CloseChoice::Terminate => self.action(Action::Terminate)?,
            CloseChoice::Background => self.running = false,
        }
        Ok(())
    }
    #[cfg(feature = "files")]
    fn open_files(&mut self, pane: PaneId) -> Result<()> {
        if self.client.is_remote() {
            self.message("Files are unavailable in remote sessions");
            return Ok(());
        }
        self.ui.files_open = !self.ui.files_open;
        self.ui.focus = if self.ui.files_open {
            Focus::Files
        } else {
            Focus::Terminal
        };
        if self.ui.files_open {
            let directory = self
                .client
                .snapshot()
                .and_then(|s| s.panes.iter().find(|p| p.id == pane))
                .and_then(|p| p.cwd.clone())
                .map_or_else(std::env::current_dir, Ok)?;
            if let Some(files) = &mut self.ui.files {
                files.set_directory(directory)?;
            } else {
                self.ui.files = Some(FileNav::new(directory)?);
            }
        }
        self.preedit.clear();
        self.dirty = true;
        Ok(())
    }
    #[cfg(feature = "files")]
    fn file_key(&mut self, symbol: u32, text: &str) -> Result<()> {
        let visible_rows = self.file_layout().rows;
        let Some(files) = &mut self.ui.files else {
            return Ok(());
        };
        let preview = files.focus() == FileFocus::Preview;
        let shortcut = if (preview || !files.is_search()) && self.preedit.is_empty() {
            match symbol_char(symbol) {
                Some('h') => Some(0xff51),
                Some('j') => Some(0xff54),
                Some('k') => Some(0xff52),
                Some('l') => Some(0xff53),
                Some('/') if !preview => Some(u32::from('/')),
                _ => None,
            }
        } else {
            None
        };
        if shortcut.is_some() {
            self.suppressed_text = Some(if text.is_empty() {
                symbol_char(symbol).unwrap().to_string()
            } else {
                text.to_owned()
            });
        }
        let symbol = shortcut.unwrap_or(symbol);
        match symbol {
            0xff09 | 0xfe20 => {
                if preview {
                    files.focus_list();
                } else {
                    files.focus_preview();
                }
            }
            0xff1b => {
                if preview {
                    files.focus_list();
                } else if files.is_search() {
                    files.stop_search();
                } else {
                    self.ui.files_open = false;
                    self.ui.focus = Focus::Terminal;
                }
            }
            0xff08 => {
                if preview {
                    files.focus_list();
                } else if files.is_search() {
                    let mut query = files.query().to_string();
                    query.pop();
                    files.set_query(query);
                } else {
                    files.parent();
                }
            }
            0xff52 | 0xff54 => {
                let delta = if symbol == 0xff52 { -1 } else { 1 };
                if preview {
                    files.scroll_preview(delta, visible_rows);
                } else {
                    files.move_selection(delta);
                }
            }
            0xff51 => {
                if preview {
                    files.focus_list();
                } else if !files.is_search() {
                    files.parent();
                }
            }
            0xff53 => {
                if !preview && !files.focus_preview() {
                    files.enter_selected();
                }
            }
            0xff0d | 0xff8d => {
                if files.selected().is_some_and(|e| e.is_dir) {
                    files.enter_selected();
                } else if let Some(data) = files.selected_shell_path() {
                    self.ui.files_open = false;
                    self.ui.focus = Focus::Terminal;
                    if let Some(pane) = self.active() {
                        self.input(pane, data)?;
                    }
                }
            }
            0x2f if shortcut.is_some() => files.start_search(),
            _ => {}
        }
        self.dirty = true;
        Ok(())
    }
    #[cfg(feature = "files")]
    fn file_layout(&self) -> FileLayout {
        let (width, height, _) = self.window.size();
        #[cfg(feature = "monitor")]
        let height = if self.ui.resources.open {
            self.layout().footer
        } else {
            height as f32
        };
        #[cfg(not(feature = "monitor"))]
        let height = height as f32;
        let w = (width as f32 * 0.7)
            .max(self.cell.0 * 24.)
            .min((width as f32 - 12.).max(1.));
        let h = (height * 0.7)
            .max(self.cell.1 * 6.)
            .min((height - 12.).max(1.));
        let x = (width as f32 - w) * 0.5;
        let y = (height - h) * 0.38;
        let search = self.ui.files.as_ref().is_some_and(FileNav::is_search);
        let top = y + self.cell.1 * if search { 1.75 } else { 0.75 };
        FileLayout {
            x,
            y,
            w,
            h,
            split: x + w * 0.42,
            top,
            rows: (((y + h - self.cell.1 * 0.4 - top) / self.cell.1).floor() as usize).max(1),
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn scrollbar_preserves_grab_position_and_reaches_both_history_ends() {
        let bar =
            Scrollbar::new(ScrollTarget::Pane(1), (10., 20., 3., 200.), 1000, 100, 450).unwrap();
        let (top, height) = bar.thumb();
        assert_eq!(height, 20.);
        assert_eq!(bar.offset_at(top + 7., 7.), 450);
        assert_eq!(bar.offset_at(27., 7.), 900);
        assert_eq!(bar.offset_at(227., 7.), 0);
        assert!(Scrollbar::new(ScrollTarget::Pane(1), (10., 20., 3., 200.), 100, 100, 0).is_none());
        let layout = GuiLayout::new(1200., 768., (10., 22.), Some(392.));
        assert_eq!(layout.side, 808.);
        assert!(layout.divider_contains((814., 100.)));
        assert!(!layout.divider_contains((817., 100.)));
    }

    #[test]
    fn chrome_and_sidebar_keep_grid_coordinates_and_pty_size_in_sync() {
        let cell = (10., 22.);
        let layout = GuiLayout::new(1200., 768., cell, Some(392.));
        let size = layout.size(cell);
        assert_eq!((size.num_cols, size.num_lines), (80, 33));
        assert_eq!(layout.top, cell.1);
        let (x, y) = layout.origin(cell);
        assert!(layout.terminal_contains((x, y + cell.1)));
        assert!(!layout.terminal_contains((x, layout.top - 1.)));
        assert!(!layout.terminal_contains((layout.side + 1., y + cell.1)));
        assert!(!layout.terminal_contains((x, layout.footer + 1.)));
        let terminal_bottom = y + size.num_lines as f32 * cell.1;
        assert!(terminal_bottom <= layout.footer - 4.);
        assert!(layout.footer - 4. - terminal_bottom < cell.1);
        assert_eq!(x, 4.);
        assert_eq!(y + cell.1, layout.top + 4.);
        let closed = GuiLayout::new(1200., 768., cell, None);
        assert_eq!(closed.origin(cell), layout.origin(cell));
        assert_eq!(closed.size(cell).num_lines, size.num_lines);
        assert_eq!(closed.size(cell).num_cols, 119);
    }
}
