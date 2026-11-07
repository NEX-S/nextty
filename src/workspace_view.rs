use crate::input::PasteTarget;
use crate::input::keysym;
use crate::panel::mix_color;

mod agent_ui;
mod render;

use crate::files::FileFocus;

use crate::{agent::panel::AgentAction, workspace_view::agent_ui::AgentScroll};
use crate::{
    config::Config,
    context,
    core::{Mode, Point, Scroll, SelectionType, Side, WindowSize},
    error::{Error, Result},
    ui::{ClosePrompt, Focus, Ui},
    width::text_width,
    workspace::{Action, Axis, Client, ClientEvent, PaneId, SplitDrag, TabId},
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
    Text(PasteTarget, String),
    Bytes(PaneId, Vec<u8>),
    Paste(PasteTarget, bool),
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

struct CachedText {
    color: u32,
    flags: u32,
    line: Line,
    used: bool,
}

#[derive(Default)]
struct TextCache {
    lines: HashMap<Box<str>, Vec<CachedText>>,
}

impl TextCache {
    fn draw(&mut self, renderer: &Renderer, text: &str, x: f32, y: f32, color: u32) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
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
        let entry = if let Some(entry) = self.lines.get_mut(text).and_then(|entries| {
            entries
                .iter_mut()
                .find(|entry| entry.color == color && entry.flags == flags)
        }) {
            entry
        } else {
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
            self.lines
                .entry(text.into())
                .or_default()
                .push_mut(CachedText {
                    color,
                    flags,
                    line,
                    used: false,
                })
        };
        entry.used = true;
        renderer.line(&entry.line, x, y);
        Ok(())
    }
    fn finish(&mut self) {
        self.lines.retain(|_, entries| {
            entries.retain_mut(|entry| std::mem::take(&mut entry.used));
            !entries.is_empty()
        });
    }
}

const CHROME: u32 = (crate::panel::CHROME << 8) | 255;
const BORDER: u32 = (crate::panel::BORDER << 8) | 255;
const INK: u32 = (crate::panel::INK << 8) | 255;
const MUTED: u32 = (crate::panel::MUTED << 8) | 255;
const ACCENT: u32 = (crate::panel::ACCENT << 8) | 255;
const PADDING: f32 = 16.;
const TERMINAL_PADDING: f32 = 4.;

#[derive(Clone, Copy)]
struct GuiLayout {
    left: f32,
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
            left: 0.,
            top: cell.1,
            footer: height - cell.1 - 12.,
            side: width - side_width,
            width,
            height,
        }
    }
    fn origin(self, cell: (f32, f32)) -> (f32, f32) {
        (
            self.left + TERMINAL_PADDING,
            self.top + TERMINAL_PADDING - cell.1,
        )
    }
    fn size(self, cell: (f32, f32)) -> WindowSize {
        WindowSize {
            num_cols: (((self.side - self.left - TERMINAL_PADDING * 2.) / cell.0).floor() as u16)
                .max(2),
            num_lines: (((self.footer - self.top - TERMINAL_PADDING * 2.) / cell.1).floor() as u16)
                .saturating_add(1)
                .max(2),
            cell_width: cell.0.round() as u16,
            cell_height: cell.1.round() as u16,
        }
    }

    fn divider_contains(self, point: (f32, f32)) -> bool {
        self.side < self.width
            && point.0 >= self.side
            && point.0 < self.side + 8.
            && (self.top..self.footer).contains(&point.1)
    }
    fn pointer_cell(self, cell: (f32, f32), pointer: (f32, f32)) -> (u16, u16) {
        let (ox, oy) = self.origin(cell);
        let size = self.size(cell);
        (
            (((pointer.0 - ox) / cell.0).max(0.) as u16).min(size.num_cols - 1),
            (((pointer.1 - oy) / cell.1).max(0.) as u16).min(size.num_lines - 1),
        )
    }
    fn terminal_contains(self, point: (f32, f32)) -> bool {
        point.0 >= self.left + TERMINAL_PADDING
            && point.0 < self.side - TERMINAL_PADDING
            && point.1 >= self.top + TERMINAL_PADDING
            && point.1 < self.footer - TERMINAL_PADDING
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    Pane(PaneId),

    Agent(AgentScroll),
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
    Resources,

    Monitor(crate::monitor::Action),
    NewTab,
    CloseTab(TabId),
    Tab(i32),
    SelectTab(usize),
    Minimize,
    Maximize,
    Close,

    Agent(crate::agent::panel::AgentAction),
}

