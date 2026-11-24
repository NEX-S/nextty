use crate::agent::panel::AgentPanel;
use crate::config::{Config, TabStyle};
use crate::core::{Mode, Point, Scroll, SelectionType, Side};
use crate::files::{FileFocus, FileNav};
use crate::input::{
    ALT, Key, KeyResult, LEFT, MIDDLE, PasteTarget, PointerKind, RIGHT, keysym, mouse_bytes,
    symbol_char,
};
use crate::monitor::{Monitor, View};
use crate::panel::{ACCENT, Grid, INK, MUTED, mix_color, rect};
use crate::width::{fit_label, scalar_width, text_width};
use crate::workspace::{
    Action, Axis, Client, Direction, PaneId, Rect, Response, Snapshot, SplitDrag, TabActivity,
    TabId, TabView,
};
use std::borrow::Cow;
use std::io;
use std::time::{Duration, Instant};

/// Lines one wheel notch scrolls.
pub(crate) const WHEEL_LINES: i32 = 3;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WARNING: u32 = 0xfe9b30;
const DANGER: u32 = 0xe87575;

pub(crate) const READ_ONLY: &str = "READ ONLY — attach again to take control; Alt+q detach";

fn tab_color(tab: &TabView) -> u32 {
    match tab.activity {
        TabActivity::Running => WARNING,
        TabActivity::Finished | TabActivity::Idle | TabActivity::Unknown => ACCENT,
        TabActivity::Failed => 0xef5350,
    }
}

/// The tab row's background, a little darker than the terminal's.
pub(crate) fn tab_row_color(config: &Config) -> u32 {
    mix_color(config.background, 0, 230)
}

/// One part of the tab bar for a frontend to draw on its first row.
pub(crate) enum TabPart<'a> {
    /// A tab's background, `width` cells from `x`.
    Tab {
        x: u16,
        width: u16,
        color: u32,
    },
    Text {
        x: u16,
        width: u16,
        text: &'a str,
        color: u32,
        background: u32,
    },
    Underline {
        x: u16,
        width: u16,
        color: u32,
    },
}

/// A part of the window. A pointer's cell is in its area's grid: the workspace view's for the
/// terminal, the monitor body's or the footer row's from their top left, else the window's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    Tabs,
    Terminal,
    Files,
    Agent,
    /// The monitor's top edge, which resizes it.
    Divider,
    Monitor,
    Footer,
    /// Anywhere while a dialog or popup takes the pointer.
    Overlay,
}

#[derive(Clone, Copy)]
pub(crate) struct Pointer {
    pub kind: PointerKind,
    pub modifiers: u32,
    pub area: Area,
    pub column: u16,
    pub row: u16,
    /// The half of the cell under the pointer; without it, frontends press left and drag right.
    pub side: Side,
}

/// What a pointer event leaves to the frontend.
// Only the desktop frontend reads the selections and paste targets.
#[cfg_attr(not(feature = "gui"), expect(dead_code))]
pub(crate) enum Effect {
    None,
    Input(PaneId, Vec<u8>),
    Copy(String),
    /// Text for the primary selection, which only the desktop frontend keeps.
    Primary(String),
    /// A finished selection in this pane, likewise for the primary selection.
    Selected(PaneId),
    Paste(PasteTarget, bool),
    Quit,
}

/// What a press on the panes keeps doing until its release.
enum Drag {
    Select {
        pane: PaneId,
        point: Point,
        side: Side,
        kind: SelectionType,
    },
    /// A split divider, with the column or row last sent.
    Split(SplitDrag, u16),
    /// The buttons, as bits by number, reported to the application in `pane`.
    Report { pane: PaneId, buttons: u8 },
}

/// The pane and point at a workspace view cell, or in `captured`, clamped into it.
pub(crate) fn pane_point(
    client: &Client,
    column: u16,
    row: u16,
    side: Side,
    captured: Option<PaneId>,
) -> Option<(PaneId, Point, Side)> {
    let snapshot = client.snapshot()?;
    let pane = match captured {
        Some(id) => snapshot.pane(id)?,
        None => snapshot.pane_at(column, row)?,
    };
    let screen = client.screens().get(&pane.id)?;
    let rect = pane.rect;
    let last = screen.columns().saturating_sub(1);
    let (column, side) = match column.checked_sub(rect.x).map(usize::from) {
        None => (0, Side::Left),
        Some(column) if column > last => (last, Side::Right),
        Some(column) => (column, side),
    };
    let row = row.clamp(rect.y, rect.y + rect.rows.saturating_sub(1)) - rect.y;
    let line = i32::from(row) - screen.display_offset() as i32;
    Some((pane.id, Point { line, column }, side))
}

