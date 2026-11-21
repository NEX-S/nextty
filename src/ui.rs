use crate::config::Config;
use crate::core::Point;
use crate::files::FileNav;
use crate::input::{KeyResult, keysym};
use crate::panel::{Grid, mix_color, rect};
use crate::width::{fit_label, printable_ascii_prefix, scalar_width, text_width};
use crate::workspace::{Action, Axis, Client, Direction, PaneId, Rect, Snapshot, TabId};

use crate::agent::panel::AgentPanel;
use crate::monitor::Monitor;
use std::{
    borrow::Cow,
    io,
    time::{Duration, Instant},
};

pub(crate) const WHEEL_LINES: usize = 3;

pub(crate) const READ_ONLY: &str = "READ ONLY — attach again to take control; Alt+q detach";

pub(crate) fn tab_color(tab: &crate::workspace::TabView) -> u32 {
    use crate::workspace::TabActivity;
    match tab.activity {
        TabActivity::Running => 0xfe9b30,
        TabActivity::Finished | TabActivity::Idle | TabActivity::Unknown => 0xb5d2a2,
        TabActivity::Failed => 0xef5350,
    }
}

pub(crate) fn open_object(client: &Client, pane: PaneId, text: &str) -> io::Result<()> {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(text);
    if !client.is_remote()
        && let Some(cwd) = client
            .snapshot()
            .and_then(|snapshot| snapshot.pane(pane))
            .and_then(|pane| pane.cwd.as_ref())
    {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub(crate) fn workspace_action(key: char, pane: PaneId) -> Option<Action> {
    Some(match key {
        's' => Action::Split {
            axis: Axis::TopBottom,
        },
        'v' => Action::Split {
            axis: Axis::LeftRight,
        },
        'h' => Action::Navigate(Direction::Left),
        'j' => Action::Navigate(Direction::Down),
        'k' => Action::Navigate(Direction::Up),
        'l' => Action::Navigate(Direction::Right),
        ',' => Action::Tab(-1),
        '.' => Action::Tab(1),
        '\r' | '\n' => Action::NewTab { pane },
        'p' | 'n' => Action::PromptJump {
            pane,
            previous: key == 'p',
        },
        _ => return None,
    })
}

pub(crate) fn copy_action(client: &Client, pane: PaneId) -> Option<Action> {
    if client
        .screens()
        .get(&pane)
        .is_some_and(|screen| screen.selection_range().is_some())
    {
        return Some(Action::CopySelection { pane, request: 0 });
    }
    client
        .snapshot()?
        .pane(pane)?
        .blocks
        .iter()
        .rfind(|block| block.output.is_some())
        .map(|block| Action::CopyBlock {
            pane,
            block: block.id,
            request: 0,
        })
}

pub(crate) fn cursor_bytes<T: AsMut<[u8]>>(mut bytes: T, mode: crate::core::Mode) -> T {
    if let [
        0x1b,
        prefix @ (b'[' | b'O'),
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F',
    ] = bytes.as_mut()
    {
        *prefix = if mode.contains(crate::core::Mode::APP_CURSOR) {
            b'O'
        } else {
            b'['
        };
    }
    bytes
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabHit {
    Select(usize),
    Close(crate::workspace::TabId),
    New,
    Scroll(i32),
}

pub(crate) struct TabBar {
    pub tabs: TabLayout,
    /// First of the three cells around the new-tab button.
    pub plus: Option<u16>,
    pub label: String,
    pub label_x: u16,
    /// Click boxes (first column, width) of the hidden-tabs counts before and after.
    pub overflow: [Option<(u16, u16)>; 2],
}

type TabLayout = Vec<(usize, u16, u16)>;

fn tab_layout(snapshot: &Snapshot, columns: u16, right_label: &str) -> TabLayout {
    let text_width = |text: &str, limit: u16| {
        let prefix = printable_ascii_prefix(&text.as_bytes()[..text.len().min(usize::from(limit))]);
        let mut width = prefix as u16;
        for ch in text[prefix..].chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            width = width.saturating_add(u16::from(scalar_width(ch)));
            if width >= limit {
                return limit;
            }
        }
        width
    };
    let active = snapshot
        .tabs
        .iter()
        .position(|tab| tab.id == snapshot.active_tab)
        .unwrap_or(0);
    let active_width = snapshot
        .tabs
        .get(active)
        .map(|tab| text_width(&tab.title, 22) + 8);
    let minimum = if snapshot.controller { 0 } else { 12 };
    let reserve = text_width(right_label, columns)
        .saturating_add(3)
        .min(columns.saturating_sub(5 + active_width.unwrap_or(9)))
        .max(minimum.min(columns.saturating_sub(5 + 9)));
    let available = columns.saturating_sub(5 + reserve);
    let tab_width = |title: &str, available| (text_width(title, 22) + 8).min(available);
    let mut start = active;
    let mut used = active_width.unwrap_or(0).min(available);
    while start > 0 {
        let width = tab_width(&snapshot.tabs[start - 1].title, available);
        if used.saturating_add(width + 1) > available {
            break;
        }
        used += width + 1;
        start -= 1;
    }
    let mut x = 3;
    let mut layout = Vec::new();
    for (index, tab) in snapshot.tabs.iter().enumerate().skip(start) {
        let remaining = (3 + available).saturating_sub(x);
        let width = if index == active {
            active_width.unwrap_or(0).min(remaining)
        } else {
            tab_width(&tab.title, remaining)
        };
        if width < 9 {
            break;
        }
        layout.push((index, x, width));
        x += width + 1;
    }
    layout
}

fn tab_chrome<'a>(
    snapshot: &Snapshot,
    columns: u16,
    base: &'a str,
) -> (TabLayout, Cow<'a, str>, usize, usize) {
    // How many tabs the layout leaves off before and after the shown ones.
    let hidden = |tabs: &TabLayout| {
        (
            tabs.first().map_or(0, |tab| tab.0),
            tabs.last()
                .map_or(0, |tab| snapshot.tabs.len().saturating_sub(tab.0 + 1)),
        )
    };
    let label = |(before, after)| {
        let mut label = match (before, after) {
            (0, 0) => return Cow::Borrowed(base),
            (0, _) => format!("{after}›"),
            (_, 0) => format!("‹{before}"),
            _ => format!("‹{before} {after}›"),
        };
        if !base.is_empty() {
            label.push(' ');
            label.push_str(base);
        }
        Cow::Owned(label)
    };
    let mut tabs = tab_layout(snapshot, columns, base);
    let mut counts = hidden(&tabs);
    let mut text = label(counts);
    // The label takes columns from the tabs, which can change what is hidden.
    if counts != (0, 0) {
        tabs = tab_layout(snapshot, columns, &text);
        if hidden(&tabs) != counts {
            counts = hidden(&tabs);
            text = label(counts);
        }
    }
    (tabs, text, counts.0, counts.1)
}

pub(crate) fn tab_bar(snapshot: &Snapshot, columns: u16, status: &str) -> TabBar {
    let (tabs, label, before, after) = tab_chrome(snapshot, columns, status);
    let end = tabs.last().map_or(3, |&(_, x, width)| x + width + 1);
    let label = fit_label(&label, columns.saturating_sub(end + 4)).into_owned();
    let label_width = text_width(&label) as u16;
    let label_x = columns.saturating_sub(label_width + 2);
    let mut column = label_x;
    let overflow = [before, after].map(|count| {
        let width = count.checked_ilog10()? as u16 + 2;
        let hit = (column + width <= label_x + label_width).then_some((column, width));
        column += width + 1;
        hit
    });
    TabBar {
        plus: (end.saturating_add(1) < columns).then(|| end.saturating_sub(1)),
        tabs,
        label,
        label_x,
        overflow,
    }
}

impl TabBar {
    pub fn at(&self, snapshot: &Snapshot, column: u16) -> Option<TabHit> {
        if let Some(&(index, x, width)) = self
            .tabs
            .iter()
            .find(|&&(_, x, width)| (x..x + width).contains(&column))
        {
            return Some(if column >= x + width - 4 && column < x + width - 1 {
                TabHit::Close(snapshot.tabs[index].id)
            } else {
                TabHit::Select(index)
            });
        }
        for (hit, direction) in self.overflow.iter().zip([-1, 1]) {
            if hit.is_some_and(|(x, width)| (x..x + width).contains(&column)) {
                return Some(TabHit::Scroll(direction));
            }
        }
        self.plus
            .is_some_and(|plus| (plus..plus + 3).contains(&column))
            .then_some(TabHit::New)
    }
}

/// The output block shown on the screen row of `point`.
pub(crate) fn block_at(client: &Client, pane: PaneId, point: Point) -> Option<(PaneId, u64)> {
    let screen = client.screens().get(&pane)?;
    let row = (point.line + screen.display_offset() as i32) as usize;
    screen
        .row_output(client.snapshot()?.pane(pane)?, row)
        .map(|block| (pane, block.id))
}

pub(crate) fn agent_hit(
    hits: &[(Rect, crate::agent::panel::AgentAction)],
    column: u16,
    row: u16,
) -> Option<crate::agent::panel::AgentAction> {
    hits.iter()
        .rfind(|(rect, _)| rect.contains(column, row))
        .map(|(_, action)| *action)
}

pub(crate) fn transcript_point(
    session: &crate::agent::panel::AgentSession,
    rows: &[(usize, Rect)],
    column: u16,
    row: u16,
    clamp: bool,
) -> Option<crate::agent::panel::TranscriptPoint> {
    let (line, rect) = if clamp {
        rows.iter()
            .rfind(|(_, rect)| rect.y <= row)
            .or_else(|| rows.first())?
    } else {
        rows.iter().find(|(_, rect)| rect.contains(column, row))?
    };
    session.point(
        *line,
        usize::from(column.saturating_sub(rect.x)).min(usize::from(rect.cols)),
    )
}

#[derive(Clone, Copy)]
pub(crate) struct PanelLayout {
    pub terminal_columns: u16,
    pub file_width: u16,
}

impl PanelLayout {
    pub fn new(terminal_columns: u16, sidebar_columns: u16, files_open: bool) -> Self {
        let file_width = if files_open {
            crate::file_panel::sidebar_width(sidebar_columns)
        } else {
            0
        };
        Self {
            terminal_columns: terminal_columns.saturating_sub(file_width).max(2),
            file_width,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClosePrompt {
    pub tab: Option<TabId>,
    pub terminate: bool,
}

pub(crate) fn close_rect(columns: u16, rows: u16) -> Rect {
    let cols = columns.saturating_sub(4).min(64);
    let height = rows.saturating_sub(2).min(7);
    rect((columns - cols) / 2, (rows - height) / 2, cols, height)
}

fn close_buttons(area: Rect, tab: bool) -> [(Rect, &'static str); 2] {
    let [cancel, confirm] = if tab {
        [" Cancel ", " Close tab "]
    } else {
        [" Keep running ", " End session "]
    };
    let (x, y) = (area.x + 2, area.y + area.rows.saturating_sub(1));
    let first = rect(x, y, cancel.len() as u16, 1);
    let second = rect(x + first.cols + 2, y, confirm.len() as u16, 1);
    [(first, cancel), (second, confirm)]
}

pub(crate) fn close_grid(
    prompt: ClosePrompt,
    name: &str,
    rect: Rect,
    pointer: Option<(u16, u16)>,
    config: &Config,
) -> Grid {
    let background = mix_color(config.foreground, config.background, 9);
    let muted = mix_color(config.foreground, config.background, 176);
    let mut grid = Grid::new(rect, config.foreground, background);
    let columns = rect.cols.saturating_sub(4);
    let (title, detail, effect) = if prompt.tab.is_some() {
        (
            format!("Close tab · {name}"),
            "Programs may still be running in this tab",
            "Closing terminates all panes in this tab".to_string(),
        )
    } else {
        let quoted = if name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        {
            name.to_string()
        } else {
            format!("'{}'", name.replace('\'', "'\\''"))
        };
        (
            format!("Close window · {name}"),
            "End session to stop tasks and delete history",
            format!("Resume with: nextty attach {quoted}"),
        )
    };
    for (row, text, color) in [
        (0, title.as_str(), config.foreground),
        (2, detail, muted),
        (3, effect.as_str(), muted),
        (5, "Tab select   Enter confirm   Esc cancel", muted),
    ] {
        if row < rect.rows.saturating_sub(1) {
            grid.text(rect.x + 2, rect.y + row, columns, text, color, background);
        }
    }
    for (index, (button, label)) in close_buttons(rect, prompt.tab.is_some())
        .into_iter()
        .enumerate()
    {
        let terminate = index == 1;
        let lit = prompt.terminate == terminate
            || pointer.is_some_and(|(column, row)| button.contains(column, row));
        let color = match (terminate, lit) {
            (true, true) => 0xe87575,
            (false, true) => crate::panel::ACCENT,
            _ => muted,
        };
        let fill = if lit {
            mix_color(config.foreground, config.background, 24)
        } else {
            background
        };
        grid.text(button.x, button.y, button.cols, label, color, fill);
    }
    grid
}

pub(crate) fn close_hit(rect: Rect, prompt: ClosePrompt, column: u16, row: u16) -> Option<bool> {
    close_buttons(rect, prompt.tab.is_some())
        .iter()
        .position(|(button, _)| button.contains(column, row))
        .map(|index| index == 1)
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

    pub fn file_preview_open(&self) -> bool {
        self.files_open
            && self
                .files
                .as_ref()
                .is_some_and(|files| files.focus() == crate::files::FileFocus::Preview)
    }

    /// A popup that takes every key and click until dismissed.
    pub fn modal(&self) -> bool {
        self.close_prompt.is_some() || self.file_preview_open()
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
    ) -> io::Result<KeyResult> {
        use crate::input::symbol_char;
        if key.modifiers & crate::input::SUPER != 0 {
            return Ok(KeyResult::Handled);
        }
        let (control, shift, paste) = (key.control(), key.shift(), key.paste());

        if let Some(prompt) = &mut self.close_prompt {
            match key.symbol {
                keysym::ESCAPE => self.close_prompt = None,
                keysym::LEFT => prompt.terminate = false,
                keysym::RIGHT => prompt.terminate = true,
                keysym::TAB | keysym::LEFT_TAB => prompt.terminate = !prompt.terminate,
                keysym::RETURN | keysym::KP_ENTER => {
                    let terminate = prompt.terminate;
                    return self.choose_close(client, terminate);
                }
                _ => (),
            }
            return Ok(KeyResult::Handled);
        }
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
            return Ok(KeyResult::Handled);
        }
        if self.resources.open && self.focus == Focus::Monitor {
            if paste && self.resources.searching {
                return Ok(KeyResult::Paste);
            }
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
            return Ok(KeyResult::Handled);
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
            let search = files.is_search() && !preview;
            let shortcut = if !search {
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
                keysym::BACKSPACE | keysym::LEFT => {
                    if files.is_search() {
                        files.key_query(symbol, control);
                    } else {
                        files.parent();
                    }
                }
                keysym::HOME | keysym::END | keysym::DELETE if files.is_search() => {
                    files.key_query(symbol, control)
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
                _ if search && control => files.key_query(symbol, true),
                _ if search && !key.text.is_empty() => files.insert_query(key.text),
                _ => (),
            }
            return Ok(KeyResult::Handled);
        }
        Ok(KeyResult::Unhandled)
    }

    pub fn choose_close(&mut self, client: &mut Client, terminate: bool) -> io::Result<KeyResult> {
        let prompt = self.close_prompt.take().unwrap();
        match (prompt.tab, terminate) {
            (Some(tab), true) => self.act(client, Action::CloseTab(tab))?,
            (None, true) => client.send(Action::Terminate)?,
            (None, false) => return Ok(KeyResult::Quit),
            (Some(_), false) => (),
        }
        Ok(KeyResult::Handled)
    }

    /// Sends a navigation action after dismissing what it would leave stale.
    pub fn act(&mut self, client: &mut Client, action: Action) -> io::Result<()> {
        if matches!(
            action,
            Action::Split { .. }
                | Action::NewTab { .. }
                | Action::Focus(_)
                | Action::Navigate(_)
                | Action::Tab(_)
                | Action::ClosePane(_)
                | Action::CloseTab(_)
        ) {
            self.search.close(client)?;
            self.focus = Focus::Terminal;
            self.cancel_rewind();
            if !matches!(action, Action::Focus(_)) {
                self.files_open = false;
            }
        }
        client.send(action)
    }

    /// Runs an Alt shortcut shared by both frontends; false when `key` is not one.
    pub fn alt(&mut self, client: &mut Client, key: char) -> io::Result<bool> {
        let Some(snapshot) = client.snapshot() else {
            return Ok(false);
        };
        let (pane, controller) = (snapshot.focused, snapshot.controller);
        if key == '/' {
            if self.search.open {
                self.search.close(client)?;
            } else {
                self.search.begin(client, pane)?;
            }
            return Ok(true);
        }
        let action = if key == 'c' {
            copy_action(client, pane)
        } else {
            workspace_action(key, pane)
        };
        if action.is_none() && !matches!(key, 'a' | 'f' | 'm' | 'c') {
            return Ok(false);
        }
        self.search.close(client)?;
        match (key, action) {
            ('a', _) => self.toggle_agent(),
            ('f', _) => self.toggle_files(client)?,
            ('m', _) => {
                self.agent.active_mut().context.menu = false;
                self.toggle_monitor();
            }
            (_, None) => self.message("No command output to copy"),
            (key, Some(action)) => {
                self.focus = Focus::Terminal;
                if controller || key == 'c' {
                    self.act(client, action)?;
                } else {
                    self.message(READ_ONLY);
                }
            }
        }
        Ok(true)
    }

    /// Columns and rows the file panel's keys scroll by.
    pub fn file_viewport(&self, list: Rect, preview: Rect) -> (usize, usize) {
        if self.file_preview_open() {
            (
                usize::from(preview.cols),
                usize::from(preview.rows.saturating_sub(5)),
            )
        } else {
            (usize::from(list.cols), usize::from(list.rows))
        }
    }

    /// A left press in the file panel; the open preview popup captures all of them.
    pub fn file_press(&mut self, list: Rect, preview: Rect, column: u16, row: u16) {
        let Some(files) = &mut self.files else {
            return;
        };
        if files.focus() == crate::files::FileFocus::Preview {
            if !preview.contains(column, row)
                || crate::panel::popup_close(preview).contains(column, row)
            {
                files.focus_list();
            }
        } else if let Some(index) =
            crate::file_panel::Layout::new(list, files).entry_at(files, column, row)
        {
            files.move_selection(index as isize - files.selection() as isize);
            files.focus_preview();
        }
    }

    /// Wheel travel in lines, positive towards the end of the list or preview.
    pub fn file_wheel(&mut self, down: isize, list: Rect, preview: Rect) {
        let Some(files) = &mut self.files else {
            return;
        };
        let rows = if files.focus() == crate::files::FileFocus::Preview {
            crate::panel::popup_body(preview).rows
        } else {
            list.rows.saturating_sub(2)
        };
        files.scroll(down, usize::from(rows));
    }

    pub fn toggle_files(&mut self, client: &Client) -> io::Result<()> {
        if client.is_remote() {
            self.message("Files are unavailable in remote sessions");
        } else if self.files_open {
            self.files_open = false;
            self.focus = Focus::Terminal;
        } else if let Some(directory) = client
            .snapshot()
            .and_then(Snapshot::focused_pane)
            .and_then(|pane| pane.cwd.clone())
        {
            match &mut self.files {
                Some(files) => files.set_directory(directory)?,
                None => self.files = Some(FileNav::new(directory)?),
            }
            self.files_open = true;
            self.focus = Focus::Files;
        } else {
            self.message("Active pane working directory is unavailable");
        }
        Ok(())
    }

    pub fn tab_click(&mut self, client: &mut Client, hit: TabHit) -> io::Result<()> {
        let snapshot = client.snapshot().unwrap();
        let action = match hit {
            TabHit::Close(id) => {
                let tab = snapshot.tabs.iter().find(|tab| tab.id == id).unwrap();
                if !self.request_close_tab(tab) {
                    return Ok(());
                }
                Action::CloseTab(id)
            }
            TabHit::Select(index) => {
                let active = snapshot
                    .tabs
                    .iter()
                    .position(|tab| tab.id == snapshot.active_tab)
                    .unwrap();
                Action::Tab(index as i32 - active as i32)
            }
            TabHit::New => Action::NewTab {
                pane: snapshot.focused,
            },
            TabHit::Scroll(direction) => Action::Tab(direction),
        };
        self.act(client, action)
    }

    /// Wheel over the agent panel: the preview if open, else the editor when
    /// given its (first row, total rows, visible rows), else the transcript.
    pub fn agent_wheel(&mut self, down: isize, editor: Option<(usize, usize, usize)>) {
        let session = self.agent.active_mut();
        if let Some(preview) = &mut session.context.preview {
            preview.scroll = preview.scroll.saturating_add_signed(down);
        } else if let Some((first, total, rows)) = editor {
            session.editor_scroll = Some(
                first
                    .saturating_add_signed(down)
                    .min(total.saturating_sub(rows)),
            );
        } else {
            session.model.scroll = session.model.scroll.saturating_add_signed(-down);
        }
    }

    pub fn monitor_drag(&mut self) {
        self.resources.dragging = true;
        self.focus_monitor();
    }

    pub fn monitor_action(
        &mut self,
        client: &mut Client,
        action: crate::monitor::Action,
    ) -> io::Result<()> {
        self.resources.action(action);
        self.monitor_jump(client)?;
        self.focus_monitor();
        Ok(())
    }

    /// The monitor takes the focus while open, and closes the file panel either way.
    fn focus_monitor(&mut self) {
        self.files_open = false;
        self.focus = if self.resources.open {
            Focus::Monitor
        } else {
            Focus::Terminal
        };
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
        [
            self.status.as_ref().map(|(_, until)| *until),
            self.resources.stats.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Polls everything the frontend's event loop waits on besides the workspace;
    /// returns whether to redraw and whether the agent's messages changed.
    pub fn service(
        &mut self,
        client: &mut Client,
        config: &Config,
        fds: &[libc::pollfd],
    ) -> io::Result<(bool, bool)> {
        let (agent, messages) = self.poll_agent(client, config)?;
        let ready = |fd| {
            fds.iter()
                .any(|source| source.fd == fd && source.revents != 0)
        };
        let layout = ready(self.agent.layout_fd()) && self.agent.poll_layout()?;
        let expired = self
            .status
            .take_if(|(_, until)| Instant::now() >= *until)
            .is_some();
        let mut changed = (agent && self.agent.open) | layout | expired;
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
            changed |= self
                .resources
                .stats
                .refresh(directory, self.resources.open)?;
        }
        if self.files_open
            && let Some(files) = &mut self.files
            && ready(files.fd())
        {
            files.drain_notification()?;
            changed |= files.poll();
        }
        Ok((changed, messages))
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

    pub fn footer<'a>(&'a self, snapshot: &'a Snapshot, remote: bool) -> Cow<'a, str> {
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

    pub fn monitor_jump(&mut self, client: &mut crate::workspace::Client) -> io::Result<()> {
        if let Some(pane) = self.resources.jump.take() {
            let snapshot = client.snapshot().unwrap();
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
        self.focus_monitor();
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
pub(crate) struct TextField {
    pub text: String,
    pub cursor: usize,
}

impl TextField {
    pub fn insert(&mut self, text: &str) -> bool {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if text.is_empty() {
            return false;
        }
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        true
    }

    pub fn key(&mut self, symbol: u32, control: bool) -> bool {
        let before = self.text.floor_char_boundary(self.cursor.saturating_sub(1));
        let after = self.text.ceil_char_boundary(self.cursor + 1);
        let letter = crate::input::symbol_char(symbol).filter(|_| control);
        let range = match (symbol, letter.map(|ch| ch.to_ascii_lowercase())) {
            (keysym::LEFT, _) => {
                self.cursor = before;
                return false;
            }
            (keysym::RIGHT, _) => {
                self.cursor = after;
                return false;
            }
            (keysym::HOME, _) | (_, Some('a')) => {
                self.cursor = 0;
                return false;
            }
            (keysym::END, _) | (_, Some('e')) => {
                self.cursor = self.text.len();
                return false;
            }
            (keysym::BACKSPACE, _) => before..self.cursor,
            (keysym::DELETE, _) => self.cursor..after,
            (_, Some('u')) => 0..self.text.len(),
            _ => return false,
        };
        if range.is_empty() {
            return false;
        }
        self.cursor = range.start;
        self.text.replace_range(range, "");
        true
    }
}

#[derive(Default)]
pub(crate) struct Search {
    pub open: bool,
    pub pane: PaneId,
    pub field: TextField,
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
        self.pending = !self.field.text.is_empty();
        client.send(Action::Search {
            pane: self.pane,
            request: self.request,
            query: self.field.text.clone(),
        })
    }
    pub fn edit(&mut self, client: &mut Client, text: &str) -> io::Result<()> {
        if self.field.insert(text) {
            self.submit(client)?;
        }
        Ok(())
    }
    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        self.field = TextField::default();
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
            _ if self.field.key(symbol, control) => self.submit(client)?,
            _ => (),
        }
        Ok(())
    }
    pub fn display(&self, preedit: &str, columns: usize) -> (String, usize) {
        let mut status = if self.pending {
            "searching".into()
        } else {
            format!(
                "{}/{}",
                if self.count == 0 { 0 } else { self.index + 1 },
                self.count
            )
        };
        if self.truncated {
            status.push('+');
        }
        if !self.controller {
            status.push_str(if columns >= 40 { " read-only" } else { " RO" });
        }
        let status = fit_label(&status, (columns.saturating_sub(5).min(columns / 2)) as u16);
        let status_width = text_width(&status);
        let budget = columns.saturating_sub(status_width + 4);
        let before = [&self.field.text[..self.field.cursor], preedit].concat();
        let mut width = 0;
        let kept: usize = before
            .chars()
            .rev()
            .take_while(|&ch| {
                let next = width + usize::from(scalar_width(ch));
                let fits = next <= budget.saturating_sub(1);
                if fits {
                    width = next;
                }
                fits
            })
            .map(char::len_utf8)
            .sum();
        let after = fit_label(
            &self.field.text[self.field.cursor..],
            budget.saturating_sub(width + 1) as u16,
        );
        let caret = 2 + width;
        let padding = columns.saturating_sub(caret + 1 + text_width(&after) + status_width);
        let label = format!(
            "⌕ {}▏{after}{:padding$}{status}",
            &before[before.len() - kept..],
            ""
        );
        let label = fit_label(&label, columns as u16).into_owned();
        (label, caret.min(columns.saturating_sub(1)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::workspace::{PaneView, TabActivity, TabView};

    pub(crate) fn size(num_cols: u16, num_lines: u16) -> crate::core::WindowSize {
        crate::core::WindowSize {
            num_cols,
            num_lines,
            cell_width: 0,
            cell_height: 0,
        }
    }

    pub(crate) fn pane(id: PaneId, tab: TabId, rect: Rect) -> PaneView {
        PaneView {
            id,
            tab,
            rect,
            title: String::new(),
            cwd: None,
            process: Default::default(),
            blocks: Vec::new(),
            command_blocks: Vec::new(),
            input_generation: 0,
            shell_prompt: None,
        }
    }

    /// A client attached to a fake daemon that already announced `tabs` and one pane each.
    pub(crate) fn pair(
        tabs: &[(TabId, &str, TabActivity)],
        cwd: Option<std::path::PathBuf>,
    ) -> (Client, crate::wire::Peer) {
        let path = std::env::temp_dir().join(format!(
            "nextty-ui-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path).unwrap();
        let mut peer = crate::wire::Peer::new(listener.accept().unwrap().0).unwrap();
        std::fs::remove_file(path).unwrap();
        let snapshot = Snapshot {
            name: "test".into(),
            controller: true,
            focused: 1,
            active_tab: tabs[0].0,
            tabs: tabs
                .iter()
                .map(|&(id, title, activity)| TabView {
                    id,
                    title: title.into(),
                    activity,
                })
                .collect(),
            panes: tabs
                .iter()
                .zip(1..)
                .map(|(&(tab, title, _), id)| PaneView {
                    title: title.into(),
                    cwd: cwd.clone(),
                    ..pane(id, tab, rect(0, 1, 80, 22))
                })
                .collect(),
        };
        peer.send_response(crate::wire::Response::State(Some(snapshot), Vec::new(), 0))
            .unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();
        (client, peer)
    }

    pub(crate) fn actions(client: &mut Client, peer: &mut crate::wire::Peer) -> Vec<Action> {
        client.flush().unwrap();
        let mut actions = Vec::new();
        while let Some(action) = peer.next_action().unwrap() {
            if !matches!(action, Action::ScreenRequest { .. }) {
                actions.push(action);
            }
        }
        actions
    }

    #[test]
    fn monitor_task_jump_switches_tabs_before_focusing_its_pane() {
        let (mut client, mut peer) = pair(
            &[
                (10, "one", TabActivity::Idle),
                (20, "two", TabActivity::Running),
            ],
            None,
        );
        let mut ui = Ui::new().unwrap();
        ui.resources.jump = Some(2);
        ui.monitor_jump(&mut client).unwrap();
        let actions = actions(&mut client, &mut peer);
        assert!(matches!(actions[..], [Action::Tab(1), Action::Focus(2)]));
        assert!(ui.focus == Focus::Terminal);
    }

    #[test]
    fn running_tab_close_defaults_to_cancel_and_keeps_keys_from_the_pty() {
        let (mut client, mut peer) = pair(&[(7, "busy", TabActivity::Running)], None);
        let mut ui = Ui::new().unwrap();
        let tab = client.snapshot().unwrap().tabs[0].clone();
        let mut press = |ui: &mut Ui, symbol, text| {
            let key = crate::input::Key {
                symbol,
                modifiers: 0,
                text,
            };
            assert!(matches!(
                ui.key(&mut client, key, (1, 1)).unwrap(),
                KeyResult::Handled
            ));
        };
        assert!(!ui.request_close_tab(&tab));
        press(&mut ui, u32::from('x'), "x");
        press(&mut ui, keysym::RETURN, "");
        assert!(ui.close_prompt.is_none());
        assert!(!ui.request_close_tab(&tab));
        press(&mut ui, keysym::TAB, "");
        press(&mut ui, keysym::RETURN, "");
        assert!(matches!(
            actions(&mut client, &mut peer)[..],
            [Action::CloseTab(7)]
        ));
    }

    #[test]
    fn alt_shortcuts_keep_panels_and_drafts_and_close_the_other_panels() {
        let directory = std::env::current_dir().unwrap();
        let (mut client, mut peer) = pair(&[(1, "test", TabActivity::Idle)], Some(directory));
        let mut ui = Ui::new().unwrap();
        assert!(ui.alt(&mut client, 'f').unwrap());
        assert!(ui.alt(&mut client, 'a').unwrap());
        assert!(ui.files_open && ui.agent.open && ui.focus == Focus::Editor);
        ui.paste(
            &mut client,
            crate::input::PasteTarget::Agent(ui.agent.active().id),
            "keep draft",
        )
        .unwrap();
        assert!(ui.alt(&mut client, 'a').unwrap());
        assert!(ui.files_open && !ui.agent.open && ui.focus == Focus::Files);
        assert!(ui.alt(&mut client, 'a').unwrap());
        assert!(ui.alt(&mut client, 'v').unwrap());
        assert!(!ui.files_open && ui.focus == Focus::Terminal);
        assert_eq!(ui.agent.active().model.input, "keep draft");
        assert!(matches!(
            actions(&mut client, &mut peer)[..],
            [Action::Split {
                axis: Axis::LeftRight
            }]
        ));
        assert!(ui.alt(&mut client, 'm').unwrap());
        assert!(ui.resources.open && !ui.files_open);
        assert!(!ui.alt(&mut client, 'z').unwrap());
    }

    #[test]
    fn tab_bar_hits_share_the_overflow_geometry() {
        let tabs = (1..=8)
            .map(|id| crate::workspace::TabView {
                id,
                title: format!("long terminal title {id}"),
                activity: TabActivity::Idle,
            })
            .collect();
        let mut snapshot = Snapshot {
            name: "tabs".into(),
            controller: true,
            focused: 1,
            active_tab: 4,
            tabs,
            panes: Vec::new(),
        };
        let bar = tab_bar(&snapshot, 60, "SSH");
        let &(index, x, width) = bar
            .tabs
            .iter()
            .find(|tab| snapshot.tabs[tab.0].id == 4)
            .unwrap();
        assert_eq!(bar.at(&snapshot, x + 5), Some(TabHit::Select(index)));
        assert_eq!(bar.at(&snapshot, x + width - 3), Some(TabHit::Close(4)));
        assert_eq!(bar.at(&snapshot, bar.plus.unwrap() + 1), Some(TabHit::New));
        let (column, _) = bar.overflow[1].unwrap();
        assert_eq!(bar.at(&snapshot, column), Some(TabHit::Scroll(1)));
        snapshot.tabs.truncate(2);
        assert!(tab_bar(&snapshot, 60, "").overflow[1].is_none());
    }

    #[test]
    fn label_inserts_preedit_at_the_cursor_after_a_literal_caret_marker() {
        let search = Search {
            field: TextField {
                text: "A▏中B".into(),
                cursor: "A▏中".len(),
            },
            controller: true,
            ..Search::default()
        };
        let (label, caret) = search.display("预", 40);
        assert!(label.starts_with("⌕ A▏中预▏B"));
        assert!(label.ends_with("0/0"));
        assert_eq!(caret, 8);
    }

    #[test]
    fn search_label_clamps_scalar_width_to_terminal_cells() {
        let search = Search {
            field: TextField {
                text: "A\u{17d8}BCDEF".into(),
                cursor: "A\u{17d8}B".len(),
            },
            controller: true,
            ..Search::default()
        };
        assert_eq!(search.display("", 16), ("⌕ A\u{17d8}B▏CDEF  0/0".into(), 6));
        let search = Search {
            field: TextField {
                text: "ABCDEF".into(),
                cursor: 2,
            },
            ..search
        };
        assert_eq!(
            search.display("\u{17d8}", 16),
            ("⌕ AB\u{17d8}▏CDEF  0/0".into(), 6)
        );
    }

    #[test]
    fn tab_layout_keeps_the_active_tab_and_the_status_label_visible() {
        let tabs = |titles: Vec<String>| -> Vec<TabView> {
            (1..)
                .zip(titles)
                .map(|(id, title)| TabView {
                    id,
                    title,
                    activity: TabActivity::Idle,
                })
                .collect()
        };
        let mut snapshot = Snapshot {
            name: "dev".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: tabs(vec!["shell".into(), "logs".into(), "editor".into()]),
            panes: Vec::new(),
        };
        assert_eq!(
            tab_layout(&snapshot, 104, "CPU 10%  MEM 20%  DSK 30%"),
            [(0, 3, 13), (1, 17, 12), (2, 30, 14)]
        );
        let mut overflow = snapshot.clone();
        overflow.tabs = tabs((0..12).map(|i| format!("tab-{i}")).collect());
        for active in [1, 6, 12] {
            overflow.active_tab = active;
            let (layout, label, before, after) = tab_chrome(&overflow, 80, "");
            assert!(
                layout
                    .iter()
                    .any(|&(index, _, _)| index as u64 + 1 == active)
            );
            assert_eq!(before + layout.len() + after, overflow.tabs.len());
            let (_, x, width) = layout.last().unwrap();
            assert!(x + width + text_width(&label) as u16 + 2 < 80);
        }
        snapshot.tabs.truncate(1);
        snapshot.controller = false;
        let readonly = tab_layout(&snapshot, 40, "Read-only  CPU 100%  MEM 100%  DSK 100%");
        let (_, x, width) = readonly[0];
        assert!(
            40 - (x + width + 1) - 4 >= 9,
            "read-only indicator remains visible"
        );
    }
}
