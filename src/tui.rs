//! ANSI frontend for an existing workspace; the daemon owns PTYs and VT state.

#[path = "tui_input.rs"]
mod input;

use crate::clipboard_reader::ClipboardReader;
use crate::core::{Flags, Mode, Point, Scroll, SelectionType, Side, WindowSize};
use crate::input::mouse_bytes;
use crate::screen::Screen;
#[cfg(feature = "agent")]
use crate::ui::{AgentAction, AgentLine, column_offset, wrap_context};
use crate::ui::{Focus, Ui};
use crate::vt::{Color, CursorShape, NamedColor, Rgb};
use crate::width::{fit_label, scalar_width, text_width};
use crate::workspace::{
    Action, Axis, Client, ClientEvent, Direction, PaneId, PaneView, Rect, Snapshot, SplitDrag,
    tab_chrome,
};
use input::{Decoder, Input, Mouse};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[2J";
const LEAVE: &[u8] = b"\x18\x1b\\\x1b[0m\x1b[?1006l\x1b[?1003l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[?7h\x1b[?25h\x1b[?1049l";

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
            #[cfg(feature = "agent")]
            {
                let (changed, _) = frontend
                    .ui
                    .poll_agent(&mut frontend.client, &frontend.config)?;
                let layout_changed = frontend.ui.agent.poll_layout()?;
                if (changed && frontend.ui.agent.open) || layout_changed {
                    frontend.agent_dirty = true;
                    frontend.dirty = true;
                }
            }
            #[cfg(feature = "monitor")]
            if let Some(snapshot) = frontend.client.snapshot() {
                let directory = if frontend.client.is_remote() {
                    Some(std::path::Path::new("/"))
                } else {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.id == snapshot.focused)
                        .and_then(|pane| pane.cwd.as_deref())
                };
                let changed = frontend
                    .ui
                    .resources
                    .stats
                    .refresh(directory, frontend.ui.resources.open)?;
                frontend.resource_dirty |= changed;
                frontend.dirty |= changed;
            }
            frontend.dirty |= frontend.ui.expire_status();
            #[cfg(feature = "files")]
            if frontend.ui.files_open
                && let Some(files) = &mut frontend.ui.files
            {
                if fds
                    .iter()
                    .any(|source| source.fd == files.fd() && source.revents != 0)
                {
                    files.drain_notification()?;
                }
                frontend.dirty |= files.poll();
            }
            if frontend.running && output.is_empty() {
                if let Some(transfer) = &mut frontend.clipboard {
                    if let Some(message) =
                        transfer.step(&mut output.bytes, frontend.clipboard_reader.as_mut())?
                    {
                        frontend.clipboard = None;
                        frontend.message(message);
                        frontend.full = true;
                    }
                } else if frontend.dirty {
                    frontend.draw(&mut output.bytes)?;
                    frontend.dirty = false;
                }
            }
            if output_ready && let Err(error) = output.flush_available(libc::STDOUT_FILENO) {
                let mut state = libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: 0,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut state, 1, 0) } >= 0
                    && state.revents & libc::POLLHUP != 0
                {
                    break;
                }
                return Err(error);
            }
            output_ready = output.is_empty();
            if !frontend.running {
                break;
            }
            let now = Instant::now();
            let mut timeout = decoder.timeout(now);
            if let Some((_, until)) = &frontend.ui.status {
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
            let socket_events = if frontend.client.wants_read() {
                libc::POLLIN
            } else {
                0
            } | if frontend.client.wants_write() {
                libc::POLLOUT
            } else {
                0
            };
            fds.clear();
            fds.extend([
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if socket_events == 0 {
                        -1
                    } else {
                        frontend.client.fd()
                    },
                    events: socket_events,
                    revents: 0,
                },
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
                #[cfg(feature = "monitor")]
                libc::pollfd {
                    fd: frontend.ui.resources.stats.fd(),
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
                #[cfg(feature = "files")]
                libc::pollfd {
                    fd: frontend
                        .ui
                        .files
                        .as_ref()
                        .filter(|_| frontend.ui.files_open)
                        .map_or(-1, |files| files.fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
                #[cfg(feature = "agent")]
                libc::pollfd {
                    fd: frontend.ui.agent.layout_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ]);
            let milliseconds = timeout.map_or(-1, |timeout| {
                timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
            });
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, milliseconds) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
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
    #[cfg(feature = "agent")]
    if frontend.client.snapshot().is_some() {
        frontend
            .ui
            .poll_agent(&mut frontend.client, &frontend.config)?;
    }
    // Closing the connection also detaches if the last nonblocking send cannot flush.
    let _ = frontend.client.send(Action::Detach);
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
    write!(
        output,
        "\x1b[{};{}H\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{}",
        y + 1,
        x + 1,
        foreground >> 16 & 255,
        foreground >> 8 & 255,
        foreground & 255,
        background >> 16 & 255,
        background >> 8 & 255,
        background & 255,
        text
    )?;
    let padding = usize::from(columns) - text_width(&text);
    output.resize(output.len() + padding, b' ');
    Ok(())
}

#[cfg(feature = "monitor")]
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
        let (from_x, from_y) = previous.unwrap_or((x, y));
        let count = from_x.abs_diff(x).max(from_y.abs_diff(y)).max(1);
        for step in 0..=count {
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

enum PasteTarget {
    Search,
    #[cfg(feature = "agent")]
    Agent(u64),
    Pane {
        pane: PaneId,
        bracketed: bool,
    },
    #[cfg(feature = "files")]
    Files,
    #[cfg(feature = "monitor")]
    Monitor,
}

struct Frontend {
    client: Client,
    size: WindowSize,
    running: bool,
    dirty: bool,
    full: bool,
    layout: Vec<(PaneId, Rect)>,
    header: Vec<u8>,
    ui: Ui,
    #[cfg(feature = "agent")]
    config: crate::config::Config,
    #[cfg(feature = "agent")]
    agent_hits: Vec<(Rect, AgentAction)>,
    #[cfg(feature = "agent")]
    agent_chat: Rect,
    #[cfg(feature = "agent")]
    agent_first: usize,
    #[cfg(feature = "agent")]
    agent_dirty: bool,
    #[cfg(feature = "monitor")]
    resource_dirty: bool,
    #[cfg(feature = "agent")]
    selecting_editor: bool,
    #[cfg(feature = "agent")]
    agent_utf8: Vec<u8>,
    #[cfg(feature = "agent")]
    last_click: Option<(Instant, PaneId, Point)>,
    paste: Option<PasteTarget>,
    selecting: Option<PaneId>,
    split_drag: Option<SplitDrag>,
    clipboard: Option<ClipboardTransfer>,
    clipboard_reader: Option<ClipboardReader>,
    copy_pending: bool,
    request: u64,
    pointer: Option<(u16, u16)>,
    #[cfg(feature = "files")]
    file_utf8: Vec<u8>,
}

impl Frontend {
    fn new(client: Client, size: WindowSize, config: crate::config::Config) -> io::Result<Self> {
        #[cfg(not(feature = "agent"))]
        let _ = config;
        Ok(Self {
            client,
            size,
            running: true,
            dirty: true,
            full: true,
            layout: Vec::new(),
            header: Vec::new(),
            ui: Ui::new()?,
            #[cfg(feature = "agent")]
            config,
            #[cfg(feature = "agent")]
            agent_hits: Vec::new(),
            #[cfg(feature = "agent")]
            agent_chat: Rect::default(),
            #[cfg(feature = "agent")]
            agent_first: 0,
            #[cfg(feature = "agent")]
            agent_dirty: true,
            #[cfg(feature = "monitor")]
            resource_dirty: true,
            #[cfg(feature = "agent")]
            selecting_editor: false,
            #[cfg(feature = "agent")]
            agent_utf8: Vec::new(),
            #[cfg(feature = "agent")]
            last_click: None,
            paste: None,
            selecting: None,
            split_drag: None,
            clipboard: None,
            clipboard_reader: None,
            copy_pending: false,
            request: 0,
            pointer: None,
            #[cfg(feature = "files")]
            file_utf8: Vec::new(),
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

    fn view_size(&self) -> WindowSize {
        let size = self.size;
        #[cfg(feature = "agent")]
        let size = if self.ui.agent.open {
            WindowSize {
                num_cols: size.num_cols.saturating_sub(self.agent_width()),
                ..size
            }
        } else {
            size
        };
        #[cfg(feature = "monitor")]
        let size = WindowSize {
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
        };
        size
    }

    fn resize(&mut self) -> io::Result<()> {
        if self.ui.search.open {
            self.ui.search.close(&mut self.client)?;
        }
        self.full = true;
        self.dirty = true;
        if self.controller() {
            self.client.send(Action::Resize(self.view_size()))?;
        }
        Ok(())
    }

    #[cfg(feature = "agent")]
    fn copy_text(&mut self, text: String) -> bool {
        if self.clipboard.is_some() {
            self.message("Clipboard transfer already in progress");
            false
        } else {
            self.clipboard = Some(ClipboardTransfer {
                reader: ClipboardSource::Memory(text.into_bytes()),
                target: b'c',
                incomplete: false,
                started: false,
                bytes: 0,
            });
            true
        }
    }

    #[cfg(feature = "agent")]
    fn agent_width(&self) -> u16 {
        (self.size.num_cols * 2 / 5)
            .clamp(20, 42)
            .min(self.size.num_cols.saturating_sub(2))
    }

    #[cfg(feature = "agent")]
    fn agent_rect(&self) -> Rect {
        let width = self.agent_width();
        Rect {
            x: self.size.num_cols - width,
            y: 1,
            cols: width,
            rows: self.view_size().num_lines.saturating_sub(1),
        }
    }

    #[cfg(feature = "agent")]
    fn editor_rect(&self) -> Rect {
        let side = self.agent_rect();
        Rect {
            x: side.x + 1,
            y: (side.y + side.rows).saturating_sub(3),
            cols: side.cols.saturating_sub(2),
            rows: 3,
        }
    }

    #[cfg(feature = "agent")]
    fn editor_layout(&self) -> crate::ui::EditorLines {
        let columns = usize::from(
            self.editor_rect()
                .cols
                .saturating_sub(u16::from(self.ui.agent.active().model.busy) * 3)
                .max(1),
        );
        self.ui.agent.active().editor_layout(columns, 3)
    }

    #[cfg(feature = "agent")]
    fn editor_pointer(&mut self, mouse: Mouse, extend: bool) {
        let rect = self.editor_rect();
        let layout = self.editor_layout();
        let (lines, mut first) = (layout.lines, layout.first);
        if extend {
            if mouse.row < rect.y {
                first = first.saturating_sub(1);
            } else if mouse.row >= rect.y + rect.rows {
                first = (first + 1).min(lines.len().saturating_sub(3));
            }
        }
        let row = usize::from(mouse.row.saturating_sub(rect.y).min(2));
        let range = lines.get(first + row).cloned().unwrap_or(
            self.ui.agent.active().model.input.len()..self.ui.agent.active().model.input.len(),
        );
        let column = mouse.column.saturating_sub(rect.x).min(rect.cols) as f32;
        let offset = column_offset(&self.ui.agent.active().model.input, range, column);
        self.ui.agent.active_mut().editor_scroll = Some(first);
        self.ui.agent.active_mut().model.set_cursor(offset, extend);
        self.dirty = true;
        self.agent_dirty = true;
    }

    #[cfg(feature = "agent")]
    fn editor_vertical(&mut self, up: bool, shift: bool) {
        let layout = self.editor_layout();
        self.ui.agent.active_mut().move_vertical(layout, up, shift);
    }

    #[cfg(feature = "agent")]
    fn agent_bytes(&mut self, bytes: &[u8], paste: Option<u64>) -> io::Result<()> {
        if paste.is_none() && self.ui.focus == Focus::Output {
            if bytes.contains(&3)
                && let Some(text) = self.ui.agent.active().selected_text()
            {
                self.copy_text(text);
                self.dirty = true;
                return Ok(());
            }
            match bytes {
                _ if bytes.contains(&3) && self.ui.agent.active_mut().model.busy => {
                    self.ui.agent.active_mut().model.abort()?
                }
                b"\x1b[A" | b"\x1bOA" | b"\x1b[5~" => {
                    self.ui.agent.active_mut().model.scroll = self
                        .ui
                        .agent
                        .active()
                        .model
                        .scroll
                        .saturating_add(if bytes.ends_with(b"~") { 10 } else { 1 })
                }
                b"\x1b[B" | b"\x1bOB" | b"\x1b[6~" => {
                    self.ui.agent.active_mut().model.scroll = self
                        .ui
                        .agent
                        .active()
                        .model
                        .scroll
                        .saturating_sub(if bytes.ends_with(b"~") { 10 } else { 1 })
                }
                _ => (),
            }
            self.dirty = true;
            self.agent_dirty = true;
            return Ok(());
        }
        if paste.is_none() {
            let key = match bytes {
                b"\x1b[1;2A" => Some((0xff52, true)),
                b"\x1b[1;2B" => Some((0xff54, true)),
                b"\x1b[1;2C" => Some((0xff53, true)),
                b"\x1b[1;2D" => Some((0xff51, true)),
                b"\x1b[1;2H" => Some((0xff50, true)),
                b"\x1b[1;2F" => Some((0xff57, true)),
                b"\x1b[13;2u" | b"\x1b[27;2;13~" => {
                    self.insert_agent("\n");
                    self.dirty = true;
                    self.agent_dirty = true;
                    return Ok(());
                }
                _ => crate::input::editing_key(bytes).map(|symbol| (symbol, false)),
            };
            if let Some((symbol, shift)) = key {
                if matches!(symbol, 0xff52 | 0xff54) {
                    self.editor_vertical(symbol == 0xff52, shift);
                } else {
                    self.ui.agent.active_mut().model.key(symbol, shift);
                }
                self.ui.agent.active_mut().editor_scroll = None;
                self.dirty = true;
                self.agent_dirty = true;
                return Ok(());
            }
            if bytes.starts_with(b"\x1b[") || bytes.starts_with(b"\x1bO") {
                return Ok(());
            }
        }
        self.agent_utf8.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.agent_utf8) {
            Ok(_) => self.agent_utf8.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        let text = String::from_utf8(self.agent_utf8.drain(..valid).collect()).unwrap();
        if let Some(id) = paste {
            if let Err(error) = self.ui.agent.paste(id, &text) {
                self.message(error);
            }
        } else {
            let mut printable = String::new();
            for ch in text.chars() {
                if !ch.is_control() {
                    printable.push(ch);
                    continue;
                }
                if !printable.is_empty() {
                    self.insert_agent(&printable);
                    printable.clear();
                }
                match ch {
                    '\u{1}' => self.ui.agent.active_mut().model.select_all(),
                    '\u{3}' => {
                        if let Some(range) = self.ui.agent.active().model.selection() {
                            self.copy_text(self.ui.agent.active().model.input[range].to_owned());
                        } else {
                            self.ui.agent.active_mut().model.clear_input();
                        }
                    }
                    '\u{18}' => {
                        if let Some(range) = self.ui.agent.active().model.selection()
                            && self.copy_text(self.ui.agent.active().model.input[range].to_owned())
                        {
                            self.ui.agent.active_mut().model.insert("").unwrap();
                        }
                    }
                    '\u{8}' | '\u{7f}' => {
                        self.ui.agent.active_mut().model.key(0xff08, false);
                    }
                    '\r' => {
                        if let Err(error) = self.ui.agent.active_mut().send(&self.client) {
                            self.message(format!("Agent: {error}"));
                        }
                    }
                    '\n' => self.insert_agent("\n"),
                    '\u{15}' => self.ui.agent.active_mut().model.clear_input(),
                    _ => (),
                }
            }
            if !printable.is_empty() {
                self.insert_agent(&printable);
            }
        }
        if paste.is_none() {
            self.ui.agent.active_mut().editor_scroll = None;
        }
        self.dirty = true;
        self.agent_dirty = true;
        Ok(())
    }

    #[cfg(feature = "agent")]
    fn insert_agent(&mut self, text: &str) {
        self.agent_dirty = true;
        if let Err(error) = self.ui.agent.active_mut().model.insert(text) {
            self.message(error);
        }
    }

    #[cfg(feature = "agent")]
    fn agent_mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        let pointer = Some((mouse.column, mouse.row));
        if self.pointer != pointer {
            self.pointer = pointer;
            self.agent_dirty = true;
            self.dirty = true;
        }
        if self.ui.agent.active().selecting() {
            self.chat_pointer(mouse);
            if mouse.release {
                self.ui.agent.active_mut().end_selection();
            }
            return Ok(());
        }
        if self.selecting_editor {
            if mouse.release {
                self.selecting_editor = false;
            } else {
                self.editor_pointer(mouse, true);
            }
            return Ok(());
        }
        if mouse.release {
            return Ok(());
        }
        if mouse.code & 64 != 0 {
            let up = mouse.code & 1 == 0;
            if let Some((_, scroll)) = &mut self.ui.agent.active_mut().context.preview {
                *scroll = if up {
                    scroll.saturating_sub(3)
                } else {
                    scroll.saturating_add(3)
                };
            } else if self.editor_rect().contains(mouse.column, mouse.row) {
                let layout = self.editor_layout();
                let (lines, first) = (layout.lines, layout.first);
                self.ui.agent.active_mut().editor_scroll = Some(if up {
                    first.saturating_sub(3)
                } else {
                    first.saturating_add(3).min(lines.len().saturating_sub(3))
                });
            } else {
                self.ui.agent.active_mut().model.scroll = if up {
                    self.ui.agent.active().model.scroll.saturating_add(3)
                } else {
                    self.ui.agent.active().model.scroll.saturating_sub(3)
                };
            }
            self.dirty = true;
            self.agent_dirty = true;
            return Ok(());
        }
        if mouse.code & 32 != 0 {
            return Ok(());
        }
        let hit = self
            .agent_hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(mouse.column, mouse.row))
            .map(|(_, action)| *action);
        let right = mouse.code & 3 == 2;
        if right
            && self.agent_chat.contains(mouse.column, mouse.row)
            && self.ui.agent.active().context.preview.is_none()
            && let Some(text) = self.ui.agent.active().selected_text()
        {
            self.copy_text(text);
            return Ok(());
        }
        if !right && let Some(AgentAction::Copy(_) | AgentAction::Tool(_)) = hit {
            let marker =
                matches!(hit, Some(AgentAction::Tool(_))) && mouse.column < self.agent_chat.x + 2;
            if !marker && let Some(point) = self.chat_point(mouse) {
                self.ui.agent_rewind_key(0);
                self.ui
                    .agent
                    .active_mut()
                    .begin_selection(point, mouse.code & 4 != 0);
                self.ui.focus = Focus::Output;
                self.dirty = true;
                self.agent_dirty = true;
                return Ok(());
            }
        }
        if let Some(action) = hit {
            if right
                && !matches!(
                    action,
                    AgentAction::Editor | AgentAction::Copy(_) | AgentAction::Tool(_)
                )
            {
                return Ok(());
            }
            match action {
                AgentAction::Editor if right => {
                    self.ui.agent_rewind_key(0);
                    self.ui.agent.active_mut().context.menu = !self.ui.agent.active().context.menu;
                    self.ui.focus = Focus::Editor;
                }
                AgentAction::Editor => {
                    self.ui.agent_rewind_key(0);
                    self.ui.focus = Focus::Editor;
                    self.ui.agent.active_mut().context.menu = false;
                    self.editor_pointer(mouse, mouse.code & 4 != 0);
                    self.selecting_editor = true;
                }
                AgentAction::Stop => self.ui.agent.active_mut().model.abort()?,
                AgentAction::Output => {
                    self.ui.agent_rewind_key(0);
                    self.ui.agent.active_mut().clear_selection();
                    self.ui.focus = Focus::Output;
                    self.ui.agent.active_mut().context.menu = false;
                }
                AgentAction::Tool(index) if !right => {
                    self.ui.focus = Focus::Output;
                    self.ui.agent.active_mut().toggle_tool(index);
                }
                AgentAction::Tool(index) | AgentAction::Copy(index) if right => {
                    self.ui.focus = Focus::Output;
                    if let Some(message) = self.ui.agent.active().model.messages.get(index) {
                        self.copy_text(message.text.clone());
                    }
                }
                AgentAction::Copy(_) => self.ui.focus = Focus::Output,
                AgentAction::Selection | AgentAction::Screen => {
                    if let Some(pane) = self.focused()
                        && let Err(error) = self.ui.agent.active_mut().attach_screen(
                            &self.client,
                            pane,
                            matches!(action, AgentAction::Selection),
                        )
                    {
                        self.message(error);
                    }
                    self.ui.agent.active_mut().context.menu = false;
                }
                AgentAction::Inspect(index) => {
                    if let Some(item) = self.ui.agent.active().context.basket.get(index) {
                        self.ui.agent.active_mut().context.preview = Some((item.clone(), 0));
                    }
                }
                AgentAction::Remove(index) => {
                    self.ui.agent.active_mut().context.basket.remove(index);
                }
                AgentAction::ClosePreview => {
                    self.ui.agent.active_mut().context.preview = None;
                    self.full = true;
                }
                AgentAction::CopyPreview => {
                    if let Some((item, _)) = &self.ui.agent.active().context.preview {
                        self.copy_text(item.snapshot.text.clone());
                    }
                }
                AgentAction::NewSession
                | AgentAction::Session(_)
                | AgentAction::CloseSession(_)
                | AgentAction::ClosePanel
                | AgentAction::Rewind(_) => {
                    self.activate_agent(action)?;
                }
                _ => (),
            }
        }
        self.dirty = true;
        self.agent_dirty = true;
        Ok(())
    }

    #[cfg(feature = "agent")]
    fn chat_point(&self, mouse: Mouse) -> Option<crate::ui::TranscriptPoint> {
        let session = self.ui.agent.active();
        let row = mouse
            .row
            .saturating_sub(self.agent_chat.y)
            .min(self.agent_chat.rows.saturating_sub(1));
        let line = self.agent_first + usize::from(row);
        if !matches!(
            session.lines.get(line),
            Some(AgentLine::Role(_) | AgentLine::Text { .. })
        ) {
            return (0..line.min(session.lines.len()))
                .rev()
                .find_map(|line| session.point(line, usize::MAX));
        }
        let indent = match session.lines.get(line)? {
            AgentLine::Role(_) => 3,
            AgentLine::Text { message, .. } => {
                let message = session.model.messages.get(*message as usize)?;
                if message.role == "You" {
                    1
                } else if message.id.is_some() {
                    2
                } else {
                    0
                }
            }
            AgentLine::Blank => return None,
        };
        session.point(
            line,
            usize::from(mouse.column.saturating_sub(self.agent_chat.x + indent)),
        )
    }

    #[cfg(feature = "agent")]
    fn chat_pointer(&mut self, mouse: Mouse) {
        if mouse.row < self.agent_chat.y {
            self.ui.agent.active_mut().model.scroll =
                self.ui.agent.active().model.scroll.saturating_add(1);
        } else if mouse.row >= self.agent_chat.y + self.agent_chat.rows {
            self.ui.agent.active_mut().model.scroll =
                self.ui.agent.active().model.scroll.saturating_sub(1);
        }
        if let Some(point) = self.chat_point(mouse) {
            self.ui.agent.active_mut().extend_selection(point);
        }
        self.agent_dirty = true;
        self.dirty = true;
    }

    #[cfg(feature = "agent")]
    fn activate_agent(&mut self, action: AgentAction) -> io::Result<()> {
        match self.ui.activate_agent(action, &self.client) {
            Ok(Some(text)) => {
                self.copy_text(text);
            }
            Ok(None) => (),
            Err(error) => self.message(error.to_string()),
        }
        if matches!(
            action,
            AgentAction::ClosePreview
                | AgentAction::ClosePanel
                | AgentAction::NewSession
                | AgentAction::Session(_)
                | AgentAction::CloseSession(_)
        ) {
            self.full = true;
            self.selecting_editor = false;
            if !matches!(self.paste, Some(PasteTarget::Agent(_))) {
                self.agent_utf8.clear();
            }
        }
        self.ui.agent.active_mut().columns = 0;
        self.agent_dirty = true;
        self.dirty = true;
        if action == AgentAction::ClosePanel {
            self.resize()?;
        }
        Ok(())
    }
    #[cfg(feature = "agent")]
    fn draw_agent(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let side = self.agent_rect();
        let editor = self.editor_rect();
        let bg = self.config.background;
        let brighter = |rgb: u32| {
            ((rgb >> 16 & 255).saturating_add(16).min(255) << 16)
                | ((rgb >> 8 & 255).saturating_add(16).min(255) << 8)
                | (rgb & 255).saturating_add(16).min(255)
        };
        let user_bg = brighter(bg);
        self.agent_hits.clear();
        for row in side.y..side.y + side.rows {
            draw_text(
                output,
                side.x,
                row,
                side.cols,
                "",
                self.config.foreground,
                bg,
            )?;
            draw_text(output, side.x, row, 1, "│", 0x343b3d, bg)?;
        }
        self.agent_hits.push((side, AgentAction::Output));
        self.draw_agent_tabs(output, side, editor, bg)?;
        let attachment_rows = self.ui.agent.active().context.basket.len() as u16;
        let rewind = self.ui.agent.active().rewind_target.is_some();
        let chat_end = editor.y.saturating_sub(
            attachment_rows
                + u16::from(attachment_rows > 0)
                + u16::from(rewind && attachment_rows == 0),
        );
        let chat_top = side.y + 1;
        let rows = usize::from(chat_end.saturating_sub(chat_top));
        let columns = usize::from(editor.cols);
        self.ui.agent.prepare_layout(columns)?;
        self.ui.agent.active_mut().model.scroll = self
            .ui
            .agent
            .active()
            .model
            .scroll
            .min(self.ui.agent.active().lines.len().saturating_sub(rows));
        let first = self
            .ui
            .agent
            .active()
            .lines
            .len()
            .saturating_sub(rows + self.ui.agent.active().model.scroll);
        self.agent_chat = Rect {
            x: editor.x,
            y: chat_top,
            cols: editor.cols,
            rows: rows as u16,
        };
        self.agent_first = first;
        for (row, line) in self
            .ui
            .agent
            .active()
            .lines
            .iter()
            .skip(first)
            .take(rows)
            .enumerate()
        {
            let y = chat_top + row as u16;
            match line {
                AgentLine::Blank => (),
                AgentLine::Role(index) => {
                    let message = &self.ui.agent.active().model.messages[*index as usize];
                    let marker = if message
                        .id
                        .as_ref()
                        .is_some_and(|id| self.ui.agent.active().expanded_tools.contains(id))
                    {
                        "▾"
                    } else {
                        "›"
                    };
                    draw_text(
                        output,
                        editor.x,
                        y,
                        editor.cols,
                        &format!(" {marker} {} ", message.role),
                        0x889391,
                        bg,
                    )?;
                    if let Some(selected) = self.ui.agent.active().selection_columns(first + row) {
                        let from = column_offset(
                            &message.role,
                            0..message.role.len(),
                            selected.start as f32,
                        );
                        let to = column_offset(
                            &message.role,
                            0..message.role.len(),
                            selected.end as f32,
                        );
                        draw_text(
                            output,
                            editor.x + 3 + selected.start as u16,
                            y,
                            (selected.end - selected.start) as u16,
                            &message.role[from..to],
                            self.config.foreground,
                            0x52634c,
                        )?;
                    }
                    self.agent_hits.push((
                        Rect {
                            x: editor.x,
                            y,
                            cols: editor.cols.min(text_width(&message.role) as u16 + 4),
                            rows: 1,
                        },
                        AgentAction::Tool(*index as usize),
                    ));
                }
                AgentLine::Text {
                    message: index,
                    range,
                } => {
                    let message = &self.ui.agent.active().model.messages[*index as usize];
                    let user = message.role == "You";
                    let message_bg =
                        if user && self.ui.agent.active().rewind_target == Some(message.turn) {
                            0x394535
                        } else if user {
                            user_bg
                        } else {
                            bg
                        };
                    let indent = if user {
                        1
                    } else if message.id.is_some() {
                        2
                    } else {
                        0
                    };
                    draw_text(
                        output,
                        editor.x,
                        y,
                        editor.cols,
                        "",
                        self.config.foreground,
                        message_bg,
                    )?;
                    let text = self.ui.agent.active().message_text(*index as usize);
                    let from = range.start as usize;
                    let to = range.end as usize;
                    if let Some(document) = self.ui.agent.active().markdown(*index as usize) {
                        for (span, style) in document.spans(from..to) {
                            if style & crate::markdown::BOLD != 0 {
                                output.write_all(b"\x1b[1m")?;
                            }
                            if style & crate::markdown::ITALIC != 0 {
                                output.write_all(b"\x1b[3m")?;
                            }
                            draw_text(
                                output,
                                editor.x + indent + text_width(&text[from..span.start]) as u16,
                                y,
                                text_width(&text[span.clone()]) as u16,
                                &text[span],
                                crate::markdown::color(style, self.config.foreground),
                                if style & crate::markdown::CODE != 0 {
                                    0x2b302d
                                } else {
                                    message_bg
                                },
                            )?;
                            output.write_all(b"\x1b[22;23m")?;
                        }
                    } else {
                        draw_text(
                            output,
                            editor.x + indent,
                            y,
                            editor.cols.saturating_sub(indent),
                            &text[from..to],
                            if message.role == "Error" {
                                0xd49790
                            } else {
                                self.config.foreground
                            },
                            message_bg,
                        )?;
                    }
                    if let Some(selected) = self.ui.agent.active().selection_columns(first + row) {
                        let text = &text[from..to];
                        let from = column_offset(text, 0..text.len(), selected.start as f32);
                        let to = column_offset(text, 0..text.len(), selected.end as f32);
                        draw_text(
                            output,
                            editor.x + indent + selected.start as u16,
                            y,
                            (selected.end - selected.start) as u16,
                            &text[from..to],
                            self.config.foreground,
                            0x52634c,
                        )?;
                    }
                    self.agent_hits.push((
                        Rect {
                            x: editor.x,
                            y,
                            cols: editor.cols,
                            rows: 1,
                        },
                        AgentAction::Copy(*index as usize),
                    ));
                    if user
                        && columns >= crate::width::text_width(crate::ui::REWIND_LABEL) + 5
                        && (row + 1 == rows
                            || !matches!(
                                self.ui.agent.active().lines.get(first + row + 1),
                                Some(AgentLine::Text { message: next, .. }) if next == index
                            ))
                    {
                        let width = crate::width::text_width(crate::ui::REWIND_LABEL) as u16;
                        let x = editor.x + editor.cols.saturating_sub(width);
                        let hovered = self.pointer.is_some_and(|(column, row)| {
                            row == y && column >= x && column < x + width
                        });
                        draw_text(
                            output,
                            x,
                            y,
                            width,
                            crate::ui::REWIND_LABEL,
                            if hovered { 0xb1c99c } else { 0x889391 },
                            message_bg,
                        )?;
                        self.agent_hits.push((
                            Rect {
                                x,
                                y,
                                cols: width,
                                rows: 1,
                            },
                            AgentAction::Rewind(*index as usize),
                        ));
                    }
                }
            }
        }
        if rewind {
            draw_text(
                output,
                editor.x,
                editor.y.saturating_sub(1),
                editor.cols,
                "Esc older · Enter confirm · type cancels",
                0xb1c99c,
                bg,
            )?;
        }
        for (index, item) in self.ui.agent.active().context.basket.iter().enumerate() {
            let y = chat_end + index as u16;
            draw_text(
                output,
                editor.x,
                y,
                editor.cols.saturating_sub(3),
                &format!(" +{} ", item.label),
                0xb5d2a2,
                bg,
            )?;
            draw_text(
                output,
                editor.x + editor.cols - 3,
                y,
                3,
                " × ",
                0x889391,
                bg,
            )?;
            self.agent_hits.push((
                Rect {
                    x: editor.x,
                    y,
                    cols: editor
                        .cols
                        .saturating_sub(3)
                        .min(text_width(&item.label) as u16 + 3),
                    rows: 1,
                },
                AgentAction::Inspect(index),
            ));
            self.agent_hits.push((
                Rect {
                    x: editor.x + editor.cols - 3,
                    y,
                    cols: 3,
                    rows: 1,
                },
                AgentAction::Remove(index),
            ));
        }
        let layout = self.editor_layout();
        let (lines, first) = (layout.lines, layout.first);
        for row in 0..editor.rows {
            draw_text(
                output,
                editor.x,
                editor.y + row,
                editor.cols,
                "",
                self.config.foreground,
                user_bg,
            )?;
            if let Some(range) = lines.get(first + usize::from(row)) {
                let text = &self.ui.agent.active().model.input[range.clone()];
                draw_text(
                    output,
                    editor.x,
                    editor.y + row,
                    editor.cols,
                    text,
                    self.config.foreground,
                    user_bg,
                )?;
                if let Some(selected) = self.ui.agent.active().model.selection() {
                    let start = range.start.max(selected.start);
                    let end = range.end.min(selected.end);
                    if start < end {
                        let column =
                            text_width(&self.ui.agent.active().model.input[range.start..start])
                                as u16;
                        draw_text(
                            output,
                            editor.x + column,
                            editor.y + row,
                            text_width(&self.ui.agent.active().model.input[start..end]) as u16,
                            &self.ui.agent.active().model.input[start..end],
                            self.config.foreground,
                            0x52634c,
                        )?;
                    }
                }
            }
        }
        if self.ui.agent.active().model.input.is_empty() {
            draw_text(
                output,
                editor.x,
                editor.y,
                editor.cols,
                "Message…",
                0x889391,
                user_bg,
            )?;
        }
        self.agent_hits.push((editor, AgentAction::Editor));
        if self.ui.agent.active().model.busy {
            draw_text(
                output,
                editor.x + editor.cols - 3,
                editor.y,
                3,
                " ■ ",
                0xe87575,
                user_bg,
            )?;
            self.agent_hits.push((
                Rect {
                    x: editor.x + editor.cols - 3,
                    y: editor.y,
                    cols: 3,
                    rows: 1,
                },
                AgentAction::Stop,
            ));
        }
        if self.ui.agent.active().context.menu {
            for (row, (label, action)) in [
                ("Selection", AgentAction::Selection),
                ("Visible screen", AgentAction::Screen),
            ]
            .into_iter()
            .enumerate()
            {
                let y = editor.y.saturating_sub(3) + row as u16;
                draw_text(
                    output,
                    editor.x,
                    y,
                    editor.cols,
                    &format!(" {label} "),
                    0xb5d2a2,
                    0x2b3028,
                )?;
                self.agent_hits.push((
                    Rect {
                        x: editor.x,
                        y,
                        cols: editor.cols.min(text_width(label) as u16 + 2),
                        rows: 1,
                    },
                    action,
                ));
            }
        }
        if let Some((item, scroll)) = &mut self.ui.agent.active_mut().context.preview {
            let columns = self.size.num_cols.saturating_sub(4);
            let rows = usize::from(self.size.num_lines.saturating_sub(5));
            let lines = wrap_context(&item.snapshot.text, usize::from(columns));
            *scroll = (*scroll).min(lines.len().saturating_sub(rows));
            for y in 1..self.size.num_lines.saturating_sub(1) {
                draw_text(
                    output,
                    0,
                    y,
                    self.size.num_cols,
                    "",
                    self.config.foreground,
                    bg,
                )?;
            }
            draw_text(
                output,
                2,
                1,
                columns,
                &format!("[ {} ]", item.label),
                0xb5d2a2,
                bg,
            )?;
            draw_text(output, self.size.num_cols - 4, 1, 3, " × ", 0x889391, bg)?;
            for (row, range) in lines.iter().skip(*scroll).take(rows).enumerate() {
                draw_text(
                    output,
                    2,
                    row as u16 + 3,
                    columns,
                    &item.snapshot.text[range.clone()],
                    self.config.foreground,
                    bg,
                )?;
            }
            draw_text(
                output,
                2,
                self.size.num_lines - 2,
                columns,
                " ⧉   Esc back",
                0xb5d2a2,
                bg,
            )?;
            self.agent_hits.clear();
            self.agent_hits.push((
                Rect {
                    x: self.size.num_cols - 4,
                    y: 1,
                    cols: 3,
                    rows: 1,
                },
                AgentAction::ClosePreview,
            ));
            self.agent_hits.push((
                Rect {
                    x: 2,
                    y: self.size.num_lines - 2,
                    cols: text_width(" ⧉ ") as u16,
                    rows: 1,
                },
                AgentAction::CopyPreview,
            ));
            output.write_all(b"\x1b[?25l")?;
        }
        let hovered = self.pointer.and_then(|(x, y)| {
            self.agent_hits
                .iter()
                .rev()
                .find(|(rect, _)| rect.contains(x, y))
                .map(|(_, action)| *action)
        });
        for (rect, action) in &self.agent_hits {
            if hovered != Some(*action)
                && !(self.ui.focus == Focus::Controls && self.ui.agent.keyboard == *action)
            {
                continue;
            }
            let session = self.ui.agent.active();
            let mut foreground = 0xb5d2a2;
            if matches!(action, AgentAction::Session(_)) {
                continue;
            }
            if matches!(
                action,
                AgentAction::NewSession | AgentAction::CloseSession(_) | AgentAction::ClosePanel
            ) {
                let active = matches!(action, AgentAction::CloseSession(index) if *index == self.ui.agent.current);
                let numerator = match action {
                    AgentAction::CloseSession(_) if active => 10,
                    AgentAction::CloseSession(_) => 9,
                    _ => 8,
                };
                let mut tab_bg = 0;
                for shift in [16, 8, 0] {
                    tab_bg |= ((bg >> shift & 255) * numerator / 10) << shift;
                }
                let (icon, color) = if *action == AgentAction::NewSession {
                    ("+", 0xb5d2a2)
                } else {
                    ("×", 0xe87575)
                };
                draw_tab_icon(output, *rect, icon, (color, color), tab_bg, true, active)?;
                continue;
            }
            let background = if *action == AgentAction::Stop {
                0x633534
            } else {
                0x343c34
            };
            let label = match *action {
                AgentAction::NewSession => " + ".into(),
                AgentAction::Session(index) => format!(
                    " {} ",
                    fit_label(
                        self.ui.agent.sessions[index].title(),
                        rect.cols.saturating_sub(4)
                    )
                ),
                AgentAction::CloseSession(_)
                | AgentAction::ClosePanel
                | AgentAction::Remove(_)
                | AgentAction::ClosePreview => " × ".into(),
                AgentAction::Tool(index) => {
                    let message = &session.model.messages[index];
                    let expanded = message
                        .id
                        .as_ref()
                        .is_some_and(|id| session.expanded_tools.contains(id));
                    format!(" {} {} ", if expanded { "▾" } else { "›" }, message.role)
                }
                AgentAction::Rewind(_) => crate::ui::REWIND_LABEL.into(),
                AgentAction::Stop => {
                    foreground = 0xe87575;
                    " ■ ".into()
                }
                AgentAction::Inspect(index) => {
                    format!(" +{} ", session.context.basket[index].label)
                }
                AgentAction::CopyPreview => " ⧉ ".into(),
                AgentAction::Selection => " Selection ".into(),
                AgentAction::Screen => " Visible screen ".into(),
                _ => continue,
            };
            draw_text(
                output,
                rect.x,
                rect.y,
                rect.cols.min(text_width(&label) as u16),
                &label,
                foreground,
                background,
            )?;
        }
        if self.ui.focus.is_agent() {
            let row = match self.ui.focus {
                Focus::Editor => editor.y,
                Focus::Output => chat_top,
                _ => self
                    .agent_hits
                    .iter()
                    .find(|(_, action)| *action == self.ui.agent.keyboard)
                    .map_or(editor.y, |(rect, _)| rect.y),
            };
            draw_text(output, side.x, row, 1, "›", 0xb1c99c, bg)?;
        }
        output.write_all(b"\x1b[0m")
    }

    #[cfg(feature = "agent")]
    fn draw_agent_tabs(
        &mut self,
        output: &mut Vec<u8>,
        side: Rect,
        editor: Rect,
        bg: u32,
    ) -> io::Result<()> {
        let mut line_bg = 0;
        for shift in [16, 8, 0] {
            line_bg |= ((bg >> shift & 255) * 4 / 5) << shift;
        }
        draw_text(
            output,
            side.x,
            side.y,
            side.cols,
            "",
            self.config.foreground,
            line_bg,
        )?;
        draw_text(output, side.x, side.y, 1, "│", 0x343b3d, line_bg)?;
        let mut x = editor.x;
        for (index, column, columns) in self.ui.agent.tabs(usize::from(side.cols.saturating_sub(1)))
        {
            x = editor.x + column as u16;
            let width = columns as u16;
            let active = index == self.ui.agent.current;
            let mut tab_bg = bg;
            if !active {
                tab_bg = 0;
                for shift in [16, 8, 0] {
                    tab_bg |= ((bg >> shift & 255) * 9 / 10) << shift;
                }
            }
            let color = if active {
                self.config.foreground
            } else {
                0x606060
            };
            if active {
                output.write_all(b"\x1b[4m")?;
            }
            draw_text(
                output,
                x,
                side.y,
                width,
                &format!(
                    " {}",
                    fit_label(
                        self.ui.agent.sessions[index].title(),
                        width.saturating_sub(4)
                    )
                ),
                color,
                tab_bg,
            )?;
            output.write_all(b"\x1b[24m")?;
            self.agent_hits.push((
                Rect {
                    x,
                    y: side.y,
                    cols: width,
                    rows: 1,
                },
                AgentAction::Session(index),
            ));
            if active
                || self
                    .pointer
                    .is_some_and(|(column, row)| row == side.y && column >= x && column < x + width)
            {
                let close = x + width.saturating_sub(3);
                let hovered = self.pointer.is_some_and(|(column, row)| {
                    row == side.y && column >= close && column < close + 3
                });
                draw_tab_icon(
                    output,
                    Rect {
                        x: close,
                        y: side.y,
                        cols: 3,
                        rows: 1,
                    },
                    "×",
                    (if active { 0x889391 } else { 0x404040 }, 0xe87575),
                    tab_bg,
                    hovered,
                    active,
                )?;
                self.agent_hits.push((
                    Rect {
                        x: close,
                        y: side.y,
                        cols: 3.min(width),
                        rows: 1,
                    },
                    AgentAction::CloseSession(index),
                ));
            }
            x += width;
        }
        let hovered = self
            .pointer
            .is_some_and(|(column, row)| row == side.y && column >= x && column < x + 3);
        draw_tab_icon(
            output,
            Rect {
                x,
                y: side.y,
                cols: 3,
                rows: 1,
            },
            "+",
            (0xb1c99c, 0xb5d2a2),
            line_bg,
            hovered,
            false,
        )?;
        self.agent_hits.push((
            Rect {
                x,
                y: side.y,
                cols: 3,
                rows: 1,
            },
            AgentAction::NewSession,
        ));
        let close = Rect {
            x: side.x + side.cols - 3,
            y: side.y,
            cols: 3,
            rows: 1,
        };
        let hovered = self.pointer.is_some_and(|(x, y)| close.contains(x, y));
        draw_tab_icon(
            output,
            close,
            "×",
            (0x889391, 0xe87575),
            line_bg,
            hovered,
            false,
        )?;
        self.agent_hits.push((close, AgentAction::ClosePanel));
        Ok(())
    }

    #[cfg(feature = "monitor")]
    fn draw_resources(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        use crate::monitor::{BACKGROUND, GREEN, MUTED, PURPLE};
        let top = self.view_size().num_lines;
        let last = self.size.num_lines - 1;
        for row in top..self.size.num_lines {
            draw_text(output, 0, row, self.size.num_cols, "", MUTED, BACKGROUND)?;
        }
        if self.ui.resources.open {
            draw_text(
                output,
                0,
                top,
                self.size.num_cols,
                &"─".repeat(usize::from(self.size.num_cols)),
                0x363d38,
                BACKGROUND,
            )?;
            let origin = i32::from(top) + 1;
            let height = last.saturating_sub(top + 2);
            let view = self.ui.resources.view(self.size.num_cols, height);
            for plot in view.plots {
                let colors = [GREEN, PURPLE];
                let width = usize::from(plot.width);
                let graphs: Vec<Vec<u8>> = plot
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
                let mut cells = Vec::new();
                let mut text = String::new();
                for row in 0..usize::from(plot.height) {
                    let y = origin + plot.y + row as i32;
                    if y < origin + 2 || y >= i32::from(last.saturating_sub(1)) {
                        continue;
                    }
                    cells.clear();
                    cells.resize(width, (0u32, GREEN));
                    for (series, graph) in graphs.iter().enumerate() {
                        for (column, &bits) in
                            graph[row * width..(row + 1) * width].iter().enumerate()
                        {
                            if bits != 0 {
                                cells[column].0 |= u32::from(bits);
                                cells[column].1 = colors[series % colors.len()];
                            }
                        }
                    }
                    let mut start = 0;
                    while start < cells.len() {
                        let color = cells[start].1;
                        let end = (start + 1..cells.len())
                            .find(|&i| cells[i].1 != color)
                            .unwrap_or(cells.len());
                        text.clear();
                        text.extend(cells[start..end].iter().map(|(bits, _)| {
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
                            (end - start) as u16,
                            &text,
                            color,
                            BACKGROUND,
                        )?;
                        start = end;
                    }
                }
            }
            for text in view.texts {
                let y = origin + text.y;
                if y >= origin
                    && y < i32::from(last.saturating_sub(1))
                    && text.x < self.size.num_cols
                {
                    draw_text(
                        output,
                        text.x,
                        y as u16,
                        text.width.min(self.size.num_cols - text.x),
                        &text.text,
                        text.color,
                        if text.selected {
                            0x29322b
                        } else if text.highlighted
                            || text.focused && self.ui.focus == Focus::Monitor
                        {
                            0x343c34
                        } else {
                            BACKGROUND
                        },
                    )?;
                }
            }
            draw_text(
                output,
                0,
                last.saturating_sub(1),
                self.size.num_cols,
                &"─".repeat(usize::from(self.size.num_cols)),
                0x363d38,
                BACKGROUND,
            )?;
            draw_text(
                output,
                1,
                last,
                self.size.num_cols.saturating_sub(2),
                if self.client.is_remote() {
                    "Local system · 1s · terminal is remote"
                } else {
                    "Local system · 1s"
                },
                MUTED,
                BACKGROUND,
            )?;
            let hovered = self.pointer.is_some_and(|(_, y)| y == last);
            draw_text(
                output,
                self.size.num_cols.saturating_sub(3),
                last,
                3,
                " ⌄ ",
                if hovered { GREEN } else { MUTED },
                if hovered { 0x343c34 } else { BACKGROUND },
            )?;
        } else {
            let label =
                self.ui
                    .resources
                    .stats
                    .label(self.controller(), None, self.client.is_remote());
            let label = format!(
                " {} ",
                fit_label(&label, self.size.num_cols.saturating_sub(4))
            );
            let x = self
                .size
                .num_cols
                .saturating_sub(text_width(&label) as u16 + 1);
            let hovered = self
                .pointer
                .is_some_and(|(column, row)| row == last && column >= x);
            draw_text(
                output,
                self.size
                    .num_cols
                    .saturating_sub(text_width(&label) as u16 + 1),
                last,
                text_width(&label) as u16,
                &label,
                if hovered { GREEN } else { MUTED },
                if hovered { 0x343c34 } else { BACKGROUND },
            )?;
        }
        Ok(())
    }

    #[cfg(feature = "monitor")]
    fn resource_mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        let top = self.view_size().num_lines;
        let last = self.size.num_lines - 1;
        let pointer =
            (self.ui.resources.open && mouse.row > top && mouse.row < last.saturating_sub(1))
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
            if !self.ui.resources.open || mouse.row == last {
                self.ui.toggle_monitor();
                return self.resize();
            } else if mouse.row == top {
                self.ui.resources.dragging = true;
                self.ui.focus = Focus::Monitor;
            } else {
                self.ui.focus = Focus::Monitor;
                let view = self
                    .ui
                    .resources
                    .view(self.size.num_cols, last.saturating_sub(top + 2));
                if let Some(hit) = view.hits.iter().rev().find(|hit| {
                    mouse.row as i32 == i32::from(top) + 1 + hit.y
                        && (hit.x..hit.x.saturating_add(hit.width)).contains(&mouse.column)
                }) {
                    self.ui.resources.action(hit.action);
                    if !self.ui.resources.open {
                        self.ui.focus = Focus::Terminal;
                        return self.resize();
                    }
                }
            }
        }
        self.resource_dirty = true;
        self.dirty = true;
        Ok(())
    }

    fn draw_active_cursor(&self, output: &mut Vec<u8>) -> io::Result<()> {
        if self.ui.help {
            return output.write_all(b"\x1b[?1l\x1b>\x1b[?25l");
        }
        if self.ui.search.open {
            return write!(
                output,
                "\x1b[1;{}H\x1b[?1l\x1b>\x1b[?25h",
                (text_width(&self.ui.search.query[..self.ui.search.cursor]) + text_width("⌕ ") + 1)
                    .min(usize::from(self.size.num_cols))
            );
        }

        #[cfg(feature = "monitor")]
        if self.ui.resources.open && (self.ui.focus == Focus::Monitor) {
            return output.write_all(b"\x1b[?1l\x1b>\x1b[?25l");
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            return output.write_all(b"\x1b[?1l\x1b>\x1b[?25l");
        }
        #[cfg(feature = "agent")]
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
                let layout = self.editor_layout();
                let rect = self.editor_rect();
                if (layout.first..layout.first + 3).contains(&layout.caret_row) {
                    return write!(
                        output,
                        "\x1b[{};{}H\x1b[?25h",
                        rect.y + (layout.caret_row - layout.first) as u16 + 1,
                        rect.x + layout.caret_column as u16 + 1
                    );
                }
                return output.write_all(b"\x1b[?25l");
            }
        }
        if let Some(snapshot) = self.client.snapshot() {
            draw_cursor(output, &self.client, snapshot, self.view_size())?;
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

    fn send_input(&mut self, pane: PaneId, data: Vec<u8>) -> io::Result<()> {
        if self.controller() {
            self.client.send(Action::Input { pane, data })
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
        if old_hover != self.hovered_block() {
            self.full = true;
        }
        Ok(())
    }

    fn event(&mut self, event: ClientEvent) -> io::Result<()> {
        match event {
            ClientEvent::Updated => {
                if self.ui.search.open && self.ui.search.observe(&mut self.client)? {
                    self.full = true;
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
                    self.clipboard_reader = Some(ClipboardReader::new(3072)?);
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
                if self.clipboard.is_some() {
                    self.message("Clipboard transfer already in progress");
                } else {
                    self.clipboard = Some(ClipboardTransfer {
                        reader: ClipboardSource::Memory(text.into_bytes()),
                        target: if primary { b'p' } else { b'c' },
                        incomplete: false,
                        started: false,
                        bytes: 0,
                    });
                }
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
                    self.full = true;
                }
            }
            ClientEvent::List(_) | ClientEvent::AgentInput { .. } | ClientEvent::History { .. } => {
            }
        }
        Ok(())
    }

    fn input(&mut self, event: Input) -> io::Result<()> {
        #[cfg(feature = "agent")]
        if matches!(event, Input::Alt(_)) && self.ui.agent.active().rewind_target.is_some() {
            self.ui.agent_rewind_key(0);
        }
        if let Input::Alt(key) = event {
            if key == b'q' {
                self.running = false;
                return Ok(());
            }
            if self.ui.help {
                self.ui.help = false;
                self.full = true;
                self.dirty = true;
                return Ok(());
            }
            if self.ui.search.open
                && matches!(
                    key,
                    b'a' | b'f'
                        | b'm'
                        | b'o'
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
                )
            {
                self.ui.search.close(&mut self.client)?;
            }
            match key {
                b'?' => {
                    self.ui.help = true;
                    self.ui.help_scroll = 0;
                    self.full = true;
                    self.dirty = true;
                    return Ok(());
                }
                b'o' => {
                    self.ui.cycle_focus(false);
                    return self.resize();
                }
                b'/' => {
                    if let Some(pane) = self.focused() {
                        self.ui.search.begin(&mut self.client, pane)?;
                    }
                    self.full = true;
                    self.dirty = true;
                    return Ok(());
                }
                #[cfg(feature = "agent")]
                b'a' => {
                    self.ui.toggle_agent();
                    return self.resize();
                }
                #[cfg(feature = "files")]
                b'f' => return self.toggle_files(),
                #[cfg(feature = "monitor")]
                b'm' => {
                    #[cfg(feature = "files")]
                    {
                        self.ui.files_open = false;
                    }
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
        if self.ui.help {
            match &event {
                Input::Escape => self.ui.help = false,
                Input::Bytes(bytes) => match bytes.as_slice() {
                    b"\x1b[A" | b"k" => self.ui.help_scroll = self.ui.help_scroll.saturating_sub(1),
                    b"\x1b[B" | b"j" => self.ui.help_scroll = self.ui.help_scroll.saturating_add(1),
                    b"\x1b[5~" => self.ui.help_scroll = self.ui.help_scroll.saturating_sub(8),
                    b"\x1b[6~" | b" " => {
                        self.ui.help_scroll = self.ui.help_scroll.saturating_add(8)
                    }
                    _ => (),
                },
                _ => (),
            }
            self.full = true;
            self.dirty = true;
            return Ok(());
        }
        if self.ui.search.open {
            match &event {
                Input::Escape => {
                    self.ui.search.close(&mut self.client)?;
                    self.full = true;
                    self.dirty = true;
                }
                Input::Bytes(bytes) => {
                    self.ui.search.bytes(&mut self.client, bytes, false)?;
                    self.full = true;
                    self.dirty = true;
                }
                Input::PasteStart => self.paste = Some(PasteTarget::Search),
                Input::Focus(_) | Input::PasteData(_) | Input::PasteEnd => (),
                _ => return Ok(()),
            }
            if !matches!(
                event,
                Input::PasteData(_) | Input::PasteEnd | Input::Focus(_)
            ) {
                return Ok(());
            }
        }

        #[cfg(feature = "monitor")]
        {
            #[cfg(feature = "agent")]
            let preview = self.ui.agent.active().context.preview.is_some();
            #[cfg(not(feature = "agent"))]
            let preview = false;
            if self.ui.resources.open && (self.ui.focus == Focus::Monitor) && !preview {
                let handled = match &event {
                    Input::Bytes(bytes) => self.ui.resources.bytes(bytes),
                    Input::Escape => self.ui.resources.bytes(b"\x1b"),
                    Input::PasteStart => {
                        self.paste = self.ui.resources.searching.then_some(PasteTarget::Monitor);
                        true
                    }
                    _ => false,
                };
                if handled {
                    if !self.ui.resources.open {
                        self.ui.focus = Focus::Terminal;
                        return self.resize();
                    }
                    self.dirty = true;
                    self.resource_dirty = true;
                    return Ok(());
                }
            }
            if matches!(event, Input::Alt(b'a' | b'f')) {
                self.ui.focus = Focus::Terminal;
            }
        }

        #[cfg(feature = "agent")]
        {
            if self.ui.agent.open
                && self.ui.focus.is_agent()
                && let Input::Bytes(bytes) = &event
                && bytes.len() > 1
                && !bytes.starts_with(b"\x1b")
                && bytes.iter().any(u8::is_ascii_control)
            {
                let mut start = 0;
                for (index, byte) in bytes.iter().enumerate() {
                    if byte.is_ascii_control() {
                        if start < index {
                            self.input(Input::Bytes(bytes[start..index].to_vec()))?;
                        }
                        self.input(Input::Bytes(vec![*byte]))?;
                        start = index + 1;
                    }
                }
                if start < bytes.len() {
                    self.input(Input::Bytes(bytes[start..].to_vec()))?;
                }
                return Ok(());
            }
            if matches!(event, Input::Alt(b'a')) {
                self.ui.toggle_agent();
                return self.resize();
            }
            let rewind_key = match &event {
                Input::Escape => Some(0xff1b),
                Input::Bytes(bytes) if bytes == b"\r" => Some(0xff0d),
                Input::Bytes(_) | Input::PasteStart => Some(0),
                _ => None,
            };
            if let Some(symbol) = rewind_key {
                let selected = self.ui.agent.active().rewind_target.is_some();
                let handled = self.ui.agent_rewind_key(symbol);
                if selected || handled {
                    self.dirty = true;
                    self.agent_dirty = true;
                }
                if handled {
                    return Ok(());
                }
            }
            if self.ui.agent.active().context.preview.is_some()
                && !matches!(
                    event,
                    Input::Mouse(_) | Input::Alt(b'q') | Input::PasteData(_) | Input::PasteEnd
                )
            {
                if let Input::Bytes(ref bytes) = event {
                    if matches!(bytes.as_slice(), b"\t" | b"\x1b[Z") {
                        self.ui.agent_tab(bytes == b"\x1b[Z");
                        self.agent_dirty = true;
                        self.dirty = true;
                        return Ok(());
                    }
                    if matches!(bytes.as_slice(), b"\r" | b" ") && self.ui.focus == Focus::Controls
                    {
                        self.activate_agent(self.ui.agent.keyboard)?;
                        return Ok(());
                    }
                }
                match event {
                    Input::Escape => {
                        self.ui.agent.active_mut().context.preview = None;
                        self.full = true;
                    }
                    Input::Bytes(ref bytes) if bytes == b"\x03" => {
                        let text = self
                            .ui
                            .agent
                            .active()
                            .context
                            .preview
                            .as_ref()
                            .unwrap()
                            .0
                            .snapshot
                            .text
                            .clone();
                        self.copy_text(text);
                    }
                    Input::Bytes(ref bytes) => {
                        let scroll = &mut self
                            .ui
                            .agent
                            .active_mut()
                            .context
                            .preview
                            .as_mut()
                            .unwrap()
                            .1;
                        if matches!(bytes.as_slice(), b"\x1b[A" | b"\x1b[5~") {
                            *scroll = scroll.saturating_sub(3);
                        } else if matches!(bytes.as_slice(), b"\x1b[B" | b"\x1b[6~") {
                            *scroll = scroll.saturating_add(3);
                        }
                    }
                    _ => (),
                }
                self.dirty = true;
                self.agent_dirty = true;
                return Ok(());
            }
            if self.ui.agent.open && self.ui.focus.is_agent() {
                let key = match &event {
                    Input::Bytes(bytes) => match bytes.as_slice() {
                        b"\t" => Some((0xff09, false, false)),
                        b"\x1b[Z" => Some((0xfe20, true, false)),
                        b"\0" => Some((32, false, true)),
                        b"\x1b[A" => Some((0xff52, false, false)),
                        b"\x1b[B" => Some((0xff54, false, false)),
                        b"\x1b[3~" => Some((0xffff, false, false)),
                        b"\r" => Some((0xff0d, false, false)),
                        b" " => Some((32, false, false)),
                        _ => None,
                    },
                    Input::Escape => Some((0xff1b, false, false)),
                    _ => None,
                };
                if let Some((symbol, shift, control)) = key
                    && (matches!(symbol, 0xff09 | 0xfe20)
                        || control
                        || self.ui.focus == Focus::Controls)
                {
                    if let Some(action) = self.ui.agent_key(symbol, shift, control) {
                        self.activate_agent(action)?;
                    }
                    self.dirty = true;
                    self.agent_dirty = true;
                    return Ok(());
                }
            }
            if self.ui.agent.open && self.ui.focus.is_agent() {
                match event {
                    Input::Escape => {
                        if self.ui.agent.active().context.menu {
                            self.ui.agent.active_mut().context.menu = false;
                        } else {
                            self.ui.focus = Focus::Terminal;
                        }
                        self.dirty = true;
                        self.agent_dirty = true;
                        return Ok(());
                    }
                    Input::Bytes(_) if self.ui.focus == Focus::Controls => return Ok(()),
                    Input::Bytes(ref bytes) => return self.agent_bytes(bytes, None),
                    Input::PasteStart => {
                        self.paste = (self.ui.focus == Focus::Editor)
                            .then_some(PasteTarget::Agent(self.ui.agent.active().id));
                        return Ok(());
                    }
                    Input::Alt(b'q')
                    | Input::Mouse(_)
                    | Input::PasteData(_)
                    | Input::PasteEnd
                    | Input::Focus(_) => {}
                    _ => return Ok(()),
                }
            }
        }
        match event {
            Input::Alt(b'q') => {
                self.running = false;
                return Ok(());
            }
            #[cfg(feature = "files")]
            Input::Alt(b'f') => {
                return self.toggle_files();
            }
            Input::Alt(key) => {
                if self.shortcut(key)? {
                    return Ok(());
                }
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    return Ok(());
                }
                if let Some(pane) = self.focused() {
                    self.send_input(pane, vec![0x1b, key])?;
                }
            }
            Input::Escape => {
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    if self
                        .ui
                        .files
                        .as_ref()
                        .is_some_and(|files| files.focus() == crate::files::FileFocus::Preview)
                    {
                        self.ui.files.as_mut().unwrap().focus_list();
                    } else if self
                        .ui
                        .files
                        .as_ref()
                        .is_some_and(|files| files.is_search())
                    {
                        self.ui.files.as_mut().unwrap().stop_search();
                    } else {
                        self.ui.files_open = false;
                        self.ui.focus = Focus::Terminal;
                    }
                    self.full = true;
                    self.dirty = true;
                    return Ok(());
                }
                if let Some(pane) = self.focused() {
                    self.send_input(pane, vec![0x1b])?;
                }
            }
            Input::Bytes(bytes) => {
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    return self.file_input(&bytes, false);
                }
                if let Some(pane) = self.focused() {
                    let mode = self
                        .client
                        .screens()
                        .get(&pane)
                        .map_or(Mode::NONE, Screen::mode);
                    self.send_input(pane, cursor_bytes(bytes, mode))?;
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
                    self.send_input(pane, if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec())?;
                }
            }
            Input::Mouse(mouse) => {
                #[cfg(feature = "agent")]
                if self.ui.agent.active().context.preview.is_some() {
                    return self.agent_mouse(mouse);
                }
                #[cfg(feature = "monitor")]
                if mouse.row < self.view_size().num_lines
                    && (self.ui.resources.pointer.take().is_some()
                        || self
                            .pointer
                            .is_some_and(|(_, row)| row >= self.view_size().num_lines))
                {
                    self.resource_dirty = true;
                    self.dirty = true;
                }
                #[cfg(feature = "monitor")]
                if self.ui.resources.dragging || mouse.row >= self.view_size().num_lines {
                    return self.resource_mouse(mouse);
                }
                #[cfg(feature = "monitor")]
                if !mouse.release && mouse.code & 0b1110_0011 == 0 {
                    self.ui.focus = Focus::Terminal;
                }
                #[cfg(feature = "agent")]
                if self.ui.agent.active().context.preview.is_some()
                    || self.selecting_editor
                    || self.ui.agent.active().selecting()
                    || self.ui.agent.open && mouse.column >= self.agent_rect().x
                {
                    return self.agent_mouse(mouse);
                }
                #[cfg(feature = "agent")]
                if !mouse.release && mouse.code & 0b1110_0011 == 0 {
                    if self.ui.agent.active().rewind_target.is_some() {
                        self.ui.agent_rewind_key(0);
                        self.agent_dirty = true;
                        self.dirty = true;
                    }
                    self.ui.focus = Focus::Terminal;
                }
                #[cfg(feature = "agent")]
                if self.ui.agent.open
                    && self
                        .pointer
                        .is_some_and(|(column, row)| self.agent_rect().contains(column, row))
                {
                    self.agent_dirty = true;
                    self.dirty = true;
                }
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    self.ui.focus = Focus::Files;
                    return self.file_mouse(mouse);
                }
                self.mouse(mouse)?;
            }
            Input::PasteStart => {
                #[cfg(feature = "files")]
                if self.ui.files_open {
                    self.paste = Some(PasteTarget::Files);
                    return Ok(());
                }
                if let Some(pane) = self.focused() {
                    let bracketed = self
                        .client
                        .screens()
                        .get(&pane)
                        .is_some_and(|screen| screen.mode().contains(Mode::BRACKETED_PASTE));
                    self.paste = Some(PasteTarget::Pane { pane, bracketed });
                    if bracketed {
                        self.send_input(pane, b"\x1b[200~".to_vec())?;
                    }
                }
            }
            Input::PasteData(bytes) => match self.paste {
                Some(PasteTarget::Search) => {
                    self.ui.search.bytes(&mut self.client, &bytes, true)?;
                    self.full = true;
                    self.dirty = true;
                }

                #[cfg(feature = "monitor")]
                Some(PasteTarget::Monitor) => {
                    self.ui.resources.query_bytes(&bytes);
                    self.resource_dirty = true;
                    self.dirty = true;
                }
                #[cfg(feature = "agent")]
                Some(PasteTarget::Agent(id)) => self.agent_bytes(&bytes, Some(id))?,
                Some(PasteTarget::Pane { pane, .. }) => self.send_input(pane, bytes)?,
                #[cfg(feature = "files")]
                Some(PasteTarget::Files) => self.file_input(&bytes, true)?,
                None => {}
            },
            Input::PasteEnd => {
                if let Some(PasteTarget::Pane {
                    pane,
                    bracketed: true,
                }) = self.paste.take()
                {
                    self.send_input(pane, b"\x1b[201~".to_vec())?;
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
            b's' => Action::Split {
                pane: 0,
                axis: Axis::TopBottom,
            },
            b'v' => Action::Split {
                pane: 0,
                axis: Axis::LeftRight,
            },
            b'h' => Action::Navigate(Direction::Left),
            b'j' => Action::Navigate(Direction::Down),
            b'k' => Action::Navigate(Direction::Up),
            b'l' => Action::Navigate(Direction::Right),
            b',' => Action::Tab(-1),
            b'.' => Action::Tab(1),
            b'\r' | b'\n' => Action::NewTab { pane },
            b'p' => Action::PromptJump {
                pane,
                previous: true,
            },
            b'n' => Action::PromptJump {
                pane,
                previous: false,
            },
            b'c' => {
                if self.copy_pending || self.clipboard.is_some() {
                    self.message("Clipboard transfer already in progress");
                    return Ok(true);
                }
                let snapshot = self.client.snapshot().unwrap();
                let current = snapshot.panes.iter().find(|view| view.id == pane).unwrap();
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
                    .or_else(|| {
                        current
                            .blocks
                            .iter()
                            .rev()
                            .find(|block| block.output.is_some())
                    });
                let Some(block) = block else {
                    self.message("No command output block available; enable shell integration");
                    return Ok(true);
                };
                self.request += 1;
                Action::CopyBlock {
                    pane,
                    block: block.id,
                    request: self.request,
                }
            }
            _ => return Ok(false),
        };
        if !self.controller() {
            self.message("READ ONLY — attach again to take control; Alt+q detach");
            return Ok(true);
        }
        #[cfg(feature = "files")]
        {
            self.ui.files_open = false;
            self.ui.focus = Focus::Terminal;
        }
        self.full = true;
        self.client.send(action)?;
        if key == b'c' {
            self.copy_pending = true;
        }
        Ok(true)
    }

    fn mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        let old_hover = self.hovered_block();
        let previous = self.pointer.replace((mouse.column, mouse.row));
        if previous != self.pointer && (mouse.row == 0 || previous.is_some_and(|(_, row)| row == 0))
        {
            self.dirty = true;
        }
        if old_hover != self.hovered_block() {
            self.full = true;
            self.dirty = true;
        }
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
                            Action::CloseTab(tab.id)
                        } else {
                            let active = snapshot
                                .tabs
                                .iter()
                                .position(|tab| tab.id == snapshot.active_tab)
                                .unwrap();
                            Action::Tab(index as i32 - active as i32)
                        };
                        self.full = true;
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
                            self.full = true;
                            return self.client.send(Action::Tab(direction));
                        }
                        column += width + 1;
                    }
                }
                if (plus.saturating_sub(1)..plus.saturating_add(2)).contains(&mouse.column)
                    && plus.saturating_add(1) < self.size.num_cols
                {
                    self.full = true;
                    return self.client.send(Action::NewTab {
                        pane: snapshot.focused,
                    });
                }
            }
            return Ok(());
        }
        let Some(view) = self.client.snapshot().and_then(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.rect.contains(mouse.column, mouse.row))
        }) else {
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
        let output_block = screen
            .row_output(view, usize::from(y))
            .map(|block| block.id);
        let selected_pane = self
            .client
            .screens()
            .iter()
            .find_map(|(&pane, screen)| screen.selection_range().map(|_| pane));
        let x = usize::from(physical_x).min(screen.columns().saturating_sub(1)) as u16;
        #[cfg(feature = "agent")]
        let object = crate::context::object_at(
            screen,
            Point {
                line: i32::from(y) - offset as i32,
                column: usize::from(x),
            },
        );
        if !self.controller() {
            return Ok(());
        }
        if !mouse.release && mouse.code & 0b1100_0011 == 0 && self.focused() != Some(pane) {
            self.client.send(Action::Focus(pane))?;
        }
        #[cfg(feature = "agent")]
        if !mouse.release && mouse.code & 0b1110_0011 == 0 && mouse.code & 16 == 0 {
            let point = Point {
                line: i32::from(y) - offset as i32,
                column: usize::from(x),
            };
            let now = Instant::now();
            let double = self.last_click.is_some_and(|(at, old_pane, old_point)| {
                old_pane == pane
                    && old_point == point
                    && now.duration_since(at) < Duration::from_millis(400)
            });
            self.last_click = Some((now, pane, point));
            if double && let Some(object) = object {
                if let Err(error) =
                    self.ui
                        .agent
                        .active_mut()
                        .attach_object(&self.client, pane, object)
                {
                    self.message(error);
                }
                self.ui.agent.open = true;
                self.ui.focus = Focus::Editor;
                return self.resize();
            }
        }
        if mode.intersects(Mode::MOUSE_MODE) && mouse.code & 4 == 0 {
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
                self.send_input(pane, data)?;
            }
        } else if mouse.code & 64 != 0 && !mouse.release {
            let up = mouse.code & 1 == 0;
            if mode.contains(Mode::ALT_SCREEN | Mode::ALTERNATE_SCROLL) {
                let bytes = if up { b"\x1b[A" } else { b"\x1b[B" }.to_vec();
                self.send_input(pane, cursor_bytes(bytes, mode))?;
            } else {
                self.client.send(Action::Scroll {
                    pane,
                    scroll: Scroll::Delta(if up { 3 } else { -3 }),
                })?;
            }
        } else if mouse.code & 3 == 2 && mouse.code & 32 == 0 && !mouse.release {
            if self.copy_pending || self.clipboard.is_some() {
                self.message("Clipboard transfer already in progress");
                return Ok(());
            }
            self.request += 1;
            let action = if let Some(pane) = selected_pane {
                Some(Action::CopySelection {
                    pane,
                    request: self.request,
                })
            } else {
                output_block.map(|block| Action::CopyBlock {
                    pane,
                    block,
                    request: self.request,
                })
            };
            if let Some(action) = action {
                self.client.send(action)?;
                self.copy_pending = true;
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
                    kind: if mouse.code & 16 != 0 {
                        SelectionType::Object
                    } else {
                        SelectionType::Simple
                    },
                })?;
            }
        }
        Ok(())
    }

    fn hovered_block(&self) -> Option<(PaneId, u64)> {
        let (x, y) = self.pointer?;
        let pane = self
            .client
            .snapshot()?
            .panes
            .iter()
            .find(|pane| pane.rect.contains(x, y))?;
        let screen = self.client.screens().get(&pane.id)?;
        screen
            .row_output(pane, usize::from(y - pane.rect.y))
            .map(|block| (pane.id, block.id))
    }

    fn draw(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let Some(snapshot) = self.client.snapshot() else {
            write!(output, "\x1b[H\x1b[0mWaiting for workspace — Alt+q detach")?;
            return Ok(());
        };
        if self.full {
            output.write_all(b"\x1b[2J")?;
        }
        let size = self.view_size();
        let label = self.label();
        let start = output.len();
        draw_tabs(output, snapshot, size.num_cols, &label, self.pointer)?;
        if self.full || self.header != output[start..] {
            self.header.clear();
            self.header.extend_from_slice(&output[start..]);
        } else {
            output.truncate(start);
        }
        let hovered = self.hovered_block();
        for pane in &snapshot.panes {
            if let Some(screen) = self.client.screens().get(&pane.id) {
                for row in 0..screen.screen_lines() {
                    if (!self.full && !screen.dirty[row])
                        || row >= usize::from(pane.rect.rows)
                        || pane.rect.y.saturating_add(row as u16) >= size.num_lines
                    {
                        continue;
                    }
                    let highlight = hovered.is_some_and(|(id, block)| {
                        id == pane.id
                            && screen
                                .row_output(pane, row)
                                .is_some_and(|current| current.id == block)
                    });
                    draw_row(output, screen, pane, row, size.num_cols, highlight)?;
                }
            }
        }
        if self.full {
            draw_separators(output, snapshot, size)?;
        }
        #[cfg(feature = "files")]
        if self.ui.files_open {
            self.draw_files(output)?;
        }
        #[cfg(feature = "monitor")]
        if self.full || self.resource_dirty {
            self.draw_resources(output)?;
            self.resource_dirty = false;
        }
        #[cfg(feature = "agent")]
        if self.ui.agent.open && (self.full || self.agent_dirty) {
            self.draw_agent(output)?;
            self.agent_dirty = false;
        }
        if self.ui.search.open {
            draw_text(
                output,
                0,
                0,
                self.size.num_cols,
                &self.ui.search.label(""),
                0xb1c99c,
                0x29322b,
            )?;
        }
        if self.ui.help {
            let width = self.size.num_cols.saturating_sub(4).min(78);
            let lines = self.ui.help_lines();
            let height = (lines.len() as u16 + 3).min(self.size.num_lines.saturating_sub(2));
            let x = (self.size.num_cols - width) / 2;
            let y = (self.size.num_lines - height) / 2;
            for row in 0..height {
                draw_text(output, x, y + row, width, "", 0xcbd0c9, 0x29322b)?;
            }
            draw_text(
                output,
                x + 1,
                y,
                width.saturating_sub(2),
                "KEYS · ↑↓ scroll · Esc back",
                0xb1c99c,
                0x29322b,
            )?;
            self.ui.help_scroll = self.ui.help_scroll.min(
                lines
                    .len()
                    .saturating_sub(height.saturating_sub(2) as usize),
            );
            for (index, line) in lines
                .iter()
                .skip(self.ui.help_scroll)
                .take(height.saturating_sub(2) as usize)
                .enumerate()
            {
                draw_text(
                    output,
                    x + 1,
                    y + index as u16 + 2,
                    width.saturating_sub(2),
                    line,
                    0xcbd0c9,
                    0x29322b,
                )?;
            }
        }
        self.draw_active_cursor(output)?;
        let (snapshot, screens) = self.client.snapshot_and_screens_mut().unwrap();
        for pane in &snapshot.panes {
            if let Some(screen) = screens.get_mut(&pane.id) {
                screen.reset_damage();
            }
        }
        self.full = false;
        Ok(())
    }

    #[cfg(feature = "files")]
    fn toggle_files(&mut self) -> io::Result<()> {
        if self.client.is_remote() {
            self.message("Files are unavailable in remote sessions");
            return Ok(());
        }
        if self.ui.files_open {
            self.ui.files_open = false;
            self.ui.focus = Focus::Terminal;
            self.full = true;
            self.dirty = true;
            return Ok(());
        }
        let directory = self
            .client
            .snapshot()
            .and_then(|snapshot| {
                snapshot
                    .panes
                    .iter()
                    .find(|pane| pane.id == snapshot.focused)
            })
            .and_then(|pane| pane.cwd.clone());
        let Some(directory) = directory else {
            self.message("Active pane working directory is unavailable");
            return Ok(());
        };
        if let Some(files) = &mut self.ui.files {
            files.set_directory(directory)?;
        } else {
            self.ui.files = Some(crate::files::FileNav::new(directory)?);
        }
        self.file_utf8.clear();
        self.ui.files_open = true;
        self.ui.focus = Focus::Files;
        self.full = true;
        self.dirty = true;
        Ok(())
    }

    #[cfg(feature = "files")]
    fn file_input(&mut self, bytes: &[u8], pasted: bool) -> io::Result<()> {
        use crate::files::FileFocus;
        if pasted
            && !bytes.is_empty()
            && self.file_utf8.is_empty()
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            let files = self.ui.files.as_mut().unwrap();
            files.insert_query(text);
            self.dirty = true;
            return Ok(());
        }
        if !pasted && matches!(bytes, b"\t" | b"\x1b[Z") {
            let files = self.ui.files.as_mut().unwrap();
            if files.focus() == crate::files::FileFocus::List {
                files.focus_preview();
            } else {
                files.focus_list();
            }
            self.dirty = true;
            return Ok(());
        }
        let visible = file_geometry(self.view_size()).rows.saturating_sub(2) as usize;
        if !pasted && bytes.starts_with(b"\x1b") {
            if matches!(bytes, b"\x1b[C" | b"\x1bOC") {
                let files = self.ui.files.as_mut().unwrap();
                if !files.enter_selected() {
                    files.focus_preview();
                }
                self.dirty = true;
                return Ok(());
            }
            if matches!(bytes, b"\x1b[D" | b"\x1bOD") {
                let files = self.ui.files.as_mut().unwrap();
                if files.focus() == FileFocus::Preview {
                    files.focus_list();
                } else if !files.is_search() {
                    files.parent();
                }
                self.dirty = true;
                return Ok(());
            }
            let delta = match bytes {
                b"\x1b[A" | b"\x1bOA" => -1,
                b"\x1b[B" | b"\x1bOB" => 1,
                _ => 0,
            };
            let files = self.ui.files.as_mut().unwrap();
            if files.focus() == FileFocus::Preview {
                files.scroll_preview(delta, visible);
            } else {
                files.move_selection(delta);
            }
            self.dirty = true;
            return Ok(());
        }
        let mut query = None;
        for &byte in bytes {
            let files = self.ui.files.as_mut().unwrap();
            if !pasted && matches!(byte, b'\r' | b'\n') {
                if let Some(query) = query.take() {
                    files.set_query(query);
                }
                if !files.enter_selected()
                    && let Some(path) = files.selected_shell_path()
                {
                    self.ui.files_open = false;
                    self.ui.focus = Focus::Terminal;
                    self.full = true;
                    if let Some(pane) = self.focused() {
                        self.send_input(pane, path)?;
                    }
                    self.dirty = true;
                    return Ok(());
                }
            } else if !pasted && files.focus() == FileFocus::Preview {
                match byte {
                    b'h' => files.focus_list(),
                    b'j' => files.scroll_preview(1, visible),
                    b'k' => files.scroll_preview(-1, visible),
                    _ => {}
                }
            } else if !files.is_search() && !pasted {
                match byte {
                    b'h' => {
                        files.parent();
                    }
                    b'j' => files.move_selection(1),
                    b'k' => files.move_selection(-1),
                    b'l' => {
                        if !files.enter_selected() {
                            files.focus_preview();
                        }
                    }
                    b'/' => files.start_search(),
                    _ => {}
                }
            } else {
                let query = query.get_or_insert_with(|| files.query().to_owned());
                if !pasted && matches!(byte, 8 | 127) {
                    self.file_utf8.clear();
                    query.pop();
                } else if pasted || byte >= 0x20 {
                    self.file_utf8.push(byte);
                    match std::str::from_utf8(&self.file_utf8) {
                        Ok(text) => {
                            query.push_str(text);
                            self.file_utf8.clear();
                        }
                        Err(error) if error.error_len().is_none() => continue,
                        Err(_) => {
                            self.file_utf8.clear();
                            self.message("File search requires UTF-8 input");
                            continue;
                        }
                    }
                }
            }
        }
        if let Some(query) = query {
            self.ui.files.as_mut().unwrap().set_query(query);
        }
        self.dirty = true;
        Ok(())
    }

    #[cfg(feature = "files")]
    fn file_mouse(&mut self, mouse: Mouse) -> io::Result<()> {
        use crate::files::FileFocus;
        let rect = file_geometry(self.view_size());
        let files = self.ui.files.as_mut().unwrap();
        if !rect.contains(mouse.column, mouse.row) {
            return Ok(());
        }
        let (left, visible, first) = file_list_geometry(rect, files);
        let body_y = rect.y + 1 + u16::from(files.is_search());
        let in_preview = mouse.column > rect.x + 1 + left;
        if !mouse.release && mouse.code & 64 != 0 {
            let delta = if mouse.code & 1 == 0 { -3 } else { 3 };
            if in_preview && files.focus_preview() {
                files.scroll_preview(delta, visible);
            } else {
                files.focus_list();
                files.move_selection(delta);
            }
        } else if !mouse.release && mouse.code & 0b1100_0011 == 0 {
            if in_preview {
                files.focus_preview();
            } else if mouse.row >= body_y && mouse.row < body_y + visible as u16 {
                files.focus_list();
                let index = first + usize::from(mouse.row - body_y);
                if index < files.entries().len() {
                    files.move_selection(index as isize - files.selection() as isize);
                }
            }
        } else if files.focus() == FileFocus::Preview {
            return Ok(());
        }
        self.dirty = true;
        Ok(())
    }

    #[cfg(feature = "files")]
    fn draw_files(&self, output: &mut Vec<u8>) -> io::Result<()> {
        use crate::files::FileFocus;
        let Some(files) = &self.ui.files else {
            return Ok(());
        };
        let rect = file_geometry(self.view_size());
        if rect.cols < 5 || rect.rows < 3 {
            write!(output, "\x1b[{};{}H\x1b[0m", rect.y + 1, rect.x + 1)?;
            fit_text(output, "Files", rect.cols);
            return Ok(());
        }
        let (left, _visible, first) = file_list_geometry(rect, files);
        let right = rect.cols.saturating_sub(left + 3);
        let preview_focus = files.focus() == FileFocus::Preview;
        let title = format!(" {} ", files.directory().display());
        let label = fit_label(&title, rect.cols - 2);
        let padding = usize::from(rect.cols - 2) - text_width(&label);
        write!(
            output,
            "\x1b[{};{}H\x1b[0;38;2;80;80;80;48;2;32;32;32m╭{}{}╮",
            rect.y + 1,
            rect.x + 1,
            label,
            "─".repeat(padding)
        )?;
        let search_rows = u16::from(files.is_search());
        let mut text = String::new();
        for row in 0..rect.rows.saturating_sub(2) {
            let content_row = row.saturating_sub(search_rows);
            let selected = row >= search_rows
                && first + usize::from(content_row) == files.selection()
                && !files.entries().is_empty();
            let bg = if selected {
                if preview_focus { 35 } else { 38 }
            } else {
                32
            };
            text.clear();
            if row == 0 && files.is_search() {
                text.push_str(" ⌕ ");
                text.push_str(files.query());
            } else if row >= search_rows
                && let Some(entry) = files.entries().get(first + usize::from(content_row))
            {
                text.push(' ');
                text.push_str(&if files.is_search() {
                    entry.display_path(files.directory())
                } else {
                    entry.display_name()
                });
                if entry.is_dir {
                    text.push('/');
                }
            }
            write!(
                output,
                "\x1b[{};{}H\x1b[0;38;2;80;80;80;48;2;32;32;32m│\x1b[38;2;102;102;102;48;2;{bg};{bg};{bg}m",
                rect.y + row + 2,
                rect.x + 1
            )?;
            fit_text(output, &text, left);
            output.extend_from_slice("\x1b[38;2;64;64;64;48;2;32;32;32m│".as_bytes());
            let preview_row = usize::from(row) + files.preview_scroll();
            let preview: std::borrow::Cow<'_, str> =
                if row == rect.rows.saturating_sub(3) && files.preview_truncated() {
                    "… preview limited to 256 KiB / 4096 lines".into()
                } else if row == 0 && files.preview_loading() {
                    "Loading…".into()
                } else if row == 0 && files.preview_error().is_some() {
                    files.preview_error().unwrap().to_string().into()
                } else {
                    files
                        .preview_lines()
                        .get(preview_row)
                        .map(String::as_str)
                        .unwrap_or_default()
                        .into()
                };
            let bg = if preview_focus { 38 } else { 32 };
            write!(output, "\x1b[38;2;102;102;102;48;2;{bg};{bg};{bg}m ")?;
            fit_text(output, &preview, right - 1);
            output.extend_from_slice("\x1b[38;2;80;80;80;48;2;32;32;32m│".as_bytes());
        }
        write!(
            output,
            "\x1b[{};{}H\x1b[0;38;2;80;80;80;48;2;32;32;32m╰{}╯\x1b[0m",
            rect.y + rect.rows,
            rect.x + 1,
            "─".repeat(usize::from(rect.cols.saturating_sub(2)))
        )?;
        if let Some(error) = files.error() {
            write!(
                output,
                "\x1b[{};{}H\x1b[0;38;2;224;91;101;48;2;32;32;32m",
                rect.y + rect.rows - 1,
                rect.x + 2
            )?;
            fit_text(output, &error.to_string(), left);
            output.extend_from_slice(b"\x1b[0m");
        }
        Ok(())
    }
}

