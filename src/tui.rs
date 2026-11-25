//! ANSI frontend for an existing workspace; the daemon owns PTYs and VT state.

mod input;

use crate::config::{Config, TabStyle};
use crate::core::{Flags, Mode, Point, SelectionRange, Side, WindowSize};
use crate::input::{KeyResult, PasteTarget, PointerKind};
use crate::panel::{Grid, MUTED, rect, rgb};
use crate::screen::Screen;
use crate::ui::{Area, Effect, Focus, Pointer, READ_ONLY, TabPart, Ui, cursor_bytes, pane_point};
use crate::vt::{Color, CursorShape, NamedColor};
use crate::workspace::{Action, Client, PaneId, PaneView, Rect, Response, Snapshot};
use input::{Decoder, Input, PointerEvent};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::time::{Duration, Instant};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?2026l\x18\x1b\\\x1b[0m\x1b[?1006l\x1b[?1003l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b>\x1b[?7h\x1b[?25h\x1b[?1049l";
const PASTE_HINT: &str = "Paste with your host terminal's shortcut";

struct TerminalGuard {
    input: RawFd,
    output: RawFd,
    termios: libc::termios,
    output_flags: i32,
    signals: File,
}

impl TerminalGuard {
    fn enter(input: RawFd, output: RawFd) -> io::Result<Self> {
        let mut termios = unsafe { std::mem::zeroed() };
        crate::sys::check(unsafe { libc::tcgetattr(input, &mut termios) })?;
        let output_flags = crate::sys::check(unsafe { libc::fcntl(output, libc::F_GETFL) })?;
        let mut signals = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut signals) };
        for signal in [
            libc::SIGHUP,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGPIPE,
            libc::SIGTSTP,
            libc::SIGWINCH,
        ] {
            unsafe { libc::sigaddset(&mut signals, signal) };
        }
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) };
        let signalfd = libc::SFD_CLOEXEC | libc::SFD_NONBLOCK;
        let signals = crate::sys::check(unsafe { libc::signalfd(-1, &signals, signalfd) })?;
        let guard = Self {
            input,
            output,
            termios,
            output_flags,
            signals: unsafe { File::from_raw_fd(signals) },
        };
        let mut raw = termios;
        unsafe { libc::cfmakeraw(&mut raw) };
        crate::sys::check(unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) })?;
        crate::sys::check(unsafe {
            libc::fcntl(output, libc::F_SETFL, output_flags | libc::O_NONBLOCK)
        })?;
        Ok(guard)
    }

    fn next_signal(&self) -> io::Result<Option<u32>> {
        let mut info = [0; size_of::<libc::signalfd_siginfo>()];
        match (&self.signals).read_exact(&mut info) {
            Ok(()) => Ok(Some(u32::from_ne_bytes(info[..4].try_into().unwrap()))),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.input, libc::TCSANOW, &self.termios);
            // Abandon queued frame bytes when detaching, including a partial OSC.
            libc::tcflush(self.output, libc::TCOFLUSH);
            libc::write(self.output, LEAVE.as_ptr().cast(), LEAVE.len());
            libc::fcntl(self.output, libc::F_SETFL, self.output_flags);
        }
    }
}

