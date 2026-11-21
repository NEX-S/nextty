//! ANSI frontend for an existing workspace; the daemon owns PTYs and VT state.

#[path = "tui_input.rs"]
mod input;

use crate::agent::panel::{AgentAction, editor};
use crate::config::Config;
use crate::core::{Flags, Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::input::{PasteTarget, TextDecoder, mouse_bytes};
use crate::panel::{Grid, rect};
use crate::screen::Screen;
use crate::ui::{Focus, READ_ONLY, Ui, cursor_bytes};
use crate::vt::{Color, CursorShape, NamedColor, Rgb};
use crate::width::text_width;
use crate::workspace::{
    Action, Axis, Client, PaneId, PaneView, Rect, Response, Snapshot, SplitDrag,
};
use input::{Decoder, Input, PointerEvent, PointerKind};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::time::{Duration, Instant};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?2026l\x18\x1b\\\x1b[0m\x1b[?1006l\x1b[?1003l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[?7h\x1b[?25h\x1b[?1049l";

fn check(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

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
        check(unsafe { libc::tcgetattr(input, &mut termios) })?;
        let output_flags = check(unsafe { libc::fcntl(output, libc::F_GETFL) })?;
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
        let signals = check(unsafe { libc::signalfd(-1, &signals, signalfd) })?;
        let guard = Self {
            input,
            output,
            termios,
            output_flags,
            signals: unsafe { File::from_raw_fd(signals) },
        };
        let mut raw = termios;
        unsafe { libc::cfmakeraw(&mut raw) };
        check(unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) })?;
        check(unsafe { libc::fcntl(output, libc::F_SETFL, output_flags | libc::O_NONBLOCK) })?;
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
    frontend.client.send(Action::Resize(frontend.view_size()))?;
    let mut stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDIN_FILENO) });
    let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) });
    let mut decoder = Decoder::default();
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let result = (|| {
        frontend.poll()?;
        while frontend.running {
            let (changed, _) = frontend
                .ui
                .service(&mut frontend.client, &frontend.config, &fds)?;
            frontend.dirty |= changed;
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
    // Closing the connection also detaches if the last nonblocking send cannot flush.
    let _ = frontend
        .client
        .send(Action::Detach)
        .and_then(|()| frontend.client.flush());
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
    check(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) })?;
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
    panel_utf8: TextDecoder,
    paste: Option<PasteTarget>,
    selecting: Option<PaneId>,
    split_drag: Option<SplitDrag>,
    last_click: Option<(Instant, PaneId, Point)>,
    /// Copies queue behind a running transfer so a cut never loses its text.
    copies: VecDeque<ClipboardTransfer>,
    pointer: Option<(u16, u16)>,
    agent: agent_ui::AgentState,
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
            panel_utf8: TextDecoder::default(),
            paste: None,
            selecting: None,
            split_drag: None,
            last_click: None,
            copies: VecDeque::new(),
            pointer: None,
            agent: agent_ui::AgentState::default(),
        })
    }

    fn panel_layout(&self) -> crate::ui::PanelLayout {
        let agent = if self.ui.agent.open {
            self.agent_width()
        } else {
            0
        };
        let available = self.size.num_cols.saturating_sub(agent);
        crate::ui::PanelLayout::new(available, available, self.ui.files_open)
    }

    fn file_width(&self) -> u16 {
        self.panel_layout().file_width
    }

    fn file_rect(&self) -> Rect {
        rect(
            0,
            1,
            self.file_width(),
            self.view_size().num_lines.saturating_sub(1),
        )
    }

    fn file_preview_rect(&self) -> Rect {
        crate::file_panel::preview_rect(rect(
            0,
            1,
            self.size.num_cols,
            self.size.num_lines.saturating_sub(2),
        ))
    }

    fn view_size(&self) -> WindowSize {
        let rows = self.size.num_lines;
        let resources = &self.ui.resources;
        let chrome = resources.rows(rows.saturating_sub(2)) + if resources.open { 2 } else { 1 };
        WindowSize {
            num_cols: self.panel_layout().terminal_columns,
            num_lines: rows.saturating_sub(chrome).max(2),
            ..self.size
        }
    }

    fn resize(&mut self) -> io::Result<()> {
        self.ui.search.close(&mut self.client)?;
        self.dirty = true;
        if self.client.controller() {
            self.client.send(Action::Resize(self.view_size()))?;
        }
        Ok(())
    }

    fn copy_text(&mut self, text: String, primary: bool) {
        let target = if primary { b'p' } else { b'c' };
        self.queue_copy(Box::new(io::Cursor::new(text.into_bytes())), target, false);
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
        use crate::input::{Key, KeyResult};
        let viewport = if self.ui.files_open && self.ui.focus == Focus::Files {
            self.ui
                .file_viewport(self.file_rect(), self.file_preview_rect())
        } else if self.ui.agent.open && self.ui.focus.is_agent() {
            let rect = editor(&self.ui, self.agent_rect()).0;
            let columns = self
                .ui
                .agent
                .active()
                .editor_columns(usize::from(rect.cols));
            (columns, usize::from(rect.rows))
        } else {
            (
                usize::from(self.size.num_cols),
                usize::from(self.size.num_lines),
            )
        };
        match self.ui.key(
            &mut self.client,
            Key {
                symbol,
                modifiers,
                text,
            },
            viewport,
        )? {
            KeyResult::Unhandled => return Ok(()),
            KeyResult::Copy(text) => self.copy_text(text, false),
            KeyResult::Paste => self.message("Paste with your host terminal's shortcut"),
            KeyResult::Quit => self.running = false,
            KeyResult::Handled => (),
        }
        self.dirty = true;
        Ok(())
    }

    fn panel_paste(&mut self, bytes: &[u8], target: PasteTarget) -> io::Result<()> {
        let text = self.panel_utf8.decode(bytes);
        self.ui.paste(&mut self.client, target, &text)?;
        self.dirty = true;
        Ok(())
    }

    fn panel_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some((symbol, modifiers)) = crate::input::sequence_key(bytes) {
            self.panel_utf8.clear();
            return self.panel_key(symbol, modifiers, "");
        }
        if matches!(bytes, [0x1b, b'[' | b'O', ..]) {
            return Ok(());
        }
        // Alt plus a non-ASCII character arrives as ESC and the character.
        let bytes = match bytes {
            [0x1b, rest @ ..] if rest.first().is_some_and(|byte| !byte.is_ascii()) => rest,
            _ => bytes,
        };
        let text = self.panel_utf8.decode(bytes);
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
        use crate::monitor::{BACKGROUND, Emphasis, MUTED};
        let (columns, rows) = (self.size.num_cols, self.size.num_lines);
        let top = self.view_size().num_lines;
        let last = rows - 1;
        let monitor_focused = self.ui.focus == Focus::Monitor;
        self.ui.resources.pointer = self
            .pointer
            .filter(|&(_, row)| self.ui.resources.open && row > top && row < last)
            .map(|(column, row)| (column, i32::from(row) - i32::from(top) - 1));
        for row in top..rows {
            grid.text(0, row, columns, "", MUTED, BACKGROUND);
        }
        if self.ui.resources.open {
            draw_border(grid, 0, top, columns, false);
            let origin = i32::from(top) + 1;
            let view = self
                .ui
                .resources
                .view(columns, last.saturating_sub(top + 1));
            for plot in view.plots {
                let width = usize::from(plot.width);
                let seconds = self.ui.resources.range as f32;
                for series in &plot.series {
                    let color = crate::monitor::series_color(series.name);
                    let height = usize::from(plot.height);
                    let graph = braille_trend(&series.points, width, height, seconds, plot.maximum);
                    for (index, &bits) in graph.iter().enumerate().filter(|(_, bits)| **bits != 0) {
                        let y = origin + plot.y + (index / width) as i32;
                        if y < origin || y >= i32::from(last) {
                            continue;
                        }
                        let (x, y) = (plot.x + (index % width) as u16, y as u16);
                        // Series overlap in one braille cell: merge dots, the last series colors it.
                        let old = grid
                            .rows
                            .get(usize::from(y))
                            .and_then(|row| row.cells.get(usize::from(x)));
                        let old = old
                            .and_then(|cell| u32::from(cell.c).checked_sub(0x2800))
                            .filter(|bits| *bits < 0x100)
                            .unwrap_or(0);
                        let braille = char::from_u32(0x2800 + (old | u32::from(bits))).unwrap();
                        grid.text(x, y, 1, &braille.to_string(), color, BACKGROUND);
                    }
                }
            }
            for text in view.texts {
                let y = origin + text.y;
                if y >= origin && y < i32::from(last) && text.x < columns {
                    grid.text(
                        text.x,
                        y as u16,
                        text.width.min(columns - text.x),
                        &text.text,
                        text.color,
                        match text.emphasis(monitor_focused) {
                            Emphasis::Selected => crate::panel::SELECTED,
                            Emphasis::Highlighted => crate::panel::HOVER,
                            Emphasis::None => BACKGROUND,
                        },
                    );
                }
            }
        }
        let view =
            self.ui
                .resources
                .footer(columns, self.client.controller(), self.client.is_remote());
        let right = view
            .texts
            .iter()
            .map(|text| text.x)
            .min()
            .unwrap_or(columns);
        for text in view.texts {
            let hovered = self
                .pointer
                .is_some_and(|(x, y)| y == last && (text.x..text.x + text.width).contains(&x))
                && view.hits.iter().any(|hit| hit.x == text.x);
            let highlighted = hovered || text.emphasis(monitor_focused) != Emphasis::None;
            let background = if highlighted {
                crate::panel::HOVER
            } else {
                BACKGROUND
            };
            grid.text(text.x, last, text.width, &text.text, text.color, background);
        }
        if let Some(snapshot) = self.client.snapshot() {
            let context = self.ui.footer(snapshot, self.client.is_remote());
            let width = right.saturating_sub(2);
            let text = crate::width::fit_middle(&context, width);
            grid.text(1, last, width, &text, MUTED, BACKGROUND);
        }
    }

    fn resource_mouse(&mut self, pointer: PointerEvent) -> io::Result<()> {
        let top = self.view_size().num_lines;
        let last = self.size.num_lines - 1;
        let columns = self.size.num_cols;
        self.dirty = true;
        if self.ui.resources.dragging {
            match pointer.kind {
                PointerKind::Release => self.ui.resources.dragging = false,
                PointerKind::Motion => {
                    let height = self.size.num_lines.saturating_sub(2);
                    self.ui
                        .resources
                        .resize(last.saturating_sub(pointer.row + 1), height);
                    return self.resize();
                }
                _ => (),
            }
        } else if pointer.kind == PointerKind::Scroll && self.ui.resources.open {
            let scroll = &mut self.ui.resources.scroll;
            *scroll = scroll.saturating_add_signed(pointer.wheel());
        } else if pointer.press(0) {
            let (view, origin) = if pointer.row == last {
                let footer = self.ui.resources.footer(
                    columns,
                    self.client.controller(),
                    self.client.is_remote(),
                );
                (footer, last)
            } else if pointer.row == top {
                self.ui.monitor_drag();
                return Ok(());
            } else {
                self.ui.focus = Focus::Monitor;
                let height = last.saturating_sub(top + 1);
                (self.ui.resources.view(columns, height), top + 1)
            };
            let hit = view.hits.iter().rfind(|hit| {
                i32::from(pointer.row) == i32::from(origin) + hit.y
                    && (hit.x..hit.x + hit.width).contains(&pointer.col)
            });
            if let Some(hit) = hit {
                self.ui.monitor_action(&mut self.client, hit.action)?;
            }
        }
        Ok(())
    }

    /// `panel_cursor` is the caret of the focused panel, if it has one.
    fn draw_cursor(
        &self,
        output: &mut Vec<u8>,
        panel_cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        let in_panel = self.ui.close_prompt.is_some() || self.ui.input_target().is_some();
        let mode = self
            .client
            .focused()
            .filter(|_| !in_panel)
            .map_or(Mode::NONE, |pane| self.client.mode(pane));
        output.extend_from_slice(if mode.contains(Mode::APP_CURSOR) {
            b"\x1b[?1h"
        } else {
            b"\x1b[?1l"
        });
        output.extend_from_slice(if mode.contains(Mode::APP_KEYPAD) {
            b"\x1b="
        } else {
            b"\x1b>"
        });
        let pane_cursor = self.pane_cursor().filter(|_| !in_panel);
        if let Some((column, row, shape)) = pane_cursor {
            write!(
                output,
                "\x1b[0m\x1b[{shape} q\x1b[{};{}H\x1b[?25h",
                row + 1,
                column + 1
            )
        } else if let Some((column, row)) = panel_cursor {
            write!(output, "\x1b[{};{}H\x1b[?25h", row + 1, column + 1)
        } else {
            output.write_all(b"\x1b[?25l")
        }
    }

    fn pane_cursor(&self) -> Option<(u32, i32, u8)> {
        let snapshot = self.client.snapshot()?;
        let pane = snapshot.focused_pane()?;
        let screen = self.client.screens().get(&pane.id)?;
        let (point, shape) = screen.cursor();
        let row = point.line + screen.display_offset() as i32;
        let x = u32::from(pane.rect.x) + point.column as u32;
        let y = i32::from(pane.rect.y) + row;
        let size = self.view_size();
        (snapshot.controller
            && shape != CursorShape::Hidden
            && (0..i32::from(pane.rect.rows)).contains(&row)
            && x < u32::from(size.num_cols)
            && y < i32::from(size.num_lines))
        .then(|| {
            let shape = match shape {
                CursorShape::Underline => 4,
                CursorShape::Beam => 6,
                _ => 2,
            };
            (x + u32::from(self.file_width()), y, shape)
        })
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
        for event in self.client.poll()? {
            self.event(event)?;
        }
        Ok(())
    }

    fn event(&mut self, event: Response) -> io::Result<()> {
        match event {
            Response::ScreenDirty => {
                self.ui.search.observe(&mut self.client)?;
                self.screen_dirty = true;
            }
            Response::Error(error) | Response::CopyError { error, .. } => self.message(error),
            Response::Detached => self.running = false,
            Response::Clipboard {
                file, incomplete, ..
            } => {
                if std::env::var("TERM").is_ok_and(|term| term == "dumb" || term == "linux") {
                    self.message("This host terminal does not support OSC 52 clipboard transfer");
                } else {
                    self.queue_copy(Box::new(file), b'c', incomplete);
                }
            }
            Response::ClipboardStore { primary, text } => self.copy_text(text, primary),
            Response::Search {
                pane,
                request,
                count,
                truncated,
            } => {
                let count = count as usize;
                self.dirty |=
                    self.ui
                        .search
                        .result(&mut self.client, pane, request, count, truncated)?;
            }
            _ => (),
        }
        Ok(())
    }

    fn input(&mut self, event: Input) -> io::Result<()> {
        let panels = self.ui.panel_state();
        let previous = self.pointer;
        if let Input::Mouse(pointer) = &event {
            self.pointer = Some((pointer.col, pointer.row));
            self.dirty |= previous != self.pointer;
        }
        self.dispatch(event, previous)?;
        if panels != self.ui.panel_state() {
            self.resize()?;
        }
        Ok(())
    }

    fn dispatch(&mut self, event: Input, previous: Option<(u16, u16)>) -> io::Result<()> {
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
                Input::Mouse(pointer) if modal => return self.modal_mouse(pointer),
                Input::Alt(key) if !modal => {
                    return self.panel_key(u32::from(key), crate::input::ALT, "");
                }
                Input::PasteStart if !modal => {
                    self.panel_utf8.clear();
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
            Input::Focus(_) => (),
            Input::Mouse(pointer) => self.route_mouse(pointer, previous)?,
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
                self.panel_utf8.clear();
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

    fn modal_mouse(&mut self, pointer: PointerEvent) -> io::Result<()> {
        self.dirty = true;
        let Some(prompt) = self.ui.close_prompt else {
            self.file_mouse(pointer);
            return Ok(());
        };
        let close = crate::ui::close_rect(self.size.num_cols, self.size.num_lines);
        if pointer.press(0)
            && let Some(terminate) = crate::ui::close_hit(close, prompt, pointer.col, pointer.row)
            && matches!(
                self.ui.choose_close(&mut self.client, terminate)?,
                crate::input::KeyResult::Quit
            )
        {
            self.running = false;
        }
        Ok(())
    }

    fn route_mouse(
        &mut self,
        pointer: PointerEvent,
        previous: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if self.agent_captures_mouse() {
            return self.agent_mouse(pointer).map(drop);
        }
        if self.ui.resources.dragging || pointer.row >= self.view_size().num_lines {
            return self.resource_mouse(pointer);
        }
        if pointer.press(0) {
            self.ui.focus = Focus::Terminal;
        }
        if self.agent_mouse(pointer)? {
            return Ok(());
        }
        if self.ui.files_open && self.file_rect().contains(pointer.col, pointer.row) {
            if pointer.kind != PointerKind::Motion && pointer.kind != PointerKind::Release {
                self.ui.focus = Focus::Files;
                self.file_mouse(pointer);
            }
            return Ok(());
        }
        self.terminal_mouse(pointer, previous)
    }

    fn terminal_mouse(
        &mut self,
        pointer: PointerEvent,
        previous: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if pointer.row == 0 {
            if pointer.press(0)
                && let Some(snapshot) = self
                    .client
                    .snapshot()
                    .filter(|snapshot| snapshot.controller)
                && let Some(hit) =
                    crate::ui::tab_bar(snapshot, self.view_size().num_cols + self.file_width(), "")
                        .at(snapshot, pointer.col)
            {
                self.ui.tab_click(&mut self.client, hit)?;
                self.dirty = true;
            }
            return Ok(());
        }
        let left = self.file_width();
        let (column, row) = (pointer.col.saturating_sub(left), pointer.row);
        if let Some(drag) = self.split_drag {
            if pointer.kind == PointerKind::Release {
                self.split_drag = None;
            } else if pointer.kind == PointerKind::Motion
                && previous.is_none_or(|(old_column, old_row)| match drag.axis {
                    Axis::LeftRight => old_column != pointer.col,
                    Axis::TopBottom => old_row != pointer.row,
                })
            {
                self.client.send(drag.action(column, row))?;
            }
            return Ok(());
        }
        let Some(snapshot) = self
            .client
            .snapshot()
            .filter(|snapshot| snapshot.controller)
        else {
            return Ok(());
        };
        if pointer.press(0)
            && let Some(drag) = snapshot.divider_at(column, row)
        {
            self.split_drag = Some(drag);
            return Ok(());
        }
        let Some(view) = snapshot.pane_at(column, row) else {
            return Ok(());
        };
        let (pane, rect, focused) = (view.id, view.rect, snapshot.focused);
        if pointer.press(0) && focused != pane {
            self.ui.act(&mut self.client, Action::Focus(pane))?;
        }
        let Some(screen) = self.client.screens().get(&pane) else {
            return Ok(());
        };
        let mode = screen.mode();
        let offset = screen.display_offset();
        let y = row - rect.y;
        let x = (column - rect.x).min(screen.columns().saturating_sub(1) as u16);
        let point = Point {
            line: i32::from(y) - offset as i32,
            column: usize::from(x),
        };
        let alt = pointer.mods & crate::input::ALT != 0;
        if pointer.press(0) && (!mode.intersects(Mode::MOUSE_MODE) || alt) {
            let now = Instant::now();
            let double = self.last_click.is_some_and(|(at, old_pane, old_point)| {
                old_pane == pane
                    && old_point == point
                    && now.duration_since(at) < Duration::from_millis(400)
            });
            self.last_click = Some((now, pane, point));
            if double && let Some(object) = crate::context::object_at(screen, point) {
                crate::ui::open_object(&self.client, pane, &object.text)?;
                return Ok(());
            }
        }
        if mode.intersects(Mode::MOUSE_MODE) && !alt {
            if pointer.kind == PointerKind::Motion
                && !mode.contains(Mode::MOUSE_MOTION)
                && (!mode.contains(Mode::MOUSE_DRAG) || pointer.button == 3)
            {
                return Ok(());
            }
            if let Some(data) = mouse_bytes(
                pointer.code,
                usize::from(x),
                usize::from(y),
                pointer.kind == PointerKind::Release,
                mode,
            ) {
                self.send_input(pane, &data)?;
            }
        } else if pointer.kind == PointerKind::Scroll {
            let up = pointer.button == 0;
            if mode.contains(Mode::ALT_SCREEN | Mode::ALTERNATE_SCROLL) && !alt {
                let bytes = if up { *b"\x1b[A" } else { *b"\x1b[B" };
                self.send_input(pane, &cursor_bytes(bytes, mode))?;
            } else {
                self.client.send(Action::Scroll {
                    pane,
                    scroll: Scroll::Delta(-pointer.wheel() as i32),
                })?;
            }
        } else if pointer.press(2) {
            if let Some((_, object)) = self.hovered_object() {
                self.copy_text(object.text, false);
            } else {
                self.message("Paste with your host terminal's shortcut");
            }
        } else if pointer.button == 0 {
            match pointer.kind {
                PointerKind::Release => self.selecting = None,
                PointerKind::Motion if self.selecting == Some(pane) => {
                    self.client.send(Action::SelectionUpdate {
                        pane,
                        point,
                        side: Side::Right,
                    })?;
                }
                PointerKind::Press => {
                    self.selecting = Some(pane);
                    self.client.send(Action::SelectionStart {
                        pane,
                        point,
                        side: Side::Left,
                        kind: SelectionType::Simple,
                    })?;
                }
                _ => (),
            }
        }
        Ok(())
    }

    fn hovered(&self) -> Option<(PaneId, Point)> {
        let (x, y) = self.pointer?;
        let x = x.checked_sub(self.file_width())?;
        let pane = self.client.snapshot()?.pane_at(x, y)?;
        let offset = self.client.screens().get(&pane.id)?.display_offset();
        Some((
            pane.id,
            Point {
                line: i32::from(y - pane.rect.y) - offset as i32,
                column: usize::from(x - pane.rect.x),
            },
        ))
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
        let size = self.view_size();
        let left = self.file_width();
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
            (frame, self.frame.take())
        };
        if !incremental {
            paint_tabs(
                &mut frame,
                snapshot,
                host_columns,
                self.pointer,
                &self.config,
            );
        }
        let mut damage = Vec::new();
        for pane in &snapshot.panes {
            if let Some(screen) = self.client.screens().get(&pane.id) {
                let rows = screen
                    .screen_lines()
                    .min(usize::from(pane.rect.rows))
                    .min(usize::from(size.num_lines.saturating_sub(pane.rect.y)));
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
                    let object = object
                        .as_ref()
                        .filter(|(id, _)| *id == pane.id)
                        .map(|(_, object)| object.range);
                    paint_row(
                        &mut frame,
                        screen,
                        pane,
                        row,
                        host_columns,
                        (highlight, object),
                        left,
                    );
                }
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
                let chrome = crate::panel::CHROME;
                frame.text(0, 0, columns, "", foreground, chrome);
                frame.text(1, 0, columns.saturating_sub(2), &label, foreground, chrome);
                cursor = Some((caret as u16 + 1, 0));
            }
            if let Some(prompt) = self.ui.close_prompt {
                let name = &self.client.snapshot().unwrap().name;
                let close = crate::ui::close_rect(self.size.num_cols, self.size.num_lines);
                let grid = crate::ui::close_grid(prompt, name, close, self.pointer, &self.config);
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

    fn file_mouse(&mut self, pointer: PointerEvent) {
        self.dirty = true;
        let (list, preview) = (self.file_rect(), self.file_preview_rect());
        if pointer.kind == PointerKind::Scroll {
            self.ui.file_wheel(pointer.wheel(), list, preview);
        } else if pointer.press(0) {
            self.ui.file_press(list, preview, pointer.col, pointer.row);
        }
    }

    fn draw_files(&mut self, grid: &mut Grid) {
        let rect = self.file_rect();
        let preview = self.file_preview_rect();
        let Some(files) = &mut self.ui.files else {
            return;
        };
        let focused =
            self.ui.focus == Focus::Files && files.focus() == crate::files::FileFocus::List;
        grid.blit(&crate::file_panel::list(
            files,
            rect,
            "",
            focused,
            &self.config,
        ));
        if files.focus() == crate::files::FileFocus::Preview {
            grid.blit(&crate::file_panel::preview(files, preview, &self.config));
        }
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

fn paint_tabs(
    grid: &mut Grid,
    snapshot: &Snapshot,
    columns: u16,
    pointer: Option<(u16, u16)>,
    config: &Config,
) {
    use crate::panel::{ACCENT, CHROME};
    use crate::ui::TabHit;
    grid.text(0, 0, columns, "", crate::panel::MUTED, CHROME);
    let controller = snapshot.controller;
    grid.text(
        1,
        0,
        1,
        "›",
        if controller { 0x969696 } else { 0xfe9b30 },
        CHROME,
    );
    let bar = crate::ui::tab_bar(
        snapshot,
        columns,
        if controller { "" } else { "Read-only  " },
    );
    let hit = pointer
        .filter(|&(_, row)| row == 0)
        .and_then(|(column, _)| bar.at(snapshot, column));
    let angled = config.tab_style == crate::config::TabStyle::Angled;
    for &(index, x, width) in &bar.tabs {
        let tab = &snapshot.tabs[index];
        let active = tab.id == snapshot.active_tab;
        let background = if active { 0x202020 } else { 0x1c1c1c };
        let close_hovered = hit == Some(TabHit::Close(tab.id));
        let title_color = if close_hovered || hit == Some(TabHit::Select(index)) {
            0xd4d9d8
        } else if active {
            0xcccccc
        } else {
            0x969696
        };
        let close_color = if close_hovered {
            0xe87575
        } else if active {
            0x949494
        } else {
            0x606060
        };
        let (left, right) = if angled { ("◢", "◣") } else { (" ", " ") };
        grid.text(x, 0, 1, left, background, CHROME);
        grid.text(x + 1, 0, 3, " • ", crate::ui::tab_color(tab), background);
        grid.text(x + 4, 0, width - 8, &tab.title, title_color, background);
        grid.text(x + width - 4, 0, 3, " × ", close_color, background);
        grid.text(x + width - 1, 0, 1, right, background, CHROME);
        let flat_active = config.tab_style == crate::config::TabStyle::Flat && active;
        if flat_active || close_hovered {
            let underline = if close_hovered && !flat_active {
                0xe87575
            } else {
                ACCENT
            };
            let row = &mut grid.rows[0];
            for column in x..x + width {
                if flat_active || column >= x + width - 4 {
                    let column = usize::from(column);
                    row.cells[column].flags.insert(Flags::UNDERLINE);
                    row.extra_mut(column).underline_color = Some(crate::panel::rgb(underline));
                }
            }
        }
    }
    if let Some(x) = bar.plus {
        let hovered = hit == Some(TabHit::New);
        let color = if hovered { ACCENT } else { 0x606060 };
        grid.text(x, 0, 3, "", color, CHROME);
        grid.text(x + 1, 0, 1, "+", color, CHROME);
        if hovered {
            grid.rows[0].cells[usize::from(x + 1)]
                .flags
                .insert(Flags::UNDERLINE);
        }
    }
    let width = text_width(&bar.label) as u16;
    grid.text(
        bar.label_x,
        0,
        width,
        &bar.label,
        if controller { 0xb8b8b8 } else { 0xfe9b30 },
        CHROME,
    );
}

fn paint_row(
    frame: &mut Grid,
    screen: &Screen,
    pane: &PaneView,
    row: usize,
    host_columns: u16,
    (highlight, object): (bool, Option<crate::core::SelectionRange>),
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
                cell.bg = Color::Spec(Rgb {
                    r: 55,
                    g: 55,
                    b: 55,
                });
            } else if highlight && default_background && !inverse {
                cell.bg = Color::Spec(Rgb {
                    r: 38,
                    g: 38,
                    b: 38,
                });
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
        if hide || target.extra(index).is_some() {
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

mod agent_ui;
#[cfg(test)]
mod tests;
