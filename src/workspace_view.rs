use crate::input::{KeyResult, PasteTarget, keysym};
use crate::panel::{mix_color, rect};

mod agent_ui;
mod render;

use crate::files::FileFocus;

use crate::agent::panel::AgentAction;
use crate::{
    config::Config,
    context,
    core::{Mode, Point, Scroll, SelectionType, Side, WindowSize},
    ui::{ClosePrompt, Focus, TabHit, Ui, WHEEL_LINES},
    width::text_width,
    workspace::{Action, Axis, Client, PaneId, Response, SplitDrag},
};
use crate::{
    input::{key_bytes, mouse_bytes, paste_bytes, symbol_char},
    native::trace::Trace,
    native::{self, Event, Line, Renderer, Run, Window},
    paint::{Frame, LineCache, text_columns},
};
use std::{
    collections::{HashMap, VecDeque},
    io::Result,
    time::{Duration, Instant},
};

enum DeferredInput {
    Text(PasteTarget, String),
    Bytes(PaneId, Vec<u8>),
    Paste(PasteTarget, bool),
}

struct CachedText {
    color: u32,
    flags: u32,
    line: Line,
    used: bool,
}

/// The renderer plus the shaped lines of UI text, which are kept while they keep being drawn.
struct Painter {
    // Lines must drop before the renderer that shaped them.
    lines: HashMap<Box<str>, Vec<CachedText>>,
    renderer: Renderer,
}

impl std::ops::Deref for Painter {
    type Target = Renderer;
    fn deref(&self) -> &Renderer {
        &self.renderer
    }
}

impl Painter {
    fn text(&mut self, text: &str, x: f32, y: f32, color: u32) {
        if !text.is_empty() {
            self.text_flags(text, x, y, color, 0);
        }
    }

    fn text_flags(&mut self, text: &str, x: f32, y: f32, color: u32, flags: u32) {
        let entry = if let Some(entry) = self.lines.get_mut(text).and_then(|entries| {
            entries
                .iter_mut()
                .find(|entry| entry.color == color && entry.flags == flags)
        }) {
            entry
        } else {
            let end = text.len() as u32;
            let run = Run {
                start: 0,
                end,
                color,
                flags,
            };
            let line = self.renderer.shape(text, &[run], &text_columns(text));
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
        self.renderer.line(&entry.line, x, y);
    }

    fn end_frame(&mut self) {
        self.lines.retain(|_, entries| {
            entries.retain_mut(|entry| std::mem::take(&mut entry.used));
            !entries.is_empty()
        });
    }
}

const fn opaque(color: u32) -> u32 {
    (color << 8) | 255
}

const CHROME: u32 = opaque(crate::panel::CHROME);
const BORDER: u32 = opaque(crate::panel::BORDER);
const INK: u32 = opaque(crate::panel::INK);
const MUTED: u32 = opaque(crate::panel::MUTED);
const ACCENT: u32 = opaque(crate::panel::ACCENT);
const DANGER: u32 = 0xe87575ff;
const WARNING: u32 = 0xfe9b30ff;
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
    panels: crate::ui::PanelLayout,
}