/// Attach the terminal frontend to a client already sent Create or Attach.
pub fn run(client: Client, config: Config) -> io::Result<()> {
    let size = terminal_size(libc::STDOUT_FILENO)?;
    let terminal = TerminalGuard::enter(libc::STDIN_FILENO, libc::STDOUT_FILENO)?;
    let mut frontend = Frontend::new(client, size, config)?;
    // One frame or one clipboard chunk at a time; a slow SSH link never queues more frames.
    let mut output = ENTER.to_vec();
    frontend
        .client
        .send(Action::Resize(frontend.layout().view))?;
    let mut stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDIN_FILENO) });
    let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) });
    let mut decoder = Decoder::default();
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let result = (|| {
        frontend.poll()?;
        while frontend.running {
            let ui = &mut frontend.ui;
            frontend.dirty |= ui.service(&mut frontend.client, &frontend.config, &fds)?;
            if output.is_empty() {
                if let Some(transfer) = frontend.copies.front_mut() {
                    if let Some(message) = transfer.step(&mut output)? {
                        frontend.copies.pop_front();
                        frontend.message(message);
                    }
                } else if frontend.dirty || frontend.screen_dirty {
                    frontend.draw(&mut output)?;
                    frontend.dirty = false;
                    frontend.screen_dirty = false;
                    frontend.client.request_screen()?;
                }
            }
            if !output.is_empty() {
                match stdout.write(&output) {
                    Ok(count) => {
                        output.drain(..count);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                    // The host terminal went away.
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => return Err(error),
                }
            }
            let now = Instant::now();
            let mut timeout = [
                decoder.timeout(now),
                frontend
                    .ui
                    .deadline()
                    .map(|until| until.saturating_duration_since(now)),
            ]
            .into_iter()
            .flatten()
            .min();
            if frontend.client.needs_poll()
                || output.is_empty()
                    && (!frontend.copies.is_empty() || frontend.dirty || frontend.screen_dirty)
            {
                timeout = Some(Duration::ZERO);
            }
            frontend.client.flush()?;
            let watch = |fd, events| libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            fds.clear();
            fds.extend([
                watch(libc::STDIN_FILENO, libc::POLLIN),
                frontend.client.pollfd(),
                watch(terminal.signals.as_raw_fd(), libc::POLLIN),
                watch(
                    libc::STDOUT_FILENO,
                    if output.is_empty() { 0 } else { libc::POLLOUT },
                ),
            ]);
            fds.extend(frontend.ui.fds());
            crate::sys::poll(&mut fds, timeout.map_or(-1, crate::sys::timeout_millis))?;
            while fds[2].revents != 0
                && let Some(signal) = terminal.next_signal()?
            {
                if signal == libc::SIGWINCH as u32 {
                    frontend.size = terminal_size(libc::STDOUT_FILENO)?;
                    frontend.frame = None;
                    frontend.resize()?;
                } else {
                    frontend.running = false;
                }
            }
            if fds[0].revents & libc::POLLIN != 0 {
                let mut bytes = [0; 8192];
                match stdin.read(&mut bytes) {
                    Ok(0) => frontend.running = false,
                    Ok(count) => {
                        for event in decoder.feed(&bytes[..count], Instant::now()) {
                            frontend.input(event)?;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                    Err(error) => return Err(error),
                }
            }
            if (fds[0].revents | fds[3].revents) & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                != 0
            {
                frontend.running = false;
            }
            if let Some(event) = decoder.expire(Instant::now()) {
                frontend.input(event)?;
            }
            frontend.client.flush()?;
            if frontend.running
                && (fds[1].revents
                    & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                    != 0
                    || frontend.client.needs_poll())
            {
                frontend.poll()?;
            }
        }
        Ok(())
    })();

    if frontend.client.snapshot().is_some() {
        frontend
            .ui
            .poll_agent(&mut frontend.client, &frontend.config)?;
    }
    // Closing the connection detaches; first send what the last iteration queued, if it can.
    let _ = frontend.client.flush();
    result
}

fn braille_trend(
    points: &[(f32, Option<f32>)],
    columns: usize,
    rows: usize,
    seconds: f32,
    maximum: f32,
) -> Vec<u8> {
    if columns == 0 || rows == 0 {
        return Vec::new();
    }
    let mut cells = vec![0u8; columns * rows];
    let bits = [[1, 8], [2, 16], [4, 32], [64, 128]];
    let mut previous = None;
    for &(age, value) in points {
        let Some(value) = value else {
            previous = None;
            continue;
        };
        let x = ((1. - age / seconds).clamp(0., 1.) * (columns * 2 - 1) as f32).round() as usize;
        let y = ((1. - value / maximum).clamp(0., 1.) * (rows * 4 - 1) as f32).round() as usize;
        let (from_x, from_y) = previous.unwrap_or((x, y));
        let count = from_x.abs_diff(x).max(from_y.abs_diff(y)).max(1);
        for step in 1..=count {
            let fraction = step as f32 / count as f32;
            let px = (from_x as f32 + (x as f32 - from_x as f32) * fraction).round() as usize;
            let py = (from_y as f32 + (y as f32 - from_y as f32) * fraction).round() as usize;
            cells[py / 4 * columns + px / 2] |= bits[py % 4][px % 2];
        }
        previous = Some((x, y));
    }
    cells
}

fn terminal_size(fd: RawFd) -> io::Result<WindowSize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    crate::sys::check(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) })?;
    if size.ws_col < 2 || size.ws_row < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TUI needs at least 2 columns and 2 rows",
        ));
    }
    Ok(WindowSize {
        num_cols: size.ws_col,
        num_lines: size.ws_row,
        cell_width: 0,
        cell_height: 0,
    })
}

/// Where the TUI places each region on the host terminal.
struct Layout {
    /// The panes' size; they start below the tab row, right of the file panel.
    view: WindowSize,
    /// The window without its tab row and footer; previews float over it.
    canvas: Rect,
    /// Columns the file panel takes on the left.
    left: u16,
    files: Rect,
    preview: Rect,
    agent: Rect,
}

