use crate::{
    config::Config,
    core::{Mode, Point, Scroll, SelectionType, Side, WindowSize},
    error::{Error, Result},
    input::{key_bytes, mouse_bytes, paste_bytes, symbol_char},
    native::{
        self, Event, Line, Renderer, Run, Window,
        trace::{Trace, monotonic},
    },
    paint::{Frame, LineCache, text_columns},
    screen::{Screen, Update},
    terminal::{Event as TerminalEvent, Shell, Terminal},
    vt::{Color, NamedColor},
    width::text_width,
};
use std::{
    collections::VecDeque,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

const PADDING: f32 = 4.;

enum Input {
    Bytes(Vec<u8>),
    Paste(bool),
}

pub struct Desktop {
    terminal: Terminal,
    screen: Screen,
    cache: LineCache,
    preedit_line: Option<Line>,
    renderer: Renderer,
    window: Window,
    config: Config,
    cell: (f32, f32),
    scale: f64,
    dirty: bool,
    running: bool,
    focused: bool,
    pointer: (f32, f32),
    selecting: bool,
    reported_buttons: u8,
    last_click: Option<(Instant, Point, u8)>,
    scroll_drag: Option<f32>,
    scroll_remainder: f32,
    preedit: String,
    preedit_cursor: usize,
    paste: Option<bool>,
    deferred: VecDeque<Input>,
    trace: Option<Trace>,
}

impl Desktop {
    pub fn new(config: Config, shell: Option<Shell>, directory: Option<PathBuf>) -> Result<Self> {
        let width = (config.font_size_px * 0.625).ceil() as i32 * 100 + 8;
        let height = (config.font_size_px * 1.35).ceil() as i32 * 32 + 8;
        let window = Window::new(width, height)?;
        let renderer = Renderer::new(&window, &config.font_family, config.font_size_px)?;
        let cell = renderer.metrics();
        let (width, height, scale) = window.size();
        let size = grid_size(width, height, cell);
        let terminal = Terminal::new(shell, directory, size, config.foreground, config.background)?;
        window.title("nextty")?;
        let trace = Trace::from_env()?;
        Ok(Self {
            terminal,
            screen: Screen::default(),
            cache: LineCache::default(),
            preedit_line: None,
            renderer,
            window,
            config,
            cell,
            scale,
            dirty: true,
            running: true,
            focused: false,
            pointer: (-1., -1.),
            selecting: false,
            reported_buttons: 0,
            last_click: None,
            scroll_drag: None,
            scroll_remainder: 0.,
            preedit: String::new(),
            preedit_cursor: 0,
            paste: None,
            deferred: VecDeque::new(),
            trace,
        })
    }

    pub fn run(mut self) -> Result<()> {
        let mut events = Vec::new();
        while self.running {
            self.dirty |= self.terminal.drain_notification()?;
            while let Ok(event) = self.terminal.events.try_recv() {
                match event {
                    TerminalEvent::Title(title) => {
                        self.window.title(title.as_deref().unwrap_or("nextty"))?
                    }
                    TerminalEvent::ClipboardStore { primary, text } => {
                        self.window.selection(primary, &text)
                    }
                    TerminalEvent::Exit(_) => self.running = false,
                    TerminalEvent::Error(error) => {
                        return Err(Error::context("terminal IO", error));
                    }
                }
            }
            let ready = self.renderer.ready()?;
            if ready && let Some(trace) = &mut self.trace {
                trace.record_swap(self.renderer.swap_time())?;
            }
            if self.running && self.dirty && ready && self.window.ready() {
                self.draw()?;
                self.dirty = false;
            }
            if !self.running {
                break;
            }
            self.window.wait(
                &mut [
                    libc::pollfd {
                        fd: self.terminal.event_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.renderer.fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ],
                -1,
                &mut events,
            )?;
            for event in events.drain(..) {
                self.event(event)?;
            }
        }
        self.renderer.finish()?;
        if let Some(trace) = &mut self.trace {
            trace.record_swap(self.renderer.swap_time())?;
            trace.finish()?;
        }
        Ok(())
    }

    fn mode(&self) -> Mode {
        self.terminal.core.lock().unwrap().mode()
    }

    fn input(&mut self, data: Vec<u8>) -> Result<()> {
        if self.paste.is_some() {
            self.deferred.push_back(Input::Bytes(data));
            return Ok(());
        }
        {
            let mut core = self.terminal.core.lock().unwrap();
            self.dirty |= core.clear_selection();
            self.dirty |= core.scroll_display(Scroll::Bottom);
        }
        self.terminal.write(data)?;
        if let Some(trace) = &mut self.trace {
            writeln!(trace.output, "write_queued {:.9}", monotonic())?;
        }
        Ok(())
    }

    fn paste(&mut self, primary: bool) {
        if !self.preedit.is_empty() {
            return;
        }
        if self.paste.is_some() {
            self.deferred.push_back(Input::Paste(primary));
        } else {
            self.paste = Some(self.mode().contains(Mode::BRACKETED_PASTE));
            self.window.paste(primary);
        }
    }

    fn pasted(&mut self, text: String) -> Result<()> {
        let Some(bracketed) = self.paste.take() else {
            return Ok(());
        };
        self.input(paste_bytes(text, bracketed))?;
        while let Some(input) = self.deferred.pop_front() {
            match input {
                Input::Bytes(bytes) => self.input(bytes)?,
                Input::Paste(primary) => {
                    self.paste(primary);
                    break;
                }
            }
        }
        Ok(())
    }

    fn copy(&self, primary: bool) {
        let text = self.terminal.core.lock().unwrap().selection_to_string();
        if let Some(text) = text {
            self.window.selection(primary, &text);
        }
    }

    fn position(&self) -> (Point, Side, usize) {
        let core = self.terminal.core.lock().unwrap();
        let x = (self.pointer.0 - PADDING) / self.cell.0;
        let row = (((self.pointer.1 - PADDING) / self.cell.1).floor().max(0.) as usize)
            .min(core.screen_lines() - 1);
        (
            Point {
                line: row as i32 - core.display_offset() as i32,
                column: (x.floor().max(0.) as usize).min(core.columns() - 1),
            },
            if x.fract() < 0.5 {
                Side::Left
            } else {
                Side::Right
            },
            row,
        )
    }

    fn report(&mut self, code: u16, release: bool, modifiers: u32) -> Result<()> {
        let (point, _, row) = self.position();
        let code = code
            + u16::from(modifiers & native::SHIFT != 0) * 4
            + u16::from(modifiers & native::ALT != 0) * 8
            + u16::from(modifiers & native::CONTROL != 0) * 16;
        if let Some(bytes) = mouse_bytes(code, point.column, row, release, self.mode()) {
            self.input(bytes)?;
        }
        Ok(())
    }

    fn scrollbar(&self) -> Option<(f32, f32)> {
        let history = self.screen.history_size();
        if history == 0 || self.screen.mode().contains(Mode::ALT_SCREEN) {
            return None;
        }
        let height = (self.window.size().1 as f32 - PADDING * 2.).max(1.);
        let thumb = (height * self.screen.screen_lines() as f32
            / (history + self.screen.screen_lines()) as f32)
            .max(20_f32.min(height));
        let top = PADDING
            + (height - thumb) * (1. - self.screen.display_offset() as f32 / history as f32);
        Some((top, thumb))
    }

    fn drag_scrollbar(&mut self) {
        let Some(grab) = self.scroll_drag else {
            return;
        };
        let mut core = self.terminal.core.lock().unwrap();
        let history = core.history_size();
        let height = (self.window.size().1 as f32 - PADDING * 2.).max(1.);
        let thumb = (height * core.screen_lines() as f32 / (history + core.screen_lines()) as f32)
            .max(20_f32.min(height));
        let travel = height - thumb;
        if travel > 0. {
            let offset = ((1. - ((self.pointer.1 - grab - PADDING) / travel).clamp(0., 1.))
                * history as f32)
                .round() as usize;
            let delta = offset as i64 - core.display_offset() as i64;
            self.dirty |= core.scroll_display(Scroll::Delta(delta as i32));
        }
    }

    fn button(&mut self, button: u32, pressed: bool, modifiers: u32) -> Result<()> {
        if !pressed {
            if button == 1 && self.scroll_drag.take().is_some() {
                return Ok(());
            }
            if button == 1 && std::mem::take(&mut self.selecting) {
                let (point, side, _) = self.position();
                self.dirty |= self
                    .terminal
                    .core
                    .lock()
                    .unwrap()
                    .selection_update(point, side);
                self.copy(true);
                return Ok(());
            }
            if (1..=3).contains(&button) && self.reported_buttons & (1 << (button - 1)) != 0 {
                self.reported_buttons &= !(1 << (button - 1));
                self.report((button - 1) as u16, true, modifiers)?;
            }
            return Ok(());
        }
        if button == 1
            && self.pointer.0 >= self.window.size().0 as f32 - PADDING
            && let Some((top, thumb)) = self.scrollbar()
        {
            self.scroll_drag = Some(if (top..top + thumb).contains(&self.pointer.1) {
                self.pointer.1 - top
            } else {
                thumb * 0.5
            });
            self.drag_scrollbar();
            return Ok(());
        }
        if self.mode().intersects(Mode::MOUSE_MODE)
            && modifiers & native::SHIFT == 0
            && (1..=3).contains(&button)
        {
            self.reported_buttons |= 1 << (button - 1);
            return self.report((button - 1) as u16, false, modifiers);
        }
        match button {
            1 => {
                let (point, side, _) = self.position();
                let now = Instant::now();
                let count = self
                    .last_click
                    .filter(|(at, old, _)| {
                        now.duration_since(*at) < Duration::from_millis(500) && *old == point
                    })
                    .map_or(1, |(_, _, count)| count % 3 + 1);
                self.last_click = Some((now, point, count));
                let kind = match count {
                    2 if modifiers & native::CONTROL != 0 => SelectionType::Object,
                    2 => SelectionType::Semantic,
                    3 => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                self.terminal
                    .core
                    .lock()
                    .unwrap()
                    .selection_start(point, side, kind);
                self.selecting = true;
                self.dirty = true;
            }
            2 => self.paste(true),
            3 => self.copy(false),
            _ => (),
        }
        Ok(())
    }

    fn motion(&mut self, modifiers: u32) -> Result<()> {
        if self.scroll_drag.is_some() {
            self.drag_scrollbar();
        } else if self.selecting {
            let (point, side, _) = self.position();
            self.dirty |= self
                .terminal
                .core
                .lock()
                .unwrap()
                .selection_update(point, side);
        } else {
            let mode = self.mode();
            if modifiers & native::SHIFT == 0
                && (mode.contains(Mode::MOUSE_MOTION)
                    || mode.contains(Mode::MOUSE_DRAG) && self.reported_buttons != 0)
            {
                let code = if self.reported_buttons == 0 {
                    35
                } else {
                    32 + self.reported_buttons.trailing_zeros() as u16
                };
                self.report(code, false, modifiers)?;
            }
        }
        Ok(())
    }

    fn scroll(&mut self, amount: f32, modifiers: u32) -> Result<()> {
        if self.scroll_drag.is_some() {
            return Ok(());
        }
        self.scroll_remainder += amount;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return Ok(());
        }
        let mode = self.mode();
        if mode.intersects(Mode::MOUSE_MODE) && modifiers & native::SHIFT == 0 {
            for _ in 0..lines.unsigned_abs() {
                self.report(if lines > 0 { 64 } else { 65 }, false, modifiers)?;
            }
        } else if mode.contains(Mode::ALT_SCREEN)
            && mode.contains(Mode::ALTERNATE_SCROLL)
            && modifiers & native::SHIFT == 0
        {
            for _ in 0..lines.unsigned_abs() * 3 {
                self.input(if lines > 0 {
                    b"\x1bOA".to_vec()
                } else {
                    b"\x1bOB".to_vec()
                })?;
            }
        } else {
            self.dirty |= self
                .terminal
                .core
                .lock()
                .unwrap()
                .scroll_display(Scroll::Delta(lines.saturating_mul(3)));
        }
        Ok(())
    }

    fn key(&mut self, symbol: u32, modifiers: u32, text: &str) -> Result<()> {
        if modifiers & (native::CONTROL | native::SHIFT) == native::CONTROL | native::SHIFT {
            match symbol_char(symbol).map(|c| c.to_ascii_lowercase()) {
                Some('c') => {
                    self.copy(false);
                    return Ok(());
                }
                Some('v') => {
                    self.paste(false);
                    return Ok(());
                }
                _ => (),
            }
        }
        if !self.preedit.is_empty() {
            return Ok(());
        }
        if modifiers == native::SHIFT {
            let lines = self.terminal.core.lock().unwrap().screen_lines() as i32;
            let scroll = match symbol {
                0xff55 => Some(Scroll::Delta(lines - 1)),
                0xff56 => Some(Scroll::Delta(1 - lines)),
                0xff50 => Some(Scroll::Top),
                0xff57 => Some(Scroll::Bottom),
                0xff63 => {
                    self.paste(false);
                    return Ok(());
                }
                _ => None,
            };
            if let Some(scroll) = scroll {
                self.dirty |= self.terminal.core.lock().unwrap().scroll_display(scroll);
                return Ok(());
            }
        }
        if let Some(bytes) = key_bytes(symbol, modifiers, text, || self.mode()) {
            self.input(bytes)?;
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        if let Some(trace) = &mut self.trace
            && matches!(event.kind, native::KEY | native::TEXT | native::PREEDIT)
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
            native::KEY => self.key(event.code, event.modifiers, &event.text)?,
            native::TEXT => {
                self.preedit.clear();
                self.preedit_line = None;
                self.dirty = true;
                self.input(event.text.into_bytes())?;
            }
            native::PREEDIT => {
                let cursor = if event.start < 0 {
                    event.text.len()
                } else {
                    event.start as usize
                };
                if !event.text.is_char_boundary(cursor) {
                    return Err(Error::msg("invalid IME preedit offset"));
                }
                self.preedit = event.text;
                self.preedit_cursor = cursor;
                self.preedit_line = None;
                self.dirty = true;
            }
            native::RESIZE => {
                let (width, height, scale) = self.window.size();
                if scale != self.scale {
                    self.scale = scale;
                    self.cache = LineCache::default();
                    self.preedit_line = None;
                }
                self.terminal.resize(grid_size(width, height, self.cell))?;
                self.dirty = true;
            }
            native::EXPOSE | native::WINDOW_STATE => self.dirty = true,
            native::POINTER_LEAVE => self.pointer = (-1., -1.),
            native::CLOSE => self.running = false,
            native::FOCUS => {
                self.focused = event.code != 0;
                self.dirty = true;
                if self.mode().contains(Mode::FOCUS_IN_OUT) {
                    self.input(if self.focused {
                        b"\x1b[I".to_vec()
                    } else {
                        b"\x1b[O".to_vec()
                    })?;
                }
                if let Some(trace) = &mut self.trace {
                    writeln!(
                        trace.output,
                        "focus {:.9} {}",
                        monotonic(),
                        u8::from(self.focused)
                    )?;
                }
            }
            native::MOTION => {
                self.pointer = (event.x as f32, event.y as f32);
                self.motion(event.modifiers)?;
            }
            native::BUTTON => {
                self.pointer = (event.x as f32, event.y as f32);
                self.button(event.code, event.start != 0, event.modifiers)?;
            }
            native::SCROLL => self.scroll(event.y as f32, event.modifiers)?,
            native::PASTE => self.pasted(event.text)?,
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
            native::FRAME_RATE => {
                if let Some(trace) = &mut self.trace {
                    writeln!(trace.output, "frame_rate {:.9} {:.0}", monotonic(), event.x)?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let begin = self.trace.as_ref().map(|_| monotonic());
        self.terminal.acknowledge_wakeup();
        let update = {
            let mut core = self.terminal.core.lock().unwrap();
            let update = Update::capture(&core, self.screen.columns() == 0);
            core.reset_damage();
            update
        };
        self.screen.apply(update)?;
        let frame = Frame::new(
            &mut self.screen,
            &self.cache,
            self.focused,
            !self.preedit.is_empty(),
        );
        let (width, height, scale) = self.window.size();
        self.renderer.begin(
            (frame.background & !255) | (self.config.background_opacity * 255.).round() as u32,
        )?;
        let size = (width as f32 - PADDING * 2., height as f32 - PADDING * 2.);
        self.renderer.clip(PADDING, PADDING, size.0, size.1)?;
        frame.paint(
            self.cell,
            self.focused,
            &mut self.cache,
            &self.renderer,
            (PADDING, PADDING),
            size,
        )?;
        let (cursor, _) = self.screen.cursor();
        let row = cursor.line + self.screen.display_offset() as i32;
        let x = PADDING + cursor.column as f32 * self.cell.0;
        let y = PADDING + row as f32 * self.cell.1;
        if !self.preedit.is_empty() && (0..self.screen.screen_lines() as i32).contains(&row) {
            if self.preedit_line.is_none() {
                let color = self
                    .screen
                    .color(Color::Named(NamedColor::Foreground))
                    .rgba();
                self.preedit_line = Some(self.renderer.shape(
                    &self.preedit,
                    &[Run {
                        start: 0,
                        end: self.preedit.len() as u32,
                        color,
                        flags: 0,
                    }],
                    &text_columns(&self.preedit),
                )?);
            }
            if let Some(line) = &self.preedit_line {
                self.renderer.line(line, x, y);
            }
            self.renderer.rect(
                x,
                y + self.cell.1 - 1.,
                text_width(&self.preedit) as f32 * self.cell.0,
                1.,
                0xb5d2a2ff,
                false,
            );
        }
        let caret = text_width(&self.preedit[..self.preedit_cursor.min(self.preedit.len())]) as f32;
        self.window.cursor_rect(
            (x + caret * self.cell.0) as i32,
            y as i32,
            self.cell.0 as i32,
            self.cell.1 as i32,
        );
        self.renderer.clip(0., 0., 0., 0.)?;
        if let Some((top, thumb)) = self.scrollbar() {
            self.renderer
                .rect(width as f32 - 3., top, 3., thumb, 0x60685fff, false);
        }
        let prepared = begin.map(|_| monotonic());
        self.renderer.present()?;
        if let Some(trace) = &mut self.trace {
            let cell = self.screen.row(0).cells[0];
            let bg = self.screen.color(cell.bg);
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
                    cell.c as u32,
                    (u32::from(bg.r) << 16) | (u32::from(bg.g) << 8) | u32::from(bg.b),
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
}

fn grid_size(width: i32, height: i32, cell: (f32, f32)) -> WindowSize {
    WindowSize {
        num_cols: (((width as f32 - PADDING * 2.) / cell.0).floor() as u16).max(2),
        num_lines: (((height as f32 - PADDING * 2.) / cell.1).floor() as u16).max(2),
        cell_width: cell.0.round() as u16,
        cell_height: cell.1.round() as u16,
    }
}
