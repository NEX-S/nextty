use crate::files::FileNav;
use crate::input::keysym;
use crate::workspace::{Action, Axis, Direction, PaneId};

use crate::agent::panel::AgentPanel;
use crate::monitor::Monitor;
use crate::search::Search;
use std::{
    borrow::Cow,
    io,
    time::{Duration, Instant},
};

pub(crate) fn tab_color(tab: &crate::workspace::TabView) -> u32 {
    use crate::workspace::TabActivity;
    match tab.activity {
        TabActivity::Running => 0xfe9b30,
        TabActivity::Finished | TabActivity::Idle | TabActivity::Unknown => 0xb5d2a2,
        TabActivity::Failed => 0xef5350,
    }
}

pub(crate) fn workspace_action(key: char, pane: PaneId) -> Option<Action> {
    Some(match key {
        's' => Action::Split {
            pane: 0,
            axis: Axis::TopBottom,
        },
        'v' => Action::Split {
            pane: 0,
            axis: Axis::LeftRight,
        },
        'h' => Action::Navigate(Direction::Left),
        'j' => Action::Navigate(Direction::Down),
        'k' => Action::Navigate(Direction::Up),
        'l' => Action::Navigate(Direction::Right),
        ',' => Action::Tab(-1),
        '.' => Action::Tab(1),
        '\r' | '\n' => Action::NewTab { pane },
        'p' => Action::PromptJump {
            pane,
            previous: true,
        },
        'n' => Action::PromptJump {
            pane,
            previous: false,
        },
        _ => return None,
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
    pub search: Search,

    pub agent: AgentPanel,

    pub resources: Monitor,

    pub files: Option<FileNav>,

    pub files_open: bool,
}

impl Ui {
    pub fn panel_state(&self) -> impl PartialEq + use<> {
        (self.resources.open, self.files_open, self.agent.open)
    }

    pub fn new() -> io::Result<Self> {
        Ok(Self {
            status: None,
            focus: Focus::Terminal,
            close_prompt: None,
            search: Search::default(),

            agent: AgentPanel::default(),

            resources: Monitor::new()?,

            files: None,

            files_open: false,
        })
    }

    pub fn load_files(&mut self, directory: std::path::PathBuf) -> io::Result<()> {
        if let Some(files) = &mut self.files {
            files.set_directory(directory)?;
        } else {
            self.files = Some(FileNav::new(directory)?);
        }
        Ok(())
    }

    pub fn input_target(&self) -> Option<crate::input::PasteTarget> {
        use crate::input::PasteTarget;

        if self.agent.active().context.preview.is_some() {
            return Some(PasteTarget::Agent(self.agent.active().id));
        }
        if self.search.open {
            return Some(PasteTarget::Search);
        }
        if self.resources.open && self.focus == Focus::Monitor {
            return Some(PasteTarget::Monitor);
        }

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

            PasteTarget::Agent(_)
                if self.focus != Focus::Editor || self.agent.active().context.preview.is_some() =>
            {
                None
            }
            target => Some(target),
        }
    }

    pub fn text_target(&self) -> Option<crate::input::PasteTarget> {
        self.paste_target().filter(|target| {
            *target != crate::input::PasteTarget::Files
                || self.files.as_ref().is_some_and(|files| {
                    files.is_search() && files.focus() == crate::files::FileFocus::List
                })
        })
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
                keysym::ESCAPE | keysym::BACKSPACE | keysym::LEFT if preview => files.focus_list(),
                keysym::ESCAPE => {
                    if files.is_search() {
                        files.stop_search();
                    } else {
                        self.files_open = false;
                        self.focus = Focus::Terminal;
                    }
                }
                keysym::BACKSPACE if files.is_search() => files.backspace_query(),
                keysym::BACKSPACE | keysym::LEFT => {
                    if !files.is_search() {
                        files.parent();
                    }
                }
                keysym::UP | keysym::DOWN => {
                    let delta = if symbol == keysym::UP { -1 } else { 1 };
                    files.scroll(delta, viewport.1);
                }
                keysym::RIGHT => {
                    if !files.enter_selected() {
                        files.focus_preview();
                    }
                }
                keysym::RETURN | keysym::KP_ENTER => {
                    if let Some(entry) = files.selected() {
                        let path = entry.path.to_string_lossy().into_owned();
                        if let Some(pane) = client.snapshot().and_then(|view| view.focused_pane()) {
                            client.send(Action::Input {
                                pane: pane.id,
                                data: path.into_bytes(),
                            })?;
                            self.focus = Focus::Terminal;
                        }
                        return Ok(KeyResult::Handled);
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
                snapshot.focused_pane().and_then(|pane| pane.cwd.as_deref())
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
            snapshot.focused_pane().and_then(|pane| pane.cwd.as_deref())
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
        self.status
            .take_if(|(_, until)| Instant::now() >= *until)
            .is_some()
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

    Editor,

    Output,

    Controls,

    Monitor,

    Files,
}

impl Focus {
    pub fn is_agent(self) -> bool {
        matches!(self, Self::Editor | Self::Output | Self::Controls)
    }
}