struct Frontend {
    client: Client,
    size: WindowSize,
    running: bool,
    /// Something other than terminal output changed, so the next draw repaints everything.
    dirty: bool,
    screen_dirty: bool,
    /// The last drawn frame; `None` repaints everything.
    frame: Option<Grid>,
    /// The frame before `frame`, kept to reuse its allocation and to diff against.
    spare: Option<Grid>,
    /// The snapshot of the last full draw, if nothing overlaid the panes.
    drawn: Option<Snapshot>,
    ui: Ui,
    config: Config,
    paste: Option<PasteTarget>,
    /// Copies queue behind a running transfer so a cut never loses its text.
    copies: VecDeque<ClipboardTransfer>,
    pointer: Option<(u16, u16)>,
}

impl Frontend {
    fn new(client: Client, size: WindowSize, config: Config) -> io::Result<Self> {
        Ok(Self {
            client,
            size,
            running: true,
            dirty: true,
            screen_dirty: false,
            frame: None,
            spare: None,
            drawn: None,
            ui: Ui::new()?,
            config,
            paste: None,
            copies: VecDeque::new(),
            pointer: None,
        })
    }

    fn layout(&self) -> Layout {
        let (columns, rows) = (self.size.num_cols, self.size.num_lines);
        let agent = (columns / 2).clamp(20, 80).min(columns.saturating_sub(2));
        let available = columns - if self.ui.agent.open { agent } else { 0 };
        let panels = crate::ui::PanelLayout::new(available, available, self.ui.files_open);
        let monitor = &self.ui.resources;
        let chrome = monitor.rows(rows.saturating_sub(2)) + if monitor.open { 2 } else { 1 };
        let view = WindowSize {
            num_cols: panels.terminal_columns,
            num_lines: rows.saturating_sub(chrome).max(2),
            ..self.size
        };
        let body = view.num_lines.saturating_sub(1);
        let canvas = rect(0, 1, columns, rows.saturating_sub(2));
        Layout {
            view,
            left: panels.file_width,
            canvas,
            files: rect(0, 1, panels.file_width, body),
            preview: crate::file_panel::preview_rect(canvas),
            agent: rect(columns - agent, 1, agent, body),
        }
    }

    fn resize(&mut self) -> io::Result<()> {
        self.ui.search.close(&mut self.client)?;
        self.client.send(Action::Resize(self.layout().view))?;
        self.dirty = true;
        Ok(())
    }

    fn copy_text(&mut self, text: String) {
        self.queue_copy(Box::new(io::Cursor::new(text.into_bytes())), b'c', false);
    }

    fn queue_copy(&mut self, reader: Box<dyn Read>, target: u8, incomplete: bool) {
        self.copies.push_back(ClipboardTransfer {
            reader,
            target,
            incomplete,
            bytes: 0,
        });
    }

    fn panel_key(&mut self, symbol: u32, modifiers: u32, text: &str) -> io::Result<()> {
        let key = crate::input::Key {
            symbol,
            modifiers,
            text,
        };
        match self.ui.key(&mut self.client, key)? {
            KeyResult::Unhandled => return Ok(()),
            KeyResult::Copy(text) => self.copy_text(text),
            KeyResult::Paste => self.message(PASTE_HINT),
            KeyResult::Quit => self.running = false,
            KeyResult::Handled => (),
        }
        self.dirty = true;
        Ok(())
    }

    fn panel_paste(&mut self, bytes: &[u8], target: PasteTarget) -> io::Result<()> {
        let text = String::from_utf8_lossy(bytes);
        self.ui.paste(&mut self.client, target, &text)?;
        self.dirty = true;
        Ok(())
    }

