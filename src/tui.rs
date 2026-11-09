//! ANSI frontend for an existing workspace; the daemon owns PTYs and VT state.

#[path = "tui_input.rs"]
mod input;

use crate::agent::panel::{AgentAction, AgentLine, editor};
use crate::clipboard_reader::ClipboardReader;
use crate::config::Config;
use crate::core::{Flags, Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::input::{PasteTarget, TextDecoder, mouse_bytes};
use crate::panel::mix_color;
use crate::screen::Screen;
use crate::ui::{Focus, Ui};
use crate::vt::{Color, CursorShape, NamedColor, Rgb};
use crate::width::{fit_label, scalar_width, text_width};
use crate::workspace::{
    Action, Axis, Client, ClientEvent, PaneId, PaneView, Rect, Snapshot, SplitDrag, tab_chrome,
};
use input::{Decoder, Input, Mouse};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?2026l\x18\x1b\\\x1b[0m\x1b[?1006l\x1b[?1003l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[?7h\x1b[?25h\x1b[?1049l";

struct TerminalGuard {
    input: RawFd,
    output: RawFd,
    termios: libc::termios,
    output_flags: i32,
    mask: libc::sigset_t,
    signals: OwnedFd,
}

impl TerminalGuard {
    fn enter(input: RawFd, output: RawFd) -> io::Result<Self> {
        if unsafe { libc::isatty(input) } != 1 || unsafe { libc::isatty(output) } != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TUI requires a terminal on stdin and stdout",
            ));
        }
        let mut termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(input, &mut termios) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let output_flags = unsafe { libc::fcntl(output, libc::F_GETFL) };
        if output_flags < 0 {
            return Err(io::Error::last_os_error());
        }
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
        let mut mask = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, &mut mask) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        let fd = unsafe { libc::signalfd(-1, &signals, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) };
            return Err(error);
        }
        let guard = Self {
            input,
            output,
            termios,
            output_flags,
            mask,
            signals: unsafe { OwnedFd::from_raw_fd(fd) },
        };
        let mut raw = termios;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(output, libc::F_SETFL, output_flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(guard)
    }

    fn next_signal(&self) -> io::Result<Option<u32>> {
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        let count = unsafe {
            libc::read(
                self.signals.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of_val(&info),
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        if count as usize != std::mem::size_of_val(&info) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short signalfd read",
            ));
        }
        Ok(Some(info.ssi_signo))
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
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut());
        }
    }
}

