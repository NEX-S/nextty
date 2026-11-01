use crate::files::FileNav;
use crate::input::keysym;

#[cfg(feature = "agent")]
use crate::agent::panel::AgentPanel;
use crate::monitor::Monitor;
use crate::search::Search;
use std::{
    borrow::Cow,
    io,
    time::{Duration, Instant},
};

pub(crate) fn mix_color(foreground: u32, background: u32, weight: u32) -> u32 {
    [0, 8, 16].into_iter().fold(0, |rgb, shift| {
        rgb | ((((foreground >> shift & 255) * weight
            + (background >> shift & 255) * (255 - weight))
            / 255)
            << shift)
    })
}

#[derive(Clone, Copy)]
pub(crate) struct ClosePrompt {
    pub tab: Option<crate::workspace::TabId>,
    pub terminate: bool,
}

pub(crate) struct Ui {
    pub status: Option<(String, Instant)>,
    pub focus: Focus,
    pub close_prompt: Option<ClosePrompt>,
    pub help: bool,
    pub help_scroll: usize,
    pub search: Search,
    #[cfg(feature = "agent")]
    pub agent: AgentPanel,

    pub resources: Monitor,

    pub files: Option<FileNav>,

    pub files_open: bool,
}

impl Ui {
    pub fn panel_state(&self) -> impl PartialEq + use<> {
        (
            self.resources.open,
            self.files_open,
            #[cfg(feature = "agent")]
            self.agent.open,
        )
    }

    pub fn new() -> io::Result<Self> {
        Ok(Self {
            status: None,
            focus: Focus::Terminal,
            close_prompt: None,
            help: false,
            help_scroll: 0,
            search: Search::default(),
            #[cfg(feature = "agent")]
            agent: AgentPanel::default(),

            resources: Monitor::new()?,

            files: None,

            files_open: false,
        })
    }
    pub fn input_target(&self) -> Option<crate::input::PasteTarget> {
        use crate::input::PasteTarget;
        #[cfg(feature = "agent")]
        if self.agent.active().context.preview.is_some() {
            return Some(PasteTarget::Agent(self.agent.active().id));
        }
        if self.search.open {
            return Some(PasteTarget::Search);
        }
        if self.resources.open && self.focus == Focus::Monitor {
            return Some(PasteTarget::Monitor);
        }
        #[cfg(feature = "agent")]
        if self.agent.open && self.focus.is_agent() {
            return Some(PasteTarget::Agent(self.agent.active().id));
        }

        if self.files_open && self.focus == Focus::Files {
            return Some(PasteTarget::Files);
        }
        None
    }

    pub fn paste_target(&self) -> Option<crate::input::PasteTarget> {
        use crate::input::PasteTarget;
        match self.input_target()? {
            PasteTarget::Monitor if !self.resources.searching => None,
            #[cfg(feature = "agent")]
            PasteTarget::Agent(_)
                if self.focus != Focus::Editor || self.agent.active().context.preview.is_some() =>
            {
                None
            }
            target => Some(target),
        }
    }

    pub fn text_target(&self) -> Option<crate::input::PasteTarget> {
        let target = self.paste_target()?;
        if target == crate::input::PasteTarget::Files
            && !self.files.as_ref().is_some_and(|files| {
                files.is_search() && files.focus() == crate::files::FileFocus::List
            })
        {
            return None;
        }
        Some(target)
    }