    fn panel_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some((symbol, modifiers)) = crate::input::sequence_key(bytes) {
            return self.panel_key(symbol, modifiers, "");
        }
        if matches!(bytes, [0x1b, b'[' | b'O', ..]) {
            return Ok(());
        }
        // Alt plus a non-ASCII character arrives as ESC and the character.
        let bytes = bytes.strip_prefix(b"\x1b").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        let mut remaining = text.as_ref();
        while let Some(ch) = remaining.chars().next() {
            if self.ui.text_target().is_some() {
                let end = remaining.find(char::is_control).unwrap_or(remaining.len());
                if end != 0 {
                    self.panel_key(0, 0, &remaining[..end])?;
                    remaining = &remaining[end..];
                    continue;
                }
            }
            let (symbol, modifiers) = crate::input::character_key(ch);
            let (typed, rest) = remaining.split_at(ch.len_utf8());
            self.panel_key(symbol, modifiers, if ch.is_control() { "" } else { typed })?;
            remaining = rest;
        }
        Ok(())
    }

    fn draw_resources(&mut self, grid: &mut Grid) {
        use crate::panel::BACKGROUND;
        let (columns, rows) = (self.size.num_cols, self.size.num_lines);
        let top = self.layout().view.num_lines;
        let last = rows - 1;
        let focused = self.ui.focus == Focus::Monitor;
        let hover = self.pointer.map(|(column, row)| self.locate(column, row));
        for row in top..rows {
            grid.text(0, row, columns, "", MUTED, BACKGROUND);
        }
        if self.ui.resources.open {
            draw_border(grid, 0, top, columns, false);
            let body = i32::from(top) + 1..i32::from(last);
            let seconds = self.ui.resources.range as f32;
            let pointer = match hover {
                Some((Area::Monitor, column, row)) => Some((column, row)),
                _ => None,
            };
            let height = last.saturating_sub(top + 1);
            let view = self.ui.resources.view(columns, height, pointer);
            for plot in view.plots {
                let (width, height) = (usize::from(plot.width), usize::from(plot.height));
                for series in &plot.series {
                    let color = crate::monitor::series_color(series.name);
                    let graph = braille_trend(&series.points, width, height, seconds, plot.maximum);
                    for (index, &bits) in graph.iter().enumerate().filter(|(_, bits)| **bits != 0) {
                        let y = body.start + plot.y + (index / width) as i32;
                        if !body.contains(&y) {
                            continue;
                        }
                        let (x, y) = (plot.x + (index % width) as u16, y as u16);
                        // Series overlap in one braille cell: merge dots, the last series colors it.
                        let old = grid.rows[usize::from(y)]
                            .cells
                            .get(usize::from(x))
                            .map_or(0, |cell| u32::from(cell.c).wrapping_sub(0x2800));
                        let bits = u32::from(bits) | if old < 0x100 { old } else { 0 };
                        let braille = char::from_u32(0x2800 + bits).unwrap();
                        grid.text(x, y, 1, &braille.to_string(), color, BACKGROUND);
                    }
                }
            }
            for text in view.texts {
                let y = (body.start + text.y) as u16;
                let background = text.background(focused).unwrap_or(BACKGROUND);
                grid.text(text.x, y, text.width, &text.text, text.color, background);
            }
        }
        let pointer = match hover {
            Some((Area::Footer, column, _)) => Some(column),
            _ => None,
        };
        let (controller, remote) = (self.client.controller(), self.client.is_remote());
        let footer = self
            .ui
            .resources
            .footer(columns, controller, remote, pointer);
        for text in &footer.texts {
            let background = text.background(focused).unwrap_or(BACKGROUND);
            grid.text(text.x, last, text.width, &text.text, text.color, background);
        }
        let (label, width) = self.ui.footer_label(&self.client, &footer);
        let label = crate::width::fit_middle(&label, width);
        grid.text(1, last, width, &label, MUTED, BACKGROUND);
    }

    /// The area under the host cell `column`, `row` or holding the pointer, and the pointer's
    /// cell in that area's grid.
    fn locate(&self, column: u16, row: u16) -> (Area, u16, u16) {
        let layout = self.layout();
        let (top, last) = (layout.view.num_lines, self.size.num_lines - 1);
        let area = self.ui.capture().unwrap_or(if row == 0 {
            Area::Tabs
        } else if row == last {
            Area::Footer
        } else if row == top {
            Area::Divider
        } else if row > top {
            Area::Monitor
        } else if self.ui.agent.open && column >= layout.agent.x {
            Area::Agent
        } else if column < layout.left {
            Area::Files
        } else {
            Area::Terminal
        });
        match area {
            Area::Terminal => (area, column.saturating_sub(layout.left), row),
            Area::Monitor => (area, column, row - top - 1),
            Area::Footer => (area, column, 0),
            _ => (area, column, row),
        }
    }

    fn mouse(&mut self, event: PointerEvent) -> io::Result<()> {
        let PointerEvent {
            column,
            row,
            kind,
            modifiers,
        } = event;
        let moved = self.pointer.replace((column, row)) != Some((column, row));
        self.dirty |= moved || kind != PointerKind::Motion;
        if self.ui.resources.dragging && kind == PointerKind::Motion {
            let rows = self.size.num_lines.saturating_sub(row + 2);
            return if self.ui.resources.resize(rows, self.size.num_lines - 2) {
                self.resize()
            } else {
                Ok(())
            };
        }
        let (area, column, row) = self.locate(column, row);
        // The host reports no position within a cell.
        let side = match kind {
            PointerKind::Press(_) => Side::Left,
            _ => Side::Right,
        };
        let pointer = Pointer {
            kind,
            modifiers,
            area,
            column,
            row,
            side,
        };
        match self.ui.pointer(&mut self.client, pointer)? {
            Effect::Input(pane, data) => self.send_input(pane, &data)?,
            Effect::Copy(text) => self.copy_text(text),
            Effect::Paste(..) => self.message(PASTE_HINT),
            Effect::Quit => self.running = false,
            // The host terminal keeps the primary selection.
            Effect::None | Effect::Primary(_) | Effect::Selected(_) => (),
        }
        Ok(())
    }

    /// Shows the focused pane's cursor, else `panel_cursor`, the caret of the focused panel.
    fn draw_cursor(
        &self,
        output: &mut Vec<u8>,
        panel_cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        let pane = (self.client.snapshot())
            .filter(|snapshot| snapshot.controller && self.ui.terminal_input())
            .and_then(Snapshot::focused_pane);
        let screen = pane.and_then(|pane| self.client.screens().get(&pane.id));
        // Keypad keys pass through raw; cursor keys are rewritten by `cursor_bytes` instead.
        let keypad = screen.is_some_and(|screen| screen.mode().contains(Mode::APP_KEYPAD));
        output.extend_from_slice(if keypad { b"\x1b=" } else { b"\x1b>" });
        if let (Some(pane), Some(screen)) = (pane, screen) {
            let (point, shape) = screen.cursor();
            let layout = self.layout();
            let row = point.line + screen.display_offset() as i32;
            let x = u32::from(pane.rect.x) + point.column as u32;
            let y = i32::from(pane.rect.y) + row;
            if shape != CursorShape::Hidden
                && (0..i32::from(pane.rect.rows)).contains(&row)
                && x < u32::from(layout.view.num_cols)
                && y < i32::from(layout.view.num_lines)
            {
                let shape = match shape {
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                    _ => 2,
                };
                let column = x + u32::from(layout.left) + 1;
                return write!(
                    output,
                    "\x1b[0m\x1b[{shape} q\x1b[{};{column}H\x1b[?25h",
                    y + 1
                );
            }
        }
        if let Some((column, row)) = panel_cursor {
            write!(output, "\x1b[{};{}H\x1b[?25h", row + 1, column + 1)
        } else {
            output.write_all(b"\x1b[?25l")
        }
    }

    fn message(&mut self, message: impl Into<String>) {
        self.ui.message(message);
        self.dirty = true;
    }

    fn send_input(&mut self, pane: PaneId, data: &[u8]) -> io::Result<()> {
        if self.client.controller() {
            self.client.send_input(pane, data)
        } else {
            self.message(READ_ONLY);
            Ok(())
        }
    }

    fn poll(&mut self) -> io::Result<()> {
        for response in self.client.poll()? {
            match self.ui.respond(&mut self.client, response)? {
                None => self.dirty = true,
                Some(Response::ScreenDirty) => self.screen_dirty = true,
                Some(Response::Detached) => self.running = false,
                Some(Response::Clipboard {
                    primary,
                    file,
                    incomplete,
                }) => {
                    if std::env::var("TERM").is_ok_and(|term| term == "dumb" || term == "linux") {
                        self.message(
                            "This host terminal does not support OSC 52 clipboard transfer",
                        );
                    } else {
                        let target = if primary { b'p' } else { b'c' };
                        self.queue_copy(Box::new(file), target, incomplete);
                    }
                }
                Some(_) => (),
            }
        }
        Ok(())
    }

    fn input(&mut self, event: Input) -> io::Result<()> {
        let panels = self.ui.panel_state();
        match event {
            Input::Mouse(pointer) => self.mouse(pointer)?,
            event => self.dispatch(event)?,
        }
        if panels != self.ui.panel_state() {
            self.resize()?;
        }
        Ok(())
    }

    fn dispatch(&mut self, event: Input) -> io::Result<()> {
        let modal = self.ui.modal();
        if !modal && let Input::Alt(key) = event {
            if key == b'q' {
                self.running = false;
                return Ok(());
            }
            if self.ui.alt(&mut self.client, char::from(key))? {
                self.dirty = true;
                return Ok(());
            }
        }
        if modal || self.ui.input_target().is_some() {
            match event {
                Input::Bytes(bytes) => return self.panel_bytes(&bytes),
                Input::Alt(key) if !modal => {
                    return self.panel_key(u32::from(key), crate::input::ALT, "");
                }
                Input::PasteStart if !modal => {
                    self.paste = self.ui.paste_target();
                    return Ok(());
                }
                _ if modal => return Ok(()),
                _ => (),
            }
        }
        let Some(pane) = self.client.focused() else {
            return Ok(());
        };
        let mode = self.client.mode(pane);
        match event {
            Input::Alt(key) => self.send_input(pane, &[0x1b, key])?,
            Input::Bytes(bytes) => self.send_input(pane, &cursor_bytes(bytes, mode))?,
            Input::Focus(focused) if mode.contains(Mode::FOCUS_IN_OUT) => {
                self.send_input(pane, if focused { b"\x1b[I" } else { b"\x1b[O" })?;
            }
            Input::Focus(_) | Input::Mouse(_) => (),
            Input::PasteStart => {
                let bracketed = mode.contains(Mode::BRACKETED_PASTE);
                self.paste = Some(PasteTarget::Pane { pane, bracketed });
                if bracketed {
                    self.send_input(pane, b"\x1b[200~")?;
                }
            }
            Input::PasteData(bytes) => match self.paste {
                Some(PasteTarget::Pane { pane, .. }) => self.send_input(pane, &bytes)?,
                Some(target) => self.panel_paste(&bytes, target)?,
                None => (),
            },
            Input::PasteEnd => {
                if let Some(PasteTarget::Pane {
                    pane,
                    bracketed: true,
                }) = self.paste.take()
                {
                    self.send_input(pane, b"\x1b[201~")?;
                }
            }
        }
        Ok(())
    }

    /// The pane cell under the pointer, which hover highlights.
    fn hovered(&self) -> Option<(PaneId, Point)> {
        let (column, row) = self.pointer?;
        let (area, column, row) = self.locate(column, row);
        if area != Area::Terminal {
            return None;
        }
        let (pane, point, _) = pane_point(&self.client, column, row, Side::Left, None)?;
        Some((pane, point))
    }

    fn hovered_object(&self) -> Option<(PaneId, crate::context::Object)> {
        let (pane, point) = self.hovered()?;
        crate::context::object_at(self.client.screens().get(&pane)?, point)
            .map(|object| (pane, object))
    }

    fn draw(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let Some(snapshot) = self.client.snapshot() else {
            self.frame = None;
            write!(output, "\x1b[H\x1b[0mWaiting for workspace — Alt+q detach")?;
            return Ok(());
        };
        output.write_all(b"\x1b[?2026h")?;
        let Layout {
            view: size, left, ..
        } = self.layout();
        let host_columns = size.num_cols + left;
        let screen = rect(0, 0, self.size.num_cols, self.size.num_lines);
        let (foreground, background) = (self.config.foreground, self.config.background);
        let hovered = self
            .hovered()
            .and_then(|(pane, point)| crate::ui::block_at(&self.client, pane, point));
        let object = self.hovered_object();
        let plain = hovered.is_none()
            && object.is_none()
            && !(self.ui.resources.open
                || self.ui.files_open
                || self.ui.agent.open
                || self.ui.search.open
                || self.ui.close_prompt.is_some());
        // Terminal output alone repaints only the rows the screens report as damaged.
        let incremental = plain
            && !self.dirty
            && self.frame.is_some()
            && self.spare.is_some()
            && self
                .drawn
                .as_ref()
                .is_some_and(|drawn| same_view(drawn, snapshot));
        let (mut frame, mut previous) = if incremental {
            (self.frame.take().unwrap(), self.spare.take())
        } else {
            let mut frame = self
                .spare
                .take()
                .unwrap_or_else(|| Grid::new(screen, foreground, background));
            frame.reset(screen, foreground, background);
            self.drawn = plain.then(|| snapshot.clone());
            let pointer = self.pointer.filter(|&(_, row)| row == 0);
            let pointer = pointer.map(|(column, _)| column);
            let config = &self.config;
            draw_tabs(
                &mut frame,
                &mut self.ui,
                snapshot,
                host_columns,
                pointer,
                config,
            );
            (frame, self.frame.take())
        };
        let mut damage = Vec::new();
        for pane in &snapshot.panes {
            let Some(screen) = self.client.screens().get(&pane.id) else {
                continue;
            };
            let rows = screen
                .screen_lines()
                .min(usize::from(pane.rect.rows))
                .min(usize::from(size.num_lines.saturating_sub(pane.rect.y)));
            let object = object
                .as_ref()
                .filter(|(id, _)| *id == pane.id)
                .map(|(_, object)| object.range);
            for row in 0..rows {
                if incremental {
                    if !screen.dirty[row] {
                        continue;
                    }
                    // Panes side by side share a host row; keep its old cells once.
                    let y = usize::from(pane.rect.y) + row;
                    if !damage.contains(&y) {
                        previous.as_mut().unwrap().rows[y].clone_from(&frame.rows[y]);
                        damage.push(y);
                    }
                }
                let highlight = hovered.is_some_and(|(id, block)| {
                    id == pane.id
                        && screen
                            .row_output(pane, row)
                            .is_some_and(|current| current.id == block)
                });
                let painted = (highlight, object);
                paint_row(&mut frame, screen, pane, row, host_columns, painted, left);
            }
        }
        let mut cursor = None;
        if !incremental {
            draw_separators(&mut frame, snapshot, size, left);
            self.draw_resources(&mut frame);
            if self.ui.files_open && !self.ui.file_preview_open() {
                self.draw_files(&mut frame);
            }
            if self.ui.agent.open {
                cursor = self.draw_agent(&mut frame)?;
            }
            if self.ui.file_preview_open() {
                self.draw_files(&mut frame);
            }
            if self.ui.search.open {
                let columns = self.size.num_cols;
                let (label, caret) = self
                    .ui
                    .search
                    .display("", usize::from(columns.saturating_sub(2)));
                let label = [" ", &label].concat();
                frame.text(0, 0, columns, &label, foreground, crate::panel::CHROME);
                cursor = Some((caret as u16 + 1, 0));
            }
            let name = &self.client.snapshot().unwrap().name;
            let (columns, rows) = (self.size.num_cols, self.size.num_lines);
            let pointer = self.pointer;
            if let Some(grid) = self
                .ui
                .close_dialog(columns, rows, name, pointer, &self.config)
            {
                frame.blit(&grid);
                cursor = None;
            }
        }
        frame.damage = incremental.then_some(damage);
        frame.ansi(output, previous.as_ref())?;
        let (_, screens) = self.client.snapshot_and_screens_mut().unwrap();
        for screen in screens.values_mut() {
            screen.reset_damage();
        }
        self.draw_cursor(output, cursor)?;
        self.spare = previous;
        self.frame = Some(frame);
        output.write_all(b"\x1b[?2026l")
    }

    fn draw_files(&mut self, grid: &mut Grid) {
        let layout = self.layout();
        let config = &self.config;
        let (list, preview) = self.ui.file_grids(layout.files, layout.preview, "", config);
        grid.blit(&list);
        if let Some(preview) = preview {
            grid.blit(&preview);
        }
    }

    fn draw_agent(&mut self, frame: &mut Grid) -> io::Result<Option<(u16, u16)>> {
        let layout = self.layout();
        let (config, pointer) = (&self.config, self.pointer);
        let panel = crate::agent::panel::draw(
            &mut self.ui,
            config,
            layout.agent,
            layout.canvas,
            pointer,
            "",
        )?;
        if let Some(backdrop) = &panel.backdrop {
            frame.blit(backdrop);
        }
        frame.blit(&panel.grid);
        self.ui.agent.spare_grid = Some(panel.grid);
        Ok(panel.cursor)
    }
}

