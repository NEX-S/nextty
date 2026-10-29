use crate::system_stats::{Process, ResourceTrend, SystemStats};
use crate::width::{fit_label, scalar_width, text_width};
use std::{borrow::Cow, io};

pub(crate) const BACKGROUND: u32 = 0x1c2220;
pub(crate) const INK: u32 = 0xcbd0c9;
pub(crate) const MUTED: u32 = 0x8b9690;
pub(crate) const GREEN: u32 = 0xb1c99c;
pub(crate) const PURPLE: u32 = 0xb3a2ca;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Close,
    Tab(u8),
    Range(u32),
    Interface,
    Sort(u8),
    Search,
    Process(u32),
}

pub(crate) struct Text {
    pub x: u16,
    pub y: i32,
    pub width: u16,
    pub text: String,
    pub color: u32,
    pub selected: bool,
    pub focused: bool,
    pub highlighted: bool,
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
}
impl View {
    fn text(&mut self, x: u16, y: i32, width: u16, text: impl Into<String>, color: u32) {
        self.texts.push(Text {
            x,
            y,
            width,
            text: text.into(),
            color,
            selected: false,
            focused: false,
            highlighted: false,
        });
    }
    fn button(&mut self, x: u16, y: i32, width: u16, text: &str, action: Action, selected: bool) {
        let width = if matches!(action, Action::Process(_) | Action::Search) {
            width
        } else {
            width.min(text_width(text) as u16 + 2)
        };
        let label = format!(" {} ", fit_label(text, width.saturating_sub(2)));
        self.text(x, y, width, label, if selected { GREEN } else { MUTED });
        self.texts.last_mut().unwrap().highlighted = selected;
        self.hits.push(Hit {
            x,
            y,
            width,
            action,
        });
    }
    fn lines(&mut self, x: u16, mut y: i32, width: u16, text: &str, color: u32) -> i32 {
        if width == 0 {
            return y;
        }
        let mut line = String::new();
        let mut columns = 0;
        for ch in text.chars() {
            if ch.is_control() && ch != '\n' {
                continue;
            }
            let advance = u16::from(scalar_width(ch).min(2));
            if ch == '\n' || columns + advance > width && !line.is_empty() {
                self.text(x, y, width, std::mem::take(&mut line), color);
                y += 1;
                columns = 0;
                if ch == '\n' {
                    continue;
                }
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
    fn details(&mut self, x: u16, mut y: i32, width: u16, rows: Vec<(String, String)>) {
        for (key, value) in rows {
            let key_width = (width / 2).min(24);
            if text_width(&value) > usize::from(width.saturating_sub(key_width))
                || text_width(&key) >= usize::from(key_width)
            {
                y = self.lines(x, y, width, &key, MUTED);
                y = self.lines(x, y, width, &value, INK);
            } else {
                self.text(x, y, key_width.saturating_sub(1), key, MUTED);
                self.text(
                    x + key_width,
                    y,
                    width.saturating_sub(key_width),
                    value,
                    INK,
                );
                y += 1;
            }
        }
    }
}

pub(crate) struct Monitor {
    pub stats: SystemStats,
    pub open: bool,
    pub height: u16,
    pub focus: Action,
    pub searching: bool,
    pub dragging: bool,
    pub tab: u8,
    pub scroll: usize,
    pub pointer: Option<(u16, i32)>,
    range: u32,
    interface: Option<String>,
    sort: u8,
    descending: bool,
    query: String,
    cursor: usize,
    selected: Option<u32>,
    pending: Vec<u8>,
    anchor: Option<(u32, i32)>,
    reveal: bool,
    width: u16,
}
impl Monitor {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            stats: SystemStats::new()?,
            open: false,
            height: 18,
            focus: Action::Tab(0),
            searching: false,
            dragging: false,
            tab: 0,
            scroll: 0,
            pointer: None,
            range: 60,
            interface: None,
            sort: 0,
            descending: true,
            query: String::new(),
            cursor: 0,
            selected: None,
            pending: Vec::new(),
            anchor: None,
            reveal: false,
            width: 80,
        })
    }
    pub fn seconds(&self) -> u32 {
        self.range
    }
    pub fn insert_query(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.query.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.pending.clear();
        self.scroll = 0;
        self.anchor = None;
    }
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.pointer = None;
        self.searching = false;
        if self.open && self.focus == Action::Close {
            self.focus = Action::Tab(self.tab);
        }
        self.stats.set_processes_enabled(self.open && self.tab == 2);
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
        self.reveal = true;
        match action {
            Action::Close => {
                self.open = false;
                self.pointer = None;
            }
            Action::Tab(tab) => {
                self.tab = tab;
                self.scroll = 0;
                self.anchor = None;
            }
            Action::Range(range) => {
                self.range = range;
            }
            Action::Interface => {
                let networks = self.stats.networks();
                let current = self.interface.as_deref().or_else(|| {
                    networks
                        .iter()
                        .find(|n| n.name != "lo")
                        .or(networks.first())
                        .map(|n| n.name.as_str())
                });
                let index = networks
                    .iter()
                    .position(|n| Some(n.name.as_str()) == current)
                    .unwrap_or(0);
                self.interface = networks
                    .get((index + 1) % networks.len().max(1))
                    .map(|n| n.name.clone());
            }
            Action::Sort(sort) => {
                self.descending = self.sort != sort || !self.descending;
                self.sort = sort;
                self.anchor = None;
            }
            Action::Search => self.searching = true,
            Action::Process(pid) => {
                self.selected = if self.selected == Some(pid) {
                    None
                } else {
                    Some(pid)
                };
            }
        }
        self.stats.set_processes_enabled(self.open && self.tab == 2);
    }
    fn processes(&self) -> Vec<&Process> {
        let query = self.query.to_lowercase();
        let numeric = query.bytes().all(|byte| byte.is_ascii_digit());
        let mut rows: Vec<_> = self
            .stats
            .processes()
            .iter()
            .filter(|p| {
                query.is_empty()
                    || if p.name.is_ascii() {
                        // comm names are short; compare ASCII without a lowercase allocation.
                        p.name
                            .as_bytes()
                            .windows(query.len())
                            .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
                    } else {
                        p.name.to_lowercase().contains(&query)
                    }
                    || numeric
                        && (p.pid.to_string().contains(&query)
                            || p.uid.to_string().contains(&query))
            })
            .collect();
        rows.sort_by(|a, b| {
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
        rows
    }
    fn controls(&self) -> Vec<Action> {
        let mut controls: Vec<_> = (0..4).map(Action::Tab).collect();
        if self.tab < 2 && self.width >= 50 {
            controls.extend([60, 300, 900].map(Action::Range));
        }
        if self.tab == 1 {
            controls.push(Action::Interface);
        }
        if self.tab == 2 {
            controls.push(Action::Search);
            let body_width = self
                .width
                .saturating_sub(if self.width >= 80 { 15 } else { 11 });
            if body_width >= 26 {
                controls.extend(
                    process_columns(body_width, self.sort)
                        .into_iter()
                        .filter(|(id, _, _)| *id >= 2)
                        .map(|(id, _, _)| Action::Sort(id - 2)),
                );
            }
            let rows = self.processes();
            let pid = match self.focus {
                Action::Process(pid) if rows.iter().any(|p| p.pid == pid) => Some(pid),
                _ => self
                    .selected
                    .filter(|pid| rows.iter().any(|p| p.pid == *pid))
                    .or_else(|| rows.first().map(|p| p.pid)),
            };
            if let Some(pid) = pid {
                controls.push(Action::Process(pid));
            }
        }
        controls
    }
    fn cycle(&mut self, backwards: bool) {
        self.searching = false;
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
        let rows = self.processes();
        let current = if let Action::Process(pid) = self.focus {
            rows.iter().position(|p| p.pid == pid).unwrap_or(0)
        } else {
            0
        };
        let index = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        let pid = rows.get(index).map(|p| p.pid);
        if let Some(pid) = pid {
            self.focus = Action::Process(pid);
            if self.selected.is_some() {
                self.selected = Some(pid);
            }
            self.searching = false;
            self.reveal = true;
        }
    }
    pub fn key(&mut self, symbol: u32, text: &str, control: bool) -> bool {
        if !self.open {
            return false;
        }
        if symbol == 0xff1b {
            self.pending.clear();
            if self.searching {
                self.searching = false;
            } else {
                self.action(Action::Close);
            }
            return true;
        }
        if matches!(symbol, 0xff09 | 0xfe20) {
            self.cycle(symbol == 0xfe20);
            return true;
        }
        if self.searching {
            match symbol {
                0xff51 => {
                    self.cursor = self.query[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i)
                }
                0xff53 => {
                    self.cursor = self.query[self.cursor..]
                        .char_indices()
                        .nth(1)
                        .map_or(self.query.len(), |(i, _)| self.cursor + i)
                }
                0xff50 => self.cursor = 0,
                0xff57 => self.cursor = self.query.len(),
                0xff08 => {
                    if let Some((i, _)) = self.query[..self.cursor].char_indices().next_back() {
                        self.query.drain(i..self.cursor);
                        self.cursor = i;
                    }
                    self.scroll = 0;
                    self.anchor = None;
                }
                0xffff => {
                    if let Some(ch) = self.query[self.cursor..].chars().next() {
                        self.query.drain(self.cursor..self.cursor + ch.len_utf8());
                    }
                    self.scroll = 0;
                    self.anchor = None;
                }
                0xff0d => {
                    self.searching = false;
                    let pid = self.processes().first().map(|p| p.pid);
                    if let Some(pid) = pid {
                        self.focus = Action::Process(pid);
                        self.reveal = true;
                    }
                }
                _ if control => match text {
                    "u" => {
                        self.query.clear();
                        self.cursor = 0;
                        self.anchor = None;
                        self.scroll = 0;
                    }
                    "a" => self.cursor = 0,
                    "e" => self.cursor = self.query.len(),
                    _ => (),
                },
                _ => self.insert_query(text),
            }
            return true;
        }
        let ch = char::from_u32(symbol);
        match symbol {
            0xff0d | 0x20 => {
                let action = self.focus;
                self.action(action);
            }
            0xff52 | 0xff54 | 0x6a | 0x6b => {
                let down = matches!(symbol, 0xff54 | 0x6a);
                match self.focus {
                    Action::Tab(tab) => {
                        let tab = if down { (tab + 1) % 4 } else { (tab + 3) % 4 };
                        self.action(Action::Tab(tab));
                    }
                    Action::Process(_) => self.move_process(if down { 1 } else { -1 }),
                    _ => {
                        self.scroll = if down {
                            self.scroll.saturating_add(1)
                        } else {
                            self.scroll.saturating_sub(1)
                        }
                    }
                }
            }
            0xff51 | 0xff53 | 0x68 | 0x6c => {
                let forward = matches!(symbol, 0xff53 | 0x6c);
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
            0xff55 | 0xff56 => {
                let down = symbol == 0xff56;
                if matches!(self.focus, Action::Process(_)) {
                    self.move_process(if down { 6 } else { -6 });
                } else {
                    self.scroll = if down {
                        self.scroll.saturating_add(6)
                    } else {
                        self.scroll.saturating_sub(6)
                    };
                }
            }
            _ if ch == Some('/') && self.tab == 2 => self.action(Action::Search),
            _ => (),
        }
        true
    }
    pub fn query_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes.iter().filter(|&&byte| byte >= 32 && byte != 127) {
            self.pending.push(byte);
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    let text = text.to_owned();
                    self.insert_query(&text);
                }
                Err(error) if error.error_len().is_some() => self.pending.clear(),
                Err(_) => (),
            }
        }
    }
    pub fn bytes(&mut self, bytes: &[u8]) -> bool {
        if !self.open {
            return false;
        }
        let symbol = match bytes {
            b"\x1b" => Some(0xff1b),
            b"\x1b[5~" => Some(0xff55),
            b"\x1b[6~" => Some(0xff56),
            b"\x1b[Z" => Some(0xfe20),
            _ => crate::input::editing_key(bytes),
        };
        if let Some(symbol) = symbol {
            self.key(symbol, "", false);
            return true;
        }
        if bytes.starts_with(b"\x1b") {
            return true;
        }
        for &byte in bytes {
            match byte {
                9 => {
                    self.key(0xff09, "", false);
                }
                8 | 127 => {
                    self.key(0xff08, "", false);
                }
                13 | 10 => {
                    self.key(0xff0d, "", false);
                }
                1 | 5 | 21 => {
                    self.key(
                        0,
                        match byte {
                            1 => "a",
                            5 => "e",
                            _ => "u",
                        },
                        true,
                    );
                }
                32..=255 if self.searching => self.query_bytes(&[byte]),
                32..=126 => {
                    self.key(u32::from(byte), "", false);
                }
                _ => (),
            }
        }
        true
    }
    fn step(&self, columns: u16) -> u32 {
        let sample = crate::system_stats::SAMPLE_INTERVAL.as_secs().max(1) as u32;
        let target = self.range.div_ceil(u32::from((columns / 10).clamp(1, 4)));
        [1, 2, 5, 10, 15, 30, 60, 120, 180, 300, 600, 900]
            .into_iter()
            .map(|n| n * sample)
            .find(|&n| n >= target)
            .unwrap_or(self.range)
    }
    fn graph(
        &self,
        view: &mut View,
        x: u16,
        y: i32,
        width: u16,
        height: u16,
        series: Vec<ResourceTrend>,
    ) {
        if width < 10 || series.is_empty() {
            return;
        }
        let maximum = series.iter().map(|s| s.maximum).fold(0., f32::max);
        let title = if series.len() == 2 {
            "NETWORK"
        } else {
            series[0].name
        };
        view.text(x, y, width, title, INK);
        let value = series
            .iter()
            .map(|s| {
                s.value.map_or_else(
                    || "—".into(),
                    |v| {
                        if s.unit == "MiB/s" && v < 1. {
                            format!("{:.0}KiB/s", v * 1024.)
                        } else {
                            format!("{v:.1}{}", s.unit)
                        }
                    },
                )
            })
            .collect::<Vec<_>>()
            .join(" / ");
        let value_width = (text_width(&value) as u16).min(width.saturating_sub(8));
        view.text(x + width - value_width, y, value_width, value, GREEN);
        view.text(
            x,
            y + 1,
            5,
            if maximum < 1. {
                format!("{maximum:.3}")
            } else {
                format!("{maximum:.0}")
            },
            MUTED,
        );
        view.text(x, y + i32::from(height), 5, "0", MUTED);
        let plot_x = x + 5;
        let plot_width = width - 5;
        let axis_y = y + i32::from(height) + 1;
        let mut age = if plot_width < 8 { 0 } else { self.range };
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
            let w = (text_width(&label) as u16).min(plot_width);
            view.text(plot_x + column.min(plot_width - w), axis_y, w, label, MUTED);
            if age == 0 {
                break;
            }
            age = if age == self.range {
                ((age - 1) / self.step(plot_width)) * self.step(plot_width)
            } else {
                age.saturating_sub(self.step(plot_width))
            };
        }
        let summary = if series.len() == 2 {
            "RX / TX · MiB/s · green / purple".to_string()
        } else {
            series[0].summary.clone()
        };
        view.text(x, axis_y + 1, width, summary, MUTED);
        view.plots.push(Plot {
            x: plot_x,
            y: y + 1,
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
            self.selected
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
        let mut view = View::default();
        if width < 12 || height < 3 {
            return view;
        }
        self.width = width;
        let side = if width >= 80 { 13 } else { 9 };
        let x = side + 1;
        let w = width.saturating_sub(x + 1);
        view.body_x = x;
        view.text(1, 0, 10, "SYSTEM", INK);
        if self.tab < 2 && width >= 50 {
            let start = width.saturating_sub(20).max(12);
            for (i, (range, label)) in [(60, "60s"), (300, "5m"), (900, "15m")]
                .into_iter()
                .enumerate()
            {
                view.button(
                    start + i as u16 * 6,
                    0,
                    5,
                    label,
                    Action::Range(range),
                    self.range == range,
                );
            }
        }
        let labels = if width >= 80 {
            ["Overview", "Network", "Processes", "Disk/Swap"]
        } else {
            ["System", "Net", "Tasks", "Disk"]
        };
        for (i, label) in labels.into_iter().enumerate() {
            if 2 + (i as u16 * 2) < height {
                view.button(
                    1,
                    2 + i as i32 * 2,
                    side - 1,
                    label,
                    Action::Tab(i as u8),
                    self.tab == i as u8,
                );
            }
        }
        let mut body = View::default();
        match self.tab {
            0 => {
                let trends = self.stats.system_trends(self.range);
                let columns = if w >= 54 { 2 } else { 1 };
                let cw = (w - (columns - 1) * 2) / columns;
                for (i, trend) in trends.into_iter().enumerate() {
                    self.graph(
                        &mut body,
                        x + (i as u16 % columns) * (cw + 2),
                        (i as u16 / columns * 8) as i32,
                        cw,
                        4,
                        vec![trend],
                    );
                }
                let y = if columns == 2 { 8 } else { 16 };
                let mut rows = self.stats.overview();
                if w >= 70 {
                    let storage = rows.split_off(6.min(rows.len()));
                    let cw = (w - 3) / 2;
                    body.details(x, y, cw, rows);
                    body.details(x + cw + 3, y, w - cw - 3, storage);
                } else {
                    body.details(x, y, w, rows);
                }
            }
            1 => {
                let name = self
                    .interface
                    .as_deref()
                    .or_else(|| {
                        self.stats
                            .networks()
                            .iter()
                            .find(|n| n.name != "lo")
                            .or(self.stats.networks().first())
                            .map(|n| n.name.as_str())
                    })
                    .unwrap_or("—");
                body.button(
                    x,
                    0,
                    w,
                    &format!("Interface: {name}  ›"),
                    Action::Interface,
                    false,
                );
                let series = self.stats.network_trends(self.range, name);
                let graph_height = height.saturating_sub(10).clamp(3, 9);
                self.graph(&mut body, x, 2, w, graph_height, series);
                body.details(
                    x,
                    i32::from(graph_height) + 6,
                    w,
                    self.stats.network_details(Some(name)),
                );
            }
            2 => self.process_view(&mut body, x, w),
            _ => body.details(x, 0, w, self.stats.storage()),
        }
        let content_height = body
            .texts
            .iter()
            .map(|t| t.y + 1)
            .chain(body.hits.iter().map(|hit| hit.y + 1))
            .max()
            .unwrap_or(0)
            .max(
                body.plots
                    .iter()
                    .map(|p| p.y + i32::from(p.height))
                    .max()
                    .unwrap_or(0),
            );
        if let Action::Process(pid) = self.focus
            && !body.hits.iter().any(|hit| hit.action == self.focus)
        {
            self.focus = Action::Search;
            if self.selected == Some(pid) {
                self.selected = None;
            }
            self.reveal = true;
        }
        self.follow_process(&body.hits);
        if self.reveal {
            if let Some(hit) = body.hits.iter().find(|hit| hit.action == self.focus) {
                let visible = i32::from(height).saturating_sub(2).max(1);
                let bottom = hit.y
                    + if self
                        .selected
                        .is_some_and(|pid| hit.action == Action::Process(pid))
                    {
                        4
                    } else {
                        1
                    };
                if hit.y < self.scroll as i32 {
                    self.scroll = hit.y.max(0) as usize;
                }
                if bottom > self.scroll as i32 + visible {
                    self.scroll = bottom.saturating_sub(visible).max(0) as usize;
                }
            }
            self.reveal = false;
        }
        self.scroll = self
            .scroll
            .min(content_height.saturating_sub(i32::from(height) - 2).max(0) as usize);
        let offset = 2 - self.scroll as i32;
        for mut text in body.texts {
            text.y += offset;
            if text.y >= 2 && text.y < i32::from(height) {
                view.texts.push(text);
            }
        }
        for mut plot in body.plots {
            plot.y += offset;
            view.plots.push(plot);
        }
        for mut hit in body.hits {
            hit.y += offset;
            if hit.y >= 2 && hit.y < i32::from(height) {
                view.hits.push(hit);
            }
        }
        view.texts.retain(|text| text.x < width);
        for text in &mut view.texts {
            text.width = text.width.min(width - text.x);
        }
        view.hits.retain(|hit| hit.x < width);
        for hit in &mut view.hits {
            hit.width = hit.width.min(width - hit.x);
        }
        // Keep every process hit for focus anchoring; allocate text only for visible rows.
        if view
            .hits
            .iter()
            .any(|hit| matches!(hit.action, Action::Process(_)))
        {
            let columns = process_columns(w, self.sort);
            for hit in &view.hits {
                if let Action::Process(pid) = hit.action {
                    let processes = self.stats.processes();
                    let process =
                        &processes[processes.binary_search_by_key(&pid, |p| p.pid).unwrap()];
                    let selected = self.selected == Some(pid);
                    view.texts.push(Text {
                        x: hit.x,
                        y: hit.y,
                        width: hit.width,
                        text: process_row(process, w, &columns),
                        color: if selected { GREEN } else { INK },
                        selected,
                        focused: false,
                        highlighted: false,
                    });
                }
            }
        }
        if let Some(hit) = view.hits.iter().find(|hit| hit.action == self.focus)
            && let Some(text) = view
                .texts
                .iter_mut()
                .find(|text| text.x == hit.x && text.y == hit.y && text.width == hit.width)
        {
            text.focused = true;
        }
        if let Some((x, y)) = self.pointer
            && let Some(hit) = view
                .hits
                .iter()
                .rev()
                .find(|hit| hit.y == y && (hit.x..hit.x + hit.width).contains(&x))
            && let Some(text) = view
                .texts
                .iter_mut()
                .find(|text| text.x == hit.x && text.y == hit.y && text.width == hit.width)
        {
            if !text.selected {
                text.color = INK;
            }
            if matches!(hit.action, Action::Process(_)) {
                text.selected = true;
            } else {
                text.highlighted = true;
            }
        }
        view
    }
    fn process_view(&self, body: &mut View, x: u16, width: u16) {
        let before = &self.query[..self.cursor];
        let mut start = 0;
        let mut columns = text_width(before);
        for (i, ch) in before.char_indices() {
            if columns <= usize::from(width.saturating_sub(12)) {
                break;
            }
            columns = columns.saturating_sub(usize::from(scalar_width(ch).min(2)));
            start = i + ch.len_utf8();
        }
        let filter = if self.searching {
            format!(
                "⌕ {}▏{}",
                &self.query[start..self.cursor],
                &self.query[self.cursor..]
            )
        } else {
            format!("⌕ {}  / to search", self.query)
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
        let show_io = columns.iter().any(|&(id, _, _)| id == 4);
        let mut column = x + 1;
        for &(id, label, cells) in &columns {
            if id >= 2 {
                let sort = id - 2;
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
                    cells + 2,
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
        if processes.is_empty() {
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
            if self.selected == Some(p.pid) {
                y = body.lines(
                    x + 1,
                    y,
                    width.saturating_sub(2),
                    &format!(
                        "PID {} · PPID {} · UID {} · {} · {} threads",
                        p.pid, p.parent, p.uid, p.state, p.threads
                    ),
                    GREEN,
                );
                y = body.lines(x + 1, y, width.saturating_sub(2), &p.command, INK);
                y = body.lines(
                    x + 1,
                    y,
                    width.saturating_sub(2),
                    &format!(
                        "cwd {}",
                        p.directory
                            .as_ref()
                            .map_or_else(|| "unavailable".into(), |d| d.display().to_string())
                    ),
                    MUTED,
                );
                if !show_io {
                    y = body.lines(
                        x + 1,
                        y,
                        width.saturating_sub(2),
                        &format!(
                            "I/O read/write: {} / {} MiB/s",
                            p.read
                                .map_or_else(|| "—".into(), |v| format!("{:.2}", v / 1048576.)),
                            p.write
                                .map_or_else(|| "—".into(), |v| format!("{:.2}", v / 1048576.))
                        ),
                        MUTED,
                    );
                }
                y += 1;
            }
        }
    }
}

fn process_columns(width: u16, sort: u8) -> Vec<(u8, &'static str, u16)> {
    let inner = width.saturating_sub(2);
    let show_rss = inner >= 32;
    let show_io = inner >= 46 && (width >= 70 || sort == 2);
    let name_width = inner
        .saturating_sub(7 + 6 + 2 + if show_rss { 9 } else { 0 } + if show_io { 10 } else { 0 });
    let mut columns = vec![(0, "PID", 7), (1, "COMMAND", name_width), (2, "CPU%", 6)];
    if show_rss {
        columns.push((3, "RSS MiB", 8));
    }
    if show_io {
        columns.push((4, "I/O MiB/s", 9));
    }
    columns
}

fn process_row(p: &Process, width: u16, columns: &[(u8, &str, u16)]) -> String {
    let mut row = String::with_capacity(usize::from(width));
    for (index, &(id, _, cells)) in columns.iter().enumerate() {
        if index > 0 {
            row.push(' ');
        }
        let field: Cow<'_, str> = match id {
            0 => p.pid.to_string().into(),
            1 => p.name.as_str().into(),
            2 => p
                .cpu
                .map_or_else(|| "—".into(), |n| format!("{n:.1}").into()),
            3 => format!("{:.1}", p.rss as f64 / 1048576.).into(),
            4 => p.read.zip(p.write).map_or_else(
                || "—".into(),
                |(a, b)| format!("{:.2}", (a + b) / 1048576.).into(),
            ),
            _ => unreachable!(),
        };
        let value = fit_label(&field, cells);
        row.push_str(&value);
        row.extend(std::iter::repeat_n(
            ' ',
            usize::from(cells).saturating_sub(text_width(&value)),
        ));
    }
    format!(" {} ", fit_label(&row, width.saturating_sub(2)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_focus_unicode_and_tabs_do_not_consume_terminal_input() {
        let mut monitor = Monitor::new().unwrap();
        assert!(!monitor.bytes(b"terminal"));
        monitor.toggle();
        monitor.action(Action::Tab(2));
        monitor.action(Action::Search);
        for byte in "中rustc".as_bytes() {
            assert!(monitor.bytes(&[*byte]));
        }
        assert_eq!(monitor.query, "中rustc");
        monitor.bytes(&[127]);
        assert_eq!(monitor.query, "中rust");
        monitor.bytes(&[21]);
        assert!(monitor.query.is_empty());
        monitor.bytes(b"\x1b");
        assert!(!monitor.searching);
        assert!(monitor.open);
        monitor.bytes(b"\x1b");
        assert!(!monitor.open);
        assert!(!monitor.bytes(b"terminal"));
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
        let view = monitor.view(80, 18);
        assert!(
            view.hits
                .iter()
                .any(|hit| matches!(hit.action, Action::Range(300)))
        );
    }
    #[test]
    fn controls_keep_cell_padding_and_highlight_the_hovered_hit() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        let view = monitor.view(120, 18);
        let hit = view
            .hits
            .iter()
            .find(|hit| hit.action == Action::Tab(1))
            .unwrap();
        let label = view
            .texts
            .iter()
            .find(|text| text.x == hit.x && text.y == hit.y)
            .unwrap();
        assert_eq!(label.text, " Network ");
        assert_eq!(hit.width, 9);
        assert!(!label.selected);
        monitor.pointer = Some((hit.x, hit.y));
        let view = monitor.view(120, 18);
        let label = view
            .texts
            .iter()
            .find(|text| text.text == " Network ")
            .unwrap();
        assert!(!label.selected);
        assert!(label.highlighted);
        monitor.pointer = None;
        assert!(
            !monitor
                .view(120, 18)
                .texts
                .iter()
                .find(|text| text.text == " Network ")
                .unwrap()
                .highlighted
        );
    }
    #[test]
    fn ansi_navigation_sequences_never_become_escape_or_filter_text() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        for sequence in [
            b"\x1b[D".as_slice(),
            b"\x1b[C",
            b"\x1b[H",
            b"\x1b[F",
            b"\x1b[3~",
            b"\x1b[1;5D",
            b"\x1bOP",
        ] {
            monitor.bytes(sequence);
            assert!(monitor.open);
        }
        monitor.action(Action::Tab(2));
        monitor.bytes(b"/");
        assert!(monitor.searching);
        monitor.query_bytes("中文".as_bytes());
        monitor.bytes(b"\x1b[D");
        monitor.query_bytes(b"X");
        assert_eq!(monitor.query, "中X文");
        monitor.bytes(b"\x1b[1;5D");
        assert!(monitor.searching);
        assert!(monitor.open);
        assert_eq!(monitor.query, "中X文");
        monitor.bytes(b"\x1b[3~");
        assert_eq!(monitor.query, "中X");
        monitor.bytes(b"\x1b");
        assert!(!monitor.searching);
        assert!(monitor.open);
    }
    #[test]
    fn keyboard_control_cycle_activates_tabs_ranges_and_filter() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        monitor.view(80, 18);
        monitor.bytes(b"\t");
        assert_eq!(monitor.focus, Action::Tab(1));
        monitor.bytes(b"\r");
        assert_eq!(monitor.tab, 1);
        monitor.bytes(b"\x1b[Z");
        assert_eq!(monitor.focus, Action::Tab(0));
        monitor.bytes(b"\x1b[B");
        assert_eq!(monitor.tab, 1);
        monitor.bytes(b"\x1b[B");
        assert_eq!(monitor.tab, 2);
        monitor.bytes(b"/");
        assert!(monitor.searching);
        monitor.bytes(b"\t");
        assert!(!monitor.searching);
        assert_eq!(monitor.focus, Action::Sort(0));
        monitor.key(0xff0d, "", false);
        assert!(!monitor.descending);
        let view = monitor.view(60, 12);
        assert!(view.texts.iter().any(|text| text.focused));
        assert!(view.texts.iter().any(|text| text.text == " Tasks "));
        assert!(!view.hits.iter().any(|hit| hit.action == Action::Sort(2)));
    }
    #[test]
    fn process_reading_anchor_tracks_pid_when_live_sort_moves_it() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        monitor.focus = Action::Process(42);
        monitor.scroll = 8;
        let hit = |y| Hit {
            x: 0,
            y,
            width: 20,
            action: Action::Process(42),
        };
        monitor.follow_process(&[hit(10)]);
        assert_eq!(monitor.scroll, 8);
        monitor.follow_process(&[hit(15)]);
        assert_eq!(monitor.scroll, 13);
        monitor.follow_process(&[hit(12)]);
        assert_eq!(monitor.scroll, 10);
        monitor.follow_process(&[]);
        assert!(monitor.anchor.is_none());
    }
    #[test]
    fn small_views_and_drag_bounds_preserve_the_terminal() {
        let mut monitor = Monitor::new().unwrap();
        monitor.toggle();
        for lines in [8, 12, 30] {
            monitor.resize(100, lines);
            assert!(monitor.rows(lines) <= lines - 6);
        }
        for tab in 0..4 {
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