fn open_object(client: &Client, pane: PaneId, text: &str) -> io::Result<()> {
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

/// What Alt+c copies: the selection, else the latest command output.
fn copy_action(client: &Client, pane: PaneId) -> Option<Action> {
    if client
        .screens()
        .get(&pane)
        .is_some_and(|screen| screen.selection_range().is_some())
    {
        return Some(Action::CopySelection {
            pane,
            primary: false,
        });
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
    Close(TabId),
    New,
    Scroll(i32),
}

pub(crate) struct TabBar {
    tabs: TabLayout,
    /// First of the three cells around the new-tab button.
    plus: Option<u16>,
    label: String,
    label_x: u16,
    /// Click boxes (first column, width) of the hidden-tabs counts before and after.
    overflow: [Option<(u16, u16)>; 2],
}

type TabLayout = Vec<(usize, u16, u16)>;

/// The tabs that fit beside `label`, as (index, column, width), always including the active tab.
fn tab_layout(snapshot: &Snapshot, columns: u16, label: &str) -> TabLayout {
    let tab_width = |title: &str| text_width(title).min(22) as u16 + 8;
    let active = snapshot
        .tabs
        .iter()
        .position(|tab| tab.id == snapshot.active_tab)
        .unwrap_or(0);
    let active_width = snapshot
        .tabs
        .get(active)
        .map_or(9, |tab| tab_width(&tab.title));
    // A read-only viewer keeps room for its indicator even beside a wide active tab.
    let minimum = if snapshot.controller { 0 } else { 12 };
    let reserve = (text_width(label).min(usize::from(columns)) as u16 + 3)
        .min(columns.saturating_sub(5 + active_width))
        .max(minimum.min(columns.saturating_sub(5 + 9)));
    let available = columns.saturating_sub(5 + reserve);
    let mut start = active;
    let mut used = active_width.min(available);
    while start > 0 {
        let width = tab_width(&snapshot.tabs[start - 1].title).min(available) + 1;
        if used + width > available {
            break;
        }
        used += width;
        start -= 1;
    }
    let mut x = 3;
    let mut layout = Vec::new();
    for (index, tab) in snapshot.tabs.iter().enumerate().skip(start) {
        let width = tab_width(&tab.title).min((3 + available).saturating_sub(x));
        if width < 9 {
            break;
        }
        layout.push((index, x, width));
        x += width + 1;
    }
    layout
}

fn tab_bar(snapshot: &Snapshot, columns: u16, status: &str) -> TabBar {
    // How many tabs the layout leaves off before and after the shown ones.
    let hidden = |tabs: &TabLayout| {
        let (first, last) = (tabs.first(), tabs.last());
        let after = last.map_or(0, |tab| snapshot.tabs.len() - tab.0 - 1);
        (first.map_or(0, |tab| tab.0), after)
    };
    let label = |(before, after): (usize, usize)| {
        let before = (before > 0).then(|| format!("‹{before}"));
        let after = (after > 0).then(|| format!("{after}›"));
        let status = (!status.is_empty()).then(|| status.to_owned());
        let parts: Vec<_> = [before, after, status].into_iter().flatten().collect();
        parts.join(" ")
    };
    let mut tabs = tab_layout(snapshot, columns, status);
    let mut counts = hidden(&tabs);
    // The counts take columns from the tabs, which can change what is hidden.
    if counts != (0, 0) {
        tabs = tab_layout(snapshot, columns, &label(counts));
        counts = hidden(&tabs);
    }
    let end = tabs.last().map_or(3, |&(_, x, width)| x + width + 1);
    let label = fit_label(&label(counts), columns.saturating_sub(end + 4)).into_owned();
    let label_width = text_width(&label) as u16;
    let label_x = columns.saturating_sub(label_width + 2);
    let mut column = label_x;
    let overflow = [counts.0, counts.1].map(|count| {
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
    fn at(&self, snapshot: &Snapshot, column: u16) -> Option<TabHit> {
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

pub(crate) struct Ui {
    pub status: Option<(String, Instant)>,
    pub focus: Focus,
    pub close_prompt: Option<ClosePrompt>,
    pub search: Search,
    pub agent: AgentPanel,
    pub resources: Monitor,
    pub files: Option<FileNav>,
    pub files_open: bool,
    drag: Option<Drag>,
    /// The last press on the panes and how many quick presses there it ends.
    click: Option<(Instant, PaneId, Point, u8)>,
    // Where the last frame drew what the pointer can hit.
    tabs: Option<TabBar>,
    file_rects: (Rect, Rect),
    close_rect: Rect,
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
            drag: None,
            click: None,
            tabs: None,
            file_rects: Default::default(),
            close_rect: Rect::default(),
        })
    }

    /// The area whose grid keeps the pointer whatever is under it.
    pub fn capture(&self) -> Option<Area> {
        let session = self.agent.active();
        if self.modal() || session.context.preview.is_some() {
            Some(Area::Overlay)
        } else if session.selecting() || self.agent.selecting_editor {
            Some(Area::Agent)
        } else {
            self.drag.as_ref().map(|_| Area::Terminal)
        }
    }

    /// Applies the pointer rules both frontends share.
    pub fn pointer(&mut self, client: &mut Client, pointer: Pointer) -> io::Result<Effect> {
        let Pointer {
            kind,
            area,
            column,
            row,
            ..
        } = pointer;
        if let Some(prompt) = self.close_prompt {
            let hit = close_buttons(self.close_rect, prompt.tab.is_some())
                .iter()
                .position(|(button, _)| button.contains(column, row));
            if kind == PointerKind::Press(LEFT)
                && let Some(index) = hit
                && let KeyResult::Quit = self.choose_close(client, index == 1)?
            {
                return Ok(Effect::Quit);
            }
            return Ok(Effect::None);
        }
        if self.file_preview_open() {
            self.file_pointer(kind, column, row);
            return Ok(Effect::None);
        }
        match self.capture() {
            Some(Area::Terminal) => return self.pane_pointer(client, pointer),
            Some(_) => return Ok(self.agent_pointer(client, pointer)),
            None => (),
        }
        match (kind, area) {
            (PointerKind::Press(button), _) => return self.press(client, button, pointer),
            (PointerKind::Release(LEFT), _) => self.resources.dragging = false,
            (_, Area::Terminal) => return self.pane_pointer(client, pointer),
            (PointerKind::Wheel(_), Area::Agent) => return Ok(self.agent_pointer(client, pointer)),
            (PointerKind::Wheel(_), Area::Files) => self.file_pointer(kind, column, row),
            (PointerKind::Wheel(lines), Area::Divider | Area::Monitor | Area::Footer)
                if self.resources.open =>
            {
                let scroll = &mut self.resources.scroll;
                *scroll = scroll.saturating_add_signed(-(lines * WHEEL_LINES) as isize);
            }
            _ => (),
        }
        Ok(Effect::None)
    }

    /// A press focuses the region under it; the monitor's controls keep its focus.
    fn press(&mut self, client: &mut Client, button: u8, pointer: Pointer) -> io::Result<Effect> {
        let Pointer {
            area, column, row, ..
        } = pointer;
        if area != Area::Agent || self.agent.hit(column, row).is_none() {
            self.agent.active_mut().context.menu = false;
        }
        if button == LEFT && !matches!(area, Area::Divider | Area::Monitor | Area::Footer) {
            self.focus = Focus::Terminal;
            self.resources.searching = false;
            if area != Area::Agent {
                self.cancel_rewind();
            }
        }
        match area {
            Area::Terminal => return self.pane_pointer(client, pointer),
            Area::Agent => return Ok(self.agent_pointer(client, pointer)),
            Area::Tabs if button == LEFT && client.controller() => {
                let snapshot = client.snapshot().unwrap();
                if let Some(hit) = self.tabs.as_ref().and_then(|bar| bar.at(snapshot, column)) {
                    self.tab_click(client, hit)?;
                }
            }
            Area::Files => {
                self.focus = Focus::Files;
                self.file_pointer(pointer.kind, column, row);
            }
            Area::Divider if button == LEFT => self.monitor_drag(),
            Area::Monitor | Area::Footer => {
                if area == Area::Monitor && button == LEFT {
                    self.focus_monitor();
                }
                let action = self.resources.hit(area == Area::Footer, column, row);
                match (button, action) {
                    (LEFT, Some(action)) => self.monitor_action(client, action)?,
                    (MIDDLE, Some(action @ crate::monitor::Action::Search)) => {
                        self.monitor_action(client, action)?;
                        return Ok(Effect::Paste(PasteTarget::Monitor, true));
                    }
                    _ => (),
                }
            }
            _ => (),
        }
        Ok(Effect::None)
    }

    /// Splits, selections, clicks, copies, pastes, mouse reports and the wheel on the panes.
    fn pane_pointer(&mut self, client: &mut Client, pointer: Pointer) -> io::Result<Effect> {
        let Pointer {
            kind,
            modifiers,
            column,
            row,
            side,
            ..
        } = pointer;
        let along = |split: SplitDrag| match split.axis {
            Axis::LeftRight => column,
            Axis::TopBottom => row,
        };
        let controller = client.controller();
        if let Some(Drag::Split(split, position)) = &mut self.drag
            && controller
        {
            match kind {
                PointerKind::Release(LEFT) => self.drag = None,
                PointerKind::Motion if along(*split) != *position => {
                    *position = along(*split);
                    client.send(split.action(column, row))?;
                }
                _ => (),
            }
            return Ok(Effect::None);
        }
        if kind == PointerKind::Press(LEFT)
            && controller
            && let Some(split) = client.snapshot().unwrap().divider_at(column, row)
        {
            self.drag = Some(Drag::Split(split, along(split)));
            return Ok(Effect::None);
        }
        let captured = match self.drag {
            Some(Drag::Select { pane, .. } | Drag::Report { pane, .. }) => Some(pane),
            _ => None,
        };
        let Some((pane, point, side)) = pane_point(client, column, row, side, captured) else {
            return Ok(Effect::None);
        };
        let screen = &client.screens()[&pane];
        let (mode, offset) = (screen.mode(), screen.display_offset());
        if kind == PointerKind::Press(RIGHT) {
            if let Some(object) = crate::context::object_at(screen, point) {
                return Ok(Effect::Copy(object.text));
            }
            if let Some((pane, block)) = block_at(client, pane, point) {
                client.send(Action::CopyBlock { pane, block })?;
                return Ok(Effect::None);
            }
        }
        if !controller {
            // A drag that lost control of the workspace ends.
            self.drag = None;
            return Ok(Effect::None);
        }
        // Alt keeps the pointer local while the application captures it.
        let reporting = mode.intersects(Mode::MOUSE_MODE) && modifiers & ALT == 0;
        let cell = (point.column, (point.line + offset as i32) as usize);
        let report = |button, release, count| {
            mouse_bytes(button, modifiers, cell, release, mode)
                .map_or(Effect::None, |data| Effect::Input(pane, data.repeat(count)))
        };
        let paste = PasteTarget::Pane {
            pane,
            bracketed: mode.contains(Mode::BRACKETED_PASTE),
        };
        match kind {
            PointerKind::Press(button) => {
                if client.focused() != Some(pane) {
                    self.act(client, Action::Focus(pane))?;
                }
                if reporting {
                    let held = match self.drag {
                        Some(Drag::Report { buttons, .. }) => buttons,
                        _ => 0,
                    };
                    let buttons = held | 1 << button;
                    self.drag = Some(Drag::Report { pane, buttons });
                    return Ok(report(button, false, 1));
                }
                match button {
                    LEFT => self.select(client, pane, point, side)?,
                    MIDDLE => return Ok(Effect::Paste(paste, true)),
                    _ => return Ok(Effect::Paste(paste, false)),
                }
            }
            PointerKind::Motion => match self.drag {
                Some(Drag::Select { .. }) => {
                    client.send(Action::SelectionUpdate { pane, point, side })?;
                }
                Some(Drag::Report { buttons, .. })
                    if reporting && mode.intersects(Mode::MOUSE_DRAG | Mode::MOUSE_MOTION) =>
                {
                    return Ok(report(32 + buttons.trailing_zeros() as u8, false, 1));
                }
                None if reporting && mode.contains(Mode::MOUSE_MOTION) => {
                    return Ok(report(32 + 3, false, 1));
                }
                _ => (),
            },
            PointerKind::Release(button) => match self.drag {
                Some(Drag::Select {
                    point: start,
                    side: start_side,
                    kind,
                    ..
                }) if button == LEFT => {
                    // The release comes where the last motion already moved the selection.
                    self.drag = None;
                    if kind != SelectionType::Simple || (point, side) != (start, start_side) {
                        return Ok(Effect::Selected(pane));
                    }
                }
                Some(Drag::Report { buttons, .. }) if buttons & 1 << button != 0 => {
                    let buttons = buttons & !(1 << button);
                    self.drag = (buttons != 0).then_some(Drag::Report { pane, buttons });
                    return Ok(report(button, true, 1));
                }
                _ => (),
            },
            PointerKind::Wheel(lines) => {
                let count = lines.unsigned_abs() as usize;
                if reporting {
                    return Ok(report(if lines > 0 { 64 } else { 65 }, false, count));
                }
                if mode.contains(Mode::ALT_SCREEN | Mode::ALTERNATE_SCROLL) && modifiers & ALT == 0
                {
                    let arrow = cursor_bytes(if lines > 0 { *b"\x1b[A" } else { *b"\x1b[B" }, mode);
                    let arrows = arrow.repeat(count * WHEEL_LINES as usize);
                    return Ok(Effect::Input(pane, arrows));
                }
                let scroll = Scroll::Delta(lines * WHEEL_LINES);
                client.send(Action::Scroll { pane, scroll })?;
            }
        }
        Ok(Effect::None)
    }

    /// A left press on a pane: one starts a selection, two open the object under it or
    /// select a word, three select lines.
    fn select(
        &mut self,
        client: &mut Client,
        pane: PaneId,
        point: Point,
        side: Side,
    ) -> io::Result<()> {
        let now = Instant::now();
        let count = match self.click {
            Some((at, clicked, place, count))
                if (clicked, place) == (pane, point) && now.duration_since(at) < DOUBLE_CLICK =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        self.click = Some((now, pane, point, count));
        if count == 2
            && let Some(object) = crate::context::object_at(&client.screens()[&pane], point)
        {
            return open_object(client, pane, &object.text);
        }
        let kind = [
            SelectionType::Simple,
            SelectionType::Semantic,
            SelectionType::Lines,
        ][usize::from(count - 1)];
        self.drag = Some(Drag::Select {
            pane,
            point,
            side,
            kind,
        });
        client.send(Action::SelectionStart {
            pane,
            point,
            side,
            kind,
        })
    }

    pub fn file_preview_open(&self) -> bool {
        self.files_open
            && (self.files.as_ref()).is_some_and(|files| files.focus() == FileFocus::Preview)
    }

    /// A popup that takes every key and click until dismissed.
    pub fn modal(&self) -> bool {
        self.close_prompt.is_some() || self.file_preview_open()
    }

    /// The panel that receives typed input, in order of precedence; `None` is the terminal.
    pub fn input_target(&self) -> Option<PasteTarget> {
        let agent = PasteTarget::Agent(self.agent.active().id);
        let monitor = self.resources.open && self.focus == Focus::Monitor;
        let files = self.files_open && self.focus == Focus::Files;
        [
            (self.file_preview_open(), PasteTarget::Files),
            (self.agent.active().context.preview.is_some(), agent),
            (self.search.open, PasteTarget::Search),
            (monitor, PasteTarget::Monitor),
            (self.agent.open && self.focus.is_agent(), agent),
            (files, PasteTarget::Files),
        ]
        .into_iter()
        .find_map(|(active, target)| active.then_some(target))
    }

    pub fn paste_target(&self) -> Option<PasteTarget> {
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

    pub fn text_target(&self) -> Option<PasteTarget> {
        self.paste_target().filter(|target| {
            *target != PasteTarget::Files
                || (self.files.as_ref())
                    .is_some_and(|files| files.is_search() && files.focus() == FileFocus::List)
        })
    }

    pub fn key(&mut self, client: &mut Client, key: Key<'_>) -> io::Result<KeyResult> {
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
        if let Some(result) = self.agent_key_input(client, &key)? {
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
            let symbol = if key.symbol == keysym::TAB && shift {
                keysym::LEFT_TAB
            } else {
                key.symbol
            };
            self.resources.key(symbol, key.text, control);
            self.monitor_jump(client)?;
            if !self.resources.open {
                self.focus = Focus::Terminal;
            }
            return Ok(KeyResult::Handled);
        }
        if self.files_open && self.focus == Focus::Files {
            return self.file_key(client, &key);
        }
        Ok(KeyResult::Unhandled)
    }

    fn file_key(&mut self, client: &mut Client, key: &Key<'_>) -> io::Result<KeyResult> {
        if key.paste() {
            return Ok(if self.file_preview_open() {
                KeyResult::Handled
            } else {
                KeyResult::Paste
            });
        }
        // Only the preview scrolls by its height.
        let rows = usize::from(crate::panel::popup_body(self.file_rects.1).rows);
        let files = self.files.as_mut().unwrap();
        let preview = files.focus() == FileFocus::Preview;
        let search = files.is_search();
        let typing = search && !preview;
        // Unless a query is being typed, h/j/k/l move like the arrow keys and / starts a query.
        let symbol = match symbol_char(key.symbol).filter(|_| !typing) {
            Some('h') => keysym::LEFT,
            Some('j') => keysym::DOWN,
            Some('k') => keysym::UP,
            Some('l') => keysym::RIGHT,
            Some('/') if !preview => {
                files.start_search();
                return Ok(KeyResult::Handled);
            }
            _ => key.symbol,
        };
        match symbol {
            keysym::TAB | keysym::LEFT_TAB if preview => files.focus_list(),
            keysym::TAB | keysym::LEFT_TAB => files.focus_preview(),
            keysym::ESCAPE | keysym::BACKSPACE | keysym::LEFT if preview => files.focus_list(),
            keysym::ESCAPE if search => files.stop_search(),
            keysym::ESCAPE => {
                self.files_open = false;
                self.focus = Focus::Terminal;
            }
            keysym::BACKSPACE | keysym::LEFT | keysym::HOME | keysym::END | keysym::DELETE
                if search =>
            {
                files.key_query(symbol, key.control())
            }
            keysym::BACKSPACE | keysym::LEFT => files.parent(),
            keysym::UP => files.scroll(-1, rows),
            keysym::DOWN => files.scroll(1, rows),
            keysym::RIGHT => {
                if !files.enter_selected() {
                    files.focus_preview();
                }
            }
            keysym::RETURN | keysym::KP_ENTER => {
                if let Some(entry) = files.selected() {
                    let data = entry.path.to_string_lossy().into_owned().into_bytes();
                    files.focus_list();
                    if let Some(pane) = client.focused() {
                        client.send(Action::Input { pane, data })?;
                        self.focus = Focus::Terminal;
                    }
                }
            }
            _ if typing && key.control() => files.key_query(symbol, true),
            _ if typing && !key.text.is_empty() => files.insert_query(key.text),
            _ => (),
        }
        Ok(KeyResult::Handled)
    }

    fn choose_close(&mut self, client: &mut Client, terminate: bool) -> io::Result<KeyResult> {
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
        let split = |axis| Some(Action::Split { axis });
        let navigate = |direction| Some(Action::Navigate(direction));
        let action = match key {
            '/' if self.search.open => return self.search.close(client).map(|()| true),
            '/' => return self.search.begin(client, pane).map(|()| true),
            'a' | 'f' | 'm' => None,
            'c' => copy_action(client, pane),
            's' => split(Axis::TopBottom),
            'v' => split(Axis::LeftRight),
            'h' => navigate(Direction::Left),
            'j' => navigate(Direction::Down),
            'k' => navigate(Direction::Up),
            'l' => navigate(Direction::Right),
            ',' => Some(Action::Tab(-1)),
            '.' => Some(Action::Tab(1)),
            '\r' | '\n' => Some(Action::NewTab { pane }),
            'p' | 'n' => Some(Action::PromptJump {
                pane,
                previous: key == 'p',
            }),
            _ => return Ok(false),
        };
        self.search.close(client)?;
        match (key, action) {
            ('a', _) => self.toggle_agent(),
            ('f', _) => self.toggle_files(client)?,
            ('m', _) => {
                self.agent.active_mut().context.menu = false;
                self.resources.toggle();
                self.focus_monitor();
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

    /// The file list and its open preview popup, whose places the pointer and keys then use.
    pub fn file_grids(
        &mut self,
        list: Rect,
        preview: Rect,
        preedit: &str,
        config: &Config,
    ) -> (Grid, Option<Grid>) {
        self.file_rects = (list, preview);
        let files = self.files.as_mut().unwrap();
        let open = files.focus() == FileFocus::Preview;
        let focused = self.focus == Focus::Files && !open;
        let grid = crate::file_panel::list(files, list, preedit, focused, config);
        (
            grid,
            open.then(|| crate::file_panel::preview(files, preview, config)),
        )
    }

    /// A left press or the wheel on the file panel; the open preview popup takes all of them.
    fn file_pointer(&mut self, kind: PointerKind, column: u16, row: u16) {
        let (list, preview) = self.file_rects;
        let files = self.files.as_mut().unwrap();
        let open = files.focus() == FileFocus::Preview;
        match kind {
            PointerKind::Press(LEFT) if open => {
                if !preview.contains(column, row)
                    || crate::panel::popup_close(preview).contains(column, row)
                {
                    files.focus_list();
                }
            }
            PointerKind::Press(LEFT) => {
                let layout = crate::file_panel::Layout::new(list, files);
                if let Some(index) = layout.entry_at(files, column, row) {
                    files.move_selection(index as isize - files.selection() as isize);
                    files.focus_preview();
                }
            }
            PointerKind::Wheel(lines) => {
                let rows = crate::panel::popup_body(preview).rows;
                files.scroll(-(lines * WHEEL_LINES) as isize, usize::from(rows));
            }
            _ => (),
        }
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

    fn monitor_drag(&mut self) {
        self.resources.dragging = true;
        self.focus_monitor();
    }

    fn monitor_action(
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
        client: &mut Client,
        target: PasteTarget,
        text: &str,
    ) -> io::Result<()> {
        match target {
            PasteTarget::Search => self.search.edit(client, text)?,
            PasteTarget::Monitor => self.resources.insert_query(text),
            PasteTarget::Files => self.files.as_mut().unwrap().insert_query(text),
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
        .map(|fd| crate::sys::watch(fd, libc::POLLIN))
        .into_iter()
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

    /// Polls everything the frontend's event loop waits on besides the workspace; returns
    /// whether to redraw.
    pub fn service(
        &mut self,
        client: &mut Client,
        config: &Config,
        fds: &[libc::pollfd],
    ) -> io::Result<bool> {
        let agent = self.poll_agent(client, config)?;
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
            changed |= files.poll()?;
        }
        Ok(changed)
    }

    /// Whether `tab` may close at once; otherwise this asks for confirmation first.
    pub fn request_close_tab(&mut self, tab: &TabView) -> bool {
        let busy = matches!(tab.activity, TabActivity::Running | TabActivity::Unknown);
        if busy {
            self.close_prompt = Some(ClosePrompt {
                tab: Some(tab.id),
                terminate: false,
            });
        }
        !busy
    }

    pub fn message(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now() + Duration::from_secs(4)));
    }

    /// The status message, else the directory and its Git branch, and the columns it has left of
    /// the `footer` counters.
    pub fn footer_label<'a>(&'a self, client: &'a Client, footer: &View) -> (Cow<'a, str>, u16) {
        let left = footer.texts.iter().map(|text| text.x).min();
        let columns = left.unwrap_or(0).saturating_sub(2);
        if let Some((message, _)) = &self.status {
            return (Cow::Borrowed(message), columns);
        }
        let Some(snapshot) = client.snapshot() else {
            return (Cow::Borrowed(""), columns);
        };
        let remote = client.is_remote();
        let directory = if self.files_open && self.focus == Focus::Files {
            self.files.as_ref().map(FileNav::directory)
        } else {
            snapshot.focused_pane().and_then(|pane| pane.cwd.as_deref())
        };
        let Some(directory) = directory else {
            return (Cow::Borrowed(""), columns);
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
        let label = match self.resources.stats.branch().filter(|_| !remote) {
            Some(branch) => Cow::Owned([path.as_ref(), " 󱐋 ", branch].concat()),
            None => path,
        };
        (label, columns)
    }

    /// Applies a daemon response both frontends handle alike; the others come back.
    pub fn respond(
        &mut self,
        client: &mut Client,
        response: Response,
    ) -> io::Result<Option<Response>> {
        match response {
            Response::ScreenDirty => {
                self.search.observe(client)?;
                return Ok(Some(response));
            }
            Response::Error(error) => self.message(error),
            Response::Search {
                pane,
                request,
                count,
                truncated,
            } => {
                let count = count as usize;
                self.search
                    .result(client, pane, request, count, truncated)?;
            }
            response => return Ok(Some(response)),
        }
        Ok(None)
    }

    /// The close dialog, if open, centered in a window of `columns` by `rows` cells.
    pub fn close_dialog(
        &mut self,
        columns: u16,
        rows: u16,
        name: &str,
        pointer: Option<(u16, u16)>,
        config: &Config,
    ) -> Option<Grid> {
        let prompt = self.close_prompt?;
        let cols = columns.saturating_sub(4).min(64);
        let height = rows.saturating_sub(2).min(7);
        let rect = rect((columns - cols) / 2, (rows - height) / 2, cols, height);
        self.close_rect = rect;
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
        let buttons = close_buttons(rect, prompt.tab.is_some());
        for ((button, label), terminate) in buttons.into_iter().zip([false, true]) {
            let lit = prompt.terminate == terminate
                || pointer.is_some_and(|(column, row)| button.contains(column, row));
            let color = match (terminate, lit) {
                (true, true) => DANGER,
                (false, true) => ACCENT,
                _ => muted,
            };
            let fill = if lit {
                mix_color(config.foreground, config.background, 24)
            } else {
                background
            };
            grid.text(button.x, button.y, button.cols, label, color, fill);
        }
        Some(grid)
    }

    /// Lays out the tab bar for clicks and has `draw` draw its parts, lit under `pointer`.
    pub fn paint_tabs(
        &mut self,
        snapshot: &Snapshot,
        columns: u16,
        status: &str,
        pointer: Option<u16>,
        config: &Config,
        mut draw: impl FnMut(TabPart<'_>),
    ) {
        let bar = tab_bar(snapshot, columns, status);
        let hit = pointer.and_then(|column| bar.at(snapshot, column));
        let row = tab_row_color(config);
        let (prompt, label) = if snapshot.controller {
            (MUTED, 0xb8b8b8)
        } else {
            (WARNING, WARNING)
        };
        let text = |x, width, text, color, background| TabPart::Text {
            x,
            width,
            text,
            color,
            background,
        };
        let underline = |x, width, color| TabPart::Underline { x, width, color };
        draw(text(1, 1, "›", prompt, row));
        for &(index, x, width) in &bar.tabs {
            let tab = &snapshot.tabs[index];
            let active = tab.id == snapshot.active_tab;
            let close = hit == Some(TabHit::Close(tab.id));
            let color = if active {
                config.background
            } else {
                mix_color(config.background, 0, 204)
            };
            let title = if active || close || hit == Some(TabHit::Select(index)) {
                config.foreground
            } else {
                MUTED
            };
            draw(TabPart::Tab { x, width, color });
            draw(text(x + 2, 1, "•", tab_color(tab), color));
            draw(text(x + 4, width - 8, &tab.title, title, color));
            let close_color = if close { DANGER } else { MUTED };
            draw(text(x + width - 3, 1, "×", close_color, color));
            if close {
                draw(underline(x + width - 3, 1, DANGER));
            }
            if active && config.tab_style == TabStyle::Flat {
                draw(underline(x, width, ACCENT));
            }
        }
        if let Some(plus) = bar.plus {
            let hovered = hit == Some(TabHit::New);
            let color = if hovered { ACCENT } else { 0x606060 };
            draw(text(plus + 1, 1, "+", color, row));
            if hovered {
                draw(underline(plus + 1, 1, color));
            }
        }
        if let Some(TabHit::Scroll(direction)) = hit
            && let Some((x, width)) = bar.overflow[usize::from(direction > 0)]
        {
            draw(underline(x, width, INK));
        }
        let width = text_width(&bar.label) as u16;
        draw(text(bar.label_x, width, &bar.label, label, row));
        self.tabs = Some(bar);
    }

    fn monitor_jump(&mut self, client: &mut Client) -> io::Result<()> {
        if let Some(pane) = self.resources.jump.take() {
            let snapshot = client.snapshot().unwrap();
            let Some(target) = snapshot.pane(pane).map(|pane| pane.tab) else {
                self.message("Task pane closed");
                return Ok(());
            };
            let index = |id| snapshot.tabs.iter().position(|tab| tab.id == id).unwrap();
            let (current, target) = (index(snapshot.active_tab), index(target));
            if target != current {
                client.send(Action::Tab(target as i32 - current as i32))?;
            }
            client.send(Action::Focus(pane))?;
            self.focus = Focus::Terminal;
        }
        Ok(())
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
    /// A search glyph and the text with a caret at the cursor, after `preedit`, scrolled to keep
    /// the caret within `columns`; returns it and the caret's column.
    pub fn label(&self, preedit: &str, columns: usize) -> (String, usize) {
        let before = [&self.text[..self.cursor], preedit].concat();
        let budget = columns.saturating_sub(3);
        let mut width = 0;
        let kept: usize = before
            .chars()
            .rev()
            .take_while(|&ch| {
                let next = width + usize::from(scalar_width(ch));
                let fits = next <= budget;
                if fits {
                    width = next;
                }
                fits
            })
            .map(char::len_utf8)
            .sum();
        let after = fit_label(&self.text[self.cursor..], (budget - width) as u16);
        let label = format!("⌕ {}▏{after}", &before[before.len() - kept..]);
        (label, 2 + width)
    }

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
        let letter = symbol_char(symbol).filter(|_| control);
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
        *self = Self {
            open: true,
            pane,
            controller: client.controller(),
            request: self.request,
            ..Self::default()
        };
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
        if !std::mem::take(&mut self.open) {
            return Ok(());
        }
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
        self.select(client)
    }
    fn select(&self, client: &mut Client) -> io::Result<()> {
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
            self.select(client)?;
        }
        Ok(true)
    }
    fn key(
        &mut self,
        client: &mut Client,
        symbol: u32,
        shift: bool,
        control: bool,
    ) -> io::Result<()> {
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
            format!("{}/{}", (self.index + 1).min(self.count), self.count)
        };
        if self.truncated {
            status.push('+');
        }
        if !self.controller {
            status.push_str(if columns >= 40 { " read-only" } else { " RO" });
        }
        let status = fit_label(&status, (columns.saturating_sub(5).min(columns / 2)) as u16);
        let status_width = text_width(&status);
        let (field, caret) = self
            .field
            .label(preedit, columns.saturating_sub(status_width + 2));
        let padding = columns.saturating_sub(text_width(&field) + status_width);
        let label =
            fit_label(&format!("{field}{:padding$}{status}", ""), columns as u16).into_owned();
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
            let key = Key {
                symbol,
                modifiers: 0,
                text,
            };
            assert!(matches!(
                ui.key(&mut client, key).unwrap(),
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
            PasteTarget::Agent(ui.agent.active().id),
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
    fn tab_bar_keeps_the_active_tab_and_status_visible_and_hits_its_geometry() {
        let mut snapshot = Snapshot {
            name: "tabs".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: (1..=12)
                .map(|id| TabView {
                    id,
                    title: format!("long terminal title {id}"),
                    activity: TabActivity::Idle,
                })
                .collect(),
            panes: Vec::new(),
        };
        for active in [1, 6, 12] {
            snapshot.active_tab = active;
            let bar = tab_bar(&snapshot, 80, "SSH");
            let &(index, x, width) = (bar.tabs.iter())
                .find(|tab| snapshot.tabs[tab.0].id == active)
                .unwrap();
            assert_eq!(bar.at(&snapshot, x + 5), Some(TabHit::Select(index)));
            assert_eq!(
                bar.at(&snapshot, x + width - 3),
                Some(TabHit::Close(active))
            );
            assert_eq!(bar.at(&snapshot, bar.plus.unwrap() + 1), Some(TabHit::New));
            let after = snapshot.tabs.len() - 1 - bar.tabs.last().unwrap().0;
            for ((hidden, direction), hit) in [(bar.tabs[0].0, -1), (after, 1)]
                .into_iter()
                .zip(bar.overflow)
            {
                assert_eq!(hit.is_some(), hidden > 0);
                if let Some((column, _)) = hit {
                    assert_eq!(bar.at(&snapshot, column), Some(TabHit::Scroll(direction)));
                }
            }
            assert!(bar.label.ends_with("SSH") && bar.plus.unwrap() + 3 <= bar.label_x);
        }
        snapshot.tabs.truncate(1);
        snapshot.controller = false;
        let bar = tab_bar(&snapshot, 40, "Read-only  CPU 100%  MEM 100%  DSK 100%");
        assert!(bar.label.starts_with("Read-only"), "{}", bar.label);
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
}
