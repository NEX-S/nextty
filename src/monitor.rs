use crate::input::keysym;
use crate::panel::{ACCENT as GREEN, INK};
use crate::system_stats::{MIB, Process, ResourceTrend, SystemStats, dash, percent};
use crate::ui::TextField;
use crate::width::{fit_label, scalar_width, text_width};
use crate::workspace::{PaneId, Snapshot};
use std::{io, time::Instant};

pub(crate) use crate::panel::{BACKGROUND, MUTED};
const RANGES: [(u32, &str); 4] = [(60, "60s"), (300, "5m"), (900, "15m"), (3600, "1h")];

pub(crate) fn series_color(name: &str) -> u32 {
    match name {
        "MEMORY" => 0x8fb5c7,
        "SWAP" | "WRITE" | "TX" | "OUT" => 0xb3a2ca,
        _ => GREEN,
    }
}

fn trend_value(value: Option<f32>, unit: &str) -> String {
    dash(value, |v| {
        if unit == "MiB/s" && v < 1. {
            format!("{:.0}KiB/s", v * 1024.)
        } else {
            format!("{v:.1}{unit}")
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Close,
    Tab(u8),
    Range(u32),
    Sort(u8),
    Search,
    Process(u32),
    Task(PaneId),
}

pub(crate) struct Text {
    pub x: u16,
    pub y: i32,
    pub width: u16,
    pub text: String,
    pub color: u32,
    emphasis: Emphasis,
    focused: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Emphasis {
    None,
    Selected,
    Highlighted,
}

impl Text {
    fn emphasis(&self, monitor_focused: bool) -> Emphasis {
        match self.emphasis {
            Emphasis::None if self.focused && monitor_focused => Emphasis::Highlighted,
            emphasis => emphasis,
        }
    }

    /// The background that marks this text, if any.
    pub fn background(&self, monitor_focused: bool) -> Option<u32> {
        match self.emphasis(monitor_focused) {
            Emphasis::Selected => Some(crate::panel::SELECTED),
            Emphasis::Highlighted => Some(crate::panel::HOVER),
            Emphasis::None => None,
        }
    }
}
pub(crate) struct Plot {
    pub x: u16,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub maximum: f32,
    pub series: Vec<ResourceTrend>,
}
struct Hit {
    x: u16,
    y: i32,
    width: u16,
    action: Action,
}
#[derive(Default)]
pub(crate) struct View {
    pub texts: Vec<Text>,
    pub plots: Vec<Plot>,
    hits: Vec<Hit>,
    pub body_x: u16,
    focus: Option<Action>,
}
fn hit_at(hits: &[Hit], x: u16, y: u16) -> Option<&Hit> {
    let y = i32::from(y);
    hits.iter()
        .rfind(|hit| hit.y == y && (hit.x..hit.x + hit.width).contains(&x))
}

impl View {
    /// Lights the control under `pointer`.
    fn hover(&mut self, pointer: Option<(u16, u16)>) {
        let Some(hit) = pointer.and_then(|(x, y)| hit_at(&self.hits, x, y)) else {
            return;
        };
        let (x, y, width) = (hit.x, hit.y, hit.width);
        let process = matches!(hit.action, Action::Process(_));
        if let Some(text) = self
            .texts
            .iter_mut()
            .find(|text| text.x == x && text.y == y && text.width == width)
        {
            if text.emphasis != Emphasis::Selected {
                text.color = INK;
            }
            text.emphasis = if process {
                Emphasis::Selected
            } else {
                Emphasis::Highlighted
            };
        }
    }

    fn text(&mut self, x: u16, y: i32, width: u16, text: impl Into<String>, color: u32) {
        self.texts.push(Text {
            x,
            y,
            width,
            text: text.into(),
            color,
            emphasis: Emphasis::None,
            focused: false,
        });
    }
    fn button(&mut self, x: u16, y: i32, width: u16, text: &str, action: Action, selected: bool) {
        let label = [" ", fit_label(text, width.saturating_sub(2)).as_ref(), " "].concat();
        self.texts.push(Text {
            x,
            y,
            width,
            text: label,
            color: if selected { GREEN } else { MUTED },
            emphasis: if selected {
                Emphasis::Highlighted
            } else {
                Emphasis::None
            },
            focused: self.focus == Some(action),
        });
        self.hits.push(Hit {
            x,
            y,
            width,
            action,
        });
    }
    /// Character-wraps `text` into `width` columns and returns the next free row.
    fn lines(&mut self, x: u16, mut y: i32, width: u16, text: &str, color: u32) -> i32 {
        let mut line = String::new();
        let mut columns = 0;
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            let advance = u16::from(scalar_width(ch));
            if columns + advance > width && !line.is_empty() {
                self.text(x, y, width, std::mem::take(&mut line), color);
                y += 1;
                columns = 0;
            }
            line.push(ch);
            columns += advance;
        }
        if !line.is_empty() {
            self.text(x, y, width, line, color);
            y += 1;
        }
        y
    }
    fn details(&mut self, x: u16, mut y: i32, width: u16, rows: &[(&str, String)]) -> i32 {
        let key_width = (width / 2).min(24);
        let value_width = width.saturating_sub(key_width);
        for (key, value) in rows {
            if text_width(value) > usize::from(value_width)
                || text_width(key) >= usize::from(key_width)
            {
                y = self.lines(x, y, width, key, MUTED);
                y = self.lines(x, y, width, value, INK);
            } else {
                self.text(x, y, key_width - 1, *key, MUTED);
                self.text(x + key_width, y, value_width, value, INK);
                y += 1;
            }
        }
        y
    }
}

struct Task {
    pane: PaneId,
    tab: String,
    command: String,
    directory: String,
    state: &'static str,
    exit: Option<i32>,
    command_id: Option<u64>,
    elapsed_ms: Option<u64>,
    sampled: Instant,
}

pub(crate) struct Monitor {
    pub stats: SystemStats,
    pub jump: Option<PaneId>,
    tasks: Vec<Task>,
    pub open: bool,
    pub height: u16,
    focus: Action,
    pub searching: bool,
    selecting_control: bool,
    pub dragging: bool,
    tab: u8,
    pub scroll: usize,
    pub range: u32,
    sort: u8,
    descending: bool,
    query: TextField,
    selected: Option<(u32, u64)>,
    process_order: Vec<usize>,
    process_revision: Option<u64>,
    /// The focused or selected process and its row, which re-sorting keeps in place.
    anchor: Option<(u32, i32)>,
    reveal: bool,
    width: u16,
    /// The controls of the last drawn body and footer.
    hits: [Vec<Hit>; 2],
}
impl Monitor {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            stats: SystemStats::new()?,
            jump: None,
            tasks: Vec::new(),
            open: false,
            height: 18,
            focus: Action::Tab(0),
            searching: false,
            selecting_control: true,
            dragging: false,
            tab: 0,
            scroll: 0,
            range: 60,
            sort: 0,
            descending: true,
            query: TextField::default(),
            selected: None,
            process_order: Vec::new(),
            process_revision: None,
            anchor: None,
            reveal: false,
            width: 80,
            hits: Default::default(),
        })
    }

    /// The control at `x`, `y` of the last drawn body, or of the footer.
    pub fn hit(&self, footer: bool, x: u16, y: u16) -> Option<Action> {
        hit_at(&self.hits[usize::from(footer)], x, y).map(|hit| hit.action)
    }
    pub fn set_workspace(&mut self, snapshot: &Snapshot) {
        let previous = std::mem::take(&mut self.tasks);
        for pane in &snapshot.panes {
            let block = pane
                .command_blocks
                .iter()
                .rev()
                .find(|block| block.has_output || block.status.is_some());
            let command_id = block.map(|block| block.id);
            let elapsed_ms = block.and_then(|block| block.elapsed_ms);
            let exit = block.and_then(|block| block.status);
            let running = block.is_some_and(|block| !block.complete && block.status.is_none())
                && pane.shell_prompt.is_none();
            let state = match exit {
                Some(0) => "Completed",
                Some(_) => "Failed",
                None if running => "Running",
                None if pane.shell_prompt.is_some() => "Idle",
                None => "Unknown",
            };
            // The elapsed time of a running command counts from when it was last observed.
            let sampled = previous
                .iter()
                .find(|task| {
                    (task.pane, task.command_id, task.elapsed_ms)
                        == (pane.id, command_id, elapsed_ms)
                })
                .map_or_else(Instant::now, |task| task.sampled);
            let command = match (&pane.process.foreground_argv, block) {
                (Some(args), _) if running || block.is_none() => args.join(" "),
                (_, Some(block)) => format!("Command #{}", block.id),
                _ => pane.title.clone(),
            };
            self.tasks.push(Task {
                pane: pane.id,
                tab: snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.id == pane.tab)
                    .map_or_else(|| pane.tab.to_string(), |tab| tab.title.clone()),
                command,
                directory: pane.cwd.as_deref().map_or_else(
                    || "unavailable".into(),
                    |cwd| cwd.to_string_lossy().into_owned(),
                ),
                state,
                exit,
                command_id,
                elapsed_ms,
                sampled,
            });
        }
        if matches!(self.focus, Action::Task(pane) if !self.tasks.iter().any(|task| task.pane == pane))
        {
            self.focus = Action::Tab(7);
        }
    }

    pub fn footer(
        &mut self,
        width: u16,
        controller: bool,
        remote: bool,
        pointer: Option<u16>,
    ) -> View {
        let mut view = View {
            focus: self.open.then_some(self.focus),
            ..View::default()
        };
        let arrow = width.saturating_sub(4);
        let toggle = if self.open {
            Action::Close
        } else {
            Action::Tab(self.tab)
        };
        let icon = crate::panel::expand_icon(self.open);
        view.button(arrow, 0, 3, icon, toggle, false);
        if self.open {
            if width >= 25 {
                let mut x = width - 25;
                for (seconds, label) in RANGES {
                    let cells = label.len() as u16 + 2;
                    let selected = self.range == seconds;
                    view.button(x, 0, cells, label, Action::Range(seconds), selected);
                    x += cells + 1;
                }
            }
        } else {
            let mut x = arrow;
            for (label, tab) in self.stats.summary.iter().zip([1, 2, 3, 5]).rev() {
                let cells = text_width(label) as u16 + 2;
                if cells > x {
                    break;
                }
                x -= cells;
                view.button(x, 0, cells, label, Action::Tab(tab), false);
            }
            let prefix = match (controller, remote) {
                (true, false) => "",
                (true, true) => "Local",
                (false, false) => "Read-only",
                (false, true) => "Read-only · Local",
            };
            let cells = text_width(prefix) as u16 + 1;
            if !prefix.is_empty() && cells <= x {
                view.text(x - cells, 0, cells, prefix, MUTED);
            }
        }
        view.hover(pointer.map(|x| (x, 0)));
        self.hits[1] = std::mem::take(&mut view.hits);
        view
    }

    pub fn insert_query(&mut self, text: &str) {
        if self.query.insert(text) {
            self.query_changed();
        }
    }
    fn query_changed(&mut self) {
        self.process_revision = None;
        self.scroll = 0;
        self.anchor = None;
    }
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.searching = false;
        self.selecting_control = true;
        if self.open && self.focus == Action::Close {
            self.focus = Action::Tab(self.tab);
        }
        self.sync_stats();
    }
    pub fn resize(&mut self, rows: u16, total: u16) {
        self.height = rows.max(8).min(total.saturating_sub(6).max(1));
    }
    pub fn rows(&self, total: u16) -> u16 {
        if self.open {
            self.height.min(total.saturating_sub(6))
        } else {
            0
        }
    }
    pub fn action(&mut self, action: Action) {
        self.searching = false;
        self.focus = action;
        self.selecting_control = false;
        self.reveal = true;
        match action {
            Action::Task(pane) => {
                self.jump = Some(pane);
                self.open = false;
            }
            Action::Close => self.open = false,
            Action::Tab(tab) => {
                self.open = true;
                self.tab = tab;
                self.scroll = 0;
                self.anchor = None;
            }
            Action::Range(range) => self.range = range,
            Action::Sort(sort) => {
                self.descending = self.sort != sort || !self.descending;
                self.sort = sort;
                self.process_revision = None;
                self.anchor = None;
            }
            Action::Search => self.searching = true,
            Action::Process(pid) => {
                self.selected = if self.selected.is_some_and(|selected| selected.0 == pid) {
                    None
                } else {
                    let processes = &self.stats.sample.processes;
                    processes
                        .binary_search_by_key(&pid, |p| p.pid)
                        .ok()
                        .map(|index| (pid, processes[index].start))
                };
            }
        }
        self.sync_stats();
    }
    /// Scans processes and reads the selected one's details only while the process page is visible.
    fn sync_stats(&mut self) {
        let visible = self.open && self.tab == 6;
        if visible {
            self.rebuild_processes();
        }
        let identity = self
            .selected
            .filter(|&id| visible && self.processes().any(|p| (p.pid, p.start) == id));
        self.stats.set_sampling(visible, identity);
    }
    /// Filters and sorts the processes of a new sample, query or order.
    fn rebuild_processes(&mut self) {
        let revision = self.stats.process_revision;
        if self.process_revision == Some(revision) {
            return;
        }
        let processes = &self.stats.sample.processes;
        self.selected = self
            .selected
            .filter(|identity| processes.iter().any(|p| (p.pid, p.start) == *identity));
        let query = self.query.text.to_lowercase();
        let numeric = query.bytes().all(|byte| byte.is_ascii_digit());
        let id_matches = |id: u32| numeric && id.to_string().contains(&query);
        self.process_order = (0..processes.len())
            .filter(|&i| {
                let p = &processes[i];
                p.name.to_lowercase().contains(&query) || id_matches(p.pid) || id_matches(p.uid)
            })
            .collect();
        let value = |p: &Process| match self.sort {
            1 => p.rss as f64,
            2 => p.read.unwrap_or(0.) + p.write.unwrap_or(0.),
            _ => f64::from(p.cpu.unwrap_or(0.)),
        };
        self.process_order.sort_unstable_by(|&a, &b| {
            let (a, b) = (&processes[a], &processes[b]);
            let (low, high) = if self.descending { (b, a) } else { (a, b) };
            let order = value(low).total_cmp(&value(high));
            order.then_with(|| a.pid.cmp(&b.pid))
        });
        self.process_revision = Some(revision);
    }
    fn processes(&self) -> impl Clone + ExactSizeIterator<Item = &Process> {
        self.process_order
            .iter()
            .map(|&index| &self.stats.sample.processes[index])
    }
    /// The controls Tab and j/k cycle through; a list is one stop, at its focused row.
    fn controls(&mut self) -> Vec<Action> {
        let mut controls: Vec<_> = (0..8).map(Action::Tab).collect();
        if self.width >= 24 {
            controls.extend(RANGES.map(|(seconds, _)| Action::Range(seconds)));
        }
        if self.tab == 6 {
            controls.push(Action::Search);
            let (_, body_width) = body_span(self.width);
            if body_width >= 26 {
                let sorts = process_columns(body_width, self.sort).len() as u8 - 2;
                controls.extend((0..sorts).map(Action::Sort));
            }
            self.rebuild_processes();
            let rows = self.processes();
            let listed = |pid: &u32| rows.clone().any(|p| p.pid == *pid);
            let focused = match self.focus {
                Action::Process(pid) => Some(pid),
                _ => None,
            };
            let pid = focused
                .filter(listed)
                .or(self.selected.map(|selected| selected.0).filter(listed))
                .or_else(|| rows.clone().next().map(|p| p.pid));
            controls.extend(pid.map(Action::Process));
        }
        if self.tab == 7 {
            let pane = match self.focus {
                Action::Task(pane) => Some(pane),
                _ => self.tasks.first().map(|task| task.pane),
            };
            controls.extend(pane.map(Action::Task));
        }
        controls
    }
    fn cycle(&mut self, backwards: bool) {
        self.searching = false;
        self.selecting_control = true;
        let controls = self.controls();
        let current = controls
            .iter()
            .position(|&action| action == self.focus)
            .unwrap_or(0);
        let step = if backwards { controls.len() - 1 } else { 1 };
        self.focus = controls[(current + step) % controls.len()];
        self.reveal = true;
    }
    fn move_process(&mut self, delta: isize) {
        self.rebuild_processes();
        let (order, processes) = (&self.process_order, &self.stats.sample.processes);
        let current = order
            .iter()
            .position(|&i| Action::Process(processes[i].pid) == self.focus)
            .unwrap_or(0);
        let index = current
            .saturating_add_signed(delta)
            .min(order.len().saturating_sub(1));
        let Some((pid, start)) = order
            .get(index)
            .map(|&i| (processes[i].pid, processes[i].start))
        else {
            return;
        };
        self.focus = Action::Process(pid);
        if self.selected.is_some() {
            self.selected = Some((pid, start));
        }
        self.searching = false;
        self.reveal = true;
        self.sync_stats();
    }
    pub fn key(&mut self, symbol: u32, text: &str, control: bool) {
        if !self.open {
            return;
        }
        let delta: isize = match symbol {
            keysym::UP | 0x6b => -1,
            keysym::DOWN | 0x6a => 1,
            keysym::PAGE_UP => -6,
            keysym::PAGE_DOWN => 6,
            _ => 0,
        };
        match symbol {
            keysym::ESCAPE if self.searching => self.searching = false,
            keysym::ESCAPE => self.action(Action::Close),
            keysym::TAB | keysym::LEFT_TAB => self.cycle(symbol == keysym::LEFT_TAB),
            keysym::RETURN if self.searching => {
                self.searching = false;
                self.rebuild_processes();
                let first = self.processes().next().map(|p| p.pid);
                if let Some(pid) = first {
                    self.focus = Action::Process(pid);
                    self.reveal = true;
                }
            }
            _ if self.searching => {
                if self.query.key(symbol, control) {
                    self.query_changed();
                } else {
                    self.insert_query(text);
                }
            }
            0x68 => self.selecting_control = true,
            0x6a | 0x6b if self.selecting_control => self.cycle(symbol == 0x6b),
            0x6c | keysym::RETURN | 0x20 => self.action(self.focus),
            _ if delta != 0 => match self.focus {
                Action::Tab(tab) if delta.abs() == 1 => {
                    self.action(Action::Tab((tab + if delta > 0 { 1 } else { 7 }) % 8));
                }
                Action::Process(_) => self.move_process(delta),
                Action::Task(pane) => {
                    if let Some(index) = self.tasks.iter().position(|task| task.pane == pane) {
                        let index = index.saturating_add_signed(delta).min(self.tasks.len() - 1);
                        self.focus = Action::Task(self.tasks[index].pane);
                        self.reveal = true;
                    }
                }
                _ => self.scroll = self.scroll.saturating_add_signed(delta),
            },
            keysym::LEFT => self.focus = Action::Tab(self.tab),
            keysym::RIGHT if matches!(self.focus, Action::Tab(_)) => {
                self.focus = self
                    .controls()
                    .into_iter()
                    .find(|action| !matches!(action, Action::Tab(_)))
                    .unwrap_or(Action::Tab(self.tab));
                self.reveal = true;
            }
            keysym::RIGHT => self.cycle(false),
            0x2f if self.tab == 6 => self.action(Action::Search),
            _ => (),
        }
    }
    fn graph(
        &self,
        view: &mut View,
        x: u16,
        y: i32,
        width: u16,
        height: u16,
        mut series: Vec<ResourceTrend>,
    ) {
        if width < 10 {
            return;
        }
        let first = &series[0];
        let percent_unit = first.unit == "%";
        let maximum = if percent_unit {
            100.
        } else {
            let peak = series
                .iter()
                .flat_map(|trend| &trend.points)
                .filter_map(|(_, value)| *value)
                .fold(0.125, f32::max);
            peak.log2().ceil().exp2()
        };
        let title = match (series.len(), first.name) {
            (2, "RX") => first.summary.as_str(),
            (2, "READ") => "DISK I/O",
            (2, "IN") => "SWAP I/O",
            _ => first.name,
        };
        if series.len() == 1 {
            let value = trend_value(first.value, first.unit);
            let value_width = (text_width(&value) as u16).min(width - 5);
            let color = series_color(first.name);
            view.text(x, y, width - value_width - 1, title, INK);
            view.text(x + width - value_width, y, value_width, value, color);
            let summary = std::mem::take(&mut series[0].summary);
            view.text(x, y + 1, width, summary, MUTED);
        } else {
            view.text(x, y, width, title, INK);
            let columns = width / series.len() as u16;
            for (i, trend) in series.iter().enumerate() {
                let label = format!("{} {}", trend.name, trend_value(trend.value, trend.unit));
                let color = series_color(trend.name);
                view.text(x + i as u16 * columns, y + 1, columns, label, color);
            }
        }
        let scale = if percent_unit {
            "100%".into()
        } else if maximum < 1. {
            format!("{:.0}K", maximum * 1024.)
        } else {
            format!("{maximum:.0}M")
        };
        view.text(x, y + 2, 5, scale, MUTED);
        view.text(x, y + 1 + i32::from(height), 5, "0", MUTED);
        let (plot_x, plot_width, axis_y) = (x + 5, width - 5, y + i32::from(height) + 2);
        // Choose tick spacing from the actual plot width so labels never collide.
        let target = self
            .range
            .div_ceil(u32::from((plot_width / 10).clamp(1, 4)));
        let step = [1, 2, 5, 10, 15, 30, 60, 120, 180, 300, 600, 900]
            .into_iter()
            .find(|&n| n >= target)
            .unwrap_or(self.range);
        let ages = std::iter::once(self.range).chain((0..self.range).step_by(step as usize).rev());
        let mut end = 0;
        for age in ages.filter(|&age| age == 0 || plot_width >= 8) {
            let label = match age {
                0 => "now".into(),
                60.. => format!("−{}m", age / 60),
                _ => format!("−{age}s"),
            };
            let cells = (text_width(&label) as u16).min(plot_width);
            let column = (1. - age as f32 / self.range as f32) * f32::from(plot_width - 1);
            let column = (column as u16).min(plot_width - cells);
            if age == 0 || column >= end && column + cells + 4 < plot_width {
                view.text(plot_x + column, axis_y, cells, label, MUTED);
                end = column + cells + 2;
            }
        }
        if series
            .iter()
            .all(|trend| trend.points.iter().all(|(_, value)| value.is_none()))
        {
            let middle = y + 2 + i32::from(height / 2);
            view.text(plot_x, middle, plot_width, "Waiting for samples", MUTED);
        }
        view.plots.push(Plot {
            x: plot_x,
            y: y + 2,
            width: plot_width,
            height,
            maximum,
            series,
        });
    }
    fn follow_process(&mut self, hits: &[Hit]) {
        let pid = match self.focus {
            Action::Process(pid) => Some(pid),
            _ => self.selected.map(|selected| selected.0),
        };
        let anchor = pid.and_then(|pid| {
            let hit = hits.iter().find(|hit| hit.action == Action::Process(pid))?;
            Some((pid, hit.y))
        });
        if let (Some((pid, y)), Some((old_pid, old_y))) = (anchor, self.anchor)
            && pid == old_pid
        {
            self.scroll = self.scroll.saturating_add_signed((y - old_y) as isize);
        }
        self.anchor = anchor;
    }
    pub fn view(&mut self, width: u16, height: u16, pointer: Option<(u16, u16)>) -> View {
        let mut view = View {
            focus: Some(self.focus),
            ..View::default()
        };
        if width < 12 || height < 3 {
            self.hits[0].clear();
            return view;
        }
        self.width = width;
        if self.tab == 6 {
            self.rebuild_processes();
        }
        let (x, w) = body_span(width);
        view.body_x = x;
        let labels = [
            ("Overview", "System"),
            ("CPU", "CPU"),
            ("Memory", "Mem"),
            ("Disk", "Disk"),
            ("Network", "Net"),
            ("Swap", "Swap"),
            ("Processes", "Procs"),
            ("Tasks", "Tasks"),
        ];
        let spacing = if height >= 13 { 2 } else { 1 };
        let first = usize::from(self.tab).saturating_sub(usize::from((height - 1) / spacing));
        for (i, (long, short)) in labels.into_iter().enumerate().skip(first) {
            let (tab, y) = (i as u8, (i - first) as u16 * spacing);
            if y < height {
                let label = if width >= 80 { long } else { short };
                let selected = self.tab == tab;
                view.button(1, i32::from(y), x - 2, label, Action::Tab(tab), selected);
                if selected {
                    view.text(0, i32::from(y), 1, "▎", GREEN);
                }
            }
        }
        let mut body = View {
            focus: view.focus,
            ..View::default()
        };
        // Wide windows put the details beside the graph, narrow ones below it.
        let graph_width = if w >= 72 { w * 3 / 5 } else { w };
        let side = graph_width < w;
        let (dx, dw) = if side {
            (x + graph_width + 3, w - graph_width - 3)
        } else {
            (x, w)
        };
        let trends = |indices| self.stats.system_trends(self.range, indices);
        let sample = &self.stats.sample;
        match self.tab {
            0 => {
                let columns = if w >= 54 { 2 } else { 1 };
                let cw = (w - (columns - 1) * 2) / columns;
                for (i, series) in [0..1, 1..2, 2..4, 4..5].into_iter().enumerate() {
                    let (column, row) = (i as u16 % columns, i as u16 / columns);
                    let gx = x + column * (cw + 2);
                    self.graph(&mut body, gx, i32::from(row * 7), cw, 3, trends(series));
                }
                body.details(x, i32::from(4 / columns * 7), w, &sample.overview);
            }
            1 => {
                self.graph(&mut body, x, 0, graph_width, 4, trends(0..1));
                let y = if side {
                    body.details(dx, 0, dw, &sample.cpu_details).max(7)
                } else {
                    7
                };
                let columns = (w / 24).clamp(1, 4);
                let cw = w / columns;
                let cores = &sample.cpu_cores;
                for (i, (name, value)) in cores.iter().enumerate() {
                    let column = x + (i as u16 % columns) * cw;
                    let row = y + 1 + i as i32 / i32::from(columns);
                    let label = match value {
                        Some(value) => format!("{name:<6} {value:>3.0}%"),
                        None => format!("{name:<6}    —"),
                    };
                    body.text(column, row, 11.min(cw), label, INK);
                    let bar_width = cw.saturating_sub(13).min(20);
                    let filled =
                        value.map_or(0, |v| (v * f32::from(bar_width) / 100.).round() as u16);
                    if bar_width > 0 {
                        let bar = |width: u16, glyph: &str| glyph.repeat(usize::from(width));
                        body.text(column + 12, row, bar_width, bar(bar_width, "─"), 0x46524a);
                        if filled > 0 {
                            body.text(column + 12, row, filled, bar(filled, "━"), GREEN);
                        }
                    }
                }
                if !side {
                    let bottom = y + 2 + cores.len().div_ceil(usize::from(columns)) as i32;
                    body.details(x, bottom, w, &sample.cpu_details);
                }
            }
            2 => {
                self.graph(&mut body, x, 0, graph_width, 5, trends(1..2));
                let y = if side { 0 } else { 9 };
                let y = body.details(dx, y, dw, &sample.memory_details);
                body.lines(dx, y + 1, dw, "Available includes reclaimable cache", MUTED);
            }
            3 => {
                self.graph(&mut body, x, 0, graph_width, 4, trends(2..4));
                let mut y = if side { 0 } else { 8 };
                if sample.disks.is_empty() {
                    body.text(dx, y, dw, "No physical disks", MUTED);
                    y += 2;
                }
                for disk in &sample.disks {
                    body.text(dx, y, dw, &disk.name, INK);
                    let [read, write] = disk.rates.map(|rate| trend_value(rate, "MiB/s"));
                    let rows = [
                        ("Read", read),
                        ("Write", write),
                        ("Busy", percent(disk.utilization)),
                    ];
                    y = body.details(dx, y + 1, dw, &rows) + 1;
                }
                body.details(x, y.max(8), w, &self.stats.disk_details());
            }
            4 => {
                if sample.networks.is_empty() {
                    body.text(x, 0, w, "No physical network interfaces", MUTED);
                }
                let graph_height = height.saturating_sub(6).clamp(3, 4);
                let below = i32::from(graph_height) + 4;
                let mut y = 0;
                for network in &sample.networks {
                    let trends = self.stats.network_trends(self.range, network);
                    self.graph(&mut body, x, y, graph_width, graph_height, trends);
                    let end = if side {
                        body.details(dx, y, dw, &network.details())
                    } else {
                        body.details(x, y + below, w, &network.details())
                    };
                    y = end.max(y + below - 1) + 2;
                }
            }
            5 => {
                if self.stats.swap_disabled() {
                    body.text(x, 0, w, "Swap is disabled", MUTED);
                } else {
                    let columns = if w >= 64 { 2 } else { 1 };
                    let cw = (w - (columns - 1) * 3) / columns;
                    self.graph(&mut body, x, 0, cw, 4, trends(4..5));
                    let (rx, ry) = if columns == 2 {
                        (x + cw + 3, 0)
                    } else {
                        (x, 8)
                    };
                    self.graph(&mut body, rx, ry, cw, 4, trends(5..7));
                }
            }
            6 => self.process_view(&mut body, x, w),
            _ => {
                body.text(x, 0, w, "Terminal tasks · Enter jumps to pane", INK);
                body.text(x, 1, w, "Latest observed command per pane", MUTED);
                if self.tasks.is_empty() {
                    body.text(x, 3, w, "No terminal panes", MUTED);
                }
                for (index, task) in self.tasks.iter().enumerate() {
                    let y = 3 + index as i32 * 4;
                    let action = Action::Task(task.pane);
                    let label = format!("{} · {}", task.state, task.command);
                    body.button(x, y, w, &label, action, self.focus == action);
                    let running = match task.state {
                        "Running" => task.sampled.elapsed().as_millis() as u64,
                        _ => 0,
                    };
                    let elapsed = task.elapsed_ms.map_or_else(
                        || "elapsed —".into(),
                        |elapsed| format!("{:.1}s", (elapsed + running) as f64 / 1000.0),
                    );
                    let exit = dash(task.exit, |status| status.to_string());
                    let details = format!(
                        "Tab {} · Pane {} · exit {exit} · {elapsed}",
                        task.tab, task.pane
                    );
                    body.text(x + 1, y + 1, w - 1, details, MUTED);
                    body.text(x + 1, y + 2, w - 1, &task.directory, MUTED);
                }
            }
        }
        let content_height = body
            .texts
            .iter()
            .map(|t| t.y + 1)
            .chain(body.hits.iter().map(|hit| hit.y + 1))
            .fold(0, i32::max);
        // A focused process that exited or no longer matches gives the focus to the filter.
        if let Action::Process(pid) = self.focus
            && !body.hits.iter().any(|hit| hit.action == self.focus)
        {
            self.focus = Action::Search;
            if self.selected.is_some_and(|selected| selected.0 == pid) {
                self.selected = None;
                self.sync_stats();
            }
            self.reveal = true;
        }
        self.follow_process(&body.hits);
        // Rows above `pinned` stay put; the rest scroll and are clipped below the pinned rows.
        let pinned = if self.tab == 6 && height > 5 { 4 } else { 0 };
        if std::mem::take(&mut self.reveal)
            && let Some(hit) = body.hits.iter().find(|hit| hit.action == self.focus)
        {
            let expanded = self
                .selected
                .is_some_and(|selected| hit.action == Action::Process(selected.0));
            let bottom = hit.y + if expanded { 4 } else { 1 };
            if hit.y >= pinned && hit.y < self.scroll as i32 + pinned {
                self.scroll = (hit.y - pinned).max(0) as usize;
            }
            if bottom > self.scroll as i32 + i32::from(height) {
                self.scroll = (bottom - i32::from(height)).max(0) as usize;
            }
        }
        self.scroll = self
            .scroll
            .min((content_height - i32::from(height)).max(0) as usize);
        let offset = -(self.scroll as i32);
        let place = |y: i32| {
            let fixed = y < pinned;
            let y = if fixed { y } else { y + offset };
            (y >= if fixed { 0 } else { pinned } && y < i32::from(height)).then_some(y)
        };
        view.texts.extend(body.texts.into_iter().filter_map(|text| {
            Some(Text {
                y: place(text.y)?,
                ..text
            })
        }));
        for mut plot in body.plots {
            plot.y += offset;
            if plot.y < i32::from(height) && plot.y + i32::from(plot.height) > 0 {
                view.plots.push(plot);
            }
        }
        view.hits.extend(body.hits.into_iter().filter_map(|hit| {
            Some(Hit {
                y: place(hit.y)?,
                ..hit
            })
        }));
        if self.scroll > 0 {
            view.text(width - 1, pinned, 1, "↑", MUTED);
        }
        if content_height > self.scroll as i32 + i32::from(height) {
            view.text(width - 1, i32::from(height - 1), 1, "↓", MUTED);
        }
        // Every process hit is kept for focus anchoring; text is built only for visible rows.
        if self.tab == 6 {
            let columns = process_columns(w, self.sort);
            let processes = &self.stats.sample.processes;
            for hit in &view.hits {
                if let Action::Process(pid) = hit.action {
                    let process =
                        &processes[processes.binary_search_by_key(&pid, |p| p.pid).unwrap()];
                    let selected = self.selected.is_some_and(|selected| selected.0 == pid);
                    view.texts.push(Text {
                        x: hit.x,
                        y: hit.y,
                        width: hit.width,
                        text: process_row(process, &columns),
                        color: if selected { GREEN } else { INK },
                        emphasis: if selected {
                            Emphasis::Selected
                        } else {
                            Emphasis::None
                        },
                        focused: self.focus == hit.action,
                    });
                }
            }
        }
        view.hover(pointer);
        self.hits[0] = std::mem::take(&mut view.hits);
        view
    }
    fn process_view(&self, body: &mut View, x: u16, width: u16) {
        let filter = if self.searching {
            self.query.label("", usize::from(width.saturating_sub(2))).0
        } else {
            format!("⌕ {}  / to search", self.query.text)
        };
        body.button(x, 0, width, &filter, Action::Search, self.searching);
        let legend = "CPU: 1 core = 100% · RSS MiB · I/O MiB/s";
        body.text(x, 1, width, legend, MUTED);
        if width < 26 {
            body.text(x, 3, width, "Widen for process list", MUTED);
            return;
        }
        let columns = process_columns(width, self.sort);
        let mut column = x + 1;
        for (id, &(label, cells)) in columns.iter().enumerate() {
            if id < 2 {
                let label = if id == 1 && cells < 7 { "NAME" } else { label };
                body.text(column, 3, cells, label, MUTED);
            } else {
                let sort = id as u8 - 2;
                let label = match (self.sort == sort, self.descending) {
                    (false, _) => label.to_owned(),
                    (true, true) => format!("{label}▾"),
                    (true, false) => format!("{label}▴"),
                };
                let cells = (text_width(&label) as u16).min(cells) + 2;
                let selected = self.sort == sort;
                body.button(column - 1, 3, cells, &label, Action::Sort(sort), selected);
            }
            column += cells + 1;
        }
        let processes = self.processes();
        if processes.len() == 0 {
            body.text(x, 4, width, "No matches · waiting for samples", MUTED);
            return;
        }
        let mut y = 4;
        for p in processes {
            body.hits.push(Hit {
                x,
                y,
                width,
                action: Action::Process(p.pid),
            });
            y += 1;
            if self.selected != Some((p.pid, p.start)) {
                continue;
            }
            let (x, inner) = (x + 1, width - 2);
            let title = format!("Selected process · {}", p.name);
            y = body.lines(x, y, inner, &title, GREEN);
            let cpu = dash(p.cpu, |cpu| format!("{cpu:.1}%"));
            let directory = p.directory.as_deref().map_or_else(
                || "unavailable".into(),
                |directory| directory.to_string_lossy().into_owned(),
            );
            let rows = [
                (
                    "CPU / RSS",
                    format!("{cpu} / {:.1} MiB", p.rss as f64 / MIB),
                ),
                (
                    "PID / parent / user",
                    format!("{} / {} / {}", p.pid, p.parent, p.uid),
                ),
                ("State / threads", format!("{} / {}", p.state, p.threads)),
                ("Directory", directory),
                ("Command", p.command.clone()),
            ];
            y = body.details(x, y, inner, &rows);
            if columns.len() < 5 {
                let io = |rate: Option<f64>| dash(rate, |rate| format!("{:.2}", rate / MIB));
                let text = format!("I/O read/write: {} / {} MiB/s", io(p.read), io(p.write));
                y = body.lines(x, y, inner, &text, MUTED);
            }
            y += 1;
        }
    }
}

