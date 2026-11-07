use crate::files::FileNav;
use crate::input::keysym;
use crate::workspace::{Action, Axis, Client, Direction, PaneId};

use crate::agent::panel::AgentPanel;
use crate::monitor::Monitor;
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

    pub fn file_preview_open(&self) -> bool {
        self.files_open
            && self
                .files
                .as_ref()
                .is_some_and(|files| files.focus() == crate::files::FileFocus::Preview)
    }

    pub fn input_target(&self) -> Option<crate::input::PasteTarget> {
        use crate::input::PasteTarget;

        if self.file_preview_open() {
            return Some(PasteTarget::Files);
        }
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
            self.monitor_jump(client)?;
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
                return Ok(if self.file_preview_open() {
                    KeyResult::Handled
                } else {
                    KeyResult::Paste
                });
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
                        files.focus_list();
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
            if self.resources.open {
                self.resources.set_workspace(snapshot);
            }
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
            && fds
                .iter()
                .any(|source| source.fd == files.fd() && source.revents != 0)
        {
            files.drain_notification()?;
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
        let path = if !remote
            && let Some(relative) = std::env::var_os("HOME")
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
            Some(branch) => Cow::Owned([path.as_ref(), " 󱐋 ", branch].concat()),
            None => path,
        }
    }

    fn expire_status(&mut self) -> bool {
        self.status
            .take_if(|(_, until)| Instant::now() >= *until)
            .is_some()
    }
    pub fn monitor_jump(&mut self, client: &mut crate::workspace::Client) -> io::Result<()> {
        if let Some(pane) = self.resources.jump.take() {
            let snapshot = client
                .snapshot()
                .ok_or_else(|| io::Error::other("workspace is not ready"))?;
            let target = snapshot
                .pane(pane)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "task pane closed"))?
                .tab;
            let current = snapshot
                .tabs
                .iter()
                .position(|tab| tab.id == snapshot.active_tab)
                .unwrap();
            let target = snapshot
                .tabs
                .iter()
                .position(|tab| tab.id == target)
                .unwrap();
            if target != current {
                client.send(Action::Tab(target as i32 - current as i32))?;
            }
            client.send(Action::Focus(pane))?;
            self.focus = Focus::Terminal;
        }
        Ok(())
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

