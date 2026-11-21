use crate::input::keysym;
use crate::system_stats::{MIB, Process, ResourceTrend, SystemStats, dash};
use crate::ui::TextField;
use crate::width::{fit_label, scalar_width, text_width};
use crate::workspace::{PaneId, Snapshot};
use std::{io, time::Instant};

pub(crate) use crate::panel::BACKGROUND;
pub(crate) use crate::panel::{ACCENT as GREEN, INK, MUTED};
const PURPLE: u32 = 0xb3a2ca;

pub(crate) fn series_color(name: &str) -> u32 {
    match name {
        "MEMORY" => 0x8fb5c7,
        "SWAP" | "WRITE" | "TX" | "OUT" => PURPLE,
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
    pub focused: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Emphasis {
    None,
    Selected,
    Highlighted,
}

impl Text {
    pub fn emphasis(&self, monitor_focused: bool) -> Emphasis {
        match self.emphasis {
            Emphasis::None if self.focused && monitor_focused => Emphasis::Highlighted,
            emphasis => emphasis,
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
pub(crate) struct Hit {
    pub x: u16,
    pub y: i32,
    pub width: u16,
    pub action: Action,
}
#[derive(Default)]
pub(crate) struct View {
    pub texts: Vec<Text>,
    pub plots: Vec<Plot>,
    pub hits: Vec<Hit>,
    pub body_x: u16,
    focus: Option<Action>,
}
impl View {
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
        if width == 0 {
            return y;
        }
        let mut paragraphs = text.split('\n').peekable();
        while let Some(paragraph) = paragraphs.next() {
            let mut line = String::new();
            let mut columns = 0;
            for ch in paragraph.chars().filter(|ch| !ch.is_control()) {
                let advance = u16::from(scalar_width(ch));
                if columns + advance > width && !line.is_empty() {
                    self.text(x, y, width, std::mem::take(&mut line), color);
                    y += 1;
                    columns = 0;
                }
                line.push(ch);
                columns += advance;
            }
            if !line.is_empty() || paragraphs.peek().is_some() {
                self.text(x, y, width, line, color);
                y += 1;
            }
        }
        y
    }
    fn details(
        &mut self,
        x: u16,
        mut y: i32,
        width: u16,
        rows: impl IntoIterator<Item = (&'static str, String)>,
    ) -> i32 {
        let key_width = (width / 2).min(24);
        let value_width = width.saturating_sub(key_width);
        for (key, value) in rows {
            if text_width(&value) > usize::from(value_width)
                || text_width(key) >= usize::from(key_width)
            {
                y = self.lines(x, y, width, key, MUTED);
                y = self.lines(x, y, width, &value, INK);
            } else {
                self.text(x, y, key_width.saturating_sub(1), key, MUTED);
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
    pub focus: Action,
    pub searching: bool,
    selecting_control: bool,
    pub dragging: bool,
    pub tab: u8,
    pub scroll: usize,
    pub pointer: Option<(u16, i32)>,
    pub range: u32,
    sort: u8,
    descending: bool,
    query: TextField,
    selected: Option<(u32, u64)>,
    process_order: Vec<usize>,
    process_revision: Option<u64>,
    anchor: Option<(u32, i32)>,
    reveal: bool,
    width: u16,
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
            pointer: None,
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
        })
    }
    pub fn set_workspace(&mut self, snapshot: &Snapshot) {
        let previous = std::mem::take(&mut self.tasks);
        for pane in &snapshot.panes {
            let command = pane
                .command_blocks
                .iter()
                .rev()
                .find(|block| block.has_output || block.status.is_some());
            let command_id = command.map(|block| block.id);
            let elapsed_ms = command.and_then(|block| block.elapsed_ms);
            let running = command.is_some_and(|block| !block.complete && block.status.is_none())
                && pane.shell_prompt.is_none();
            let state = match command {
                Some(block) if block.status.is_some_and(|status| status != 0) => "Failed",
                Some(block) if block.status == Some(0) => "Completed",
                _ if running => "Running",
                _ if pane.shell_prompt.is_some() => "Idle",
                _ => "Unknown",
            };
            // The elapsed time of a running command counts from when it was last observed.
            let sampled = previous
                .iter()
                .find(|task| task.pane == pane.id)
                .filter(|task| task.command_id == command_id && task.elapsed_ms == elapsed_ms)
                .map_or_else(Instant::now, |task| task.sampled);
            let command_text = match (&pane.process.foreground_argv, command) {
                (Some(args), _) if running || command.is_none() => args.join(" "),
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
                command: command_text,
                directory: pane.cwd.as_deref().map_or_else(
                    || "unavailable".into(),
                    |cwd| cwd.to_string_lossy().into_owned(),
                ),
                state,
                exit: command.and_then(|block| block.status),
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

    pub fn footer(&self, width: u16, controller: bool, remote: bool) -> View {
        let mut view = View {
            focus: self.open.then_some(self.focus),
            ..View::default()
        };
        let arrow = width.saturating_sub(4);
        view.button(
            arrow,
            0,
            3,
            crate::panel::expand_icon(self.open),
            if self.open {
                Action::Close
            } else {
                Action::Tab(self.tab)
            },
            false,
        );
        if self.open {
            if width >= 25 {
                let mut x = width - 25;
                for (seconds, label) in [(60, "60s"), (300, "5m"), (900, "15m"), (3600, "1h")] {
                    let cells = text_width(label) as u16 + 2;
                    view.button(
                        x,
                        0,
                        cells,
                        label,
                        Action::Range(seconds),
                        self.range == seconds,
                    );
                    x += cells + 1;
                }
            }
        } else {
            let mut x = arrow;
            for (index, label) in self.stats.summary.iter().enumerate().rev() {
                let cells = text_width(label) as u16 + 2;
                if cells > x {
                    break;
                }
                x -= cells;
                view.button(x, 0, cells, label, Action::Tab([1, 2, 3, 5][index]), false);
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
        self.pointer = None;
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
            Action::Close => {
                self.open = false;
                self.pointer = None;
            }
            Action::Tab(tab) => {
                self.open = true;
                self.tab = tab;
                self.scroll = 0;
                self.anchor = None;
            }
            Action::Range(range) => {
                self.range = range;
            }
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
        self.stats.set_processes_enabled(visible);
        let identity = self
            .selected
            .filter(|&id| visible && self.processes().any(|p| (p.pid, p.start) == id));
        self.stats.set_process_detail(identity);
    }
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
        self.process_order.clear();
        self.process_order.extend(
            processes
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.name.to_lowercase().contains(&query)
                        || numeric
                            && [p.pid, p.uid]
                                .iter()
                                .any(|id| id.to_string().contains(&query))
                })
                .map(|(index, _)| index),
        );
        self.process_order.sort_unstable_by(|&a, &b| {
            let a = &processes[a];
            let b = &processes[b];
            let value = |p: &Process| match self.sort {
                1 => p.rss as f64,
                2 => p.read.unwrap_or(0.) + p.write.unwrap_or(0.),
                _ => f64::from(p.cpu.unwrap_or(0.)),
            };
            let order = value(a).total_cmp(&value(b));
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order.then_with(|| a.pid.cmp(&b.pid))
        });
        self.process_revision = Some(revision);
        self.sync_stats();
    }
    fn processes(&self) -> impl Clone + ExactSizeIterator<Item = &Process> {
        self.process_order
            .iter()
            .map(|&index| &self.stats.sample.processes[index])
    }
    fn controls(&mut self) -> Vec<Action> {
        let mut controls: Vec<_> = (0..8).map(Action::Tab).collect();
        if self.width >= 24 {
            controls.extend([60, 300, 900, 3600].map(Action::Range));
        }
        if self.tab == 6 {
            controls.push(Action::Search);
            let body_width = self
                .width
                .saturating_sub(if self.width >= 80 { 15 } else { 11 });
            if body_width >= 26 {
                controls.extend(
                    (0..process_columns(body_width, self.sort).len() as u8 - 2).map(Action::Sort),
                );
            }
            self.rebuild_processes();
            let rows = self.processes();
            let pid = match self.focus {
                Action::Process(pid) if rows.clone().any(|p| p.pid == pid) => Some(pid),
                _ => self
                    .selected
                    .map(|selected| selected.0)
                    .filter(|pid| rows.clone().any(|p| p.pid == *pid))
                    .or_else(|| rows.clone().next().map(|p| p.pid)),
            };
            if let Some(pid) = pid {
                controls.push(Action::Process(pid));
            }
        }
        if self.tab == 7 {
            let pane = match self.focus {
                Action::Task(pane) => Some(pane),
                _ => self.tasks.first().map(|task| task.pane),
            };
            if let Some(pane) = pane {
                controls.push(Action::Task(pane));
            }
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
        let index = if backwards {
            (current + controls.len() - 1) % controls.len()
        } else {
            (current + 1) % controls.len()
        };
        self.focus = controls[index];
        self.reveal = true;
    }
    fn move_process(&mut self, delta: isize) {
        self.rebuild_processes();
        let pid = {
            let rows = self.processes();
            let current = if let Action::Process(pid) = self.focus {
                rows.clone().position(|p| p.pid == pid).unwrap_or(0)
            } else {
                0
            };
            let index = current
                .saturating_add_signed(delta)
                .min(rows.len().saturating_sub(1));
            self.process_order.get(index).map(|&index| {
                let process = &self.stats.sample.processes[index];
                (process.pid, process.start)
            })
        };
        if let Some((pid, start)) = pid {
            self.focus = Action::Process(pid);
            if self.selected.is_some() {
                self.selected = Some((pid, start));
            }
            self.searching = false;
            self.reveal = true;
            self.sync_stats();
        }
    }
    pub fn key(&mut self, symbol: u32, text: &str, control: bool) {
        if !self.open {
            return;
        }
        if symbol == keysym::ESCAPE {
            if self.searching {
                self.searching = false;
            } else {
                self.action(Action::Close);
            }
            return;
        }
        if matches!(symbol, keysym::TAB | keysym::LEFT_TAB) {
            self.cycle(symbol == keysym::LEFT_TAB);
            return;
        }
        if self.searching {
            match symbol {
                keysym::RETURN => {
                    self.searching = false;
                    self.rebuild_processes();
                    let pid = self.processes().next().map(|p| p.pid);
                    if let Some(pid) = pid {
                        self.focus = Action::Process(pid);
                        self.reveal = true;
                    }
                }
                _ if self.query.key(symbol, control) => self.query_changed(),
                _ => self.insert_query(text),
            }
            return;
        }
        match symbol {
            0x68 => self.selecting_control = true,
            0x6a | 0x6b if self.selecting_control => self.cycle(symbol == 0x6b),
            0x6c | keysym::RETURN | 0x20 => self.action(self.focus),
            keysym::UP | keysym::DOWN | keysym::PAGE_UP | keysym::PAGE_DOWN | 0x6a | 0x6b => {
                let delta: isize = match symbol {
                    keysym::UP | 0x6b => -1,
                    keysym::DOWN | 0x6a => 1,
                    keysym::PAGE_UP => -6,
                    _ => 6,
                };
                match self.focus {
                    Action::Tab(tab) if delta.abs() == 1 => {
                        let tab = if delta > 0 {
                            (tab + 1) % 8
                        } else {
                            (tab + 7) % 8
                        };
                        self.action(Action::Tab(tab));
                    }
                    Action::Process(_) => self.move_process(delta),
                    Action::Task(pane) => {
                        if let Some(index) = self.tasks.iter().position(|task| task.pane == pane) {
                            let index =
                                index.saturating_add_signed(delta).min(self.tasks.len() - 1);
                            self.focus = Action::Task(self.tasks[index].pane);
                            self.reveal = true;
                        }
                    }
                    _ => self.scroll = self.scroll.saturating_add_signed(delta),
                }
            }
            keysym::LEFT | keysym::RIGHT => {
                let forward = symbol == keysym::RIGHT;
                if matches!(self.focus, Action::Tab(_)) && forward {
                    self.focus = self
                        .controls()
                        .into_iter()
                        .find(|action| !matches!(action, Action::Tab(_)))
                        .unwrap_or(Action::Tab(self.tab));
                    self.reveal = true;
                } else if !forward {
                    self.focus = Action::Tab(self.tab);
                } else {
                    self.cycle(false);
                }
            }
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
        if width < 10 || series.is_empty() {
            return;
        }
        let first = &series[0];
        let unit = first.unit;
        let maximum = if unit == "%" {
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
            let value_width = (text_width(&value) as u16).min(width.saturating_sub(5));
            view.text(x, y, width.saturating_sub(value_width + 1), title, INK);
            view.text(
                x + width - value_width,
                y,
                value_width,
                value,
                series_color(first.name),
            );
            view.text(
                x,
                y + 1,
                width,
                std::mem::take(&mut series[0].summary),
                MUTED,
            );
        } else {
            view.text(x, y, width, title, INK);
            let columns = width / series.len() as u16;
            for (i, trend) in series.iter().enumerate() {
                view.text(
                    x + i as u16 * columns,
                    y + 1,
                    columns,
                    format!("{} {}", trend.name, trend_value(trend.value, trend.unit)),
                    series_color(trend.name),
                );
            }
        }
        let scale = if unit == "%" {
            format!("{maximum:.0}%")
        } else if maximum < 1. {
            format!("{:.0}K", maximum * 1024.)
        } else {
            format!("{maximum:.0}M")
        };
        view.text(x, y + 2, 5, scale, MUTED);
        view.text(x, y + 1 + i32::from(height), 5, "0", MUTED);
        let plot_x = x + 5;
        let plot_width = width - 5;
        let axis_y = y + i32::from(height) + 2;
        // Choose tick spacing from the actual plot width so labels never collide.
        let target = self
            .range
            .div_ceil(u32::from((plot_width / 10).clamp(1, 4)));
        let step = [1, 2, 5, 10, 15, 30, 60, 120, 180, 300, 600, 900]
            .into_iter()
            .find(|&n| n >= target)
            .unwrap_or(self.range);
        let mut age = if plot_width < 8 { 0 } else { self.range };
        let mut end = 0;
        loop {
            let label = if age == 0 {
                "now".into()
            } else {
                format!(
                    "−{}{}",
                    if age >= 60 { age / 60 } else { age },
                    if age >= 60 { "m" } else { "s" }
                )
            };
            let column = ((1. - age as f32 / self.range as f32)
                * f32::from(plot_width.saturating_sub(1))) as u16;
            let cells = (text_width(&label) as u16).min(plot_width);
            let column = column.min(plot_width - cells);
            if age == 0 || column >= end && column + cells + 4 < plot_width {
                view.text(plot_x + column, axis_y, cells, label, MUTED);
                end = column + cells + 2;
            }
            if age == 0 {
                break;
            }
            age = if age == self.range {
                ((age - 1) / step) * step
            } else {
                age.saturating_sub(step)
            };
        }
        if series
            .iter()
            .all(|trend| trend.points.iter().all(|(_, value)| value.is_none()))
        {
            view.text(
                plot_x,
                y + 2 + i32::from(height / 2),
                plot_width,
                "Waiting for samples",
                MUTED,
            );
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
        let pid = if let Action::Process(pid) = self.focus {
            Some(pid)
        } else {
            self.selected.map(|selected| selected.0)
        };
        if let Some(pid) = pid
            && let Some(hit) = hits.iter().find(|hit| hit.action == Action::Process(pid))
        {
            if let Some((old_pid, old_y)) = self.anchor
                && old_pid == pid
            {
                self.scroll = self.scroll.saturating_add_signed((hit.y - old_y) as isize);
            }
            self.anchor = Some((pid, hit.y));
        } else {
            self.anchor = None;
        }
    }
    pub fn view(&mut self, width: u16, height: u16) -> View {
        let mut view = View {
            focus: Some(self.focus),
            ..View::default()
        };
        if width < 12 || height < 3 {
            return view;
        }
        self.width = width;
        if self.tab == 6 {
            self.rebuild_processes();
        }
        let side = if width >= 80 { 13 } else { 9 };
        let x = side + 1;
        let w = width.saturating_sub(x + 1);
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
        let first =
            usize::from(self.tab).saturating_sub(usize::from(height.saturating_sub(1) / spacing));
        for (i, labels) in labels.into_iter().enumerate().skip(first) {
            let label = if width >= 80 { labels.0 } else { labels.1 };
            let y = (i - first) as u16 * spacing;
            if y < height {
                view.button(
                    1,
                    i32::from(y),
                    side - 1,
                    label,
                    Action::Tab(i as u8),
                    self.tab == i as u8,
                );
                if self.tab == i as u8 {
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
        match self.tab {
            0 => {
                let columns = if w >= 54 { 2 } else { 1 };
                let cw = (w - (columns - 1) * 2) / columns;
                for (i, series) in [0..1, 1..2, 2..4, 4..5].into_iter().enumerate() {
                    let i = i as u16;
                    let (column, row) = (i % columns, i / columns);
                    let trends = self.stats.system_trends(self.range, series);
                    self.graph(
                        &mut body,
                        x + column * (cw + 2),
                        i32::from(row * 7),
                        cw,
                        3,
                        trends,
                    );
                }
                body.details(x, i32::from(4 / columns * 7), w, self.stats.overview());
            }
            1 => {
                self.graph(
                    &mut body,
                    x,
                    0,
                    graph_width,
                    4,
                    self.stats.system_trends(self.range, 0..1),
                );
                let y = if side {
                    body.details(dx, 0, dw, self.stats.cpu_details()).max(7)
                } else {
                    7
                };
                let columns = (w / 24).clamp(1, 4);
                let cw = w / columns;
                let cores = &self.stats.sample.cpu_cores;
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
                let bottom = if cores.is_empty() {
                    y
                } else {
                    y + 3 + (cores.len() as i32 - 1) / i32::from(columns)
                };
                if !side {
                    body.details(x, bottom, w, self.stats.cpu_details());
                }
            }
            2 => {
                self.graph(
                    &mut body,
                    x,
                    0,
                    graph_width,
                    5,
                    self.stats.system_trends(self.range, 1..2),
                );
                let y = body.details(
                    dx,
                    if side { 0 } else { 9 },
                    dw,
                    self.stats.memory_details(),
                );
                body.lines(dx, y + 1, dw, "Available includes reclaimable cache", MUTED);
            }
            3 => {
                self.graph(
                    &mut body,
                    x,
                    0,
                    graph_width,
                    4,
                    self.stats.system_trends(self.range, 2..4),
                );
                let mut y = if side { 0 } else { 8 };
                if self.stats.sample.disks.is_empty() {
                    body.text(dx, y, dw, "No physical disks", MUTED);
                    y += 2;
                }
                for disk in &self.stats.sample.disks {
                    body.text(dx, y, dw, &disk.name, INK);
                    let rows = [
                        ("Read", trend_value(disk.rates[0], "MiB/s")),
                        ("Write", trend_value(disk.rates[1], "MiB/s")),
                        ("Busy", dash(disk.utilization, |v| format!("{v:.0}%"))),
                    ];
                    y = body.details(dx, y + 1, dw, rows) + 1;
                }
                let y = if side { y.max(8) } else { y };
                body.details(x, y, w, self.stats.disk_details());
            }
            4 => {
                if self.stats.sample.networks.is_empty() {
                    body.text(x, 0, w, "No physical network interfaces", MUTED);
                }
                let graph_height = height.saturating_sub(6).clamp(3, 4);
                let mut y = 0;
                for network in &self.stats.sample.networks {
                    // Scroll clamping can only move the viewport upward.
                    if y < self.scroll as i32 + i32::from(height) {
                        let trends = self.stats.network_trends(self.range, network);
                        self.graph(&mut body, x, y, graph_width, graph_height, trends);
                    }
                    let end = if side {
                        body.details(dx, y, dw, network.details())
                    } else {
                        body.details(x, y + i32::from(graph_height) + 4, w, network.details())
                    };
                    y = end.max(y + i32::from(graph_height) + 3) + 2;
                }
            }
            5 => {
                if self.stats.swap_disabled() {
                    body.text(x, 0, w, "Swap is disabled", MUTED);
                } else {
                    let columns = if w >= 64 { 2 } else { 1 };
                    let cw = (w - (columns - 1) * 3) / columns;
                    let usage = self.stats.system_trends(self.range, 4..5);
                    let rates = self.stats.system_trends(self.range, 5..7);
                    self.graph(&mut body, x, 0, cw, 4, usage);
                    let (rx, ry) = if columns == 2 {
                        (x + cw + 3, 0)
                    } else {
                        (x, 8)
                    };
                    self.graph(&mut body, rx, ry, cw, 4, rates);
                }
            }
            6 => self.process_view(&mut body, x, w),
            7 => {
                body.text(x, 0, w, "Terminal tasks · Enter jumps to pane", INK);
                body.text(x, 1, w, "Latest observed command per pane", MUTED);
                if self.tasks.is_empty() {
                    body.text(x, 3, w, "No terminal panes", MUTED);
                }
                for (index, task) in self.tasks.iter().enumerate() {
                    let y = 3 + index as i32 * 4;
                    let action = Action::Task(task.pane);
                    body.button(
                        x,
                        y,
                        w,
                        &format!("{} · {}", task.state, task.command),
                        action,
                        self.focus == action,
                    );
                    let elapsed = task.elapsed_ms.map_or_else(
                        || "elapsed —".into(),
                        |elapsed| {
                            let running = task.state == "Running";
                            let elapsed = elapsed
                                + if running {
                                    task.sampled.elapsed().as_millis() as u64
                                } else {
                                    0
                                };
                            format!("{:.1}s", elapsed as f64 / 1000.0)
                        },
                    );
                    let exit = dash(task.exit, |status| status.to_string());
                    let details = format!(
                        "Tab {} · Pane {} · exit {exit} · {elapsed}",
                        task.tab, task.pane
                    );
                    body.text(x + 1, y + 1, w.saturating_sub(1), details, MUTED);
                    body.text(x + 1, y + 2, w.saturating_sub(1), &task.directory, MUTED);
                }
            }
            _ => unreachable!(),
        }
        let content_height = body
            .texts
            .iter()
            .map(|t| t.y + 1)
            .chain(body.hits.iter().map(|hit| hit.y + 1))
            .chain(body.plots.iter().map(|p| p.y + i32::from(p.height)))
            .fold(0, i32::max);
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
        let pinned = if self.tab == 6 && height > 5 { 4 } else { 0 };
        if self.reveal {
            if let Some(hit) = body.hits.iter().find(|hit| hit.action == self.focus) {
                let visible = i32::from(height).max(1);
                let expanded = self
                    .selected
                    .is_some_and(|selected| hit.action == Action::Process(selected.0));
                let bottom = hit.y + if expanded { 4 } else { 1 };
                if hit.y >= pinned && hit.y < self.scroll as i32 + pinned {
                    self.scroll = (hit.y - pinned).max(0) as usize;
                }
                if bottom > self.scroll as i32 + visible {
                    self.scroll = bottom.saturating_sub(visible).max(0) as usize;
                }
            }
            self.reveal = false;
        }
        self.scroll = self
            .scroll
            .min(content_height.saturating_sub(i32::from(height)).max(0) as usize);
        // Rows above `pinned` stay put; the rest scroll and are clipped below the pinned rows.
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
        view.texts.retain_mut(|text| {
            text.width = text.width.min(width.saturating_sub(text.x));
            text.x < width
        });
        view.hits.retain_mut(|hit| {
            hit.width = hit.width.min(width.saturating_sub(hit.x));
            hit.x < width
        });
        // Every process hit is kept for focus anchoring; text is built only for visible rows.
        if self.tab == 6 {
            let columns = process_columns(w, self.sort);
            for hit in &view.hits {
                if let Action::Process(pid) = hit.action {
                    let processes = &self.stats.sample.processes;
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
        if let Some((x, y)) = self.pointer
            && let Some(hit) = view
                .hits
                .iter()
                .rfind(|hit| hit.y == y && (hit.x..hit.x + hit.width).contains(&x))
            && let Some(text) = view
                .texts
                .iter_mut()
                .find(|text| text.x == hit.x && text.y == hit.y && text.width == hit.width)
        {
            if text.emphasis != Emphasis::Selected {
                text.color = INK;
            }
            text.emphasis = if matches!(hit.action, Action::Process(_)) {
                Emphasis::Selected
            } else {
                Emphasis::Highlighted
            };
        }
        view
    }
    fn process_view(&self, body: &mut View, x: u16, width: u16) {
        let filter = if self.searching {
            let before = &self.query.text[..self.query.cursor];
            let mut start = 0;
            let mut columns = text_width(before);
            for (i, ch) in before.char_indices() {
                if columns <= usize::from(width.saturating_sub(12)) {
                    break;
                }
                columns = columns.saturating_sub(usize::from(scalar_width(ch)));
                start = i + ch.len_utf8();
            }
            format!(
                "⌕ {}▏{}",
                &self.query.text[start..self.query.cursor],
                &self.query.text[self.query.cursor..]
            )
        } else {
            format!("⌕ {}  / to search", self.query.text)
        };
        body.button(x, 0, width, &filter, Action::Search, self.searching);
        body.text(
            x,
            1,
            width,
            "CPU: 1 core = 100% · RSS MiB · I/O MiB/s",
            MUTED,
        );
        if width < 26 {
            body.text(x, 3, width, "Widen for process list", MUTED);
            return;
        }
        let columns = process_columns(width, self.sort);
        let show_io = columns.len() == 5;
        let mut column = x + 1;
        for (id, &(label, cells)) in columns.iter().enumerate() {
            if id >= 2 {
                let sort = id as u8 - 2;
                let label = format!(
                    "{label}{}",
                    if self.sort == sort {
                        if self.descending { "▾" } else { "▴" }
                    } else {
                        ""
                    }
                );
                body.button(
                    column - 1,
                    3,
                    (text_width(&label) as u16).min(cells) + 2,
                    &label,
                    Action::Sort(sort),
                    self.sort == sort,
                );
            } else {
                body.text(
                    column,
                    3,
                    cells,
                    if id == 1 && cells < 7 { "NAME" } else { label },
                    MUTED,
                );
            }
            column += cells + 1;
        }
        let processes = self.processes();
        if processes.len() == 0 {
            body.text(x, 4, width, "No matches · waiting for samples", MUTED);
            return;
        }
        body.hits.reserve(processes.len());
        let mut y = 4;
        for p in processes {
            body.hits.push(Hit {
                x,
                y,
                width,
                action: Action::Process(p.pid),
            });
            y += 1;
            if self.selected == Some((p.pid, p.start)) {
                y = body.lines(
                    x + 1,
                    y,
                    width.saturating_sub(2),
                    &format!("Selected process · {}", p.name),
                    GREEN,
                );
                y = body.details(
                    x + 1,
                    y,
                    width.saturating_sub(2),
                    [
                        (
                            "CPU / RSS",
                            format!(
                                "{} / {:.1} MiB",
                                dash(p.cpu, |cpu| format!("{cpu:.1}%")),
                                p.rss as f64 / MIB
                            ),
                        ),
                        (
                            "PID / parent / user",
                            format!("{} / {} / {}", p.pid, p.parent, p.uid),
                        ),
                        ("State / threads", format!("{} / {}", p.state, p.threads)),
                        (
                            "Directory",
                            p.directory.as_deref().map_or_else(
                                || "unavailable".into(),
                                |directory| directory.to_string_lossy().into_owned(),
                            ),
                        ),
                        ("Command", p.command.clone()),
                    ],
                );
                if !show_io {
                    y = body.lines(
                        x + 1,
                        y,
                        width.saturating_sub(2),
                        &format!(
                            "I/O read/write: {} / {} MiB/s",
                            dash(p.read, |v| format!("{:.2}", v / MIB)),
                            dash(p.write, |v| format!("{:.2}", v / MIB))
                        ),
                        MUTED,
                    );
                }
                y += 1;
            }
        }
    }
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

fn process_row(p: &Process, columns: &[(&str, u16)]) -> String {
    let mut row = String::from(" ");
    for (index, &(_, cells)) in columns.iter().enumerate() {
        if index > 0 {
            row.push(' ');
        }
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
        row.push_str(&value);
        row.extend(std::iter::repeat_n(
            ' ',
            usize::from(cells).saturating_sub(text_width(&value)),
        ));
    }
    row.truncate(row.trim_end().len());
    row.push(' ');
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_focus_unicode_and_tabs_do_not_consume_terminal_input() {
        let mut monitor = Monitor::new().unwrap();
        monitor.key(u32::from('t'), "t", false);
        assert!(monitor.query.text.is_empty());
        monitor.toggle();
        monitor.action(Action::Tab(6));
        monitor.action(Action::Search);
        monitor.key(0, "中rustc", false);
        assert_eq!(monitor.query.text, "中rustc");
        monitor.key(keysym::BACKSPACE, "", false);
        assert_eq!(monitor.query.text, "中rust");
        monitor.key(u32::from('A'), "", true);
        assert_eq!(monitor.query.cursor, 0);
        monitor.key(u32::from('e'), "", true);
        assert_eq!(monitor.query.cursor, monitor.query.text.len());
        monitor.key(u32::from('u'), "", true);
        assert!(monitor.query.text.is_empty());
        monitor.key(keysym::ESCAPE, "", false);
        assert!(!monitor.searching);
        assert!(monitor.open);
        monitor.key(keysym::ESCAPE, "", false);
        assert!(!monitor.open);
        monitor.key(u32::from('t'), "t", false);
        assert!(monitor.query.text.is_empty());
        monitor.toggle();
        monitor.action(Action::Tab(3));
        let view = monitor.view(80, 18);
        assert!(
            !view
                .hits
                .iter()
                .any(|hit| matches!(hit.action, Action::Range(_)))
        );
        monitor.action(Action::Tab(0));
        let view = monitor.footer(80, true, false);
        assert!(
            view.hits
                .iter()
                .any(|hit| matches!(hit.action, Action::Range(300)))
        );
    }
    #[test]
    fn keyboard_control_cycle_activates_tabs_ranges_and_filter() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        monitor.view(80, 18);
        monitor.key(u32::from('j'), "j", false);
        assert_eq!(monitor.focus, Action::Tab(1));
        assert_eq!(monitor.tab, 0);
        monitor.key(u32::from('l'), "l", false);
        assert_eq!(monitor.tab, 1);
        monitor.key(u32::from('h'), "h", false);
        monitor.key(u32::from('k'), "k", false);
        assert_eq!(monitor.focus, Action::Tab(0));
        assert_eq!(monitor.tab, 1);
        monitor.key(u32::from('l'), "l", false);
        assert_eq!(monitor.tab, 0);
        monitor.key(keysym::TAB, "", false);
        assert_eq!(monitor.focus, Action::Tab(1));
        monitor.key(keysym::RETURN, "", false);
        assert_eq!(monitor.tab, 1);
        monitor.key(keysym::LEFT_TAB, "", false);
        assert_eq!(monitor.focus, Action::Tab(0));
        monitor.key(keysym::DOWN, "", false);
        assert_eq!(monitor.tab, 1);
        monitor.key(keysym::DOWN, "", false);
        assert_eq!(monitor.tab, 2);
        monitor.action(Action::Tab(6));
        monitor.key(u32::from('/'), "", false);
        assert!(monitor.searching);
        monitor.key(keysym::TAB, "", false);
        assert!(!monitor.searching);
        assert_eq!(monitor.focus, Action::Sort(0));
        monitor.key(keysym::RETURN, "", false);
        assert!(!monitor.descending);
        let view = monitor.view(60, 12);
        assert!(view.texts.iter().any(|text| text.focused));
        assert!(view.texts.iter().any(|text| text.text == " Procs "));
        assert!(!view.hits.iter().any(|hit| hit.action == Action::Sort(2)));
    }
    #[test]
    fn terminal_tasks_keep_unknown_state_and_jump_to_selected_pane() {
        use crate::core::CommandBlock;
        use crate::workspace::{PaneView, TabActivity, TabView};
        use std::sync::Arc;
        let pane = |id, command_blocks| PaneView {
            id,
            tab: 1,
            rect: Default::default(),
            title: format!("pane {id}"),
            cwd: Some("/project".into()),
            process: Arc::new(crate::terminal::ProcessMetadata::default()),
            blocks: Vec::new(),
            command_blocks,
            input_generation: 0,
            shell_prompt: None,
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
            panes: vec![
                pane(
                    1,
                    vec![CommandBlock {
                        id: 12,
                        complete: false,
                        status: None,
                        has_output: true,
                        elapsed_ms: Some(1200),
                    }],
                ),
                pane(2, Vec::new()),
            ],
        };
        let mut monitor = Monitor::new().unwrap();
        monitor.set_workspace(&snapshot);
        assert_eq!(monitor.tasks[0].state, "Running");
        assert_eq!(monitor.tasks[1].state, "Unknown");
        snapshot.panes[0].command_blocks[0].status = Some(3);
        monitor.set_workspace(&snapshot);
        assert_eq!(monitor.tasks[0].state, "Failed");
        assert_eq!(monitor.tasks[0].exit, Some(3));
        monitor.toggle();
        monitor.action(Action::Tab(7));
        monitor.focus = Action::Task(1);
        monitor.key(keysym::DOWN, "", false);
        assert_eq!(monitor.focus, Action::Task(2));
        let view = monitor.view(100, 18);
        assert!(view.hits.iter().any(|hit| hit.action == Action::Task(2)));
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
        for tab in 0..7 {
            monitor.action(Action::Tab(tab));
            for width in [12, 30, 80, 120] {
                let view = monitor.view(width, 10);
                assert!(
                    view.texts
                        .iter()
                        .all(|text| text.x + text.width <= width && (0..10).contains(&text.y))
                );
                assert!(
                    view.hits
                        .iter()
                        .all(|hit| hit.x + hit.width <= width && (0..10).contains(&hit.y))
                );
            }
        }
    }
}