/// Whether two snapshots draw the same; input generations, prompts and process data change per keystroke.
fn same_view(a: &Snapshot, b: &Snapshot) -> bool {
    (&a.name, a.controller, a.focused, a.active_tab, &a.tabs)
        == (&b.name, b.controller, b.focused, b.active_tab, &b.tabs)
        && a.panes.len() == b.panes.len()
        && a.panes.iter().zip(&b.panes).all(|(a, b)| {
            (a.id, a.tab, a.rect, &a.title, &a.cwd, &a.blocks)
                == (b.id, b.tab, b.rect, &b.title, &b.cwd, &b.blocks)
        })
}

/// Draws the tab row, lit at the hovered `pointer` column.
fn draw_tabs(
    frame: &mut Grid,
    ui: &mut Ui,
    snapshot: &Snapshot,
    columns: u16,
    pointer: Option<u16>,
    config: &Config,
) {
    let row = crate::ui::tab_row_color(config);
    frame.text(0, 0, columns, "", MUTED, row);
    let status = if snapshot.controller {
        ""
    } else {
        "Read-only  "
    };
    ui.paint_tabs(
        snapshot,
        columns,
        status,
        pointer,
        config,
        |part| match part {
            TabPart::Tab { x, width, color } => match config.tab_style {
                TabStyle::Angled => {
                    frame.text(x, 0, 1, "◢", color, row);
                    frame.text(x + 1, 0, width - 2, "", color, color);
                    frame.text(x + width - 1, 0, 1, "◣", color, row);
                }
                TabStyle::Flat => frame.text(x, 0, width, "", color, color),
            },
            TabPart::Text {
                x,
                width,
                text,
                color,
                background,
            } => frame.text(x, 0, width, text, color, background),
            TabPart::Underline { x, width, color } => {
                for column in usize::from(x)..usize::from(x + width) {
                    frame.rows[0].cells[column].flags.insert(Flags::UNDERLINE);
                    frame.rows[0].extra_mut(column).underline_color = Some(rgb(color));
                }
            }
        },
    );
}