    pub fn key(
        &mut self,
        client: &mut crate::workspace::Client,
        key: crate::input::Key<'_>,
        viewport: (usize, usize),
    ) -> io::Result<crate::input::KeyResult> {
        use crate::input::{KeyResult, symbol_char};
        if key.modifiers & crate::input::SUPER != 0 {
            return Ok(KeyResult::Handled);
        }
        let (control, shift, paste) = (key.control(), key.shift(), key.paste());
        #[cfg(feature = "agent")]
        if let Some(result) = self.agent_key_input(client, &key, viewport)? {
            return Ok(result);
        }
        if self.search.open {
            if paste {
                return Ok(KeyResult::Paste);
            }
            self.search.key(client, key.symbol, shift, control)?;
            if !control && !key.text.is_empty() {
                self.search.edit(client, key.text)?;
            }
            return Ok(key.text_result());
        }
        if self.resources.open && self.focus == Focus::Monitor {
            if paste && self.resources.searching {
                return Ok(KeyResult::Paste);
            }
            let searching = self.resources.searching;
            self.resources.key(
                if key.symbol == keysym::TAB && shift {
                    keysym::LEFT_TAB
                } else {
                    key.symbol
                },
                key.text,
                control,
            );
            if !self.resources.open {
                self.focus = Focus::Terminal;
            }
            return Ok(if searching {
                key.text_result()
            } else {
                KeyResult::Handled
            });
        }

        if self.files_open && self.focus == Focus::Files {
            use crate::files::FileFocus;
            if paste {
                return Ok(KeyResult::Paste);
            }
            let Some(files) = &mut self.files else {
                return Ok(KeyResult::Handled);
            };
            let preview = files.focus() == FileFocus::Preview;
            let shortcut = if preview || !files.is_search() {
                match symbol_char(key.symbol) {
                    Some('h') => Some(keysym::LEFT),
                    Some('j') => Some(keysym::DOWN),
                    Some('k') => Some(keysym::UP),
                    Some('l') => Some(keysym::RIGHT),
                    Some('/') if !preview => Some(u32::from('/')),
                    _ => None,
                }
            } else {
                None
            };
            let symbol = shortcut.unwrap_or(key.symbol);
            match symbol {
                keysym::TAB | keysym::LEFT_TAB => {
                    if preview {
                        files.focus_list();
                    } else {
                        files.focus_preview();
                    }
                }
                keysym::ESCAPE => {
                    if preview {
                        files.focus_list();
                    } else if files.is_search() {
                        files.stop_search();
                    } else {
                        self.files_open = false;
                        self.focus = Focus::Terminal;
                    }
                }
                keysym::BACKSPACE => {
                    if preview {
                        files.focus_list();
                    } else if files.is_search() {
                        files.backspace_query();
                    } else {
                        files.parent();
                    }
                }
                keysym::UP | keysym::DOWN => {
                    let delta = if symbol == keysym::UP { -1 } else { 1 };
                    if preview {
                        files.scroll_preview(delta, viewport.1);
                    } else {
                        files.move_selection(delta);
                    }
                }
                keysym::LEFT => {
                    if preview {
                        files.focus_list();
                    } else if !files.is_search() {
                        files.parent();
                    }
                }
                keysym::RIGHT => {
                    if !files.enter_selected() {
                        files.focus_preview();
                    }
                }
                keysym::RETURN | keysym::KP_ENTER => {
                    if let Some(entry) = files.selected() {
                        let path = entry.path.to_string_lossy().into_owned();
                        self.message("Copied file path");
                        return Ok(KeyResult::Copy(path, false));
                    }
                }
                0x2f if shortcut.is_some() => files.start_search(),
                _ if files.is_search() && !preview && !control && !key.text.is_empty() => {
                    files.insert_query(key.text)
                }
                _ => (),
            }
            return Ok(if shortcut.is_some() || !files.is_search() || preview {
                KeyResult::Handled
            } else {
                key.text_result()
            });
        }
        Ok(KeyResult::Unhandled)
    }

    pub fn paste(
        &mut self,
        client: &mut crate::workspace::Client,
        target: crate::input::PasteTarget,
        text: &str,
    ) -> io::Result<()> {
        use crate::input::PasteTarget;
        match target {
            PasteTarget::Search => self.search.edit(client, text)?,
            PasteTarget::Monitor => self.resources.insert_query(text),
            PasteTarget::Files => {
                if let Some(files) = &mut self.files {
                    files.insert_query(text);
                }
            }
            #[cfg(feature = "agent")]
            PasteTarget::Agent(id) => {
                if let Err(error) = self.agent.insert(id, text) {
                    self.message(error);
                }
            }
            PasteTarget::Pane { .. } => unreachable!("terminal paste remains raw frontend input"),
        }
        Ok(())
    }