#[derive(Clone, Copy, PartialEq)]
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
    scrollbars: Vec<Scrollbar>,
    scrollbar_drag: Option<ScrollbarDrag>,
    maximized: bool,
    hovered: Option<(PaneId, crate::context::Object)>,

    hovered_block: Option<(PaneId, u64)>,
    hover_cell: Option<(PaneId, Point)>,
    hovered_control: Option<GuiHit>,
    control_hits: Vec<GuiHit>,

    suppressed_text: Option<String>,
    trace: Option<Trace>,

    agent: agent_ui::AgentState,
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

            agent: agent_ui::AgentState::default(),
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            maximized: false,
            hovered: None,

            hovered_block: None,
            hover_cell: None,
            hovered_control: None,
            control_hits: Vec::new(),

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
            let ready = self.renderer.ready()?;
            if ready && let Some(trace) = &mut self.trace {
                trace.record_swap(self.renderer.swap_time())?;
            }
            let mut timeout = if self.dirty && ready && self.window.ready() {
                0
            } else {
                -1
            };
            if let Some(until) = self.ui.deadline() {
                let remaining =
                    crate::poll::timeout_millis(until.saturating_duration_since(Instant::now()));
                timeout = if timeout < 0 {
                    remaining
                } else {
                    timeout.min(remaining)
                };
            }

            if self.client.needs_poll() {
                timeout = 0;
            }
            self.client.flush()?;
            pollfds.clear();
            pollfds.extend([
                self.client.pollfd(),
                libc::pollfd {
                    fd: self.renderer.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.client.receiver_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ]);
            pollfds.extend(self.ui.fds());
            self.window.wait(&mut pollfds, timeout, &mut events)?;
            if pollfds[2].revents != 0 {
                crate::eventfd::consume_notification(pollfds[2].fd)?;
            }
            if pollfds[0].revents & libc::POLLOUT != 0 {
                self.client.flush()?;
            }
            for event in events.drain(..) {
                self.event(event)?;
            }
            self.client.flush()?;
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
                            self.ui.search.observe(&mut self.client)?;
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
                    ClientEvent::Error(error) | ClientEvent::CopyError { error, .. } => {
                        self.message(error)
                    }
                    ClientEvent::Detached => self.running = false,
                    ClientEvent::List(_)
                    | ClientEvent::AgentInput { .. }
                    | ClientEvent::AgentFile { .. }
                    | ClientEvent::AgentNewTab { .. }
                    | ClientEvent::History { .. } => (),
                    ClientEvent::ClipboardStore { primary, text } => {
                        self.window.selection(primary, &text)
                    }
                }
            }

            self.poll_agent_events(&pollfds)?;
            self.refresh_hover();

            let (_, changed) = self.ui.service(&self.client, &pollfds)?;
            self.dirty |= changed;
            if self.running && self.dirty && self.renderer.ready()? && self.window.ready() {
                if let Some(trace) = &mut self.trace {
                    trace.record_swap(self.renderer.swap_time())?;
                }
                self.draw()?;
                self.dirty = false;
                self.client.request_screen()?;
            }
        }

        if self.client.snapshot().is_some() {
            self.ui.poll_agent(&mut self.client, &self.config)?;
        }
        self.client.flush()?;
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
            self.ui.search.close(&mut self.client)?;
            self.ui.focus = Focus::Terminal;

            self.ui.cancel_rewind();

            if !matches!(action, Action::Focus(_)) && std::mem::take(&mut self.ui.files_open) {
                self.resize()?;
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
        self.ui.search.close(&mut self.client)?;
        self.hover_cell = None;
        self.hovered = None;

        self.hovered_block = None;
        self.action(Action::Resize(self.layout().size(self.cell)))?;
        self.dirty = true;
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            trace.event(&event)?;
        }

        if self.agent_event(&event) {
            return Ok(());
        }
        match event.kind {
            native::WINDOW_STATE => {
                self.maximized = event.code & 8 != 0;
                self.dirty = true;
            }
            native::KEY => self.key(event.code, event.modifiers, &event.text)?,
            native::TEXT => {
                if self.ui.close_prompt.is_some()
                    || self.preview_open()
                    || self.ui.file_preview_open()
                {
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
                if self.ui.input_target().is_some() {
                    if let Some(target) = self.ui.text_target() {
                        if self.paste.is_some() {
                            self.deferred
                                .push_back(DeferredInput::Text(target, event.text));
                        } else {
                            self.panel_key(0, 0, &event.text)?;
                        }
                        self.dirty = true;
                    }
                    return Ok(());
                }
                if let Some(pane) = self.active() {
                    self.input(pane, event.text.into_bytes())?;
                }
            }
            native::PREEDIT => {
                if self.ui.resources.open && self.ui.focus == Focus::Monitor {
                    self.preedit.clear();
                    return Ok(());
                }
                if self.ui.close_prompt.is_some()
                    || self.preview_open()
                    || self.ui.file_preview_open()
                {
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
    fn composition_finished(&mut self) -> bool {
        if self.preedit.is_empty() {
            return true;
        }
        self.message("Finish or cancel IME composition first");
        false
    }

    fn workspace_key(&mut self, pane: PaneId, symbol: u32, modifiers: u32) -> Result<bool> {
        if modifiers & (native::ALT | native::CONTROL | native::SUPER) != native::ALT {
            return Ok(false);
        }
        if !self.composition_finished() {
            return Ok(true);
        }
        let key = match symbol {
            keysym::RETURN | keysym::KP_ENTER => Some('\r'),
            _ => symbol_char(symbol).map(|c| c.to_ascii_lowercase()),
        };
        match key {
            Some('m') => {
                self.ui.search.close(&mut self.client)?;
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
            Some('c') => {
                self.copy_block(pane, None)?;
                return Ok(true);
            }

            Some('f') => {
                self.ui.search.close(&mut self.client)?;
                self.open_files(pane)?;
                return Ok(true);
            }
            _ => (),
        }
        let Some(action) = key.and_then(|key| crate::ui::workspace_action(key, pane)) else {
            return Ok(false);
        };
        if self.controller() {
            self.action(action)?;
        }
        Ok(true)
    }
    fn key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<()> {
        if self.ui.file_preview_open() {
            self.panel_key(symbol, modifiers, "")?;
            return Ok(());
        }
        if let Some(prompt) = self.ui.close_prompt.as_mut() {
            let terminate = prompt.terminate;
            match symbol {
                keysym::RETURN | keysym::KP_ENTER => self.choose_close(terminate)?,
                keysym::ESCAPE => {
                    self.ui.close_prompt = None;
                    self.dirty = true;
                }
                keysym::LEFT => {
                    prompt.terminate = true;
                    self.dirty = true;
                }
                keysym::RIGHT => {
                    prompt.terminate = false;
                    self.dirty = true;
                }
                keysym::TAB | keysym::LEFT_TAB => {
                    prompt.terminate = !terminate;
                    self.dirty = true;
                }
                _ => {
                    if symbol_char(symbol).is_some_and(|c| c == 'x' || c == 'X') {
                        self.choose_close(true)?;
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
        if modifiers == native::ALT && symbol_char(symbol) == Some('a') {
            if !self.composition_finished() {
                return Ok(());
            }
            self.ui.search.close(&mut self.client)?;
            self.ui.toggle_agent();
            self.resize()?;
            return Ok(());
        }

        if self.workspace_key(pane, symbol, modifiers)? {
            return Ok(());
        }

        if self.ui.agent.open
            && self.ui.focus == Focus::Editor
            && (!self.preedit.is_empty() || self.paste.is_some())
        {
            return Ok(());
        }
        if self.panel_key(symbol, modifiers, "")? {
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

        if modifiers & (native::SHIFT | native::CONTROL | native::ALT) == native::SHIFT {
            let scroll = match symbol {
                keysym::HOME => Some(Scroll::Top),
                keysym::END => Some(Scroll::Bottom),
                keysym::INSERT => {
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
    fn panel_key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<bool> {
        use crate::input::{Key, KeyResult};
        let mut viewport = (1, 1);
        if self.ui.files_open && self.ui.focus == Focus::Files {
            let layout = self.layout();
            let rect = if self
                .ui
                .files
                .as_ref()
                .is_some_and(|files| files.focus() == FileFocus::Preview)
            {
                self.file_preview_layout(layout)
            } else {
                self.file_rect(layout)
            };
            viewport = (
                usize::from(rect.cols),
                usize::from(rect.rows.saturating_sub(if self.ui.file_preview_open() {
                    5
                } else {
                    0
                })),
            );
        } else if let Some(layout) = self.editor_viewport() {
            viewport = (layout.columns, layout.rows);
        }

        let panels = self.ui.panel_state();
        let action = self.ui.key(
            &mut self.client,
            Key {
                symbol,
                modifiers,
                text,
            },
            viewport,
        )?;
        let suppress = !matches!(action, KeyResult::Text);
        match action {
            KeyResult::Unhandled => return Ok(false),
            KeyResult::Copy(text, _cut) => {
                self.window.selection(false, &text);

                if _cut {
                    self.ui.agent.insert(self.ui.agent.active().id, "").unwrap();
                }
            }
            KeyResult::Paste => {
                if let Some(target) = self.ui.paste_target() {
                    self.request_paste(target, false);
                }
            }
            KeyResult::Text | KeyResult::Handled => (),
        }
        if suppress
            && modifiers & native::CONTROL == 0
            && let Some(ch) = symbol_char(symbol)
        {
            self.suppressed_text = Some(ch.to_string());
        }
        if panels != self.ui.panel_state() {
            self.resize()?;
        }
        self.dirty = true;
        Ok(true)
    }

    fn copy(&mut self, pane: PaneId, primary: bool) -> Result<()> {
        self.action(Action::CopySelection {
            pane,
            request: u64::from(primary),
        })
    }

    fn copy_block(&mut self, pane: PaneId, block: Option<u64>) -> Result<()> {
        let block = block.or_else(|| {
            self.client
                .snapshot()?
                .pane(pane)?
                .blocks
                .iter()
                .rfind(|b| b.output.is_some())
                .map(|b| b.id)
        });
        if let Some(block) = block {
            self.action(Action::CopyBlock {
                pane,
                block,
                request: 0,
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
        if !self.composition_finished() {
            return;
        }
        let target = if self.ui.files_open && self.ui.focus == Focus::Files {
            if let Some(files) = &mut self.ui.files {
                files.focus_list();
                if !files.is_search() {
                    files.start_search();
                }
            }
            self.dirty = true;
            PasteTarget::Files
        } else {
            PasteTarget::Pane {
                pane,
                bracketed: self.mode(pane).contains(Mode::BRACKETED_PASTE),
            }
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
    fn pasted(&mut self, text: String) -> Result<()> {
        let Some(target) = self.paste.take() else {
            return Ok(());
        };
        match target {
            PasteTarget::Pane { pane, bracketed } => self.action(Action::Input {
                pane,
                data: paste_bytes(text, bracketed),
            })?,
            target => self.ui.paste(&mut self.client, target, &text)?,
        }
        self.dirty = true;
        while let Some(input) = self.deferred.pop_front() {
            match input {
                DeferredInput::Text(target, text) => {
                    self.ui.paste(&mut self.client, target, &text)?
                }
                DeferredInput::Bytes(pane, data) => self.action(Action::Input { pane, data })?,
                DeferredInput::Paste(target, primary) => {
                    self.request_paste(target, primary);
                    break;
                }
            }
        }
        Ok(())
    }
    fn layout(&self) -> GuiLayout {
        let (width, height, _) = self.window.size();

        let agent_width = self.ui.agent.open.then(|| {
            self.ui
                .agent
                .width
                .unwrap_or(self.cell.0 * 36. + PADDING * 2.)
        });
        let mut layout = GuiLayout::new(width as f32, height as f32, self.cell, agent_width);

        if self.ui.files_open {
            layout.left = f32::from(crate::file_panel::sidebar_width(
                (layout.side / self.cell.0).floor() as u16,
            )) * self.cell.0;
        }
        if self.ui.resources.open {
            let total = ((layout.height - layout.top - 12.) / self.cell.1).max(0.) as u16;
            layout.footer -= f32::from(self.ui.resources.rows(total)) * self.cell.1 + 6.;
        }
        layout
    }
    fn preview_open(&self) -> bool {
        self.ui.agent.active().context.preview.is_some()
    }
    fn scrollbar_visible(&self, target: ScrollTarget) -> bool {
        self.preview_open() == (target == ScrollTarget::Agent(AgentScroll::Preview))
    }
    fn hit(&self, captured: Option<PaneId>) -> Option<(PaneId, Point, Side)> {
        let layout = self.layout();
        let snapshot = self.client.snapshot()?;
        let pane = if let Some(id) = captured {
            snapshot.pane(id)?
        } else {
            if self.preview_open()
                || self.ui.file_preview_open()
                || !layout.terminal_contains(self.pointer)
            {
                return None;
            }
            let (x, y) = layout.pointer_cell(self.cell, self.pointer);
            snapshot.pane_at(x, y)?
        };
        let rect = pane.rect;
        let screen = self.client.screens().get(&pane.id)?;
        let (ox, oy) = layout.origin(self.cell);
        let row = ((self.pointer.1 - oy) / self.cell.1 - f32::from(rect.y))
            .floor()
            .clamp(0., f32::from(rect.rows - 1)) as usize;
        let x = (self.pointer.0 - ox) / self.cell.0 - f32::from(rect.x);
        Some((
            pane.id,
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
        let layout = self.layout();
        {
            let top = layout.footer + 6.;
            let pointer = (self.ui.resources.open
                && !self.preview_open()
                && self.ui.close_prompt.is_none()
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

        let monitor_divider = self.ui.resources.open
            && self.ui.close_prompt.is_none()
            && !self.preview_open()
            && (self.ui.resources.dragging || (self.pointer.1 - layout.footer).abs() <= 5.);
        let horizontal = {
            {
                let divider = !self.ui.files_open
                    && (self.agent.drag.is_some()
                        || (self.ui.close_prompt.is_none()
                            && !self.preview_open()
                            && layout.divider_contains(self.pointer)));
                if divider != self.agent.divider_hover {
                    self.agent.divider_hover = divider;
                    self.dirty = true;
                }
                divider
            }
        };
        self.window.resize_cursor(horizontal, monitor_divider);

        let hit = self.hit(None).map(|(pane, point, _)| (pane, point));
        {
            let block = hit.and_then(|(pane, point)| self.block_at(pane, point));
            if block != self.hovered_block {
                self.hovered_block = block;
                self.dirty = true;
            }
        }
        let control = self
            .control_hits
            .iter()
            .rfind(|hit| hit.contains(self.pointer))
            .copied();
        if self.hovered_control != control {
            self.hovered_control = control;
            self.dirty = true;
        }
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
    fn gui_action(&mut self, action: GuiAction) -> Result<()> {
        if !matches!(action, GuiAction::Agent(_)) {
            self.ui.agent.active_mut().context.menu = false;
        }

        match action {
            GuiAction::Resources => {
                self.ui.toggle_monitor();
                self.resize()?;
            }

            GuiAction::Monitor(action) => {
                let files_open = std::mem::take(&mut self.ui.files_open);
                let open = self.ui.resources.open;
                self.ui.resources.action(action);
                self.ui.monitor_jump(&mut self.client)?;
                self.ui.focus = if self.ui.resources.open {
                    Focus::Monitor
                } else {
                    Focus::Terminal
                };
                if open != self.ui.resources.open || files_open {
                    self.resize()?;
                }
            }
            GuiAction::CloseTab(tab) => {
                if self.composition_finished()
                    && self.controller()
                    && let Some(target) = self.client.snapshot().and_then(|snapshot| {
                        snapshot.tabs.iter().find(|candidate| candidate.id == tab)
                    })
                    && self.ui.request_close_tab(target)
                {
                    self.action(Action::CloseTab(tab))?;
                }
            }
            GuiAction::Tab(direction) => {
                if self.controller() {
                    self.ui.focus = Focus::Terminal;
                    self.action(Action::Tab(direction))?;
                }
            }
            GuiAction::SelectTab(index) => {
                if self.composition_finished()
                    && let Some(snapshot) = self.client.snapshot()
                {
                    let active = snapshot
                        .tabs
                        .iter()
                        .position(|tab| tab.id == snapshot.active_tab)
                        .unwrap_or(0);
                    self.action(Action::Tab(index as i32 - active as i32))?;
                }
            }
            GuiAction::NewTab => {
                if self.composition_finished()
                    && self.controller()
                    && let Some(pane) = self.active()
                {
                    self.action(Action::NewTab { pane })?;
                }
            }
            GuiAction::Minimize => self.window.minimize(),
            GuiAction::Maximize => self.window.maximize(!self.maximized),
            GuiAction::Close => self.open_close(),

            GuiAction::Agent(action) => {
                if !self.agent_action(action)? {
                    return Ok(());
                }
            }
        }
        self.control_hits.clear();
        self.hovered_control = None;
        self.dirty = true;
        Ok(())
    }

    fn block_at(&self, id: PaneId, point: Point) -> Option<(PaneId, u64)> {
        if self.ui.close_prompt.is_some() {
            return None;
        }

        let pane = self.client.snapshot()?.pane(id)?;
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
        if self.ui.file_preview_open() {
            if pressed && button == 1 {
                let rect = self.file_preview_layout(self.layout());
                let column = (self.pointer.0 / self.cell.0) as u16;
                let row = (self.pointer.1 / self.cell.1) as u16;
                if !rect.contains(column, row)
                    || crate::panel::popup_close(rect).contains(column, row)
                {
                    self.ui.files.as_mut().unwrap().focus_list();
                    self.dirty = true;
                }
            }
            return Ok(());
        }
        if !pressed {
            return self.button_release(button, modifiers);
        }
        if self.ui.close_prompt.is_some() {
            if button == 1 {
                let (width, height, _) = self.window.size();
                let rects = close_layout(width as f32, height as f32, self.cell);
                for (terminate, &(x, y, w, h)) in [true, false].into_iter().zip(&rects[1..]) {
                    if self.pointer.0 >= x
                        && self.pointer.0 < x + w
                        && self.pointer.1 >= y
                        && self.pointer.1 < y + h
                    {
                        self.choose_close(terminate)?;
                        break;
                    }
                }
            }
            return Ok(());
        }
        let layout = self.layout();
        if self.pointer.1 < layout.top
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
                .rfind(|bar| bar.contains(self.pointer) && (self.scrollbar_visible(bar.target)))
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

        if self.ui.agent.active().context.preview.is_some() {
            return self.preview_button(button);
        }

        if self.footer_button(button)? {
            return Ok(());
        }

        if button == 1 && layout.divider_contains(self.pointer) {
            if self.composition_finished() {
                self.agent.drag = Some(self.pointer.0 - layout.side);
            }
            return Ok(());
        }

        if self.ui.files_open && self.pointer.0 < layout.left {
            self.ui.focus = Focus::Files;
            let rect = self.file_rect(layout);
            if button == 1
                && let Some(files) = &mut self.ui.files
                && let Some(index) = crate::file_panel::Layout::new(rect, files).entry_at(
                    files,
                    (self.pointer.0 / self.cell.0) as u16,
                    (self.pointer.1 / self.cell.1) as u16,
                )
            {
                files.move_selection(index as isize - files.selection() as isize);
                files.focus_preview();
                self.dirty = true;
            }
            return Ok(());
        }
        if let Some(action) = self
            .control_hits
            .iter()
            .rfind(|hit| hit.contains(self.pointer))
            .map(|hit| hit.action)
        {
            if self.agent_control_button(button, action, modifiers)? {
                return Ok(());
            }
            if button == 1 {
                self.gui_action(action)?;
            }
            return Ok(());
        }

        if !self.agent_button(button, layout) {
            return Ok(());
        }

        if self.pointer.1 >= layout.footer
            || self.pointer.0 >= layout.side && self.pointer.1 >= layout.top
        {
            return Ok(());
        }
        if button == 1
            && layout.terminal_contains(self.pointer)
            && self.controller()
            && self.preedit.is_empty()
        {
            let (column, row) = layout.pointer_cell(self.cell, self.pointer);
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
            && (modifiers & native::ALT != 0
                || self
                    .hit(None)
                    .is_some_and(|(pane, _, _)| !self.mode(pane).intersects(Mode::MOUSE_MODE)))
        {
            if let Some((_, object)) = &self.hovered {
                self.window.selection(false, &object.text);
                return Ok(());
            }
            if let Some((pane, _, _)) = self.hit(None) {
                self.paste(pane, false);
            }
            return Ok(());
        }
        self.terminal_button(button, modifiers)
    }

    fn button_release(&mut self, button: u32, modifiers: u32) -> Result<()> {
        if self.agent_button_release(button)? {
            return Ok(());
        }

        if button == 1 && std::mem::take(&mut self.ui.resources.dragging) {
            self.refresh_hover();
            return Ok(());
        }

        if button == 1 && self.scrollbar_drag.take().is_some() {
            self.dirty = true;
            return Ok(());
        }
        if button == 1 && self.split_drag.take().is_some() {
            return Ok(());
        }
        if button == 1
            && let Some(drag) = self.selecting.take()
        {
            if let Some((_, point, side)) = self.hit(Some(drag.pane)) {
                self.action(Action::SelectionUpdate {
                    pane: drag.pane,
                    point,
                    side,
                })?;
                if drag.kind != SelectionType::Simple || point != drag.point || side != drag.side {
                    self.copy(drag.pane, true)?;
                }
            }
            return Ok(());
        }
        if (1..=3).contains(&button) && self.reported_buttons & (1 << (button - 1)) != 0 {
            self.reported_buttons &= !(1 << (button - 1));
            if let Some(pane) = self.reported_pane
                && let Some((_, point, _)) = self.hit(Some(pane))
            {
                self.mouse(pane, point, (button - 1) as u8, true, modifiers, 1)?;
            }
            if self.reported_buttons == 0 {
                self.reported_pane = None;
            }
        }
        Ok(())
    }

    /// Clicks on the footer and the monitor divider; returns whether one was consumed.
    fn footer_button(&mut self, button: u32) -> Result<bool> {
        let layout = self.layout();
        if self.ui.resources.open && (self.pointer.1 - layout.footer).abs() <= 5. && button == 1 {
            if std::mem::take(&mut self.ui.files_open) {
                self.resize()?;
            }
            self.ui.resources.dragging = true;
            self.ui.focus = Focus::Monitor;
            self.dirty = true;
            self.refresh_hover();
            return Ok(true);
        }
        if self.pointer.1 >= layout.footer {
            if let Some(action) = self
                .control_hits
                .iter()
                .rfind(|hit| hit.contains(self.pointer))
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
            return Ok(true);
        }
        self.ui.focus = Focus::Terminal;
        self.ui.resources.searching = false;
        Ok(false)
    }

    fn terminal_button(&mut self, button: u32, modifiers: u32) -> Result<()> {
        if !self.controller() {
            return Ok(());
        }
        let hit = self
            .reported_pane
            .and_then(|pane| self.hit(Some(pane)))
            .or_else(|| self.hit(None));
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
            && modifiers & native::ALT == 0
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

            if count == 2
                && let Some((id, object)) = &self.hovered
                && *id == pane
            {
                let mut command = std::process::Command::new("xdg-open");
                command.arg(&object.text);
                if !self.client.is_remote()
                    && let Some(cwd) = self
                        .client
                        .snapshot()
                        .and_then(|snapshot| snapshot.pane(pane))
                        .and_then(|pane| pane.cwd.as_ref())
                {
                    command.current_dir(cwd);
                }
                let mut child = command.spawn()?;
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            let kind = match count {
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
    fn motion(&mut self, pointer: (f32, f32), modifiers: u32) -> Result<()> {
        let layout = self.layout();
        let previous = std::mem::replace(&mut self.pointer, pointer);
        let hovered_bar = |point| self.scrollbars.iter().position(|bar| bar.contains(point));
        if hovered_bar(previous) != hovered_bar(pointer) {
            self.dirty = true;
        }
        if self.ui.close_prompt.is_some() {
            self.dirty |= previous != pointer;
            return Ok(());
        }

        if self.agent_motion(previous, layout)? {
            return Ok(());
        }

        if self.ui.resources.dragging {
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

        if self.scrollbar_drag.is_some() {
            self.drag_scrollbar(pointer.1)?;
            return Ok(());
        }
        if self.preview_open() || self.ui.file_preview_open() {
            self.refresh_hover();
            return Ok(());
        }

        if self.ui.files_open && self.pointer.0 < layout.left {
            return Ok(());
        }
        if let Some(drag) = self.split_drag {
            let (column, row) = layout.pointer_cell(self.cell, pointer);
            let (old_column, old_row) = layout.pointer_cell(self.cell, previous);
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
            .and_then(|pane| self.hit(Some(pane)))
            .or_else(|| self.hit(None));
        if let Some((pane, point, side)) = hit {
            if self.selecting.is_some_and(|drag| drag.pane == pane) {
                self.action(Action::SelectionUpdate { pane, point, side })?;
            } else {
                let mode = self.mode(pane);
                if modifiers & native::ALT == 0
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

            ScrollTarget::Agent(target) => match target {
                AgentScroll::Transcript => self.ui.agent.active_mut().model.scroll = offset,
                AgentScroll::Editor => {
                    self.ui.agent.active_mut().editor_scroll = Some(drag.bar.maximum() - offset)
                }
                AgentScroll::Preview => {
                    if let Some(crate::agent::panel::ContextPreview { scroll, .. }) =
                        &mut self.ui.agent.active_mut().context.preview
                    {
                        *scroll = drag.bar.maximum() - offset;
                    }
                }
            },
        }
        self.dirty = true;
        Ok(())
    }
    fn scroll(&mut self, amount: f32, modifiers: u32) -> Result<()> {
        if self.ui.file_preview_open() {
            let rect = self.file_preview_layout(self.layout());
            self.ui.files.as_mut().unwrap().scroll(
                -amount.round() as isize,
                usize::from(crate::panel::popup_body(rect).rows),
            );
            self.dirty = true;
            return Ok(());
        }
        if self.scrollbar_drag.is_some() {
            return Ok(());
        }

        if self.agent_scroll(amount) {
            return Ok(());
        }

        if self.ui.resources.open && self.pointer.1 >= self.layout().footer {
            self.ui.resources.scroll = if amount > 0. {
                self.ui.resources.scroll.saturating_sub(3)
            } else {
                self.ui.resources.scroll.saturating_add(3)
            };
            self.dirty = true;
            return Ok(());
        }
        if self.ui.close_prompt.is_some() {
            return Ok(());
        }

        if self.ui.files_open && self.pointer.0 < self.layout().left {
            let rect = self.file_rect(self.layout());
            if let Some(files) = &mut self.ui.files {
                files.scroll(-amount.round() as isize, usize::from(rect.rows));
            }
            self.dirty = true;
            return Ok(());
        }
        let Some((pane, point, _)) = self.hit(None) else {
            return Ok(());
        };
        self.scroll_remainder += amount;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return Ok(());
        }
        let mode = self.mode(pane);
        if mode.intersects(Mode::MOUSE_MODE) && modifiers & native::ALT == 0 {
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
            && modifiers & native::ALT == 0
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
        let terminal_input = self.ui.close_prompt.is_none()
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
            self.caches.retain(|id, _| snapshot.pane(*id).is_some());
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

                    if let Some((id, block)) = self.hovered_block
                        && id == pane.id
                    {
                        for row in screen.block_row_range(pane, block) {
                            if screen
                                .row_output(pane, row)
                                .is_some_and(|current| current.id == block)
                            {
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
        self.footer(layout)?;
        self.renderer
            .clip(0., layout.top, layout.width, layout.footer - layout.top)?;
        let divider = (mix_color(self.config.foreground, self.config.background, 24) << 8) | 255;
        if self.ui.files_open {
            let x = (self.file_rect(layout).cols as f32 - 0.5) * self.cell.0;
            self.renderer.rect(
                x,
                layout.top,
                1.,
                layout.footer - layout.top,
                divider,
                false,
            );
        }

        if self.ui.agent.open {
            let x = ((layout.side / self.cell.0).floor() + 0.5) * self.cell.0;
            self.renderer.rect(
                x,
                layout.top,
                1.,
                layout.footer - layout.top,
                divider,
                false,
            );
        }
        self.renderer.clip(0., 0., 0., 0.)?;

        if self.ui.files_open && !self.ui.file_preview_open() {
            self.draw_files(layout)?;
        }
        self.draw_agent(layout)?;
        self.draw_scrollbars()?;
        if self.ui.file_preview_open() {
            self.draw_files(layout)?;
        }
        if self.ui.search.open {
            self.draw_search(layout)?;
        }
        if self.ui.close_prompt.is_some() {
            self.draw_close(width as f32, height as f32)?;
        }
        self.text.finish();
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
                    (cell.c as u32, color.packed())
                })
                .unwrap_or((0, 0));
            trace.frame(
                begin.unwrap(),
                prepared.unwrap(),
                marker,
                (width, height, scale),
                !self.preedit.is_empty(),
                self.focused,
            );
        }
        Ok(())
    }
    fn open_close(&mut self) {
        self.preedit.clear();
        self.ui.close_prompt = Some(ClosePrompt {
            tab: None,
            terminate: false,
        });
        self.dirty = true;
    }
    fn choose_close(&mut self, terminate: bool) -> Result<()> {
        if let Some(tab) = self.ui.close_prompt.unwrap().tab {
            self.ui.close_prompt = None;
            self.dirty = true;
            if terminate {
                self.action(Action::CloseTab(tab))?;
            }
        } else if terminate {
            self.action(Action::Terminate)?;
        } else {
            self.running = false;
        }
        Ok(())
    }

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
                .and_then(|snapshot| snapshot.pane(pane))
                .and_then(|p| p.cwd.clone())
                .map_or_else(std::env::current_dir, Ok)?;
            self.ui.load_files(directory)?;
        }
        self.preedit.clear();
        self.resize()
    }

    fn file_rect(&self, layout: GuiLayout) -> crate::workspace::Rect {
        crate::workspace::Rect {
            x: 0,
            y: 1,
            cols: (layout.left / self.cell.0).floor() as u16,
            rows: ((layout.footer - layout.top) / self.cell.1).floor().max(1.) as u16,
        }
    }

    fn file_preview_layout(&self, layout: GuiLayout) -> crate::workspace::Rect {
        crate::file_panel::preview_rect(crate::workspace::Rect {
            x: 0,
            y: 1,
            cols: (layout.width / self.cell.0).floor() as u16,
            rows: ((layout.footer - layout.top) / self.cell.1).floor() as u16,
        })
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