fn paint_row(
    frame: &mut Grid,
    screen: &Screen,
    pane: &PaneView,
    row: usize,
    host_columns: u16,
    (highlight, object): (bool, Option<SelectionRange>),
    left: u16,
) {
    let x = pane.rect.x + left;
    if x >= host_columns {
        return;
    }
    let y = pane.rect.y + row as u16;
    let physical_columns = usize::from(pane.rect.cols.min(host_columns - x));
    let columns = screen.columns().min(physical_columns);
    let data = screen.row(row);
    let previous_used = frame.rows[usize::from(y)].used;
    frame.put_row(x, y, data, physical_columns);
    let line = row as i32 - screen.display_offset() as i32;
    let selection = screen
        .selection_range()
        .filter(|selection| (selection.start.line..=selection.end.line).contains(&line));
    let target = &mut frame.rows[usize::from(y)];
    let start = usize::from(x);
    let used = if highlight || selection.is_some_and(|selection| line < selection.end.line) {
        columns
    } else {
        let text_end = data.cells[..columns]
            .iter()
            .rposition(|cell| *cell != crate::core::Cell::default())
            .map_or(0, |column| column + 1);
        data.extras()
            .last()
            .map_or(0, |(column, _)| column + 1)
            .max(text_end)
            .max(selection.map_or(0, |selection| selection.end.column + 1))
            .min(columns)
    };
    target.used = previous_used.max(start + used);
    for column in 0..physical_columns {
        let index = start + column;
        let hide = {
            let cell = &mut target.cells[index];
            let selected = object.is_some_and(|range| range.contains(Point { line, column }))
                || !cell.flags.contains(Flags::PROMPT)
                    && selection
                        .is_some_and(|selection| selection.contains(Point { line, column }));
            let default_background = cell.bg == Color::Named(NamedColor::Background);
            let inverse = cell.flags.contains(Flags::INVERSE);
            let mut fg = screen.color(cell.fg);
            let mut bg = screen.color(cell.bg);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            cell.fg = Color::Spec(fg);
            cell.bg = Color::Spec(bg);
            cell.flags.remove(Flags::INVERSE);
            if selected {
                cell.bg = rgb(0x373737);
            } else if highlight && default_background && !inverse {
                cell.bg = rgb(crate::panel::BLOCK_HOVER);
            }
            let hide = column >= columns
                || cell.flags.contains(Flags::HIDDEN)
                || cell.c.is_control()
                || cell.flags.contains(Flags::WIDE_CHAR) && column + 1 >= columns;
            if hide {
                cell.c = ' ';
                cell.flags.remove(Flags::HIDDEN);
            }
            hide
        };
        if target.extra(index).is_some() {
            let extra = target.extra_mut(index);
            if hide {
                extra.zerowidth.clear();
            }
            if let Some(color) = &mut extra.underline_color {
                *color = Color::Spec(screen.color(*color));
            }
        }
    }
}