#[cfg(feature = "files")]
fn file_geometry(size: WindowSize) -> Rect {
    let cols = (size.num_cols.saturating_mul(7) / 10)
        .max(12)
        .min(size.num_cols);
    let rows = (size.num_lines.saturating_mul(7) / 10)
        .max(5)
        .min(size.num_lines);
    Rect {
        x: (size.num_cols - cols) / 2,
        y: (size.num_lines - rows) * 2 / 5,
        cols,
        rows,
    }
}

#[cfg(feature = "files")]
fn file_list_geometry(rect: Rect, files: &crate::files::FileNav) -> (u16, usize, usize) {
    let left = rect.cols.saturating_sub(3) * 42 / 100;
    let visible = usize::from(rect.rows.saturating_sub(2 + u16::from(files.is_search())));
    let first = files.selection().saturating_sub(visible.saturating_sub(1));
    (left, visible, first)
}

fn draw_tab_icon(
    output: &mut Vec<u8>,
    rect: Rect,
    label: &str,
    colors: (u32, u32),
    bg: u32,
    hovered: bool,
    active: bool,
) -> io::Result<()> {
    output.write_all(b"\x1b[24;59m")?;
    if active {
        output.write_all(b"\x1b[4;58;2;181;210;162m")?;
    }
    let fg = if hovered { colors.1 } else { colors.0 };
    draw_text(output, rect.x, rect.y, rect.cols, "", fg, bg)?;
    if hovered {
        write!(
            output,
            "\x1b[4;58;2;{};{};{}m",
            fg >> 16 & 255,
            fg >> 8 & 255,
            fg & 255
        )?;
    }
    draw_text(output, rect.x + 1, rect.y, 1, label, fg, bg)?;
    output.write_all(b"\x1b[24;59m")
}

