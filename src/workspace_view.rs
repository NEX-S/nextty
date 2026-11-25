mod render;

use crate::input::{
    ALT, CONTROL, KeyResult, LEFT, PasteTarget, PointerKind, SHIFT, SUPER, key_bytes, keysym,
    paste_bytes, symbol_char,
};
use crate::native::{self, Event, Line, Renderer, Run, Window, trace::Trace};
use crate::paint::{LineCache, text_columns};
use crate::panel::{MIX_SOFT, mix_color, rect};
use crate::{
    config::Config,
    context,
    core::{Mode, Point, Scroll, Side, WindowSize},
    ui::{Area, ClosePrompt, Effect, Focus, Pointer, TabHit, Ui, pane_point},
    width::text_width,
    workspace::{Action, Client, PaneId, Response},
};
use std::{
    collections::{HashMap, VecDeque},
    io::Result,
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
            let run = Run {
                start: 0,
                end: text.len() as u32,
                color,
                flags,
            };
            let line = Line::new(text, &[run], &text_columns(text));
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
const DANGER: u32 = opaque(crate::panel::DANGER);
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

    /// The top of the status bar, below the panes and the monitor.
    fn status(self, cell: (f32, f32)) -> f32 {
        self.height - cell.1 - 12.
    }
    /// Rows available to the monitor, counting the status bar it displaces.
    fn monitor_rows(self, cell: (f32, f32)) -> u16 {
        ((self.height - self.top - 12.) / cell.1) as u16
    }
    /// Rows between the tab row and the status bar.
    fn body_rows(self, cell: (f32, f32)) -> u16 {
        ((self.footer - self.top) / cell.1).max(1.) as u16
    }
    /// The window without its tab row and status bar; previews float over it.
    fn canvas(self, cell: (f32, f32)) -> crate::workspace::Rect {
        rect(
            0,
            1,
            (self.width / cell.0) as u16,
            (self.height / cell.1) as u16 - 2,
        )
    }
    fn divider_contains(self, point: (f32, f32)) -> bool {
        self.side < self.width
            && point.0 >= self.side
            && point.0 < self.side + 8.
            && (self.top..self.footer).contains(&point.1)
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
    Tab(TabHit),
    Minimize,
    Maximize,
    Close,
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

pub struct WorkspaceView {
    client: Client,
    ui: Ui,
    caches: HashMap<PaneId, LineCache>,
    painter: Painter,
    window: Window,
    config: Config,
    cell: (f32, f32),
    scale: f64,
    dirty: bool,
    running: bool,
    focused: bool,
    pointer: (f32, f32),
    scroll_remainder: f32,
    preedit: String,
    paste: Option<PasteTarget>,
    deferred: VecDeque<DeferredInput>,
    scrollbars: Vec<Scrollbar>,
    scrollbar_drag: Option<ScrollbarDrag>,
    maximized: bool,
    /// The controller state the window title was last built for.
    access: Option<bool>,
    hovered: Option<(PaneId, crate::context::Object)>,
    hovered_block: Option<(PaneId, u64)>,
    hover_cell: Option<(PaneId, Point)>,
    control_hits: Vec<GuiHit>,
    trace: Option<Trace>,
    /// Where the pointer grabbed the Agent panel's divider, from its left edge.
    agent_drag: Option<f32>,
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
            scroll_remainder: 0.,
            preedit: String::new(),
            paste: None,
            deferred: VecDeque::new(),
            agent_drag: None,
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            maximized: false,
            access: None,
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
                timeout = crate::sys::timeout_until(timeout, until);
            }

            if self.client.needs_poll() {
                timeout = 0;
            }
            self.client.flush()?;
            pollfds.clear();
            pollfds.extend([
                self.client.pollfd(),
                crate::sys::watch(self.painter.fd(), libc::POLLIN),
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
            let responses = if pollfds[0].revents
                & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                != 0
                || self.client.needs_poll()
            {
                self.client.poll()?
            } else {
                Vec::new()
            };
            for response in responses {
                match self.ui.respond(&mut self.client, response)? {
                    None | Some(Response::ScreenDirty) => self.dirty = true,
                    Some(Response::Clipboard {
                        primary,
                        file,
                        incomplete,
                    }) => {
                        self.window.selection_file(primary, file);
                        self.message(if incomplete {
                            "Copied retained output; history is incomplete"
                        } else {
                            "Copied"
                        });
                    }
                    Some(Response::Detached) => self.running = false,
                    Some(_) => (),
                }
            }

            self.dirty |= self.ui.service(&mut self.client, &self.config, &pollfds)?;
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
    fn copy_text(&mut self, text: &str) -> Result<()> {
        self.window.selection(false, text)?;
        self.message("Copied");
        Ok(())
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
                self.dirty = true;
            }
            native::MOTION => self.motion((event.x as f32, event.y as f32), event.modifiers)?,
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
            self.dirty = true;
        }
    }

    fn workspace_key(&mut self, symbol: u32, modifiers: u32) -> Result<bool> {
        if modifiers & (ALT | CONTROL | SUPER) != ALT {
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
        if modifiers == ALT && symbol_char(symbol) == Some('q') {
            self.running = false;
            return Ok(());
        }
        if self.workspace_key(symbol, modifiers)? {
            return Ok(());
        }

        let panel_text = if modifiers & (CONTROL | ALT | SUPER) == 0 {
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
        if modifiers & (CONTROL | SHIFT) == CONTROL | SHIFT {
            match symbol_char(symbol).map(|c| c.to_ascii_lowercase()) {
                Some('c') => {
                    let primary = false;
                    return self.client.send(Action::CopySelection { pane, primary });
                }
                Some('v') => {
                    self.paste(pane, false);
                    return Ok(());
                }
                _ => (),
            }
        }

        if modifiers & (SHIFT | CONTROL | ALT) == SHIFT {
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
        let key = crate::input::Key {
            symbol,
            modifiers,
            text,
        };
        match self.ui.key(&mut self.client, key)? {
            KeyResult::Unhandled => return Ok(false),
            KeyResult::Copy(text) => self.window.selection(false, &text)?,
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

    fn paste(&mut self, pane: PaneId, primary: bool) {
        if !self.client.controller() || !self.composition_finished() {
            return;
        }
        let bracketed = self.client.mode(pane).contains(Mode::BRACKETED_PASTE);
        self.request_paste(PasteTarget::Pane { pane, bracketed }, primary);
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
            let total = layout.monitor_rows(self.cell);
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
    /// The area under `point` or holding the pointer, and the pointer's cell in that area's grid.
    fn locate(&self, (x, y): (f32, f32)) -> (Area, u16, u16, Side) {
        let layout = self.layout();
        let area = self.ui.capture().unwrap_or(if y < layout.top {
            Area::Tabs
        } else if self.ui.resources.open && (y - layout.footer).abs() <= 5. {
            Area::Divider
        } else if y >= layout.status(self.cell) {
            Area::Footer
        } else if y >= layout.footer {
            Area::Monitor
        } else if x >= layout.side {
            Area::Agent
        } else if x < layout.left {
            Area::Files
        } else {
            Area::Terminal
        });
        let (x, y) = match area {
            Area::Terminal => {
                let (ox, oy) = layout.origin(self.cell);
                (x - ox, y - oy)
            }
            Area::Monitor => (x - PADDING, y - layout.footer - 6.),
            Area::Footer => (x - layout.width % self.cell.0, 0.),
            _ => (x, y),
        };
        let column = (x / self.cell.0).max(0.);
        let side = if column.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        (area, column as u16, (y / self.cell.1).max(0.) as u16, side)
    }
    fn refresh_hover(&mut self) {
        let layout = self.layout();
        let unobstructed = self.ui.close_prompt.is_none() && !self.preview_open();
        let monitor_divider = self.ui.resources.open
            && unobstructed
            && (self.ui.resources.dragging || (self.pointer.1 - layout.footer).abs() <= 5.);
        let agent_divider = !self.ui.files_open
            && (self.agent_drag.is_some()
                || (unobstructed && layout.divider_contains(self.pointer)));
        self.window.resize_cursor(agent_divider, monitor_divider);

        let hit = match self.locate(self.pointer) {
            (Area::Terminal, column, row, _) => {
                pane_point(&self.client, column, row, Side::Left, None)
                    .map(|(pane, point, _)| (pane, point))
            }
            _ => None,
        };
        let block = hit.and_then(|(pane, point)| crate::ui::block_at(&self.client, pane, point));
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
        self.ui.agent.active_mut().context.menu = false;
        match action {
            GuiAction::Tab(hit) => {
                if self.composition_finished() && self.client.controller() {
                    self.ui.tab_click(&mut self.client, hit)?;
                }
            }
            GuiAction::Minimize => self.window.minimize(),
            GuiAction::Maximize => self.window.maximize(!self.maximized),
            GuiAction::Close => self.open_close(),
        }
        self.control_hits.clear();
        self.dirty = true;
        Ok(())
    }

    /// Hands a pointer event to the shared rules and carries out what they leave to the window.
    fn pointer_event(&mut self, kind: PointerKind, modifiers: u32) -> Result<()> {
        let (area, column, row, side) = self.locate(self.pointer);
        let pointer = Pointer {
            kind,
            modifiers,
            area,
            column,
            row,
            side,
        };
        match self.ui.pointer(&mut self.client, pointer)? {
            Effect::None => (),
            Effect::Input(pane, data) => self.input(pane, data)?,
            Effect::Copy(text) => self.copy_text(&text)?,
            Effect::Primary(text) => self.window.selection(true, &text)?,
            Effect::Selected(pane) => {
                let primary = true;
                self.client.send(Action::CopySelection { pane, primary })?;
            }
            Effect::Paste(target, primary) => self.request_paste(target, primary),
            Effect::Quit => self.running = false,
        }
        Ok(())
    }
    /// Window controls, scrollbars and the Agent panel's divider are the window's own; the
    /// close dialog takes every click.
    fn button(&mut self, code: u32, pressed: bool, modifiers: u32) -> Result<()> {
        // xterm numbers the left, middle and right buttons 0, 1 and 2.
        let Some(button) = [1, 2, 3].iter().position(|&button| button == code) else {
            return Ok(());
        };
        let button = button as u8;
        self.dirty = true;
        if !pressed {
            if button == LEFT
                && (self.scrollbar_drag.take().is_some() || self.agent_drag.take().is_some())
            {
                return Ok(());
            }
            return self.pointer_event(PointerKind::Release(button), modifiers);
        }
        if self.ui.close_prompt.is_none() {
            if let Some(hit) = self.control_at(self.pointer) {
                if button == LEFT {
                    self.gui_action(hit.action)?;
                }
                return Ok(());
            }
            if button == LEFT
                && let Some(bar) =
                    self.scrollbars.iter().copied().rfind(|bar| {
                        bar.contains(self.pointer) && self.scrollbar_visible(bar.target)
                    })
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
                return self.drag_scrollbar(self.pointer.1);
            }
        }
        if !self.composition_finished() {
            return Ok(());
        }
        let layout = self.layout();
        if button == LEFT && self.ui.capture().is_none() && layout.divider_contains(self.pointer) {
            self.agent_drag = Some(self.pointer.0 - layout.side);
            return Ok(());
        }
        self.pointer_event(PointerKind::Press(button), modifiers)
    }
    fn motion(&mut self, point: (f32, f32), modifiers: u32) -> Result<()> {
        let previous = std::mem::replace(&mut self.pointer, point);
        let hovered_bar = |point| self.scrollbars.iter().position(|bar| bar.contains(point));
        let hovered_control = |point| self.control_at(point).map(|hit| hit.action);
        if hovered_bar(previous) != hovered_bar(point)
            || hovered_control(previous) != hovered_control(point)
        {
            self.dirty = true;
        }
        let layout = self.layout();
        if let Some(grab) = self.agent_drag {
            let minimum = self.cell.0 * 24. + PADDING * 2.;
            let width =
                (layout.width - point.0 + grab).clamp(minimum, (layout.width * 0.62).max(minimum));
            if Some(width) != self.ui.agent.width {
                self.ui.agent.width = Some(width);
                if self.layout().size(self.cell) != layout.size(self.cell) {
                    self.resize()?;
                }
                self.dirty = true;
            }
            return Ok(());
        }
        if self.scrollbar_drag.is_some() {
            return self.drag_scrollbar(point.1);
        }
        if self.ui.resources.dragging {
            let bottom = layout.status(self.cell);
            let total = layout.monitor_rows(self.cell);
            let rows = ((bottom - point.1) / self.cell.1).round() as u16;
            if self.ui.resources.resize(rows, total) {
                self.resize()?;
            }
            return Ok(());
        }
        // Panels light what the pointer is over; the panes do through `refresh_hover`.
        let (area, column, row, _) = self.locate(point);
        let (old_area, old_column, old_row, _) = self.locate(previous);
        self.dirty |=
            area != Area::Terminal && (area, column, row) != (old_area, old_column, old_row);
        self.pointer_event(PointerKind::Motion, modifiers)
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
        if lines == 0 || self.scrollbar_drag.is_some() {
            return Ok(());
        }
        self.dirty = true;
        self.pointer_event(PointerKind::Wheel(lines), modifiers)
    }
    fn draw(&mut self) -> Result<()> {
        let scale = self.window.size().2;
        if scale != self.scale {
            self.scale = scale;
            self.caches.clear();
            self.painter.lines.clear();
        }
        if let Some(snapshot) = self.client.snapshot()
            && self.access != Some(snapshot.controller)
        {
            self.access = Some(snapshot.controller);
            let remote = if self.client.is_remote() {
                " · SSH"
            } else {
                ""
            };
            let access = if snapshot.controller {
                ""
            } else {
                " · read-only"
            };
            self.window
                .title(&format!("nextty — {}{remote}{access}", snapshot.name))?;
        }
        let alpha = (self.config.background_opacity * 255.).round() as u32;
        self.painter.begin((self.config.background << 8) | alpha);
        let layout = self.layout();
        let (grid_x, grid_y) = layout.origin(self.cell);
        self.control_hits.clear();
        self.scrollbars.clear();
        self.chrome(layout);
        let terminal_input = self.ui.terminal_input();
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
                    if !screen.mode().contains(Mode::ALT_SCREEN) {
                        let right_edge = pane.rect.x + pane.rect.cols == right;
                        let x = if right_edge {
                            layout.side
                        } else {
                            origin.0 + w
                        };
                        let lines = screen.screen_lines();
                        let total = screen.history_size() + lines;
                        let target = ScrollTarget::Pane(pane.id);
                        let bounds = (x - 3., origin.1, 3., h);
                        let bar = Scrollbar::new(target, bounds, total, lines, offset);
                        self.scrollbars.extend(bar);
                    }
                    let hide_cursor = pane.id == snapshot.focused
                        && (!terminal_input || !self.preedit.is_empty());
                    let background = cache.update(screen, active, hide_cursor);
                    if background >> 8 != self.config.background {
                        self.painter.clear((background & !255) | alpha);
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
                                    opaque(crate::panel::BLOCK_HOVER),
                                );
                            }
                        }
                    }
                    cache.paint(&self.painter, self.cell, origin, (w, h));
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
                        BORDER,
                    );
                }
                if pane.rect.y > 1 {
                    self.painter.rect(
                        grid_x + left * self.cell.0,
                        grid_y + top * self.cell.1,
                        (end_x - left) * self.cell.0,
                        1.,
                        BORDER,
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
            MIX_SOFT,
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
        let name = self.client.snapshot().map_or("", |s| s.name.as_str());
        let (columns, rows) = (layout.width / self.cell.0, layout.height / self.cell.1);
        let (columns, rows, pointer) = (columns as u16, rows as u16, Some(self.cell()));
        if let Some(grid) = self
            .ui
            .close_dialog(columns, rows, name, pointer, &self.config)
        {
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
    fn file_rect(&self, layout: GuiLayout) -> crate::workspace::Rect {
        rect(0, 1, layout.panels.file_width, layout.body_rows(self.cell))
    }

    fn file_preview_layout(&self, layout: GuiLayout) -> crate::workspace::Rect {
        crate::file_panel::preview_rect(layout.canvas(self.cell))
    }
}