fn draw_border(frame: &mut Grid, x: u16, y: u16, count: u16, vertical: bool) {
    for step in 0..count {
        let (x, y) = if vertical {
            (x, y + step)
        } else {
            (x + step, y)
        };
        if let Some(cell) = frame
            .rows
            .get_mut(usize::from(y))
            .and_then(|row| row.cells.get_mut(usize::from(x)))
        {
            cell.c = if vertical { '│' } else { '─' };
            cell.fg = crate::panel::rgb(crate::panel::BORDER);
        }
    }
}

fn draw_separators(frame: &mut Grid, snapshot: &Snapshot, size: WindowSize, left: u16) {
    for PaneView { rect, .. } in &snapshot.panes {
        if rect.x > 0 && rect.x <= size.num_cols {
            let rows = rect.rows.min(size.num_lines.saturating_sub(rect.y));
            draw_border(frame, rect.x + left - 1, rect.y, rows, true);
        }
        if rect.y > 1 && rect.y <= size.num_lines {
            let columns = rect.cols.min(size.num_cols.saturating_sub(rect.x));
            draw_border(frame, rect.x + left, rect.y - 1, columns, false);
        }
    }
}

// A multiple of three keeps intermediate base64 chunks unpadded.
const OSC52_CHUNK_SIZE: usize = 48 * 1024;

struct ClipboardTransfer {
    reader: Box<dyn Read>,
    target: u8,
    incomplete: bool,
    bytes: u64,
}

impl ClipboardTransfer {
    fn step(&mut self, output: &mut Vec<u8>) -> io::Result<Option<String>> {
        if self.bytes == 0 {
            output.extend_from_slice(b"\x1b]52;");
            output.extend_from_slice(&[self.target, b';']);
        }
        let mut chunk = Vec::new();
        let read = self
            .reader
            .by_ref()
            .take(OSC52_CHUNK_SIZE as u64)
            .read_to_end(&mut chunk);
        if read.is_err() || chunk.is_empty() {
            output.extend_from_slice(b"\x1b\\");
        }
        read?;
        if chunk.is_empty() {
            return Ok(Some(format!(
                "Sent {} bytes via OSC 52{}; host may reject clipboard limits",
                self.bytes,
                if self.incomplete {
                    " (incomplete output)"
                } else {
                    ""
                }
            )));
        }
        self.bytes += chunk.len() as u64;
        crate::base64::encode(&chunk, output);
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