#[derive(Default)]
pub(crate) struct Search {
    pub open: bool,
    pub pane: PaneId,
    pub query: String,
    pub cursor: usize,
    pub index: usize,
    pub pending: bool,
    pub count: usize,
    pub truncated: bool,
    controller: bool,
    request: u64,
}
impl Search {
    pub fn begin(&mut self, client: &mut Client, pane: PaneId) -> io::Result<()> {
        self.close(client)?;
        self.open = true;
        self.controller = client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller);
        self.pane = pane;
        self.submit(client)
    }
    pub fn observe(&mut self, client: &mut Client) -> io::Result<()> {
        if self.open
            && !client.snapshot().is_some_and(|snapshot| {
                snapshot.controller == self.controller && snapshot.pane(self.pane).is_some()
            })
        {
            self.close(client)?;
        }
        Ok(())
    }
    fn submit(&mut self, client: &mut Client) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        self.request += 1;
        self.index = 0;
        self.count = 0;
        self.truncated = false;
        self.pending = !self.query.is_empty();
        client.send(Action::Search {
            pane: self.pane,
            request: self.request,
            query: self.query.clone(),
        })
    }
    pub fn edit(&mut self, client: &mut Client, text: &str) -> io::Result<()> {
        let text = crate::input::without_controls(text);
        if text.is_empty() {
            return Ok(());
        }
        self.query.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.submit(client)
    }
    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        self.query = String::new();
        self.cursor = 0;
        if !std::mem::take(&mut self.open) {
            return Ok(());
        }
        self.pending = false;
        self.count = 0;
        client.send(Action::SearchCancel {
            pane: self.pane,
            request: self.request,
        })
    }
    pub fn next(&mut self, client: &mut Client, previous: bool) -> io::Result<()> {
        self.observe(client)?;
        if !self.open || !self.controller || self.pending || self.count == 0 {
            return Ok(());
        }
        self.index = if previous {
            (self.index + self.count - 1) % self.count
        } else {
            (self.index + 1) % self.count
        };
        client.send(Action::SearchSelect {
            pane: self.pane,
            request: self.request,
            index: self.index as u32,
        })
    }
    pub fn result(
        &mut self,
        client: &mut Client,
        pane: PaneId,
        request: u64,
        count: usize,
        truncated: bool,
    ) -> io::Result<bool> {
        self.observe(client)?;
        if !self.open || pane != self.pane || request != self.request {
            return Ok(false);
        }
        self.pending = false;
        self.index = count.saturating_sub(1);
        self.count = count;
        self.truncated = truncated;
        if self.controller && self.count != 0 {
            client.send(Action::SearchSelect {
                pane,
                request,
                index: self.index as u32,
            })?;
        }
        Ok(true)
    }
    pub fn key(
        &mut self,
        client: &mut Client,
        symbol: u32,
        shift: bool,
        control: bool,
    ) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        match symbol {
            keysym::ESCAPE => self.close(client)?,
            keysym::RETURN | keysym::KP_ENTER => self.next(client, shift)?,
            keysym::UP => self.next(client, true)?,
            keysym::DOWN => self.next(client, false)?,
            keysym::LEFT => self.cursor = self.previous(),
            keysym::RIGHT => self.cursor = self.following(),
            keysym::HOME => self.cursor = 0,
            keysym::END => self.cursor = self.query.len(),
            keysym::BACKSPACE | keysym::DELETE => {
                let range = if symbol == keysym::BACKSPACE {
                    self.previous()..self.cursor
                } else {
                    self.cursor..self.following()
                };
                if self.delete(range) {
                    self.submit(client)?;
                }
            }
            _ if control
                && matches!(char::from_u32(symbol), Some('a' | 'A' | 'u' | 'U'))
                && self.delete(0..self.query.len()) =>
            {
                self.submit(client)?;
            }
            _ => (),
        }
        Ok(())
    }
    fn previous(&self) -> usize {
        self.query
            .floor_char_boundary(self.cursor.saturating_sub(1))
    }
    fn following(&self) -> usize {
        self.query.ceil_char_boundary(self.cursor + 1)
    }
    fn delete(&mut self, range: std::ops::Range<usize>) -> bool {
        self.cursor = range.start;
        let changed = !range.is_empty();
        self.query.replace_range(range, "");
        changed
    }
    pub fn display(&self, preedit: &str, columns: usize) -> (String, usize) {
        use crate::width::{fit_label, scalar_width, text_width};
        let mut status = if self.pending {
            Cow::Borrowed("searching")
        } else {
            Cow::Owned(format!(
                "{}/{}",
                if self.count == 0 { 0 } else { self.index + 1 },
                self.count
            ))
        };
        if self.truncated {
            status.to_mut().push('+');
        }
        if !self.controller {
            status
                .to_mut()
                .push_str(if columns >= 40 { " read-only" } else { " RO" });
        }
        let status = fit_label(&status, (columns.saturating_sub(5).min(columns / 2)) as u16);
        let status_width = text_width(&status);
        let budget = columns.saturating_sub(status_width + 4);
        let before = &self.query[..self.cursor];
        let mut width = 0;
        let start = preedit
            .char_indices()
            .map(|(index, ch)| (before.len() + index, ch))
            .rev()
            .chain(before.char_indices().rev())
            .take_while(|(_, ch)| {
                let next = width + usize::from(scalar_width(*ch).min(2));
                if next > budget.saturating_sub(1) {
                    false
                } else {
                    width = next;
                    true
                }
            })
            .last()
            .map_or(before.len() + preedit.len(), |(index, _)| index);
        let preedit = &preedit[start.saturating_sub(before.len())..];
        let before = &before[start.min(before.len())..];
        let after = fit_label(
            &self.query[self.cursor..],
            budget.saturating_sub(width + 1) as u16,
        );
        let caret = 2 + width;
        let padding = columns.saturating_sub(caret + 1 + text_width(&after) + status_width);
        let mut label = String::with_capacity(
            "⌕ ▏".len() + before.len() + preedit.len() + after.len() + padding + status.len(),
        );
        for piece in ["⌕ ", before, preedit, "▏", &after] {
            label.push_str(piece);
        }
        label.extend(std::iter::repeat_n(' ', padding));
        label.push_str(&status);
        match fit_label(&label, columns as u16) {
            Cow::Borrowed(text) => label.truncate(text.len()),
            Cow::Owned(text) => label = text,
        }
        (label, caret.min(columns.saturating_sub(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_inserts_preedit_at_the_cursor_after_a_literal_caret_marker() {
        let search = Search {
            query: "A▏中B".into(),
            cursor: "A▏中".len(),
            controller: true,
            ..Search::default()
        };
        let (label, caret) = search.display("预", 40);
        assert!(label.starts_with("⌕ A▏中预▏B"));
        assert!(label.ends_with("0/0"));
        assert_eq!(caret, 8);
        let (label, caret) = search.display("", 12);
        assert!(label.ends_with("0/0"));
        assert!(crate::width::text_width(&label) <= 12);
        assert!(caret < 12);
    }

    #[test]
    fn search_label_clamps_scalar_width_to_terminal_cells() {
        let search = Search {
            query: "A\u{17d8}BCDEF".into(),
            cursor: "A\u{17d8}B".len(),
            controller: true,
            ..Search::default()
        };
        assert_eq!(search.display("", 16), ("⌕ A\u{17d8}B▏CDEF  0/0".into(), 6));
        let search = Search {
            query: "ABCDEF".into(),
            cursor: 2,
            ..search
        };
        assert_eq!(
            search.display("\u{17d8}", 16),
            ("⌕ AB\u{17d8}▏CDEF  0/0".into(), 6)
        );
    }
}