/// Column and width of the page body, right of the tab labels.
fn body_span(width: u16) -> (u16, u16) {
    let x = if width >= 80 { 14 } else { 10 };
    (x, width.saturating_sub(x + 1))
}

/// Visible columns as (label, cells) in the order PID, COMMAND, CPU%, RSS, I/O; the last two are optional.
fn process_columns(width: u16, sort: u8) -> Vec<(&'static str, u16)> {
    let inner = width.saturating_sub(2);
    let show_rss = inner >= 32;
    let show_io = inner >= 46 && (width >= 70 || sort == 2);
    let name_width = inner
        .saturating_sub(7 + 6 + 2 + if show_rss { 9 } else { 0 } + if show_io { 10 } else { 0 });
    let mut columns = vec![("PID", 7), ("COMMAND", name_width), ("CPU%", 6)];
    if show_rss {
        columns.push(("RSS MiB", 8));
    }
    if show_io {
        columns.push(("I/O MiB/s", 9));
    }
    columns
}

/// A process list row, each field padded to its column.
fn process_row(p: &Process, columns: &[(&str, u16)]) -> String {
    let mut row = String::new();
    for (index, &(_, cells)) in columns.iter().enumerate() {
        let field = match index {
            0 => p.pid.to_string(),
            1 => p.name.clone(),
            2 => dash(p.cpu, |cpu| format!("{cpu:.1}")),
            3 => format!("{:.1}", p.rss as f64 / MIB),
            _ => dash(p.read.zip(p.write), |(read, write)| {
                format!("{:.2}", (read + write) / MIB)
            }),
        };
        let value = fit_label(&field, cells);
        row.push(' ');
        row.push_str(&value);
        let padding = usize::from(cells).saturating_sub(text_width(&value));
        row.extend(std::iter::repeat_n(' ', padding));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_control_cycle_activates_tabs_ranges_and_filter() {
        let mut monitor = Monitor::new().unwrap();
        monitor.key(u32::from('/'), "/", false);
        monitor.toggle();
        monitor.view(80, 18, None);
        monitor.key(u32::from('j'), "j", false);
        assert_eq!((monitor.focus, monitor.tab), (Action::Tab(1), 0));
        let view = monitor.view(80, 18, None);
        let hit = monitor.hits[0]
            .iter()
            .find(|hit| hit.action == Action::Tab(1))
            .unwrap();
        let text = view
            .texts
            .iter()
            .find(|text| text.x == hit.x && text.y == hit.y)
            .unwrap();
        assert!(text.emphasis(true) == Emphasis::Highlighted);
        assert!(text.emphasis(false) == Emphasis::None);
        monitor.key(u32::from('l'), "l", false);
        assert_eq!(monitor.tab, 1);
        monitor.key(u32::from('h'), "h", false);
        monitor.key(u32::from('k'), "k", false);
        assert_eq!((monitor.focus, monitor.tab), (Action::Tab(0), 1));
        monitor.key(keysym::TAB, "", false);
        monitor.key(keysym::RETURN, "", false);
        assert_eq!(monitor.tab, 1);
        monitor.key(keysym::DOWN, "", false);
        assert_eq!(monitor.tab, 2);
        monitor.action(Action::Tab(6));
        monitor.key(u32::from('/'), "", false);
        monitor.key(0, "中rustc", false);
        monitor.key(keysym::BACKSPACE, "", false);
        assert_eq!(monitor.query.text, "中rust");
        monitor.key(keysym::TAB, "", false);
        assert!(!monitor.searching);
        assert_eq!(monitor.focus, Action::Sort(0));
        monitor.key(keysym::RETURN, "", false);
        assert!(!monitor.descending);
        let view = monitor.view(60, 12, None);
        assert!(view.texts.iter().any(|text| text.text == " Procs "));
        assert!(
            !monitor.hits[0]
                .iter()
                .any(|hit| hit.action == Action::Sort(2))
        );
        monitor.footer(80, true, false, None);
        assert!(
            monitor.hits[1]
                .iter()
                .any(|h| h.action == Action::Range(300))
        );
        monitor.action(Action::Search);
        monitor.key(keysym::ESCAPE, "", false);
        assert!(!monitor.searching && monitor.open);
        monitor.key(keysym::ESCAPE, "", false);
        monitor.key(u32::from('/'), "/", false);
        assert!(!monitor.open && monitor.query.text == "中rust");
    }

    #[test]
    fn terminal_tasks_keep_unknown_state_and_jump_to_selected_pane() {
        use crate::core::CommandBlock;
        use crate::workspace::{PaneView, TabActivity, TabView};
        let pane = |id, command_blocks| PaneView {
            command_blocks,
            ..crate::ui::tests::pane(id, 1, Default::default())
        };
        let running = CommandBlock {
            id: 12,
            complete: false,
            status: None,
            has_output: true,
            elapsed_ms: Some(1200),
        };
        let mut snapshot = Snapshot {
            name: "test".into(),
            controller: true,
            focused: 1,
            active_tab: 1,
            tabs: vec![TabView {
                id: 1,
                title: "build".into(),
                activity: TabActivity::Unknown,
            }],
            panes: vec![pane(1, vec![running]), pane(2, Vec::new())],
        };
        let mut monitor = Monitor::new().unwrap();
        monitor.set_workspace(&snapshot);
        assert_eq!(
            (monitor.tasks[0].state, monitor.tasks[1].state),
            ("Running", "Unknown")
        );
        snapshot.panes[0].command_blocks[0].status = Some(3);
        monitor.set_workspace(&snapshot);
        assert_eq!(
            (monitor.tasks[0].state, monitor.tasks[0].exit),
            ("Failed", Some(3))
        );
        monitor.toggle();
        monitor.action(Action::Tab(7));
        monitor.focus = Action::Task(1);
        monitor.key(keysym::DOWN, "", false);
        assert_eq!(monitor.focus, Action::Task(2));
        monitor.view(100, 18, None);
        assert!(
            monitor.hits[0]
                .iter()
                .any(|hit| hit.action == Action::Task(2))
        );
        monitor.key(keysym::RETURN, "", false);
        assert_eq!(monitor.jump.take(), Some(2));
        assert!(!monitor.open);
    }

    #[test]
    fn small_views_and_drag_bounds_preserve_the_terminal() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        for lines in [8, 12, 30] {
            monitor.resize(100, lines);
            assert!(monitor.rows(lines) <= lines - 6);
        }
        for tab in 0..8 {
            monitor.action(Action::Tab(tab));
            for width in (12..130).step_by(3) {
                let view = monitor.view(width, 10, None);
                assert!(
                    view.texts
                        .iter()
                        .all(|text| text.x + text.width <= width && (0..10).contains(&text.y))
                );
                assert!(
                    monitor.hits[0]
                        .iter()
                        .all(|hit| hit.x + hit.width <= width && (0..10).contains(&hit.y))
                );
            }
        }
    }

    fn process(pid: u32, name: &str) -> Process {
        Process {
            pid,
            name: name.into(),
            uid: 1000,
            rss: u64::from(pid),
            ..Process::default()
        }
    }

    fn pids(monitor: &mut Monitor) -> Vec<u32> {
        monitor.view(80, 20, None);
        monitor.hits[0]
            .iter()
            .filter_map(|hit| match hit.action {
                Action::Process(pid) => Some(pid),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn process_view_filters_and_drops_the_selection_of_a_reused_pid() {
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.processes =
            vec![process(1, "init"), process(2, "ΟΣ"), process(3, "K-worker")];
        monitor.toggle();
        monitor.action(Action::Tab(6));
        monitor.action(Action::Search);
        for (query, pid) in [("1000", 1), ("ος", 2), ("k", 3)] {
            monitor.key(u32::from('u'), "", true);
            monitor.insert_query(query);
            assert!(pids(&mut monitor).contains(&pid), "{query}");
        }
        monitor.key(u32::from('u'), "", true);
        monitor.action(Action::Sort(1));
        assert_eq!(pids(&mut monitor), [3, 2, 1]);
        monitor.action(Action::Process(2));
        // A new sample reorders the rows and replaces process 2 by a new process with the same pid.
        let processes = &mut monitor.stats.sample.processes;
        processes[0].rss = 40;
        processes[1].start = 1;
        monitor.stats.process_revision += 1;
        assert_eq!(pids(&mut monitor), [1, 3, 2]);
        assert!(monitor.selected.is_none());
        // Samples taken while the page is hidden carry no processes.
        monitor.action(Action::Process(1));
        monitor.view(80, 20, None);
        monitor.action(Action::Tab(1));
        monitor.stats.sample.processes = Vec::new();
        monitor.stats.process_revision += 1;
        monitor.action(Action::Tab(6));
        monitor.view(80, 20, None);
    }
}