/// Attach the terminal frontend to a client already sent Create or Attach.
pub fn run(client: Client, config: crate::config::Config) -> io::Result<()> {
    let terminal = TerminalGuard::enter(libc::STDIN_FILENO, libc::STDOUT_FILENO)?;
    let size = terminal_size(libc::STDOUT_FILENO)?;
    let mut frontend = Frontend::new(client, size, config)?;
    let mut output = Output {
        bytes: ENTER.to_vec(),
        written: 0,
    };
    frontend.client.send(Action::Resize(frontend.view_size()))?;
    let mut decoder = Decoder::default();
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let result = (|| {
        let mut output_ready = true;
        frontend.poll()?;
        while frontend.running {
            {
                let (changed, _) = frontend
                    .ui
                    .poll_agent(&mut frontend.client, &frontend.config)?;
                let layout_changed = fds.iter().any(|source| {
                    source.fd == frontend.ui.agent.layout_fd() && source.revents != 0
                }) && frontend.ui.agent.poll_layout()?;
                if (changed && frontend.ui.agent.open) || layout_changed {
                    frontend.agent.dirty = true;
                    frontend.dirty = true;
                }
            }

            let (resources, changed) = frontend.ui.service(&frontend.client, &fds)?;
            frontend.resource_dirty |= resources;
            frontend.dirty |= changed;
            if output.is_empty() {
                if let Some(transfer) = &mut frontend.clipboard {
                    if let Some(message) =
                        transfer.step(&mut output.bytes, frontend.clipboard_reader.as_mut())?
                    {
                        frontend.clipboard = None;
                        frontend.message(message);
                    }
                } else if frontend.dirty {
                    frontend.draw(&mut output.bytes)?;
                    frontend.dirty = false;
                    frontend.client.request_screen()?;
                }
            }
            if output_ready && let Err(error) = output.flush_available(libc::STDOUT_FILENO) {
                let mut state = libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: 0,
                    revents: 0,
                };
                crate::poll::poll(std::slice::from_mut(&mut state), 0)?;
                if state.revents & libc::POLLHUP != 0 {
                    break;
                }
                return Err(error);
            }
            output_ready = output.is_empty();
            let now = Instant::now();
            let mut timeout = decoder.timeout(now);

            if let Some(until) = frontend.ui.deadline() {
                let remaining = until.saturating_duration_since(now);
                timeout = Some(timeout.map_or(remaining, |timeout| timeout.min(remaining)));
            }
            let clipboard_waiting = frontend.clipboard.as_ref().is_some_and(|transfer| {
                matches!(transfer.reader, ClipboardSource::File)
                    && frontend.clipboard_reader.as_ref().unwrap().pending()
            });
            if output.is_empty()
                && (frontend.clipboard.is_some() && !clipboard_waiting
                    || frontend.clipboard.is_none() && frontend.dirty)
            {
                timeout = Some(Duration::ZERO);
            }
            if frontend.client.needs_poll() {
                timeout = Some(Duration::ZERO);
            }
            frontend.client.flush()?;
            fds.clear();
            fds.extend([
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
                frontend.client.pollfd(),
                libc::pollfd {
                    fd: terminal.signals.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: if output.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
                libc::pollfd {
                    fd: frontend.client.receiver_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: frontend
                        .clipboard_reader
                        .as_ref()
                        .filter(|_| output.is_empty() && clipboard_waiting)
                        .map_or(-1, ClipboardReader::fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ]);
            fds.extend(frontend.ui.fds());
            let milliseconds = timeout.map_or(-1, crate::poll::timeout_millis);
            crate::poll::poll(&mut fds, milliseconds)?;
            if fds[4].revents != 0 {
                crate::eventfd::consume_notification(fds[4].fd)?;
            }
            if fds[5].revents != 0 {
                crate::eventfd::consume_notification(fds[5].fd)?;
            }
            output_ready |= fds[3].revents & libc::POLLOUT != 0;
            if fds[1].revents & libc::POLLOUT != 0 {
                frontend.client.flush()?;
            }
            while fds[2].revents != 0
                && let Some(signal) = terminal.next_signal()?
            {
                if signal == libc::SIGWINCH as u32 {
                    frontend.size = terminal_size(libc::STDOUT_FILENO)?;
                    frontend.full = true;
                    frontend.dirty = true;
                    if frontend.controller() {
                        frontend.resize()?;
                    }
                } else {
                    frontend.running = false;
                }
            }
            if fds[0].revents & libc::POLLIN != 0 {
                let mut bytes = [0; 8192];
                let count = unsafe {
                    libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len())
                };
                if count > 0 {
                    for event in decoder.feed(&bytes[..count as usize], Instant::now()) {
                        frontend.input(event)?;
                    }
                } else if count == 0 {
                    frontend.running = false;
                } else {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        return Err(error);
                    }
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
                    || fds[4].revents != 0
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

// Keep one frame or one clipboard chunk; a slow SSH link never queues more frames.
struct Output {
    bytes: Vec<u8>,
    written: usize,
}

impl Output {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn flush_available(&mut self, fd: RawFd) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        let remaining = &self.bytes[self.written..];
        let count = unsafe { libc::write(fd, remaining.as_ptr().cast(), remaining.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Ok(());
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write TUI frame"));
        }
        self.written += count as usize;
        if self.written == self.bytes.len() {
            self.bytes.clear();
            self.written = 0;
        }
        Ok(())
    }
}

fn draw_text(
    output: &mut Vec<u8>,
    x: u16,
    y: u16,
    columns: u16,
    text: &str,
    foreground: u32,
    background: u32,
) -> io::Result<()> {
    let text = fit_label(text, columns);
    crate::panel::cursor_position(output, y + 1, x + 1)?;
    output.extend_from_slice(b"\x1b[38;2");
    crate::panel::sgr_rgb(output, Rgb::from_packed(foreground));
    output.extend_from_slice(b";48;2");
    crate::panel::sgr_rgb(output, Rgb::from_packed(background));
    output.push(b'm');
    output.extend_from_slice(text.as_bytes());
    let padding = usize::from(columns) - text_width(&text);
    output.resize(output.len() + padding, b' ');
    Ok(())
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
    let mut previous: Option<(usize, usize)> = None;
    for &(age, value) in points {
        let Some(value) = value else {
            previous = None;
            continue;
        };
        let x = ((1. - age / seconds).clamp(0., 1.) * (columns * 2 - 1) as f32).round() as usize;
        let y = ((1. - value / maximum).clamp(0., 1.) * (rows * 4 - 1) as f32).round() as usize;
        if previous == Some((x, y)) {
            continue;
        }
        let (from_x, from_y) = previous.unwrap_or((x, y));
        let count = from_x.abs_diff(x).max(from_y.abs_diff(y));
        if from_x == x {
            for py in from_y.min(y) + 1..from_y.max(y) {
                cells[py / 4 * columns + x / 2] |= bits[py % 4][x % 2];
            }
        } else {
            for step in 1..count {
                let fraction = step as f32 / count as f32;
                let px = (from_x as f32 + (x as f32 - from_x as f32) * fraction).round() as usize;
                let py = (from_y as f32 + (y as f32 - from_y as f32) * fraction).round() as usize;
                cells[py / 4 * columns + px / 2] |= bits[py % 4][px % 2];
            }
        }
        cells[y / 4 * columns + x / 2] |= bits[y % 4][x % 2];
        previous = Some((x, y));
    }
    cells
}

fn terminal_size(fd: RawFd) -> io::Result<WindowSize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
    dirty: bool,
    full: bool,
    damage: Vec<bool>,
    overlays: smallvec::SmallVec<[Rect; 5]>,
    frame_size: WindowSize,
    layout: Vec<(PaneId, Rect)>,
    header: Vec<u8>,
    ui: Ui,
    config: crate::config::Config,

    resource_dirty: bool,

    footer: String,
    panel_utf8: TextDecoder,
    panel_focus: Focus,
    paste: Option<PasteTarget>,
    selecting: Option<PaneId>,
    split_drag: Option<SplitDrag>,
    clipboard: Option<ClipboardTransfer>,
    clipboard_reader: Option<ClipboardReader>,
    copy_pending: bool,
    pointer: Option<(u16, u16)>,

    agent: agent_ui::AgentState,
}

impl Frontend {
    fn new(client: Client, size: WindowSize, config: crate::config::Config) -> io::Result<Self> {
        Ok(Self {
            client,
            size,
            running: true,
            dirty: true,
            full: true,
            damage: vec![false; usize::from(size.num_lines)],
            overlays: smallvec::SmallVec::new(),
            frame_size: size,
            layout: Vec::new(),
            header: Vec::new(),
            ui: Ui::new()?,
            config,

            agent: agent_ui::AgentState {
                dirty: true,
                ..Default::default()
            },

            resource_dirty: true,

            footer: String::new(),
            panel_utf8: TextDecoder::default(),
            panel_focus: Focus::Terminal,
            paste: None,
            selecting: None,
            split_drag: None,
            clipboard: None,
            clipboard_reader: None,
            copy_pending: false,
            pointer: None,
        })
    }

    fn label(&self) -> std::borrow::Cow<'_, str> {
        let status = self
            .ui
            .status
            .as_ref()
            .map_or("", |(text, _)| text.as_str());
        if self.controller() {
            return status.into();
        }
        if status.is_empty() {
            return "Read-only  ".into();
        }
        format!("Read-only  {status}").into()
    }

    fn file_width(&self) -> u16 {
        if !self.ui.files_open {
            return 0;
        }
        let available = self.size.num_cols;

        let available = if self.ui.agent.open {
            available.saturating_sub(self.agent_width())
        } else {
            available
        };
        crate::file_panel::sidebar_width(available)
    }

    fn file_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 1,
            cols: self.file_width(),
            rows: self.view_size().num_lines.saturating_sub(1),
        }
    }

    fn file_preview_rect(&self) -> Rect {
        crate::file_panel::preview_rect(Rect {
            x: 0,
            y: 1,
            cols: self.size.num_cols,
            rows: self.size.num_lines.saturating_sub(2),
        })
    }

    fn view_size(&self) -> WindowSize {
        let size = self.size;

        let size = if self.ui.agent.open {
            WindowSize {
                num_cols: size.num_cols.saturating_sub(self.agent_width()),
                ..size
            }
        } else {
            size
        };

        WindowSize {
            num_cols: size.num_cols.saturating_sub(self.file_width()).max(2),
            num_lines: size
                .num_lines
                .saturating_sub(
                    self.ui
                        .resources
                        .rows(self.size.num_lines.saturating_sub(2))
                        + if self.ui.resources.open { 2 } else { 1 },
                )
                .max(2),
            ..size
        }
    }

    fn resize(&mut self) -> io::Result<()> {
        self.ui.search.close(&mut self.client)?;
        let size = self.view_size();
        let changed = size != self.frame_size;
        self.full |= changed;
        self.dirty = true;
        if self.controller() && changed {
            self.client.send(Action::Resize(size))?;
        }
        Ok(())
    }

    fn copy_text(&mut self, text: String, primary: bool) -> bool {
        if self.clipboard.is_some() {
            self.message("Clipboard transfer already in progress");
            false
        } else {
            self.clipboard = Some(ClipboardTransfer {
                reader: ClipboardSource::Memory(text.into_bytes()),
                target: if primary { b'p' } else { b'c' },
                incomplete: false,
                started: false,
                bytes: 0,
            });
            true
        }
    }

    fn panel_key(&mut self, symbol: u32, modifiers: u32, text: &str) -> io::Result<()> {
        use crate::input::{Key, KeyResult};
        let mut viewport = (
            usize::from(self.size.num_cols),
            usize::from(self.size.num_lines),
        );
        if self.ui.files_open && self.ui.focus == Focus::Files {
            let preview = self.ui.file_preview_open();
            let rect = if preview {
                self.file_preview_rect()
            } else {
                self.file_rect()
            };
            viewport = (
                usize::from(rect.cols),
                usize::from(rect.rows.saturating_sub(if preview { 5 } else { 0 })),
            );
        } else if self.ui.agent.open && self.ui.focus.is_agent() {
            let rect = editor(&self.ui, self.agent_rect()).0;
            viewport = (
                self.ui
                    .agent
                    .active()
                    .editor_columns(usize::from(rect.cols)),
                usize::from(rect.rows),
            );
        }
        let target = self.ui.input_target();
        let panels = self.ui.panel_state();
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
            KeyResult::Copy(text, cut) => {
                let copied = self.copy_text(text, false);
                if copied && cut {
                    self.ui.agent.insert(self.ui.agent.active().id, "").unwrap();
                }
            }
            KeyResult::Paste => self.message("Paste with your host terminal's shortcut"),
            KeyResult::Handled => (),
        }

        self.agent.dirty |= matches!(target, Some(PasteTarget::Agent(_)));
        self.resource_dirty |= matches!(target, Some(PasteTarget::Monitor));
        self.dirty = true;
        if panels != self.ui.panel_state() {
            self.resize()?;
        }
        Ok(())
    }

    fn panel_bytes(&mut self, bytes: &[u8], pasted: Option<PasteTarget>) -> io::Result<()> {
        if pasted.is_none() {
            if let Some((symbol, modifiers)) = crate::input::sequence_key(bytes) {
                self.panel_utf8.clear();
                self.panel_key(symbol, modifiers, "")?;
                return Ok(());
            }
            if bytes.starts_with(b"\x1b[") || bytes.starts_with(b"\x1bO") {
                return Ok(());
            }
            if self.panel_focus != self.ui.focus {
                self.panel_utf8.clear();
                self.panel_focus = self.ui.focus;
            }
        }
        let bytes = if pasted.is_none()
            && bytes.first() == Some(&0x1b)
            && bytes.get(1).is_some_and(|byte| !byte.is_ascii())
        {
            &bytes[1..]
        } else {
            bytes
        };
        let text = self.panel_utf8.decode(bytes);
        if let Some(target) = pasted {
            self.ui.paste(&mut self.client, target, &text)?;
            self.dirty = true;
            self.resource_dirty = true;

            self.agent.dirty = true;
        } else {
            let mut remaining = text.as_ref();
            while !remaining.is_empty() {
                if self.ui.text_target().is_some() {
                    let end = remaining.find(char::is_control).unwrap_or(remaining.len());
                    if end != 0 {
                        self.panel_key(0, 0, &remaining[..end])?;
                        remaining = &remaining[end..];
                        continue;
                    }
                }
                let ch = remaining.chars().next().unwrap();
                let (symbol, modifiers) = crate::input::character_key(ch);
                let key_text = if ch.is_control() {
                    ""
                } else {
                    &remaining[..ch.len_utf8()]
                };
                self.panel_key(symbol, modifiers, key_text)?;
                remaining = &remaining[ch.len_utf8()..];
            }
        }
        Ok(())
    }

    fn draw_resources(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        use crate::monitor::{BACKGROUND, Emphasis, GREEN, MUTED};
        let top = self.view_size().num_lines;
        let last = self.size.num_lines - 1;
        for row in top..self.size.num_lines {
            draw_text(output, 0, row, self.size.num_cols, "", MUTED, BACKGROUND)?;
        }
        if self.ui.resources.open {
            draw_text(output, 0, top, 0, "", crate::panel::BORDER, BACKGROUND)?;
            draw_rule(output, usize::from(self.size.num_cols))?;
            let origin = i32::from(top) + 1;
            let height = last.saturating_sub(top + 1);
            let view = self.ui.resources.view(self.size.num_cols, height);
            let mut cells = Vec::new();
            let mut text = String::new();
            for plot in view.plots {
                let width = usize::from(plot.width);
                let graphs: smallvec::SmallVec<[Vec<u8>; 2]> = plot
                    .series
                    .iter()
                    .map(|series| {
                        braille_trend(
                            &series.points,
                            width,
                            usize::from(plot.height),
                            self.ui.resources.seconds() as f32,
                            plot.maximum,
                        )
                    })
                    .collect();
                for row in 0..usize::from(plot.height) {
                    let y = origin + plot.y + row as i32;
                    if y < origin || y >= i32::from(last) {
                        continue;
                    }
                    cells.clear();
                    cells.resize(width, (0u32, GREEN));
                    for (series, graph) in plot.series.iter().zip(&graphs) {
                        let color = crate::monitor::series_color(series.name);
                        for (column, &bits) in
                            graph[row * width..(row + 1) * width].iter().enumerate()
                        {
                            if bits != 0 {
                                cells[column].0 |= u32::from(bits);
                                cells[column].1 = color;
                            }
                        }
                    }
                    let mut start = 0;
                    for run in cells.chunk_by(|a, b| a.1 == b.1) {
                        text.clear();
                        text.extend(run.iter().map(|(bits, _)| {
                            if *bits == 0 {
                                ' '
                            } else {
                                char::from_u32(0x2800 + bits).unwrap()
                            }
                        }));
                        draw_text(
                            output,
                            plot.x + start as u16,
                            y as u16,
                            run.len() as u16,
                            &text,
                            run[0].1,
                            BACKGROUND,
                        )?;
                        start += run.len();
                    }
                }
            }
            for text in view.texts {
                let y = origin + text.y;
                if y >= origin && y < i32::from(last) && text.x < self.size.num_cols {
                    draw_text(
                        output,
                        text.x,
                        y as u16,
                        text.width.min(self.size.num_cols - text.x),
                        &text.text,
                        text.color,
                        match text.emphasis(self.ui.focus == Focus::Monitor) {
                            Emphasis::Selected => crate::panel::SELECTED,
                            Emphasis::Highlighted => crate::panel::HOVER,
                            Emphasis::None => BACKGROUND,
                        },
                    )?;
                }
            }
        }
        let view = self.ui.resources.footer(
            self.size.num_cols,
            self.controller(),
            self.client.is_remote(),
        );
        let right = view
            .texts
            .iter()
            .map(|text| text.x)
            .min()
            .unwrap_or(self.size.num_cols);
        for text in view.texts {
            let hovered = self
                .pointer
                .is_some_and(|(x, y)| y == last && (text.x..text.x + text.width).contains(&x))
                && view.hits.iter().any(|hit| hit.x == text.x);
            draw_text(
                output,
                text.x,
                last,
                text.width,
                &text.text,
                text.color,
                if hovered || text.emphasis(self.ui.focus == Focus::Monitor) != Emphasis::None {
                    crate::panel::HOVER
                } else {
                    BACKGROUND
                },
            )?;
        }
        if let Some(snapshot) = self.client.snapshot() {
            let context = self.ui.footer(snapshot, self.client.is_remote());
            draw_text(
                output,
                1,
                last,
                right.saturating_sub(2),
                &crate::width::fit_middle(&context, right.saturating_sub(2)),
                MUTED,
                BACKGROUND,
            )?;
        }
        Ok(())
    }

    fn resource_mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        let files_open = self.ui.files_open;
        let top = self.view_size().num_lines;
        let last = self.size.num_lines - 1;
        let pointer = (self.ui.resources.open && mouse.row > top && mouse.row < last)
            .then_some((mouse.column, i32::from(mouse.row) - i32::from(top) - 1));
        let pointer_changed =
            self.ui.resources.pointer != pointer || self.pointer != Some((mouse.column, mouse.row));
        self.pointer = Some((mouse.column, mouse.row));
        self.ui.resources.pointer = pointer;
        if self.ui.resources.dragging {
            if mouse.release {
                self.ui.resources.dragging = false;
            } else if mouse.code & 32 != 0 {
                self.ui.resources.resize(
                    last.saturating_sub(mouse.row + 1),
                    self.size.num_lines.saturating_sub(2),
                );
                return self.resize();
            }
            return Ok(());
        }
        if mouse.code & 32 != 0 {
            self.resource_dirty |= pointer_changed;
            self.dirty |= pointer_changed;
            return Ok(());
        }
        if mouse.code & 64 != 0 && !mouse.release && self.ui.resources.open {
            let scroll = &mut self.ui.resources.scroll;
            *scroll = if mouse.code & 1 == 0 {
                scroll.saturating_sub(3)
            } else {
                scroll.saturating_add(3)
            };
        } else if !mouse.release && mouse.code & 0b1110_0011 == 0 {
            self.ui.files_open = false;
            if mouse.row == last {
                let view = self.ui.resources.footer(
                    self.size.num_cols,
                    self.controller(),
                    self.client.is_remote(),
                );
                if let Some(hit) = view
                    .hits
                    .iter()
                    .find(|hit| (hit.x..hit.x + hit.width).contains(&mouse.column))
                {
                    let open = self.ui.resources.open;
                    self.ui.resources.action(hit.action);
                    self.ui.monitor_jump(&mut self.client)?;
                    self.ui.focus = if self.ui.resources.open {
                        Focus::Monitor
                    } else {
                        Focus::Terminal
                    };
                    if open != self.ui.resources.open {
                        return self.resize();
                    }
                }
            } else if mouse.row == top {
                self.ui.resources.dragging = true;
                self.ui.focus = Focus::Monitor;
            } else {
                self.ui.focus = Focus::Monitor;
                let view = self
                    .ui
                    .resources
                    .view(self.size.num_cols, last.saturating_sub(top + 1));
                if let Some(hit) = view.hits.iter().rfind(|hit| {
                    mouse.row as i32 == i32::from(top) + 1 + hit.y
                        && (hit.x..hit.x.saturating_add(hit.width)).contains(&mouse.column)
                }) {
                    self.ui.resources.action(hit.action);
                    self.ui.monitor_jump(&mut self.client)?;
                    if !self.ui.resources.open {
                        self.ui.focus = Focus::Terminal;
                        return self.resize();
                    }
                }
            }
        }
        if files_open != self.ui.files_open {
            self.resize()?;
        }
        self.resource_dirty = true;
        self.dirty = true;
        Ok(())
    }

    fn draw_active_cursor(
        &self,
        output: &mut Vec<u8>,
        ui_cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if self.ui.close_prompt.is_some() {
            return output.write_all(b"\x1b[?1l\x1b>\x1b[?25l");
        }
        if self.ui.search.open {
            let (column, row) = ui_cursor.unwrap();
            return write!(
                output,
                "\x1b[{};{}H\x1b[?1l\x1b>\x1b[?25h",
                row + 1,
                column + 1
            );
        }

        if self.ui.resources.open && self.ui.focus == Focus::Monitor
            || self.ui.files_open && self.ui.focus == Focus::Files
        {
            return output.write_all(b"\x1b[?1l\x1b>\x1b[?25l");
        }

        if self.ui.agent.open {
            if self.ui.focus.is_agent() {
                output.write_all(b"\x1b[?1l\x1b>")?;
            }
            if self.ui.agent.active().context.preview.is_some()
                || matches!(self.ui.focus, Focus::Output | Focus::Controls)
            {
                return output.write_all(b"\x1b[?25l");
            }
            if self.ui.focus == Focus::Editor {
                let cursor = ui_cursor.or_else(|| {
                    let (rect, layout) = editor(&self.ui, self.agent_rect());
                    (layout.first..layout.first + usize::from(rect.rows))
                        .contains(&layout.caret_row)
                        .then(|| {
                            (
                                rect.x + layout.caret_column as u16,
                                rect.y + (layout.caret_row - layout.first) as u16,
                            )
                        })
                });
                if let Some((column, row)) = cursor {
                    return write!(output, "\x1b[{};{}H\x1b[?25h", row + 1, column + 1);
                }
                return output.write_all(b"\x1b[?25l");
            }
        }
        if let Some(snapshot) = self.client.snapshot() {
            draw_cursor(
                output,
                &self.client,
                snapshot,
                self.view_size(),
                self.file_width(),
            )?;
        }
        Ok(())
    }

    fn focused(&self) -> Option<PaneId> {
        self.client.snapshot().map(|snapshot| snapshot.focused)
    }

    fn controller(&self) -> bool {
        self.client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller)
    }
    fn message(&mut self, message: impl Into<String>) {
        self.ui.message(message);
        self.dirty = true;
    }

    fn send_input(&mut self, pane: PaneId, data: &[u8]) -> io::Result<()> {
        if self.controller() {
            self.client.send_input(pane, data)
        } else {
            self.message("READ ONLY — attach again to take control; Alt+q detach");
            Ok(())
        }
    }

    fn poll(&mut self) -> io::Result<()> {
        let old_hover = self.hovered_block();
        for event in self.client.poll()? {
            self.event(event)?;
        }
        let hovered = self.hovered_block();
        if old_hover != hovered {
            self.damage_block(old_hover);
            self.damage_block(hovered);
        }
        Ok(())
    }

    fn event(&mut self, event: ClientEvent) -> io::Result<()> {
        match event {
            ClientEvent::Updated => {
                if self.ui.search.open {
                    self.ui.search.observe(&mut self.client)?;
                }
                self.dirty = true;
                let Some(snapshot) = self.client.snapshot() else {
                    return Ok(());
                };
                let layout = snapshot.panes.iter().map(|pane| (pane.id, pane.rect));
                if !self.layout.iter().copied().eq(layout.clone()) {
                    self.full = true;
                    self.layout.clear();
                    self.layout.extend(layout);
                }
            }
            ClientEvent::Error(error) | ClientEvent::CopyError { error, .. } => {
                self.copy_pending = false;
                self.message(error);
            }
            ClientEvent::Detached => self.running = false,
            ClientEvent::Clipboard {
                request: _,
                file,
                incomplete,
            } => {
                self.copy_pending = false;
                if self.clipboard_reader.is_none() {
                    self.clipboard_reader = Some(ClipboardReader::new(OSC52_CHUNK_SIZE)?);
                }
                if self.clipboard.is_some() {
                    self.clipboard_reader.as_ref().unwrap().discard(file)?;
                    self.message("Clipboard transfer already in progress");
                } else if std::env::var("TERM").is_ok_and(|term| term == "dumb" || term == "linux")
                {
                    self.clipboard_reader.as_ref().unwrap().discard(file)?;
                    self.message("This host terminal does not support OSC 52 clipboard transfer");
                } else {
                    self.clipboard_reader.as_ref().unwrap().queue(file)?;
                    self.clipboard = Some(ClipboardTransfer {
                        reader: ClipboardSource::File,
                        target: b'c',
                        incomplete,
                        started: false,
                        bytes: 0,
                    });
                }
            }
            ClientEvent::ClipboardStore { primary, text } => {
                self.copy_text(text, primary);
            }
            ClientEvent::Search {
                pane,
                request,
                count,
                truncated,
            } => {
                if self
                    .ui
                    .search
                    .result(&mut self.client, pane, request, count, truncated)?
                {
                    self.dirty = true;
                }
            }
            ClientEvent::List(_)
            | ClientEvent::AgentInput { .. }
            | ClientEvent::History { .. }
            | ClientEvent::AgentFile { .. }
            | ClientEvent::AgentNewTab { .. } => {}
        }
        Ok(())
    }

    fn input(&mut self, event: Input) -> io::Result<()> {
        let close_rect = self.close_rect();
        if self.ui.file_preview_open() {
            match event {
                Input::Escape => {
                    self.panel_key(crate::input::keysym::ESCAPE, 0, "")?;
                }
                Input::Bytes(bytes) => {
                    self.panel_bytes(&bytes, None)?;
                }
                Input::Mouse(mouse) => {
                    self.file_mouse(mouse);
                }
                _ => (),
            }
            return Ok(());
        }
        if let Some(prompt) = self.ui.close_prompt.as_mut() {
            let tab = prompt.tab.unwrap();
            match &event {
                Input::Escape => self.ui.close_prompt = None,
                Input::Bytes(bytes)
                    if matches!(bytes.as_slice(), b"\t" | b"\x1b[D" | b"\x1b[C" | b"\x1b[Z") =>
                {
                    prompt.terminate = !prompt.terminate
                }
                Input::Bytes(bytes) if bytes.as_slice() == b"\r" => {
                    if prompt.terminate {
                        self.client.send(Action::CloseTab(tab))?;
                    }
                    self.ui.close_prompt = None;
                }
                Input::Mouse(mouse)
                    if mouse.code & 3 == 0
                        && !mouse.release
                        && mouse.row == close_rect.y + close_rect.rows.saturating_sub(1) =>
                {
                    let left = close_rect.x + 2;
                    if mouse.column >= left && mouse.column < left.saturating_add(10) {
                        self.ui.close_prompt = None;
                    } else if mouse.column >= left.saturating_add(12)
                        && mouse.column < left.saturating_add(24)
                    {
                        self.client.send(Action::CloseTab(tab))?;
                        self.ui.close_prompt = None;
                    }
                }
                _ => (),
            }
            self.dirty = true;
            return Ok(());
        }

        if let Input::Alt(key) = event {
            if self.ui.agent.active().rewind_target.is_some() {
                self.ui.cancel_rewind();
            }
            if key == b'q' {
                self.running = false;
                return Ok(());
            }
            if matches!(
                key,
                b'a' | b'f'
                    | b'm'
                    | b'v'
                    | b's'
                    | b'h'
                    | b'j'
                    | b'k'
                    | b'l'
                    | b','
                    | b'.'
                    | b'\r'
                    | b'\n'
                    | b'p'
                    | b'n'
                    | b'c'
            ) {
                self.ui.search.close(&mut self.client)?;
            }
            match key {
                b'/' => {
                    if let Some(pane) = self.focused() {
                        self.ui.search.begin(&mut self.client, pane)?;
                    }
                    self.dirty = true;
                    return Ok(());
                }

                b'a' => {
                    self.ui.toggle_agent();
                    return self.resize();
                }

                b'f' => return self.toggle_files(),

                b'm' => {
                    self.ui.toggle_monitor();
                    return self.resize();
                }
                _ => (),
            }
            if self.shortcut(key)? {
                self.ui.focus = Focus::Terminal;
                return Ok(());
            }
        }
        if self.ui.input_target().is_some() {
            match &event {
                Input::Escape => {
                    self.panel_key(crate::input::keysym::ESCAPE, 0, "")?;
                    return Ok(());
                }
                Input::Bytes(bytes) => return self.panel_bytes(bytes, None),
                Input::Alt(key) => {
                    self.panel_key(u32::from(*key), crate::input::ALT, "")?;
                    return Ok(());
                }
                Input::PasteStart => {
                    self.panel_utf8.clear();
                    self.paste = self.ui.paste_target();
                    return Ok(());
                }
                _ => (),
            }
        }
        match event {
            Input::Alt(key) => {
                if let Some(pane) = self.focused() {
                    self.send_input(pane, &[0x1b, key])?;
                }
            }
            Input::Escape => {
                if let Some(pane) = self.focused() {
                    self.send_input(pane, &[0x1b])?;
                }
            }
            Input::Bytes(bytes) => {
                if let Some(pane) = self.focused() {
                    let mode = self
                        .client
                        .screens()
                        .get(&pane)
                        .map_or(Mode::NONE, Screen::mode);
                    self.send_input(pane, &cursor_bytes(bytes, mode))?;
                }
            }
            Input::Focus(focused) => {
                if let Some(pane) = self.focused()
                    && self
                        .client
                        .screens()
                        .get(&pane)
                        .is_some_and(|screen| screen.mode().contains(Mode::FOCUS_IN_OUT))
                {
                    self.send_input(pane, if focused { b"\x1b[I" } else { b"\x1b[O" })?;
                }
            }
            Input::Mouse(mouse) => {
                if self.ui.agent.active().context.preview.is_some()
                    || self.agent.selecting_editor
                    || self.ui.agent.active().selecting()
                {
                    self.agent_mouse(mouse)?;
                    return Ok(());
                }

                if mouse.row < self.view_size().num_lines
                    && (self.ui.resources.pointer.take().is_some()
                        || self
                            .pointer
                            .is_some_and(|(_, row)| row >= self.view_size().num_lines))
                {
                    self.resource_dirty = true;
                    self.dirty = true;
                }

                if self.ui.resources.dragging || mouse.row >= self.view_size().num_lines {
                    return self.resource_mouse(mouse);
                }

                if !mouse.release && mouse.code & 0b1110_0011 == 0 {
                    self.ui.focus = Focus::Terminal;
                }

                if self.agent_mouse(mouse)? {
                    return Ok(());
                }

                if self.ui.files_open && self.file_rect().contains(mouse.column, mouse.row) {
                    if !mouse.release && mouse.code & 32 == 0 {
                        self.ui.focus = Focus::Files;
                        self.file_mouse(mouse);
                    }
                    return Ok(());
                }
                self.mouse(mouse)?;
            }
            Input::PasteStart => {
                if let Some(pane) = self.focused() {
                    let bracketed = self
                        .client
                        .screens()
                        .get(&pane)
                        .is_some_and(|screen| screen.mode().contains(Mode::BRACKETED_PASTE));
                    self.paste = Some(PasteTarget::Pane { pane, bracketed });
                    if bracketed {
                        self.send_input(pane, b"\x1b[200~")?;
                    }
                }
            }
            Input::PasteData(bytes) => match self.paste {
                Some(PasteTarget::Pane { pane, .. }) => self.send_input(pane, &bytes)?,
                Some(target) => self.panel_bytes(&bytes, Some(target))?,
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

    fn shortcut(&mut self, key: u8) -> io::Result<bool> {
        let Some(pane) = self.focused() else {
            return Ok(false);
        };
        let action = match key {
            b'c' => {
                if self.copy_pending || self.clipboard.is_some() {
                    self.message("Clipboard transfer already in progress");
                    return Ok(true);
                }
                let snapshot = self.client.snapshot().unwrap();
                let current = snapshot.pane(pane).unwrap();
                let line = self.client.screens().get(&pane).map_or(0, |screen| {
                    let line = screen.cursor().0.line;
                    let offset = screen.display_offset() as i32;
                    if (0..screen.screen_lines() as i32).contains(&(line + offset)) {
                        line
                    } else {
                        -offset
                    }
                });
                let block = current
                    .blocks
                    .iter()
                    .filter(|block| block.output.is_some() && block.line <= line)
                    .max_by_key(|block| block.line)
                    .or_else(|| current.blocks.iter().rfind(|block| block.output.is_some()));
                let Some(block) = block else {
                    self.message("No command output block available; enable shell integration");
                    return Ok(true);
                };
                Action::CopyBlock {
                    pane,
                    block: block.id,
                    request: 0,
                }
            }
            key => {
                let Some(action) = crate::ui::workspace_action(char::from(key), pane) else {
                    return Ok(false);
                };
                action
            }
        };
        if !self.controller() {
            self.message("READ ONLY — attach again to take control; Alt+q detach");
            return Ok(true);
        }

        let files_open = std::mem::take(&mut self.ui.files_open);
        self.ui.focus = Focus::Terminal;
        if files_open {
            self.resize()?;
        }
        self.client.send(action)?;
        if key == b'c' {
            self.copy_pending = true;
        }
        Ok(true)
    }

    fn mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        let previous = self.pointer;
        if previous != Some((mouse.column, mouse.row)) {
            let old_object = self.hovered_object();
            let old_hover = self.hovered_block();
            self.pointer = Some((mouse.column, mouse.row));
            if mouse.row == 0 || previous.is_some_and(|(_, row)| row == 0) {
                self.dirty = true;
            }
            let hovered = self.hovered_block();
            if old_hover != hovered {
                self.damage_block(old_hover);
                self.damage_block(hovered);
            }
            let object = self.hovered_object();
            if old_object != object {
                for (pane, object) in old_object.iter().chain(object.iter()) {
                    let screen = &self.client.screens()[pane];
                    let view = self.client.snapshot().unwrap().pane(*pane).unwrap();
                    for line in object.range.start.line..=object.range.end.line {
                        let row = line + screen.display_offset() as i32 + i32::from(view.rect.y);
                        if let Some(damage) = self.damage.get_mut(row as usize) {
                            *damage = true;
                        }
                    }
                }
                self.dirty = true;
            }
        }
        let left = if mouse.row == 0 { 0 } else { self.file_width() };
        let mut mouse = mouse;
        mouse.column = mouse.column.saturating_sub(left);
        let previous = previous.map(|(column, row)| (column.saturating_sub(left), row));
        if let Some(drag) = self.split_drag {
            if mouse.release {
                self.split_drag = None;
            } else if mouse.code & 32 != 0
                && previous.is_none_or(|(column, row)| match drag.axis {
                    Axis::LeftRight => column != mouse.column,
                    Axis::TopBottom => row != mouse.row,
                })
            {
                self.client.send(drag.action(mouse.column, mouse.row))?;
            }
            return Ok(());
        }
        if self.controller()
            && !mouse.release
            && mouse.code & 0b1110_0011 == 0
            && let Some(drag) = self
                .client
                .snapshot()
                .and_then(|s| s.divider_at(mouse.column, mouse.row))
        {
            self.split_drag = Some(drag);
            return Ok(());
        }
        if mouse.row == 0 && !mouse.release && mouse.code & 0b1100_0011 == 0 {
            if self.controller()
                && let Some(snapshot) = self.client.snapshot()
            {
                let status = self.label();
                let (tabs, label, before, after) =
                    tab_chrome(snapshot, self.view_size().num_cols, &status);
                let mut plus = 3;
                for (index, x, width) in tabs {
                    plus = x + width + 1;
                    let tab = &snapshot.tabs[index];
                    if (x..x + width).contains(&mouse.column) {
                        let action = if (x + width - 4..x + width - 1).contains(&mouse.column) {
                            if !self.ui.request_close_tab(tab) {
                                self.dirty = true;
                                return Ok(());
                            }
                            Action::CloseTab(tab.id)
                        } else {
                            let active = snapshot
                                .tabs
                                .iter()
                                .position(|tab| tab.id == snapshot.active_tab)
                                .unwrap();
                            Action::Tab(index as i32 - active as i32)
                        };
                        return self.client.send(action);
                    }
                }
                let max = self.view_size().num_cols.saturating_sub(plus + 4);
                let fitted = fit_label(&label, max);
                let mut column = self
                    .view_size()
                    .num_cols
                    .saturating_sub(text_width(&fitted) as u16 + 2);
                for (count, direction) in [(before, -1), (after, 1)] {
                    if count > 0 {
                        let width = count.ilog10() as u16 + 2;
                        if (column..column + width).contains(&mouse.column) {
                            return self.client.send(Action::Tab(direction));
                        }
                        column += width + 1;
                    }
                }
                if (plus.saturating_sub(1)..plus.saturating_add(2)).contains(&mouse.column)
                    && plus.saturating_add(1) < self.size.num_cols
                {
                    return self.client.send(Action::NewTab {
                        pane: snapshot.focused,
                    });
                }
            }
            return Ok(());
        }
        let Some(view) = self
            .client
            .snapshot()
            .and_then(|snapshot| snapshot.pane_at(mouse.column, mouse.row))
        else {
            return Ok(());
        };
        let pane = view.id;
        let physical_x = mouse.column - view.rect.x;
        let y = mouse.row - view.rect.y;
        let Some(screen) = self.client.screens().get(&pane) else {
            return Ok(());
        };
        let mode = screen.mode();
        let offset = screen.display_offset();
        let x = usize::from(physical_x).min(screen.columns().saturating_sub(1)) as u16;
        if !self.controller() {
            return Ok(());
        }
        if !mouse.release && mouse.code & 0b1100_0011 == 0 && self.focused() != Some(pane) {
            self.client.send(Action::Focus(pane))?;
        }

        if !mouse.release
            && mouse.code & 0b1110_0011 == 0
            && (!mode.intersects(Mode::MOUSE_MODE) || mouse.code & 8 != 0)
        {
            let point = Point {
                line: i32::from(y) - offset as i32,
                column: usize::from(x),
            };
            let now = Instant::now();
            let double = self
                .agent
                .last_click
                .is_some_and(|(at, old_pane, old_point)| {
                    old_pane == pane
                        && old_point == point
                        && now.duration_since(at) < Duration::from_millis(400)
                });
            self.agent.last_click = Some((now, pane, point));
            if double
                && let Some(object) = self
                    .client
                    .screens()
                    .get(&pane)
                    .and_then(|screen| crate::context::object_at(screen, point))
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
        }

        if mode.intersects(Mode::MOUSE_MODE) && mouse.code & 8 == 0 {
            if mouse.code & 32 != 0
                && !mode.contains(Mode::MOUSE_MOTION)
                && (!mode.contains(Mode::MOUSE_DRAG) || mouse.code & 3 == 3)
            {
                return Ok(());
            }
            if let Some(data) = mouse_bytes(
                mouse.code,
                usize::from(x),
                usize::from(y),
                mouse.release,
                mode,
            ) {
                self.send_input(pane, &data)?;
            }
        } else if mouse.code & 64 != 0 && !mouse.release {
            let up = mouse.code & 1 == 0;
            if mode.contains(Mode::ALT_SCREEN | Mode::ALTERNATE_SCROLL) && mouse.code & 8 == 0 {
                let bytes = if up { *b"\x1b[A" } else { *b"\x1b[B" };
                self.send_input(pane, &cursor_bytes(bytes, mode))?;
            } else {
                self.client.send(Action::Scroll {
                    pane,
                    scroll: Scroll::Delta(if up { 3 } else { -3 }),
                })?;
            }
        } else if mouse.code & 3 == 2 && mouse.code & 32 == 0 && !mouse.release {
            if let Some((_, object)) = self.hovered_object() {
                self.copy_text(object.text, false);
            } else {
                self.message("Paste with your host terminal's shortcut");
            }
        } else if mouse.code & 3 == 0 {
            let point = Point {
                line: i32::from(y) - offset as i32,
                column: usize::from(x),
            };
            if mouse.release {
                self.selecting = None;
            } else if mouse.code & 32 != 0 {
                if self.selecting == Some(pane) {
                    self.client.send(Action::SelectionUpdate {
                        pane,
                        point,
                        side: Side::Right,
                    })?;
                }
            } else {
                self.selecting = Some(pane);
                self.client.send(Action::SelectionStart {
                    pane,
                    point,
                    side: Side::Left,
                    kind: SelectionType::Simple,
                })?;
            }
        }
        Ok(())
    }

    fn damage_block(&mut self, target: Option<(PaneId, u64)>) {
        let Some((id, block)) = target else {
            return;
        };
        let Some((snapshot, screens)) = self.client.snapshot_and_screens_mut() else {
            return;
        };
        let Some(pane) = snapshot.pane(id) else {
            return;
        };
        let Some(screen) = screens.get_mut(&id) else {
            return;
        };
        let range = screen.block_row_range(pane, block);
        for row in range.start..range.end.min(usize::from(pane.rect.rows)) {
            if screen
                .row_output(pane, row)
                .is_some_and(|current| current.id == block)
            {
                screen.dirty[row] = true;
            }
        }
        self.dirty = true;
    }

    fn close_rect(&self) -> Rect {
        let cols = self.size.num_cols.saturating_sub(4).min(64);
        let rows = 7.min(self.size.num_lines.saturating_sub(2));
        Rect {
            x: (self.size.num_cols - cols) / 2,
            y: (self.size.num_lines - rows) / 2,
            cols,
            rows,
        }
    }

    fn damage_overlays(&mut self) {
        self.damage.resize(usize::from(self.size.num_lines), false);
        let mut overlays = smallvec::SmallVec::new();

        if self.ui.file_preview_open() {
            overlays.push(self.file_preview_rect());
        }
        if self.ui.agent.active().context.preview.is_some() {
            overlays.push(crate::file_panel::preview_rect(Rect {
                x: 0,
                y: 1,
                cols: self.size.num_cols,
                rows: self.size.num_lines.saturating_sub(2),
            }));
        }
        if self.ui.search.open {
            overlays.push(Rect {
                x: 0,
                y: 0,
                cols: self.size.num_cols,
                rows: 1,
            });
        }
        if self.ui.close_prompt.is_some() {
            overlays.push(self.close_rect());
        }
        if self.overlays != overlays {
            for rect in self.overlays.iter().chain(&overlays) {
                for row in rect.y..rect.y.saturating_add(rect.rows).min(self.size.num_lines) {
                    self.damage[usize::from(row)] = true;
                }
            }
            self.overlays = overlays;
        }
    }

    fn hovered_object(&self) -> Option<(PaneId, crate::context::Object)> {
        let (x, y) = self.pointer?;
        let x = x.checked_sub(self.file_width())?;
        let pane = self.client.snapshot()?.pane_at(x, y)?;
        let screen = self.client.screens().get(&pane.id)?;
        let point = Point {
            line: i32::from(y - pane.rect.y) - screen.display_offset() as i32,
            column: usize::from(x - pane.rect.x),
        };
        crate::context::object_at(screen, point).map(|object| (pane.id, object))
    }

    fn hovered_block(&self) -> Option<(PaneId, u64)> {
        let (x, y) = self.pointer?;
        let x = x.checked_sub(self.file_width())?;
        let pane = self.client.snapshot()?.pane_at(x, y)?;
        let screen = self.client.screens().get(&pane.id)?;
        screen
            .row_output(pane, usize::from(y - pane.rect.y))
            .map(|block| (pane.id, block.id))
    }

    fn draw(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        self.damage_overlays();
        let Some(snapshot) = self.client.snapshot() else {
            write!(output, "\x1b[H\x1b[0mWaiting for workspace — Alt+q detach")?;
            return Ok(());
        };
        output.write_all(b"\x1b[?2026h")?;
        if self.full {
            output.write_all(b"\x1b[2J")?;
        }
        let size = self.view_size();
        let label = self.label();
        let start = output.len();
        draw_tabs(
            output,
            snapshot,
            size.num_cols + self.file_width(),
            &label,
            self.pointer,
            &self.config,
        )?;
        if self.full || self.damage[0] || self.header != output[start..] {
            self.header.clear();
            self.header.extend_from_slice(&output[start..]);
        } else {
            output.truncate(start);
        }
        let hovered = self.hovered_block();
        let object = self.hovered_object();
        for pane in &snapshot.panes {
            if let Some(screen) = self.client.screens().get(&pane.id) {
                let rows = screen
                    .screen_lines()
                    .min(usize::from(pane.rect.rows))
                    .min(usize::from(size.num_lines.saturating_sub(pane.rect.y)));
                for row in 0..rows {
                    if !self.full
                        && !screen.dirty[row]
                        && !self.damage[usize::from(pane.rect.y) + row]
                    {
                        continue;
                    }
                    let highlight = hovered.is_some_and(|(id, block)| {
                        id == pane.id
                            && screen
                                .row_output(pane, row)
                                .is_some_and(|current| current.id == block)
                    });
                    draw_row(
                        output,
                        screen,
                        pane,
                        row,
                        size.num_cols + self.file_width(),
                        (
                            highlight,
                            object
                                .as_ref()
                                .filter(|(id, _)| *id == pane.id)
                                .map(|(_, object)| object.range),
                        ),
                        self.file_width(),
                    )?;
                }
            }
        }
        draw_separators(
            output,
            snapshot,
            size,
            self.full,
            &self.damage,
            self.file_width(),
        )?;

        let footer_changed = {
            let footer = self.ui.footer(snapshot, self.client.is_remote());
            let changed = self.footer != footer;
            if changed {
                self.footer = footer.into_owned();
            }
            changed
        };
        if self.full
            || self.resource_dirty
            || footer_changed
            || self.damage[usize::from(size.num_lines)..]
                .iter()
                .any(|dirty| *dirty)
        {
            self.draw_resources(output)?;
            self.resource_dirty = false;
        }

        if self.ui.files_open && !self.ui.file_preview_open() {
            self.draw_files(output)?;
        }

        let agent_cursor = if self.ui.agent.open
            && (self.full
                || self.agent.dirty
                || self.ui.agent.active().context.preview.is_some()
                || self.damage.iter().any(|dirty| *dirty))
        {
            let cursor = self.draw_agent(output)?;
            self.agent.dirty = false;
            cursor
        } else {
            None
        };

        if self.ui.file_preview_open() {
            self.draw_files(output)?;
        }
        let ui_cursor = if self.ui.search.open {
            let (label, caret) = self
                .ui
                .search
                .display("", usize::from(self.size.num_cols.saturating_sub(2)));
            draw_text(
                output,
                0,
                0,
                self.size.num_cols,
                "",
                self.config.foreground,
                crate::panel::CHROME,
            )?;
            draw_text(
                output,
                1,
                0,
                self.size.num_cols.saturating_sub(2),
                &label,
                self.config.foreground,
                crate::panel::CHROME,
            )?;
            Some((caret as u16 + 1, 0))
        } else {
            agent_cursor
        };
        if let Some(prompt) = self.ui.close_prompt {
            let rect = self.close_rect();
            let background = mix_color(self.config.foreground, self.config.background, 9);
            let muted = mix_color(self.config.foreground, self.config.background, 176);
            let mut grid = crate::panel::Grid::new(rect, self.config.foreground, background);
            let cols = rect.cols.saturating_sub(4);
            grid.text(
                rect.x + 2,
                rect.y,
                cols,
                "Close tab?",
                self.config.foreground,
                background,
            );
            grid.text(
                rect.x + 2,
                rect.y + 2,
                cols,
                "Programs may still be running in this tab.",
                muted,
                background,
            );
            grid.text(
                rect.x + 2,
                rect.y + 3,
                cols,
                "Closing stops all panes in this tab.",
                muted,
                background,
            );
            if rect.rows >= 7 {
                grid.text(
                    rect.x + 2,
                    rect.y + 5,
                    cols,
                    "Tab select   Enter confirm   Esc cancel",
                    muted,
                    background,
                );
            }
            let row = rect.y + rect.rows.saturating_sub(1);
            grid.text(
                rect.x + 2,
                row,
                cols,
                "  Cancel      Close tab  ",
                muted,
                background,
            );
            let (x, width, label, color) = if prompt.terminate {
                (
                    rect.x + 14,
                    12.min(cols.saturating_sub(12)),
                    " Close tab ",
                    0xe87575,
                )
            } else {
                (rect.x + 2, 10.min(cols), " Cancel ", crate::panel::ACCENT)
            };
            grid.text(
                x,
                row,
                width,
                label,
                color,
                mix_color(self.config.foreground, self.config.background, 24),
            );
            grid.ansi(output, None, &[])?;
        }
        self.draw_active_cursor(output, ui_cursor)?;
        let (snapshot, screens) = self.client.snapshot_and_screens_mut().unwrap();
        for pane in &snapshot.panes {
            if let Some(screen) = screens.get_mut(&pane.id) {
                screen.reset_damage();
            }
        }
        self.full = false;
        self.damage.fill(false);
        self.frame_size = size;
        output.write_all(b"\x1b[?2026l")?;
        Ok(())
    }

    fn toggle_files(&mut self) -> io::Result<()> {
        if self.client.is_remote() {
            self.message("Files are unavailable in remote sessions");
            return Ok(());
        }
        if self.ui.files_open {
            self.ui.files_open = false;
            self.ui.focus = Focus::Terminal;
            return self.resize();
        }
        let directory = self
            .client
            .snapshot()
            .and_then(Snapshot::focused_pane)
            .and_then(|pane| pane.cwd.clone());
        let Some(directory) = directory else {
            self.message("Active pane working directory is unavailable");
            return Ok(());
        };
        self.ui.load_files(directory)?;
        self.panel_utf8.clear();
        self.ui.files_open = true;
        self.ui.focus = Focus::Files;
        self.resize()
    }

    fn file_mouse(&mut self, mouse: Mouse) {
        let preview = self.ui.file_preview_open();
        let rect = if preview {
            self.file_preview_rect()
        } else {
            self.file_rect()
        };
        let files = self.ui.files.as_mut().unwrap();
        if preview
            && (!rect.contains(mouse.column, mouse.row)
                || crate::panel::popup_close(rect).contains(mouse.column, mouse.row))
        {
            if !mouse.release && mouse.code & 0b1100_0011 == 0 {
                files.focus_list();
                self.dirty = true;
            }
            return;
        }
        if !mouse.release && mouse.code & 64 != 0 {
            files.scroll(
                if mouse.code & 1 == 0 { -3 } else { 3 },
                usize::from(if preview {
                    crate::panel::popup_body(rect).rows
                } else {
                    rect.rows.saturating_sub(2)
                }),
            );
        } else if !preview
            && !mouse.release
            && mouse.code & 0b1100_0011 == 0
            && let Some(index) =
                crate::file_panel::Layout::new(rect, files).entry_at(files, mouse.column, mouse.row)
        {
            files.move_selection(index as isize - files.selection() as isize);
            files.focus_preview();
        }
        self.dirty = true;
    }

    fn draw_files(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let rect = self.file_rect();
        let preview = self.file_preview_rect();
        let Some(files) = &mut self.ui.files else {
            return Ok(());
        };
        let focused =
            self.ui.focus == Focus::Files && files.focus() == crate::files::FileFocus::List;
        crate::file_panel::list(files, rect, "", focused, &self.config).ansi(output, None, &[])?;
        if files.focus() == crate::files::FileFocus::Preview {
            crate::file_panel::preview(files, preview, &self.config).ansi(output, None, &[])?;
        }
        Ok(())
    }
}

fn draw_tabs(
    output: &mut Vec<u8>,
    snapshot: &Snapshot,
    columns: u16,
    label: &str,
    pointer: Option<(u16, u16)>,
    config: &Config,
) -> io::Result<()> {
    let style = config.tab_style;
    output.write_all(b"\x1b[H\x1b[0;48;2;25;25;25m\x1b[2K")?;
    let marker = if snapshot.controller {
        "150;150;150"
    } else {
        "254;155;48"
    };
    write!(output, "\x1b[1;2H\x1b[38;2;{marker}m›")?;
    let (tabs, label, _, _) = tab_chrome(snapshot, columns, label);
    let mut end = 3;
    for (index, x, width) in tabs {
        let tab = &snapshot.tabs[index];
        let active = tab.id == snapshot.active_tab;
        let bg = if active { 32 } else { 28 };
        let fg = if active { 204 } else { 150 };
        let color = crate::ui::tab_color(tab);
        let close = if active { "148;148;148" } else { "96;96;96" };
        let hovered =
            pointer.is_some_and(|(column, row)| row == 0 && (x..x + width).contains(&column));
        let (left, right) = if style == crate::config::TabStyle::Angled {
            ("◢", "◣")
        } else {
            (" ", " ")
        };
        let flat_active = style == crate::config::TabStyle::Flat && active;
        let selected_style = if flat_active {
            "4;58;2;181;210;162;"
        } else {
            ""
        };
        write!(
            output,
            "\x1b[1;{}H\x1b[0;{selected_style}38;2;{bg};{bg};{bg};48;2;25;25;25m{left}\x1b[38;2;{};{};{};48;2;{bg};{bg};{bg}m • \x1b[38;2;{fg};{fg};{fg}m",
            x + 1,
            color >> 16 & 255,
            color >> 8 & 255,
            color & 255
        )?;
        if hovered {
            output.write_all(b"\x1b[38;2;212;217;216m")?;
        }
        fit_text(output, &tab.title, width - 8);
        let close_hovered = pointer.is_some_and(|(column, row)| {
            row == 0 && (x + width - 4..x + width - 1).contains(&column)
        });
        let close = if close_hovered { "232;117;117" } else { close };
        let underline = if close_hovered {
            "4;58;2;232;117;117"
        } else if flat_active {
            "4;58;2;181;210;162"
        } else {
            "24;59"
        };
        let edge_style = if flat_active {
            "4;58;2;181;210;162"
        } else {
            "24;59"
        };
        write!(
            output,
            "\x1b[38;2;{close};48;2;{bg};{bg};{bg}m \x1b[{underline}m×\x1b[{edge_style}m \x1b[38;2;{bg};{bg};{bg};48;2;25;25;25m{right}\x1b[24;59m"
        )?;
        end = x + width + 1;
    }
    if end.saturating_add(1) < columns {
        let hovered = pointer.is_some_and(|(column, row)| {
            row == 0 && (end.saturating_sub(1)..end + 2).contains(&column)
        });
        let x = end.saturating_sub(1);
        let fg = if hovered { 0xb5d2a2 } else { 0x606060 };
        output.write_all(b"\x1b[24;59m")?;
        draw_text(output, x, 0, 3, "", fg, 0x191919)?;
        if hovered {
            write!(
                output,
                "\x1b[4;58;2;{};{};{}m",
                fg >> 16 & 255,
                fg >> 8 & 255,
                fg & 255
            )?;
        }
        write!(output, "\x1b[1;{}H+\x1b[24;59m", x + 2)?;
    }
    let max = columns.saturating_sub(end + 4);
    if max > 0 {
        let label = fit_label(&label, max);
        let width = text_width(&label) as u16;
        let x = columns - width - 2;
        let color = if !snapshot.controller {
            "254;155;48"
        } else {
            "184;184;184"
        };
        write!(
            output,
            "\x1b[1;{}H\x1b[0;38;2;{color};48;2;25;25;25m{label}",
            x + 1
        )?;
    }
    output.write_all(b"\x1b[0m")
}

fn cursor_bytes<T: AsMut<[u8]>>(mut bytes: T, mode: Mode) -> T {
    if let [
        0x1b,
        prefix @ (b'[' | b'O'),
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F',
    ] = bytes.as_mut()
    {
        *prefix = if mode.contains(Mode::APP_CURSOR) {
            b'O'
        } else {
            b'['
        };
    }
    bytes
}

fn draw_row(
    output: &mut Vec<u8>,
    screen: &Screen,
    pane: &PaneView,
    row: usize,
    host_columns: u16,
    highlight: (bool, Option<crate::core::SelectionRange>),
    left: u16,
) -> io::Result<()> {
    let (highlight, object) = highlight;
    let mut rect = pane.rect;
    rect.x += left;
    if rect.x >= host_columns {
        return Ok(());
    }
    let physical_columns = usize::from(rect.cols.min(host_columns - rect.x));
    let columns = screen.columns().min(physical_columns);
    let data = screen.row(row);
    let line = row as i32 - screen.display_offset() as i32;
    let selection = screen
        .selection_range()
        .filter(|selection| (selection.start.line..=selection.end.line).contains(&line));
    let columns = if highlight || selection.is_some_and(|selection| line < selection.end.line) {
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
    crate::panel::cursor_position(output, rect.y + row as u16 + 1, rect.x + 1)?;
    if columns < physical_columns {
        let bg = screen.color(Color::Named(NamedColor::Background));
        output.extend_from_slice(b"\x1b[0;48;2");
        crate::panel::sgr_rgb(output, bg);
        write!(output, "m\x1b[{}X", physical_columns)?;
    }
    let mut previous = None;
    for (column, cell, extra) in data.cells_with_extras(0..columns) {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let selected = object.is_some_and(|range| range.contains(Point { line, column }))
            || !cell.flags.contains(Flags::PROMPT)
                && selection.is_some_and(|selection| selection.contains(Point { line, column }));
        let style = (cell.fg, cell.bg, cell.flags & Flags::TEXT_STYLE, selected);
        if previous != Some(style) {
            let mut fg = screen.color(cell.fg);
            let mut bg = screen.color(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selected {
                bg = Rgb {
                    r: 55,
                    g: 55,
                    b: 55,
                };
            } else if highlight
                && cell.bg == Color::Named(NamedColor::Background)
                && !cell.flags.contains(Flags::INVERSE)
            {
                bg = Rgb {
                    r: 38,
                    g: 38,
                    b: 38,
                };
            }
            output.extend_from_slice(b"\x1b[0;38;2");
            crate::panel::sgr_rgb(output, fg);
            output.extend_from_slice(b";48;2");
            crate::panel::sgr_rgb(output, bg);
            for (flag, sgr) in [
                (Flags::BOLD, b'1'),
                (Flags::DIM, b'2'),
                (Flags::ITALIC, b'3'),
                (Flags::STRIKEOUT, b'9'),
            ] {
                if cell.flags.contains(flag) {
                    output.extend_from_slice(&[b';', sgr]);
                }
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                output.extend_from_slice(b";4");
            }
            output.push(b'm');
            previous = Some(style);
        }
        if cell.flags.contains(Flags::HIDDEN)
            || cell.c.is_control()
            || cell.flags.contains(Flags::WIDE_CHAR) && column + 1 >= columns
        {
            output.resize(output.len() + cell.width().min(columns - column), b' ');
        } else {
            if cell.c.is_ascii() {
                output.push(cell.c as u8);
            } else {
                output.extend_from_slice(cell.c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            if let Some(extra) = extra {
                for ch in &extra.zerowidth {
                    if !ch.is_control() {
                        output.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                }
            }
        }
    }
    output.extend_from_slice(b"\x1b[0m");
    Ok(())
}

fn draw_rule(output: &mut impl Write, columns: usize) -> io::Result<()> {
    const RULE: &[u8] = "────────────────────────────────".as_bytes();
    let mut bytes = columns * "─".len();
    while bytes != 0 {
        let count = bytes.min(RULE.len());
        output.write_all(&RULE[..count])?;
        bytes -= count;
    }
    Ok(())
}

fn draw_separators(
    output: &mut impl Write,
    snapshot: &Snapshot,
    size: WindowSize,
    full: bool,
    damage: &[bool],
    left: u16,
) -> io::Result<()> {
    output.write_all(b"\x1b[0;38;2;48;48;48m")?;
    for pane in &snapshot.panes {
        if pane.rect.x > 0 && pane.rect.x <= size.num_cols {
            for row in pane.rect.y
                ..pane
                    .rect
                    .y
                    .saturating_add(pane.rect.rows)
                    .min(size.num_lines)
            {
                if full || damage[usize::from(row)] {
                    crate::panel::cursor_position(output, row + 1, pane.rect.x + left)?;
                    output.write_all("│".as_bytes())?;
                }
            }
        }
        if pane.rect.y > 1
            && pane.rect.y <= size.num_lines
            && (full || damage[usize::from(pane.rect.y - 1)])
        {
            crate::panel::cursor_position(output, pane.rect.y, pane.rect.x + left + 1)?;
            draw_rule(
                output,
                usize::from(
                    pane.rect
                        .cols
                        .min(size.num_cols.saturating_sub(pane.rect.x)),
                ),
            )?;
        }
    }
    Ok(())
}

fn draw_cursor(
    output: &mut impl Write,
    client: &Client,
    snapshot: &Snapshot,
    size: WindowSize,
    left: u16,
) -> io::Result<()> {
    let Some(pane) = snapshot.focused_pane() else {
        return output.write_all(b"\x1b[?25l");
    };
    let Some(screen) = client.screens().get(&pane.id) else {
        return output.write_all(b"\x1b[?25l");
    };
    let mode = screen.mode();
    output.write_all(if mode.contains(Mode::APP_CURSOR) {
        b"\x1b[?1h"
    } else {
        b"\x1b[?1l"
    })?;
    output.write_all(if mode.contains(Mode::APP_KEYPAD) {
        b"\x1b="
    } else {
        b"\x1b>"
    })?;
    let (point, shape) = screen.cursor();
    let row = point.line + screen.display_offset() as i32;
    let x = u32::from(pane.rect.x) + point.column as u32;
    let y = i32::from(pane.rect.y) + row;
    if !snapshot.controller
        || shape == CursorShape::Hidden
        || row < 0
        || row >= i32::from(pane.rect.rows)
        || x >= u32::from(size.num_cols)
        || y >= i32::from(size.num_lines)
    {
        output.write_all(b"\x1b[?25l")
    } else {
        write!(
            output,
            "\x1b[0m\x1b[{} q\x1b[{};{}H\x1b[?25h",
            match shape {
                CursorShape::Underline => 4,
                CursorShape::Beam => 6,
                _ => 2,
            },
            y + 1,
            x + u32::from(left) + 1
        )
    }
}

fn fit_text(output: &mut Vec<u8>, text: &str, columns: u16) {
    let end = text.len().min(usize::from(columns));
    let prefix = &text.as_bytes()[..end];
    // A non-ASCII scalar at the boundary may still use zero columns.
    if text.as_bytes().get(end).is_none_or(u8::is_ascii)
        && prefix.iter().all(|byte| (b' '..=b'~').contains(byte))
    {
        output.extend_from_slice(prefix);
        output.resize(output.len() + usize::from(columns) - end, b' ');
        return;
    }
    let mut used = 0;
    let mut start = 0;
    let mut end = 0;
    for ch in text.chars() {
        let control = ch.is_control();
        let width = if control {
            1
        } else {
            u16::from(scalar_width(ch).min(2))
        };
        if used + width > columns {
            break;
        }
        if control {
            output.extend_from_slice(&text.as_bytes()[start..end]);
            output.push(b' ');
            start = end + ch.len_utf8();
        }
        end += ch.len_utf8();
        used += width;
    }
    output.extend_from_slice(&text.as_bytes()[start..end]);
    output.resize(output.len() + usize::from(columns - used), b' ');
}

// A multiple of three keeps intermediate base64 chunks unpadded.
const OSC52_CHUNK_SIZE: usize = 48 * 1024;

enum ClipboardSource {
    File,
    Memory(Vec<u8>),
}

struct ClipboardTransfer {
    reader: ClipboardSource,
    target: u8,
    incomplete: bool,
    started: bool,
    bytes: u64,
}

impl ClipboardTransfer {
    fn step(
        &mut self,
        output: &mut Vec<u8>,
        reader: Option<&mut ClipboardReader>,
    ) -> io::Result<Option<String>> {
        if !self.started {
            output.extend_from_slice(b"\x1b]52;");
            output.extend_from_slice(&[self.target, b';']);
            self.started = true;
        }
        let file_chunk;
        let chunk = match &self.reader {
            ClipboardSource::File => {
                file_chunk = match reader.unwrap().read() {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => return Ok(None),
                    Err(error) => {
                        output.extend_from_slice(b"\x1b\\");
                        return Err(error);
                    }
                };
                file_chunk.as_slice()
            }
            ClipboardSource::Memory(bytes) if bytes.is_empty() => &[],
            ClipboardSource::Memory(bytes) => {
                let remaining = &bytes[self.bytes as usize..];
                &remaining[..remaining.len().min(OSC52_CHUNK_SIZE)]
            }
        };
        if chunk.is_empty() {
            output.extend_from_slice(b"\x1b\\");
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
        crate::base64::encode(chunk, output);
        if let ClipboardSource::Memory(bytes) = &mut self.reader
            && self.bytes == bytes.len() as u64
        {
            // The final encoded chunk may remain blocked on stdout.
            *bytes = Vec::new();
        }
        Ok(None)
    }
}

mod agent_ui;
#[cfg(test)]
mod tests;