fn draw_tabs(
    output: &mut Vec<u8>,
    snapshot: &Snapshot,
    columns: u16,
    label: &str,
    pointer: Option<(u16, u16)>,
) -> io::Result<()> {
    output.write_all(b"\x1b[H\x1b[0;48;2;25;25;25m\x1b[2K")?;
    let marker = if snapshot.controller {
        "156;143;220"
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
        let fg = if active { 148 } else { 96 };
        let dot = if active { "175;196;96" } else { "64;64;64" };
        let close = if active { "156;143;220" } else { "64;64;64" };
        let hovered =
            pointer.is_some_and(|(column, row)| row == 0 && (x..x + width).contains(&column));
        write!(
            output,
            "\x1b[1;{}H\x1b[0;38;2;{bg};{bg};{bg};48;2;25;25;25m◢\x1b[38;2;{dot};48;2;{bg};{bg};{bg}m • \x1b[38;2;{fg};{fg};{fg}m",
            x + 1
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
        } else {
            "24;59"
        };
        write!(
            output,
            "\x1b[38;2;{close};48;2;{bg};{bg};{bg}m \x1b[{underline}m×\x1b[24;59m \x1b[38;2;{bg};{bg};{bg};48;2;25;25;25m◣\x1b[24;59m"
        )?;
        end = x + width + 1;
    }
    if end.saturating_add(1) < columns {
        let hovered = pointer.is_some_and(|(column, row)| {
            row == 0 && (end.saturating_sub(1)..end + 2).contains(&column)
        });
        draw_tab_icon(
            output,
            Rect {
                x: end.saturating_sub(1),
                y: 0,
                cols: 3,
                rows: 1,
            },
            "+",
            (0x606060, 0xb5d2a2),
            0x191919,
            hovered,
            false,
        )?;
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

fn cursor_bytes(mut bytes: Vec<u8>, mode: Mode) -> Vec<u8> {
    if bytes.len() == 3
        && bytes[0] == 0x1b
        && matches!(bytes[1], b'[' | b'O')
        && matches!(bytes[2], b'A' | b'B' | b'C' | b'D' | b'H' | b'F')
    {
        bytes[1] = if mode.contains(Mode::APP_CURSOR) {
            b'O'
        } else {
            b'['
        };
    }
    bytes
}

fn draw_row(
    output: &mut impl Write,
    screen: &Screen,
    pane: &PaneView,
    row: usize,
    host_columns: u16,
    highlight: bool,
) -> io::Result<()> {
    let rect = pane.rect;
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
    if columns < physical_columns {
        let bg = screen.color(Color::Named(NamedColor::Background));
        write!(
            output,
            "\x1b[{};{}H\x1b[0;48;2;{};{};{}m\x1b[{}X",
            rect.y + row as u16 + 1,
            rect.x + 1,
            bg.r,
            bg.g,
            bg.b,
            physical_columns
        )?;
    } else {
        write!(output, "\x1b[{};{}H", rect.y + row as u16 + 1, rect.x + 1)?;
    }
    let mut previous = None;
    for (column, cell, extra) in data.cells_with_extras(0..columns) {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let selected = !cell.flags.contains(Flags::PROMPT)
            && selection.is_some_and(|selection| selection.contains(Point { line, column }));
        let style = (
            cell.fg,
            cell.bg,
            cell.flags
                & (Flags::BOLD
                    | Flags::DIM
                    | Flags::ITALIC
                    | Flags::INVERSE
                    | Flags::ALL_UNDERLINES
                    | Flags::STRIKEOUT),
            selected,
        );
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
            write!(
                output,
                "\x1b[0;38;2;{};{};{};48;2;{};{};{}",
                fg.r, fg.g, fg.b, bg.r, bg.g, bg.b
            )?;
            for (flag, sgr) in [
                (Flags::BOLD, 1),
                (Flags::DIM, 2),
                (Flags::ITALIC, 3),
                (Flags::STRIKEOUT, 9),
            ] {
                if cell.flags.contains(flag) {
                    write!(output, ";{sgr}")?;
                }
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                output.write_all(b";4")?;
            }
            output.write_all(b"m")?;
            previous = Some(style);
        }
        if cell.flags.contains(Flags::HIDDEN)
            || cell.c.is_control()
            || cell.flags.contains(Flags::WIDE_CHAR) && column + 1 >= columns
        {
            output.write_all(b" ")?;
            if cell.flags.contains(Flags::WIDE_CHAR) && column + 1 < columns {
                output.write_all(b" ")?;
            }
        } else {
            output.write_all(cell.c.encode_utf8(&mut [0; 4]).as_bytes())?;
            if let Some(extra) = extra {
                for ch in &extra.zerowidth {
                    if !ch.is_control() {
                        output.write_all(ch.encode_utf8(&mut [0; 4]).as_bytes())?;
                    }
                }
            }
        }
    }
    output.write_all(b"\x1b[0m")
}

fn draw_separators(
    output: &mut impl Write,
    snapshot: &Snapshot,
    size: WindowSize,
) -> io::Result<()> {
    output.write_all(b"\x1b[0;38;5;240m")?;
    for pane in &snapshot.panes {
        if pane.rect.x > 0 && pane.rect.x <= size.num_cols {
            for row in pane.rect.y
                ..pane
                    .rect
                    .y
                    .saturating_add(pane.rect.rows)
                    .min(size.num_lines)
            {
                write!(output, "\x1b[{};{}H│", row + 1, pane.rect.x)?;
            }
        }
        if pane.rect.y > 1 && pane.rect.y <= size.num_lines {
            write!(
                output,
                "\x1b[{};{}H{}",
                pane.rect.y,
                pane.rect.x + 1,
                "─".repeat(usize::from(
                    pane.rect
                        .cols
                        .min(size.num_cols.saturating_sub(pane.rect.x))
                ))
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
) -> io::Result<()> {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == snapshot.focused)
    else {
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
            x + 1
        )
    }
}

fn fit_text(output: &mut Vec<u8>, text: &str, columns: u16) {
    let end = text.len().min(usize::from(columns));
    let prefix = &text.as_bytes()[..end];
    // A non-ASCII scalar at the boundary may still use zero columns.
    if text.as_bytes().get(end).is_none_or(u8::is_ascii)
        && prefix.is_ascii()
        && !prefix.iter().any(u8::is_ascii_control)
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
        // A fixed multiple of three keeps all intermediate base64 chunks unpadded.
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
            ClipboardSource::Memory(bytes) => {
                let remaining = &bytes[self.bytes as usize..];
                &remaining[..remaining.len().min(3072)]
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
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