    pub fn fds(&self) -> impl Iterator<Item = libc::pollfd> {
        [
            self.resources.stats.fd(),
            self.files
                .as_ref()
                .filter(|_| self.files_open)
                .map_or(-1, FileNav::fd),
            #[cfg(feature = "agent")]
            self.agent.layout_fd(),
        ]
        .into_iter()
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.status.as_ref().map(|(_, until)| *until)
    }

    pub fn service(
        &mut self,
        client: &crate::workspace::Client,
        fds: &[libc::pollfd],
    ) -> io::Result<(bool, bool)> {
        let mut resources = false;
        if let Some(snapshot) = client.snapshot() {
            let directory = if client.is_remote() {
                Some(std::path::Path::new("/"))
            } else if self.files_open && self.focus == Focus::Files {
                self.files.as_ref().map(FileNav::directory)
            } else {
                snapshot
                    .panes
                    .iter()
                    .find(|pane| pane.id == snapshot.focused)
                    .and_then(|pane| pane.cwd.as_deref())
            };
            resources = self
                .resources
                .stats
                .refresh(directory, self.resources.open)?;
        }
        let mut changed = resources | self.expire_status();
        if self.files_open
            && let Some(files) = &mut self.files
        {
            if fds
                .iter()
                .any(|source| source.fd == files.fd() && source.revents != 0)
            {
                files.drain_notification()?;
            }
            changed |= files.poll();
        }
        Ok((resources, changed))
    }

    pub fn tab_color(
        &self,
        tab: &crate::workspace::TabView,
        foreground: u32,
        background: u32,
    ) -> u32 {
        use crate::workspace::TabActivity;
        match tab.activity {
            TabActivity::Running => 0xfe9b30,
            TabActivity::Finished => 0xb5d2a2,
            TabActivity::Failed => 0xe87575,
            TabActivity::Idle | TabActivity::Unknown => mix_color(foreground, background, 64),
        }
    }

    pub fn request_close_tab(&mut self, tab: &crate::workspace::TabView) -> bool {
        use crate::workspace::TabActivity;
        if matches!(tab.activity, TabActivity::Running | TabActivity::Unknown) {
            self.close_prompt = Some(ClosePrompt {
                tab: Some(tab.id),
                terminate: false,
            });
            false
        } else {
            true
        }
    }