impl GuiLayout {
    fn new(
        width: f32,
        height: f32,
        cell: (f32, f32),
        agent_width: Option<f32>,
        files_open: bool,
    ) -> Self {
        let side_width = agent_width.map_or(0., |width_px| width_px.min(width * 0.62));
        let side = width - side_width;
        let panels = crate::ui::PanelLayout::new(
            (((side - TERMINAL_PADDING * 2.) / cell.0) as u16).max(2),
            (side / cell.0) as u16,
            files_open,
        );
        Self {
            left: f32::from(panels.file_width) * cell.0,
            top: cell.1,
            footer: height - cell.1 - 12.,
            side,
            width,
            height,
            panels,
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
            num_cols: self.panels.terminal_columns,
            num_lines: (((self.footer - self.top - TERMINAL_PADDING * 2.) / cell.1) as u16)
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
            (((pointer.0 - ox) / cell.0) as u16).min(size.num_cols - 1),
            (((pointer.1 - oy) / cell.1) as u16).min(size.num_lines - 1),
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
    Transcript,
    Editor,
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
    /// The bar along the right edge of a cell-grid rectangle showing `rect.rows` of `total` lines.
    fn in_rect(
        target: ScrollTarget,
        rect: crate::workspace::Rect,
        cell: (f32, f32),
        total: usize,
        offset: usize,
    ) -> Option<Self> {
        let bounds = (
            f32::from(rect.x + rect.cols) * cell.0 - 3.,
            f32::from(rect.y) * cell.1,
            3.,
            f32::from(rect.rows) * cell.1,
        );
        Self::new(target, bounds, total, usize::from(rect.rows), offset)
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
    Monitor(crate::monitor::Action),
    Tab(TabHit),
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

#[derive(Default)]
struct PaneCache {
    lines: LineCache,
    ime_cursor: Option<Point>,
}

impl PaneCache {
    fn ime_position(&mut self, screen: &crate::screen::Screen) -> (usize, usize) {
        let (cursor, shape) = screen.cursor();
        // Hidden cursors can visit drawing cells; retain this pane's insertion point.
        if shape != crate::vt::CursorShape::Hidden {
            self.ime_cursor = Some(cursor);
        }
        let cursor = self.ime_cursor.unwrap_or(cursor);
        (
            cursor.column.min(screen.columns() - 1),
            (cursor.line + screen.display_offset() as i32)
                .clamp(0, screen.screen_lines() as i32 - 1) as usize,
        )
    }
}

pub struct WorkspaceView {
    client: Client,
    ui: Ui,
    caches: HashMap<PaneId, PaneCache>,
    painter: Painter,
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
    control_hits: Vec<GuiHit>,
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
            painter: Painter {
                lines: HashMap::new(),
                renderer,
            },
            window,
            config,
            cell,
            scale,
            caches: HashMap::new(),
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
            control_hits: Vec::new(),
            trace,
        };
        view.resize()?;
        Ok(view)
    }
    pub fn run(mut self) -> Result<()> {
        let mut events = Vec::new();
        let mut pollfds = Vec::new();
        while self.running {
            let ready = self.painter.ready()?;
            let mut timeout = if self.dirty && ready && self.window.ready() {
                0
            } else {
                -1
            };
            if let Some(until) = self.ui.deadline() {
                let remaining =
                    crate::sys::timeout_millis(until.saturating_duration_since(Instant::now()));
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
                    fd: self.painter.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ]);
            pollfds.extend(self.ui.fds());
            self.window.wait(&mut pollfds, timeout, &mut events)?;
            if pollfds[0].revents & libc::POLLOUT != 0 {
                self.client.flush()?;
            }
            for event in events.drain(..) {
                let panels = self.ui.panel_state();
                self.handle(event)?;
                if panels != self.ui.panel_state() {
                    self.resize()?;
                }
            }
            self.client.flush()?;
            let previous = self.client.snapshot().map(|s| (s.focused, s.controller));
            let responses = if pollfds[0].revents
                & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                != 0
                || self.client.needs_poll()
            {
                self.client.poll()?
            } else {
                Vec::new()
            };
            for event in responses {
                match event {
                    Response::ScreenDirty => {
                        self.dirty = true;
                        self.ui.search.observe(&mut self.client)?;
                        let current = self.client.snapshot().map(|s| (s.focused, s.controller));
                        if previous != current {
                            for (pane, _) in previous.into_iter().chain(current) {
                                if let Some(screen) = self.client.screen_mut(pane) {
                                    screen.damage_cursor();
                                }
                            }
                            if let Some(snapshot) = self.client.snapshot() {
                                let ssh = if self.client.is_remote() {
                                    " · SSH"
                                } else {
                                    ""
                                };
                                let access = if snapshot.controller {
                                    ""
                                } else {
                                    " · read-only"
                                };
                                let title = format!("nextty — {}{ssh}{access}", snapshot.name);
                                self.window.title(&title)?;
                            }
                        }
                    }
                    Response::Clipboard {
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
                    Response::Search {
                        pane,
                        request,
                        count,
                        truncated,
                    } => {
                        self.dirty |= self.ui.search.result(
                            &mut self.client,
                            pane,
                            request,
                            count as usize,
                            truncated,
                        )?;
                    }
                    Response::Error(error) | Response::CopyError { error, .. } => {
                        self.message(error)
                    }
                    Response::Detached => self.running = false,
                    Response::ClipboardStore { primary, text } => {
                        self.window.selection(primary, &text)
                    }
                    _ => (),
                }
            }

            let revision = self.ui.agent.active().model.revision;
            let (changed, messages_changed) =
                self.ui.service(&mut self.client, &self.config, &pollfds)?;
            self.dirty |= changed;
            self.agent_refresh(revision, messages_changed);
            self.refresh_hover();
            if self.running && self.dirty && self.painter.ready()? && self.window.ready() {
                self.draw()?;
                self.dirty = false;
                self.client.request_screen()?;
            }
        }

        if self.client.snapshot().is_some() {
            self.ui.poll_agent(&mut self.client, &self.config)?;
        }
        self.client.flush()?;
        self.painter.finish()
    }

    fn message(&mut self, text: impl Into<String>) {
        self.ui.message(text);
        self.dirty = true;
    }
    fn damage_cursor(&mut self) {
        if let Some(pane) = self.client.focused()
            && let Some(screen) = self.client.screen_mut(pane)
        {
            screen.damage_cursor();
        }
    }
    fn input(&mut self, pane: PaneId, data: Vec<u8>) -> Result<()> {
        if !self.client.controller() {
            return Ok(());
        }
        if self.paste.is_some() {
            self.deferred.push_back(DeferredInput::Bytes(pane, data));
            return Ok(());
        }
        self.client.send(Action::Input { pane, data })
    }
    fn resize(&mut self) -> Result<()> {
        self.ui.search.close(&mut self.client)?;
        self.hover_cell = None;
        self.hovered = None;
        self.hovered_block = None;
        self.client
            .send(Action::Resize(self.layout().size(self.cell)))?;
        self.dirty = true;
        Ok(())
    }
    fn handle(&mut self, event: Event) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            trace.event(&event)?;
        }

        match event.kind {
            native::WINDOW_STATE => {
                self.maximized = event.code != 0;
                self.dirty = true;
            }
            native::KEY => self.key(event.code, event.modifiers, &event.text)?,
            native::TEXT => {
                if self.ui.agent.active().rewind_target.is_some() {
                    self.ui.cancel_rewind();
                }
                if self.ui.modal() || self.preview_open() {
                    return Ok(());
                }
                self.clear_preedit();
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
                if let Some(pane) = self.client.focused() {
                    self.input(pane, event.text.into_bytes())?;
                }
            }
            native::PREEDIT => {
                if self.ui.agent.open && matches!(self.ui.focus, Focus::Output | Focus::Controls) {
                    return Ok(());
                }
                if self.ui.resources.open && self.ui.focus == Focus::Monitor {
                    self.preedit.clear();
                    return Ok(());
                }
                if self.ui.modal() || self.preview_open() {
                    return Ok(());
                }
                self.preedit = event.text;
                self.dirty = true;
                self.damage_cursor();
            }
            native::RESIZE => self.resize()?,
            native::EXPOSE => self.dirty = true,
            native::CLOSE => self.open_close(),
            native::FOCUS => {
                self.focused = event.code != 0;
                if let Some(pane) = self.client.focused()
                    && self.client.mode(pane).contains(Mode::FOCUS_IN_OUT)
                {
                    let report: &[u8] = if self.focused { b"\x1b[I" } else { b"\x1b[O" };
                    self.input(pane, report.to_vec())?;
                }
                self.damage_cursor();
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

    fn clear_preedit(&mut self) {
        if !self.preedit.is_empty() {
            self.preedit.clear();
            self.damage_cursor();
            self.dirty = true;
        }
    }

    fn workspace_key(&mut self, symbol: u32, modifiers: u32) -> Result<bool> {
        if modifiers & (native::ALT | native::CONTROL | native::SUPER) != native::ALT {
            return Ok(false);
        }
        if !self.composition_finished() {
            return Ok(true);
        }
        let key = match symbol {
            keysym::RETURN | keysym::KP_ENTER => '\r',
            _ => match symbol_char(symbol) {
                Some(key) => key.to_ascii_lowercase(),
                None => return Ok(false),
            },
        };
        let handled = self.ui.alt(&mut self.client, key)?;
        self.dirty |= handled;
        Ok(handled)
    }
    fn key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<()> {
        if self.ui.modal() {
            self.panel_key(symbol, modifiers, "")?;
            return Ok(());
        }
        let Some(pane) = self.client.focused() else {
            return Ok(());
        };
        if modifiers == native::ALT && symbol_char(symbol) == Some('q') {
            self.running = false;
            return Ok(());
        }
        if self.workspace_key(symbol, modifiers)? {
            return Ok(());
        }

        let panel_text = if modifiers & (native::CONTROL | native::ALT | native::SUPER) == 0 {
            text
        } else {
            ""
        };
        if !panel_text.is_empty() {
            self.clear_preedit();
            if self.paste.is_some()
                && let Some(target) = self.ui.text_target()
            {
                self.deferred
                    .push_back(DeferredInput::Text(target, panel_text.into()));
                return Ok(());
            }
        }

        if self.ui.agent.open
            && self.ui.focus == Focus::Editor
            && (!self.preedit.is_empty() || self.paste.is_some())
        {
            return Ok(());
        }
        if self.panel_key(symbol, modifiers, panel_text)? {
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
                return self.client.send(Action::Scroll { pane, scroll });
            }
        }
        if self.preedit.is_empty()
            && let Some(data) = key_bytes(symbol, modifiers, text, || self.client.mode(pane))
        {
            self.input(pane, data)?;
        }
        Ok(())
    }
    fn panel_key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<bool> {
        use crate::input::Key;
        let mut viewport = (1, 1);
        if self.ui.files_open && self.ui.focus == Focus::Files {
            let layout = self.layout();
            viewport = self
                .ui
                .file_viewport(self.file_rect(layout), self.file_preview_layout(layout));
        } else if let Some(layout) = self.editor_viewport() {
            viewport = (layout.columns, layout.rows);
        }

        let action = self.ui.key(
            &mut self.client,
            Key {
                symbol,
                modifiers,
                text,
            },
            viewport,
        )?;
        match action {
            KeyResult::Unhandled => return Ok(false),
            KeyResult::Copy(text) => self.window.selection(false, &text),
            KeyResult::Paste => {
                if let Some(target) = self.ui.paste_target() {
                    self.request_paste(target, false);
                }
            }
            KeyResult::Quit => self.running = false,
            KeyResult::Handled => (),
        }
        self.dirty = true;
        Ok(true)
    }

    fn copy(&mut self, pane: PaneId, primary: bool) -> Result<()> {
        self.client.send(Action::CopySelection {
            pane,
            request: u64::from(primary),
        })
    }
    fn paste(&mut self, pane: PaneId, primary: bool) {
        if !self.client.controller() {
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
                bracketed: self.client.mode(pane).contains(Mode::BRACKETED_PASTE),
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
            PasteTarget::Pane { pane, bracketed } => self.client.send(Action::Input {
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
                DeferredInput::Bytes(pane, data) => {
                    self.client.send(Action::Input { pane, data })?
                }
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
        let mut layout = GuiLayout::new(
            width as f32,
            height as f32,
            self.cell,
            agent_width,
            self.ui.files_open,
        );
        if self.ui.resources.open {
            let total = ((layout.height - layout.top - 12.) / self.cell.1) as u16;
            layout.footer -= f32::from(self.ui.resources.rows(total)) * self.cell.1 + 6.;
        }
        layout
    }
    fn cell(&self) -> (u16, u16) {
        (
            (self.pointer.0 / self.cell.0) as u16,
            (self.pointer.1 / self.cell.1) as u16,
        )
    }
    fn close_rect(&self) -> crate::workspace::Rect {
        let (width, height, _) = self.window.size();
        crate::ui::close_rect(
            (width as f32 / self.cell.0) as u16,
            (height as f32 / self.cell.1) as u16,
        )
    }
    fn tab_bar(
        &self,
        snapshot: &crate::workspace::Snapshot,
        layout: GuiLayout,
    ) -> crate::ui::TabBar {
        let columns = ((layout.width - self.control_width()) / self.cell.0) as u16;
        crate::ui::tab_bar(
            snapshot,
            columns,
            if self.client.is_remote() { "SSH" } else { "" },
        )
    }
    fn control_at(&self, point: (f32, f32)) -> Option<GuiHit> {
        self.control_hits
            .iter()
            .rfind(|hit| hit.contains(point))
            .copied()
    }
    fn preview_open(&self) -> bool {
        self.ui.agent.active().context.preview.is_some()
    }
    fn scrollbar_visible(&self, target: ScrollTarget) -> bool {
        self.preview_open() == (target == ScrollTarget::Preview)
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
        let row = (((self.pointer.1 - oy) / self.cell.1 - f32::from(rect.y)) as usize)
            .min(usize::from(rect.rows - 1));
        let x = (self.pointer.0 - ox) / self.cell.0 - f32::from(rect.x);
        Some((
            pane.id,
            Point {
                line: row as i32 - screen.display_offset() as i32,
                column: (x as usize).min(screen.columns().saturating_sub(1)),
            },
            if x < screen.columns() as f32 && x.fract() < 0.5 {
                Side::Left
            } else {
                Side::Right
            },
        ))
    }
    /// The pane under the pointer, or the one a drag or mouse report keeps capturing.
    fn captured_hit(&self) -> Option<(PaneId, Point, Side)> {
        let captured = self.selecting.map(|drag| drag.pane).or(self.reported_pane);
        captured
            .and_then(|pane| self.hit(Some(pane)))
            .or_else(|| self.hit(None))
    }
    fn refresh_hover(&mut self) {
        let layout = self.layout();
        let unobstructed = self.ui.close_prompt.is_none() && !self.preview_open();
        let monitor = self.ui.resources.open && unobstructed;
        let top = layout.footer + 6.;
        let pointer = (monitor
            && (PADDING..layout.width - PADDING).contains(&self.pointer.0)
            && (top..layout.height - self.cell.1 - 12.).contains(&self.pointer.1))
        .then(|| {
            (
                ((self.pointer.0 - PADDING) / self.cell.0) as u16,
                ((self.pointer.1 - top) / self.cell.1).floor() as i32,
            )
        });
        if self.ui.resources.pointer != pointer {
            self.ui.resources.pointer = pointer;
            self.dirty = true;
        }
        let monitor_divider =
            monitor && (self.ui.resources.dragging || (self.pointer.1 - layout.footer).abs() <= 5.);
        let agent_divider = !self.ui.files_open
            && (self.agent.drag.is_some()
                || (unobstructed && layout.divider_contains(self.pointer)));
        self.window.resize_cursor(agent_divider, monitor_divider);

        let hit = self.hit(None).map(|(pane, point, _)| (pane, point));
        let block = hit
            .filter(|_| self.ui.close_prompt.is_none())
            .and_then(|(pane, point)| crate::ui::block_at(&self.client, pane, point));
        if block != self.hovered_block {
            self.hovered_block = block;
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
            GuiAction::Monitor(action) => self.ui.monitor_action(&mut self.client, action)?,
            GuiAction::Tab(hit) => {
                if self.composition_finished() && self.client.controller() {
                    self.ui.tab_click(&mut self.client, hit)?;
                }
            }
            GuiAction::Minimize => self.window.minimize(),
            GuiAction::Maximize => self.window.maximize(!self.maximized),
            GuiAction::Close => self.open_close(),
            GuiAction::Agent(action) => {
                if !self.agent_action(action) {
                    return Ok(());
                }
            }
        }
        self.control_hits.clear();
        self.dirty = true;
        Ok(())
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
        let mode = self.client.mode(pane);
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
                self.file_press();
            }
            return Ok(());
        }
        if !pressed {
            return self.button_release(button, modifiers);
        }
        if let Some(prompt) = self.ui.close_prompt {
            let (column, row) = self.cell();
            if button == 1
                && let Some(terminate) =
                    crate::ui::close_hit(self.close_rect(), prompt, column, row)
            {
                if let KeyResult::Quit = self.ui.choose_close(&mut self.client, terminate)? {
                    self.running = false;
                }
                self.dirty = true;
            }
            return Ok(());
        }
        let layout = self.layout();
        if self.pointer.1 < layout.top
            && let Some(action) =
                self.control_at(self.pointer)
                    .map(|hit| hit.action)
                    .filter(|action| {
                        matches!(
                            action,
                            GuiAction::Minimize | GuiAction::Maximize | GuiAction::Close
                        )
                    })
        {
            if button == 1 {
                self.gui_action(action)?;
            }
            return Ok(());
        }
        if button == 1
            && self.pointer.1 < layout.top
            && let Some(snapshot) = self.client.snapshot()
            && let Some(hit) = self.tab_bar(snapshot, layout).at(snapshot, self.cell().0)
        {
            self.gui_action(GuiAction::Tab(hit))?;
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
            if button == 1 {
                self.file_press();
            }
            return Ok(());
        }
        if let Some(action) = crate::ui::agent_hit(&self.agent.hits, self.cell().0, self.cell().1) {
            if self.agent_control_button(button, action, modifiers)? {
                return Ok(());
            }
            if button == 1 {
                self.gui_action(GuiAction::Agent(action))?;
            }
            return Ok(());
        }
        if let Some(hit) = self.control_at(self.pointer) {
            if button == 1 {
                self.gui_action(hit.action)?;
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
            && self.client.controller()
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
            && let Some((pane, point, _)) = self.hit(None)
        {
            if let Some(object) = self
                .client
                .screens()
                .get(&pane)
                .and_then(|screen| context::object_at(screen, point))
            {
                self.window.selection(false, &object.text);
                self.message("Copied");
                return Ok(());
            }
            if let Some((_, block)) = crate::ui::block_at(&self.client, pane, point) {
                self.client.send(Action::CopyBlock {
                    pane,
                    block,
                    request: 0,
                })?;
                return Ok(());
            }
            if modifiers & native::ALT != 0 || !self.client.mode(pane).intersects(Mode::MOUSE_MODE)
            {
                self.paste(pane, false);
                return Ok(());
            }
        }
        self.terminal_button(button, modifiers)
    }

    fn button_release(&mut self, button: u32, modifiers: u32) -> Result<()> {
        if self.agent_button_release(button)? {
            return Ok(());
        }

        if button == 1 {
            if std::mem::take(&mut self.ui.resources.dragging) {
                self.refresh_hover();
                return Ok(());
            }
            if self.scrollbar_drag.take().is_some() {
                self.dirty = true;
                return Ok(());
            }
            if self.split_drag.take().is_some() {
                return Ok(());
            }
            if let Some(drag) = self.selecting.take() {
                if let Some((_, point, side)) = self.hit(Some(drag.pane)) {
                    let pane = drag.pane;
                    self.client
                        .send(Action::SelectionUpdate { pane, point, side })?;
                    if drag.kind != SelectionType::Simple
                        || point != drag.point
                        || side != drag.side
                    {
                        self.copy(pane, true)?;
                    }
                }
                return Ok(());
            }
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
            self.ui.monitor_drag();
            self.dirty = true;
            self.refresh_hover();
            return Ok(true);
        }
        if self.pointer.1 < layout.footer {
            self.ui.focus = Focus::Terminal;
            self.ui.resources.searching = false;
            return Ok(false);
        }
        if let Some(hit) = self.control_at(self.pointer) {
            let search = hit.action == GuiAction::Monitor(crate::monitor::Action::Search);
            if button == 1 || button == 2 && search {
                self.gui_action(hit.action)?;
            }
            if button == 2 && search {
                self.request_paste(PasteTarget::Monitor, true);
            }
        }
        Ok(true)
    }

    fn terminal_button(&mut self, button: u32, modifiers: u32) -> Result<()> {
        if !self.client.controller() {
            return Ok(());
        }
        let Some((pane, point, side)) = self.captured_hit() else {
            return Ok(());
        };
        if self.client.focused() != Some(pane) {
            if !self.preedit.is_empty() {
                return Ok(());
            }
            self.ui.act(&mut self.client, Action::Focus(pane))?;
            self.dirty = true;
        }
        let mode = self.client.mode(pane);
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
                crate::ui::open_object(&self.client, pane, &object.text)?;
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
            self.client.send(Action::SelectionStart {
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
        let hovered_control = |point| self.control_at(point).map(|hit| hit.action);
        if hovered_bar(previous) != hovered_bar(pointer)
            || hovered_control(previous) != hovered_control(pointer)
        {
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
            let total = ((layout.height - layout.top - 12.) / self.cell.1) as u16;
            let rows = ((bottom - pointer.1) / self.cell.1).round() as u16;
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
                self.client.send(drag.action(column, row))?;
            }
            return Ok(());
        }
        self.refresh_hover();
        if let Some((pane, point, side)) = self.captured_hit() {
            if self.selecting.is_some_and(|drag| drag.pane == pane) {
                self.client
                    .send(Action::SelectionUpdate { pane, point, side })?;
            } else {
                let mode = self.client.mode(pane);
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
                    self.client.send(Action::Scroll {
                        pane,
                        scroll: Scroll::Top,
                    })?;
                } else {
                    self.client.scroll_to(pane, offset)?;
                }
            }
            ScrollTarget::Pane(_) => return Ok(()),

            ScrollTarget::Transcript => self.ui.agent.active_mut().model.scroll = offset,
            ScrollTarget::Editor => {
                self.ui.agent.active_mut().editor_scroll = Some(drag.bar.maximum() - offset)
            }
            ScrollTarget::Preview => {
                if let Some(preview) = &mut self.ui.agent.active_mut().context.preview {
                    preview.scroll = drag.bar.maximum() - offset;
                }
            }
        }
        self.dirty = true;
        Ok(())
    }
    fn scroll(&mut self, amount: f32, modifiers: u32) -> Result<()> {
        self.scroll_remainder += amount;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return Ok(());
        }
        let step = lines as isize * WHEEL_LINES as isize;
        if self.ui.file_preview_open() {
            self.file_wheel(step);
            return Ok(());
        }
        if self.scrollbar_drag.is_some() {
            return Ok(());
        }

        if self.agent_scroll(-step) {
            return Ok(());
        }

        if self.ui.resources.open && self.pointer.1 >= self.layout().footer {
            let scroll = &mut self.ui.resources.scroll;
            *scroll = scroll.saturating_add_signed(-step);
            self.dirty = true;
            return Ok(());
        }
        if self.ui.close_prompt.is_some() {
            return Ok(());
        }

        if self.ui.files_open && self.pointer.0 < self.layout().left {
            self.file_wheel(step);
            return Ok(());
        }
        let Some((pane, point, _)) = self.hit(None) else {
            return Ok(());
        };
        let mode = self.client.mode(pane);
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
            let bytes = if lines > 0 { *b"\x1b[A" } else { *b"\x1b[B" };
            let bytes = crate::ui::cursor_bytes(bytes, mode);
            self.input(pane, bytes.repeat(lines.unsigned_abs() as usize * 3))?;
        } else {
            self.client.send(Action::Scroll {
                pane,
                scroll: Scroll::Delta(lines.saturating_mul(3)),
            })?;
        }
        Ok(())
    }
    fn draw(&mut self) -> Result<()> {
        let scale = self.window.size().2;
        if scale != self.scale {
            self.scale = scale;
            self.caches.clear();
            self.painter.lines.clear();
        }
        let background =
            (self.config.background << 8) | (self.config.background_opacity * 255.).round() as u32;
        self.painter.begin(background);
        let layout = self.layout();
        let (grid_x, grid_y) = layout.origin(self.cell);
        self.control_hits.clear();
        self.scrollbars.clear();
        self.chrome(layout);
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
                self.painter.clip(origin.0, origin.1, w, h);
                let active = pane.id == snapshot.focused && self.focused && snapshot.controller;
                let cache = self.caches.entry(pane.id).or_default();
                if let Some(screen) = screens.get_mut(&pane.id) {
                    let (ime_column, ime_row) = cache.ime_position(screen);
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
                        &cache.lines,
                        active,
                        pane.id == snapshot.focused
                            && (!terminal_input || !self.preedit.is_empty()),
                    );
                    if frame.background >> 8 != self.config.background {
                        self.painter.clear(
                            (frame.background & !255)
                                | (self.config.background_opacity * 255.).round() as u32,
                        );
                    }

                    if let Some((id, block)) = self.hovered_block
                        && id == pane.id
                    {
                        for row in 0..screen.screen_lines() {
                            if screen
                                .row_output(pane, row)
                                .is_some_and(|current| current.id == block)
                            {
                                self.painter.rect(
                                    origin.0,
                                    origin.1 + row as f32 * self.cell.1,
                                    w,
                                    self.cell.1,
                                    0x262626ff,
                                );
                            }
                        }
                    }
                    frame.paint(
                        self.cell,
                        active,
                        &mut cache.lines,
                        &self.painter,
                        origin,
                        (w, h),
                    );
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
                            self.painter.rect(x, y, w, self.cell.1, 0x45605266);
                        }
                    }
                    if pane.id == snapshot.focused && terminal_input {
                        let x = origin.0 + ime_column as f32 * self.cell.0;
                        let y = origin.1 + ime_row as f32 * self.cell.1;
                        if !self.preedit.is_empty() {
                            let count = text_width(&self.preedit) as f32;
                            self.painter.rect(
                                x,
                                y,
                                self.cell.0 * count,
                                self.cell.1,
                                opaque(self.config.background),
                            );
                            self.painter
                                .text(&self.preedit, x, y, opaque(self.config.foreground));
                            self.painter.rect(
                                x,
                                y + self.cell.1 - 2.,
                                self.cell.0 * count,
                                1.,
                                0x9c8fdcff,
                            );
                        }
                        // Anchor to the insertion point while the IME replaces its preedit text.
                        self.window.cursor_rect(
                            x as i32,
                            y as i32,
                            self.cell.0 as i32,
                            self.cell.1 as i32,
                        );
                    }
                }
                self.painter.clip(0., 0., 0., 0.);
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
                    self.painter.rect(
                        grid_x + left * self.cell.0,
                        grid_y + top * self.cell.1,
                        1.,
                        (end_y - top) * self.cell.1,
                        0x303030ff,
                    );
                }
                if pane.rect.y > 1 {
                    self.painter.rect(
                        grid_x + left * self.cell.0,
                        grid_y + top * self.cell.1,
                        (end_x - left) * self.cell.0,
                        1.,
                        0x303030ff,
                    );
                }
            }
        }
        self.draw_window_controls(layout);
        self.footer(layout);
        self.painter
            .clip(0., layout.top, layout.width, layout.footer - layout.top);
        let divider = opaque(mix_color(
            self.config.foreground,
            self.config.background,
            24,
        ));
        let height = layout.footer - layout.top;
        if self.ui.files_open {
            let x = (self.file_rect(layout).cols as f32 - 0.5) * self.cell.0;
            self.painter.rect(x, layout.top, 1., height, divider);
        }
        if self.ui.agent.open {
            let x = ((layout.side / self.cell.0).floor() + 0.5) * self.cell.0;
            self.painter.rect(x, layout.top, 1., height, divider);
        }
        self.painter.clip(0., 0., 0., 0.);

        if self.ui.files_open && !self.ui.file_preview_open() {
            self.draw_files(layout);
        }
        self.draw_agent(layout)?;
        self.draw_scrollbars();
        if self.ui.file_preview_open() {
            self.draw_files(layout);
        }
        if self.ui.search.open {
            self.draw_search(layout);
        }
        if let Some(prompt) = self.ui.close_prompt {
            let name = self.client.snapshot().map_or("", |s| s.name.as_str());
            let pointer = Some(self.cell());
            let grid =
                crate::ui::close_grid(prompt, name, self.close_rect(), pointer, &self.config);
            self.draw_panel(&grid);
        }
        self.painter.end_frame();
        self.painter.present();
        if let Some(trace) = &mut self.trace {
            let marker = self
                .client
                .snapshot()
                .and_then(|snapshot| self.client.screens().get(&snapshot.focused))
                .map_or(0, |screen| screen.color(screen.row(0).cells[0].bg).packed());
            trace.frame(marker, self.focused)?;
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
    fn file_press(&mut self) {
        let layout = self.layout();
        let (column, row) = self.cell();
        self.ui.file_press(
            self.file_rect(layout),
            self.file_preview_layout(layout),
            column,
            row,
        );
        self.dirty = true;
    }
    fn file_wheel(&mut self, step: isize) {
        let layout = self.layout();
        self.ui.file_wheel(
            -step,
            self.file_rect(layout),
            self.file_preview_layout(layout),
        );
        self.dirty = true;
    }
    fn file_rect(&self, layout: GuiLayout) -> crate::workspace::Rect {
        rect(
            0,
            1,
            layout.panels.file_width,
            (((layout.footer - layout.top) / self.cell.1) as u16).max(1),
        )
    }

    fn file_preview_layout(&self, layout: GuiLayout) -> crate::workspace::Rect {
        crate::file_panel::preview_rect(rect(
            0,
            1,
            (layout.width / self.cell.0) as u16,
            ((layout.footer - layout.top) / self.cell.1) as u16,
        ))
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn ime_anchor_stays_with_each_pane_when_tui_cursors_are_hidden() {
        use crate::{core::Core, screen::Screen, screen::Update, vt::Parser};

        let mut core = Core::new(
            WindowSize {
                num_cols: 40,
                num_lines: 12,
                cell_width: 8,
                cell_height: 16,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = Parser::new();
        let mut screen = Screen::default();
        let mut left = PaneCache::default();
        let mut right = PaneCache::default();
        parser.advance(&mut core, b"\x1b[3;5H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(left.ime_position(&screen), (4, 2));
        parser.advance(&mut core, b"\x1b[9;7H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(right.ime_position(&screen), (6, 8));

        parser.advance(&mut core, b"\x1b[?25l\x1b[12;40H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(left.ime_position(&screen), (4, 2));
        assert_eq!(right.ime_position(&screen), (6, 8));

        parser.advance(&mut core, b"\x1b[?25h\x1b[10;8H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(right.ime_position(&screen), (7, 9));
        assert_eq!(left.ime_cursor, Some(Point { line: 2, column: 4 }));
    }

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
    }
}