    pub fn message(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now() + Duration::from_secs(4)));
    }

    pub fn footer<'a>(
        &'a self,
        snapshot: &'a crate::workspace::Snapshot,
        remote: bool,
    ) -> Cow<'a, str> {
        if let Some((message, _)) = &self.status {
            return Cow::Borrowed(message);
        }
        let directory = if self.files_open && self.focus == Focus::Files {
            self.files.as_ref().map(FileNav::directory)
        } else {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.id == snapshot.focused)
                .and_then(|pane| pane.cwd.as_deref())
        };
        let Some(directory) = directory else {
            return Cow::Borrowed("");
        };
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let path = if !remote
            && let Some(relative) = home
                .as_deref()
                .and_then(|home| directory.strip_prefix(home).ok())
        {
            if relative.as_os_str().is_empty() {
                "~".into()
            } else {
                Cow::Owned(format!("~/{}", relative.display()))
            }
        } else {
            directory.to_string_lossy()
        };
        match self.resources.stats.branch().filter(|_| !remote) {
            Some(branch) => Cow::Owned(format!("{path} 󱐋 {branch}")),
            None => path,
        }
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
        let targets = [
            self.files_open.then_some(Focus::Files),
            Some(Focus::Terminal),
            #[cfg(feature = "agent")]
            self.agent.open.then_some(Focus::Editor),
            #[cfg(feature = "agent")]
            self.agent.open.then_some(Focus::Output),
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

    pub fn help_view(
        &mut self,
        rect: crate::workspace::Rect,
        config: &crate::config::Config,
    ) -> crate::panel::Grid {
        let background = mix_color(config.foreground, config.background, 9);
        let muted = mix_color(config.foreground, config.background, 176);
        let mut grid = crate::panel::Grid::new(rect, config.foreground, background);
        let columns = rect.cols.saturating_sub(4);
        grid.text(
            rect.x + 2,
            rect.y,
            columns,
            "Keyboard shortcuts",
            config.foreground,
            background,
        );
        let count = usize::from(rect.rows.saturating_sub(4));
        self.help_scroll = self
            .help_scroll
            .min(self.help_lines().count().saturating_sub(count));
        for (row, line) in self
            .help_lines()
            .skip(self.help_scroll)
            .take(count)
            .enumerate()
        {
            let y = rect.y + 2 + row as u16;
            if let Some((key, description)) = line.split_once("  ") {
                let key_width = 16.min(columns);
                grid.text(
                    rect.x + 2,
                    y,
                    key_width,
                    key.trim(),
                    crate::panel::ACCENT,
                    background,
                );
                grid.text(
                    rect.x + 2 + key_width,
                    y,
                    columns.saturating_sub(key_width),
                    description.trim(),
                    muted,
                    background,
                );
            } else if !line.is_empty() {
                grid.text(rect.x + 2, y, columns, line, config.foreground, background);
            }
        }
        grid.text(
            rect.x + 2,
            rect.y + rect.rows.saturating_sub(1),
            columns,
            "↑↓ scroll   Esc close",
            muted,
            background,
        );
        grid
    }

    pub fn help_lines(&self) -> impl Clone + Iterator<Item = &'static str> + use<> {
        [
            (
                self.focus == Focus::Files,
                &[
                    "Files",
                    "Enter          Copy selected absolute path",
                    "/              Filter filenames",
                    "l / Right      Expand directory / preview file",
                    "h / Left       Collapse directory / go to parent",
                    "Esc            Close preview, search, then tree",
                    "",
                ][..],
            ),
            (
                self.focus == Focus::Monitor,
                &[
                    "Monitor",
                    "Tab/Shift+Tab  Monitor controls",
                    "Arrows / hjkl  Navigate; Enter activates",
                    "/              Filter processes",
                    "Esc            Leave filter, then close monitor",
                    "",
                ][..],
            ),
            #[cfg(feature = "agent")]
            (
                self.focus.is_agent(),
                &[
                    "Agent",
                    "Tab/Shift+Tab  Agent input / output / controls",
                    "Enter / Space  Activate selected control",
                    "Ctrl+Space     Attach selection / visible screen",
                    "Enter          Send draft; Ctrl+J inserts newline",
                    "Ctrl+C         Copy selection; otherwise stop while busy",
                    "Esc            Select previous user message; repeat for older",
                    "Enter          Confirm selected rewind; click input to cancel",
                    "",
                ][..],
            ),
            (
                self.focus == Focus::Terminal,
                &[
                    "Terminal",
                    "Alt+p / Alt+n  Previous / next command",
                    "Alt+c          Copy command output",
                    "Alt+/          Find in this pane",
                    "",
                ][..],
            ),
            (
                true,
                &[
                    "Workspace",
                    "Alt+v / Alt+s  Split left-right / top-bottom",
                    "Alt+h/j/k/l    Focus pane",
                    "Alt+, / Alt+.  Previous / next tab",
                    "Alt+Enter      New tab",
                    "Alt+o          Cycle input focus",
                    "Alt+m          System monitor",
                    "Alt+f          Files",
                    #[cfg(feature = "agent")]
                    "Alt+a          Agent input",
                    "Alt+q          Detach; keep workspace running",
                    "Alt+?          Help",
                ][..],
            ),
        ]
        .into_iter()
        .filter(|(visible, _)| *visible)
        .flat_map(|(_, lines)| lines.iter().copied())
    }

    pub fn toggle_monitor(&mut self) {
        self.resources.toggle();

        self.files_open = false;
        self.focus = if self.resources.open {
            Focus::Monitor
        } else {
            Focus::Terminal
        };
    }
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

    Monitor,

    Files,
}
#[cfg(feature = "agent")]
impl Focus {
    pub fn is_agent(self) -> bool {
        matches!(self, Self::Editor | Self::Output | Self::Controls)
    }
}
