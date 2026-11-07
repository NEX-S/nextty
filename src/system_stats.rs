//! Asynchronous local resource samples and one hour of history.

use std::{
    collections::{HashMap, VecDeque},
    ffi::CString,
    fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub struct SystemStats {
    sampler: Arc<Sampler>,
    sample: Sample,
    process_revision: u64,
    directory: Option<PathBuf>,
    summary: [String; 4],
}

pub struct ResourceTrend {
    pub name: &'static str,
    pub value: Option<f32>,
    pub summary: String,
    pub points: Vec<(f32, Option<f32>)>,
    pub maximum: f32,
    pub unit: &'static str,
}

#[derive(Clone)]
pub struct Network {
    pub name: String,
    pub rx: Option<f64>,
    pub tx: Option<f64>,
    pub received: u64,
    pub transmitted: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

impl Network {
    pub fn details(&self) -> [(&'static str, String); 3] {
        [
            (
                "Total RX / TX",
                format!(
                    "{:.2} GiB / {:.2} GiB",
                    self.received as f64 / 1073741824.,
                    self.transmitted as f64 / 1073741824.
                ),
            ),
            (
                "Errors RX / TX",
                format!("{} / {}", self.rx_errors, self.tx_errors),
            ),
            (
                "Dropped RX / TX",
                format!("{} / {}", self.rx_dropped, self.tx_dropped),
            ),
        ]
    }
}

pub struct Process {
    pub pid: u32,
    pub(crate) start: u64,
    pub name: Box<str>,
    pub uid: u32,
    pub state: char,
    pub cpu: Option<f32>,
    pub rss: u64,
    pub read: Option<f64>,
    pub write: Option<f64>,
    pub threads: u32,
    pub parent: u32,
    pub command: Box<str>,
    pub directory: Option<Box<Path>>,
}

struct ProcessDetails {
    identity: (u32, u64),
    command: String,
    directory: Option<PathBuf>,
}

type NetworkRates = (Arc<str>, [Option<f32>; 2]);

struct TrendPoint {
    at: Instant,
    values: [Option<f32>; 7],
    networks: Box<[NetworkRates]>,
}

struct Sampler {
    state: Mutex<State>,
    changed: Condvar,
    wakeup: OwnedFd,
}

#[derive(Default)]
struct State {
    directory: Option<PathBuf>,
    processes_enabled: bool,
    expanded: bool,
    process_detail: Option<(u32, u64)>,
    detail_result: Option<ProcessDetails>,
    result: Option<Sample>,
    history: VecDeque<TrendPoint>,
    initialized: bool,
    closed: bool,
}

#[derive(Default)]
struct Sample {
    directory: Option<PathBuf>,
    branch: Option<String>,
    cpu: Option<f32>,
    iowait: Option<f32>,
    cores: Option<u64>,
    load: Option<[f64; 3]>,
    tasks: Option<String>,
    uptime: Option<u64>,
    pressure: [Option<f32>; 3],
    memory: Option<Usage>,
    memory_parts: [Option<u64>; 3],
    cpu_cores: Vec<(Arc<str>, Option<f32>)>,
    swap: Option<Usage>,
    swap_rates: [Option<f64>; 2],
    disk: Option<Usage>,
    disks: Vec<Disk>,
    networks: Vec<Network>,
    processes: Vec<Process>,
}

#[derive(Clone, Copy)]
struct Usage {
    used: u64,
    total: u64,
    available: u64,
    inodes: Option<f64>,
}

#[derive(Clone, Copy)]
struct CpuCounters {
    total: u64,
    idle: u64,
    iowait: u64,
}

#[derive(Clone, Copy)]
struct ProcessCounters {
    ticks: u64,
    io: Option<[u64; 2]>,
}

pub struct Disk {
    pub name: String,
    pub rates: [Option<f64>; 2],
    pub utilization: Option<f64>,
}

struct DiskCounters {
    bytes: [u64; 2],
    busy_ms: u64,
}

fn percent(value: Option<f32>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:.0}%"))
}
fn capacity(usage: Option<Usage>) -> String {
    usage.map_or_else(
        || "—".into(),
        |u| {
            if u.total == 0 {
                return "disabled".into();
            }
            format!(
                "{:.1} / {:.1} GiB",
                u.used as f64 / 1073741824.,
                u.total as f64 / 1073741824.
            )
        },
    )
}
fn bytes(value: u64) -> String {
    format!("{:.2} GiB", value as f64 / 1073741824.)
}

impl SystemStats {
    pub fn new() -> io::Result<Self> {
        let sampler = Arc::new(Sampler {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            wakeup: crate::eventfd::new()?,
        });
        let worker = Arc::clone(&sampler);
        thread::Builder::new()
            .name("nextty-stats".into())
            .spawn(move || sample_resources(worker))?;
        Ok(Self {
            sampler,
            sample: Sample::default(),
            process_revision: 0,
            directory: None,
            summary: [" —", " —", " —", " —"].map(str::to_owned),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.sampler.wakeup.as_raw_fd()
    }

    pub fn set_processes_enabled(&mut self, enabled: bool) {
        self.sampler.state.lock().unwrap().processes_enabled = enabled;
    }

    #[cfg(any(feature = "gui", feature = "tui", test))]
    pub(crate) fn process_revision(&self) -> u64 {
        self.process_revision
    }

    #[cfg(any(feature = "gui", feature = "tui", test))]
    pub(crate) fn set_process_detail(&mut self, identity: Option<(u32, u64)>) {
        let mut state = self.sampler.state.lock().unwrap();
        if state.process_detail != identity {
            state.process_detail = identity;
            self.sampler.changed.notify_one();
        }
    }

    pub fn refresh(&mut self, directory: Option<&Path>, expanded: bool) -> io::Result<bool> {
        let mut state = self.sampler.state.lock().unwrap();
        state.expanded = expanded;
        let directory_changed = state.directory.as_deref() != directory;
        if directory_changed {
            state.directory = directory.map(Path::to_path_buf);
            self.directory = state.directory.clone();
        }
        if !state.initialized {
            state.initialized = true;
            self.sampler.changed.notify_one();
        }
        let sampled = state.result.is_some() || state.detail_result.is_some();
        if sampled {
            crate::eventfd::consume_notification(self.fd())?;
        }
        let result = state.result.take();
        let detail = state.detail_result.take();
        drop(state);
        if !sampled && !directory_changed {
            return Ok(false);
        }
        let update_summary = result.is_some() || directory_changed;
        let branch_changed = result
            .as_ref()
            .is_some_and(|sample| sample.branch != self.sample.branch);
        if let Some(sample) = result {
            self.sample = sample;
            self.process_revision += 1;
        }
        if let Some(detail) = detail
            && let Ok(index) = self
                .sample
                .processes
                .binary_search_by_key(&detail.identity, |p| (p.pid, p.start))
        {
            let process = &mut self.sample.processes[index];
            process.command = detail.command.into_boxed_str();
            process.directory = detail.directory.map(PathBuf::into_boxed_path);
        }
        if !update_summary {
            return Ok(expanded);
        }
        let values = sample_values(&self.sample);
        let disk = self.disk().and_then(|u| {
            let total = u.used + u.available;
            (total > 0).then(|| u.used as f32 * 100. / total as f32)
        });
        let summary = [
            (" ", values[0]),
            (" ", values[1]),
            (" ", disk),
            (" ", values[4]),
        ]
        .map(|(label, value)| {
            use std::fmt::Write;
            let mut text = String::with_capacity(8);
            text.push_str(label);
            match value {
                Some(value) => write!(text, "{value:.0}%").unwrap(),
                None => text.push('—'),
            }
            text
        });
        let changed =
            branch_changed || directory_changed || summary != self.summary || expanded && sampled;
        self.summary = summary;
        Ok(changed)
    }

    fn disk(&self) -> Option<Usage> {
        self.sample
            .disk
            .filter(|_| self.sample.directory == self.directory)
    }

    pub fn branch(&self) -> Option<&str> {
        self.sample
            .branch
            .as_deref()
            .filter(|_| self.sample.directory == self.directory)
    }

    pub fn networks(&self) -> &[Network] {
        &self.sample.networks
    }
    pub fn processes(&self) -> &[Process] {
        &self.sample.processes
    }

    pub fn system_trends(
        &self,
        seconds: u32,
        indices: std::ops::Range<usize>,
    ) -> Vec<ResourceTrend> {
        let values = sample_values(&self.sample);
        let mut result: Vec<_> = indices
            .clone()
            .map(|index| ResourceTrend {
                name: ["CPU", "MEMORY", "READ", "WRITE", "SWAP", "IN", "OUT"][index],
                value: values[index],
                summary: match index {
                    0 => self.sample.cores.map_or_else(
                        || "— logical cores".into(),
                        |cores| format!("{cores} logical cores"),
                    ),
                    1 => capacity(self.sample.memory),
                    2 | 3 => "Physical disks · read / write".into(),
                    4 => capacity(self.sample.swap),
                    _ => "Swap in / out".into(),
                },
                points: Vec::new(),
                maximum: 100.,
                unit: if matches!(index, 2 | 3 | 5 | 6) {
                    "MiB/s"
                } else {
                    "%"
                },
            })
            .collect();
        {
            let state = self.sampler.state.lock().unwrap();
            let now = Instant::now();
            let start = state.history.partition_point(|point| {
                now.saturating_duration_since(point.at).as_secs_f32() > seconds as f32
            });
            let history = state.history.range(start..);
            if let Some((first, rest)) = result.split_first_mut() {
                first.points = history
                    .clone()
                    .map(|point| {
                        (
                            now.saturating_duration_since(point.at).as_secs_f32(),
                            point.values[indices.start],
                        )
                    })
                    .collect();
                for (index, trend) in (indices.start + 1..indices.end).zip(rest) {
                    trend.points = history
                        .clone()
                        .zip(&first.points)
                        .map(|(point, (age, _))| (*age, point.values[index]))
                        .collect();
                }
            }
        }
        for trend in &mut result {
            if trend.unit == "MiB/s" {
                let peak = trend
                    .points
                    .iter()
                    .filter_map(|(_, value)| *value)
                    .fold(0.125f32, f32::max);
                trend.maximum = 2f32.powf(peak.log2().ceil());
            }
        }
        result
    }

    pub fn network_trends(&self, seconds: u32, network: &Network) -> Vec<ResourceTrend> {
        let state = self.sampler.state.lock().unwrap();
        let now = Instant::now();
        let start = state.history.partition_point(|point| {
            now.saturating_duration_since(point.at).as_secs_f32() > seconds as f32
        });
        let history = state.history.range(start..);
        let mut points: [Vec<(f32, Option<f32>)>; 2] =
            std::array::from_fn(|_| Vec::with_capacity(history.len()));
        let mut peak = 0.125f32;
        let mut index = 0;
        for point in history {
            let age = now.saturating_duration_since(point.at).as_secs_f32();
            // Sorted samples usually keep an interface at the same position.
            if point
                .networks
                .get(index)
                .is_none_or(|(name, _)| name.as_ref() != network.name.as_str())
            {
                index = point
                    .networks
                    .iter()
                    .position(|(name, _)| name.as_ref() == network.name.as_str())
                    .unwrap_or(point.networks.len());
            }
            let values = point
                .networks
                .get(index)
                .map(|(_, values)| *values)
                .unwrap_or([None; 2]);
            for (points, value) in points.iter_mut().zip(values) {
                peak = value.map_or(peak, |value| peak.max(value));
                points.push((age, value));
            }
        }
        drop(state);
        let maximum = 2f32.powf(peak.log2().ceil());
        [("RX", network.rx), ("TX", network.tx)]
            .into_iter()
            .zip(points)
            .map(|((name, value), points)| ResourceTrend {
                name,
                value: value.map(|v| (v / 1048576.) as f32),
                summary: {
                    use std::fmt::Write;
                    let mut text = String::with_capacity(network.name.len() + 24);
                    text.push_str(&network.name);
                    text.push_str(" · ");
                    match value {
                        Some(value) if value >= 1048576. => {
                            write!(text, "{:.2} MiB/s", value / 1048576.).unwrap();
                        }
                        Some(value) => write!(text, "{:.1} KiB/s", value / 1024.).unwrap(),
                        None => text.push('—'),
                    }
                    text
                },
                points,
                maximum,
                unit: "MiB/s",
            })
            .collect()
    }

    pub fn overview(&self) -> [(&'static str, String); 2] {
        [
            ("Load · 1/5/15 min", self.load()),
            (
                "Uptime",
                self.sample.uptime.map_or_else(
                    || "—".into(),
                    |s| format!("{}d {:02}h {:02}m", s / 86400, s / 3600 % 24, s / 60 % 60),
                ),
            ),
        ]
    }
    fn load(&self) -> String {
        self.sample.load.map_or_else(
            || "—".into(),
            |v| format!("{:.2} / {:.2} / {:.2}", v[0], v[1], v[2]),
        )
    }
    pub fn cpu_details(&self) -> [(&'static str, String); 4] {
        [
            ("Load · 1/5/15 min", self.load()),
            ("I/O wait", percent(self.sample.iowait)),
            ("CPU pressure · 10s", percent(self.sample.pressure[0])),
            (
                "Tasks · running/total",
                self.sample.tasks.clone().unwrap_or_else(|| "—".into()),
            ),
        ]
    }
    pub fn cpu_cores(&self) -> &[(Arc<str>, Option<f32>)] {
        &self.sample.cpu_cores
    }
    pub fn memory_details(&self) -> [(&'static str, String); 5] {
        let [free, cached, buffers] = self
            .sample
            .memory_parts
            .map(|part| part.map_or_else(|| "—".into(), bytes));
        [
            (
                "Available",
                self.sample
                    .memory
                    .map_or_else(|| "—".into(), |u| bytes(u.available)),
            ),
            ("Free", free),
            ("Cached", cached),
            ("Buffers", buffers),
            ("Memory pressure · 10s", percent(self.sample.pressure[1])),
        ]
    }
    pub fn disks(&self) -> &[Disk] {
        &self.sample.disks
    }
    pub fn disk_details(&self) -> [(&'static str, String); 4] {
        [
            (
                "Filesystem path",
                self.directory
                    .as_ref()
                    .map_or_else(|| "—".into(), |p| p.display().to_string()),
            ),
            ("Used / total", capacity(self.disk())),
            ("I/O pressure · 10s", percent(self.sample.pressure[2])),
            (
                "Available / inodes",
                self.disk().map_or_else(
                    || "—".into(),
                    |u| {
                        let available = u.available as f64 / 1073741824.;
                        match u.inodes {
                            Some(inodes) => format!("{available:.2} GiB / {inodes:.1}%"),
                            None => format!("{available:.2} GiB / —"),
                        }
                    },
                ),
            ),
        ]
    }
    pub fn swap_disabled(&self) -> bool {
        self.sample.swap.is_some_and(|usage| usage.total == 0)
    }

    pub fn summary(&self) -> &[String; 4] {
        &self.summary
    }
}

impl Drop for SystemStats {
    fn drop(&mut self) {
        self.sampler.state.lock().unwrap().closed = true;
        self.sampler.changed.notify_one();
        // A filesystem call may not return promptly; the worker owns its remaining lifetime.
    }
}

fn sample_resources(sampler: Arc<Sampler>) {
    let mut previous_cpu: Option<CpuCounters> = None;
    let mut physical_networks = HashMap::new();
    let mut previous_net: HashMap<Arc<str>, (u64, u64)> = HashMap::new();
    let mut previous_disk: HashMap<String, DiskCounters> = HashMap::new();
    let mut previous_cores: HashMap<Arc<str>, CpuCounters> = HashMap::new();
    let mut previous_swap: Option<[u64; 2]> = None;
    let mut previous_processes = HashMap::new();
    let mut previous_at: Option<Instant> = None;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let mut state = sampler.state.lock().unwrap();
    while !state.initialized && !state.closed {
        state = sampler.changed.wait(state).unwrap();
    }
    while !state.closed {
        let start = Instant::now();
        let elapsed = previous_at.map(|at| start.duration_since(at).as_secs_f64());
        let directory = state.directory.clone();
        let expanded = state.expanded;
        let processes_enabled = state.processes_enabled;
        let mut process_detail = state.process_detail;
        drop(state);
        let mut sample = Sample {
            directory,
            ..Sample::default()
        };
        sample.branch = sample.directory.as_deref().and_then(git_branch);
        let mut text = String::with_capacity(8192);
        let cpu_text = read_text("/proc/stat", &mut text).ok();
        let cpu = cpu_text
            .and_then(|text| text.lines().next())
            .and_then(cpu_counters)
            .map(|(_, counters)| counters);
        if expanded {
            if let Some(text) = cpu_text {
                let mut counters = HashMap::with_capacity(previous_cores.len());
                sample.cpu_cores.reserve(previous_cores.len());
                for line in text
                    .lines()
                    .skip(1)
                    .take_while(|line| line.starts_with("cpu"))
                {
                    if let Some((name, new)) = cpu_counters(line) {
                        let (name, old) = previous_cores.remove_entry(name).map_or_else(
                            || (Arc::from(name), None),
                            |(name, old)| (name, Some(old)),
                        );
                        let value = old.and_then(|old| {
                            counter_percent(new.total, old.total, new.idle, old.idle)
                                .map(|idle| 100. - idle)
                        });
                        sample.cpu_cores.push((Arc::clone(&name), value));
                        counters.insert(name, new);
                    }
                }
                previous_cores = counters;
            }
        } else {
            previous_cores = HashMap::new();
        }
        if let Some((old, new)) = previous_cpu.zip(cpu) {
            sample.cpu =
                counter_percent(new.total, old.total, new.idle, old.idle).map(|idle| 100. - idle);
            sample.iowait = counter_percent(new.total, old.total, new.iowait, old.iowait);
        }
        previous_cpu = cpu;
        if expanded {
            sample.cores = match unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } {
                n if n > 0 => Some(n as u64),
                _ => None,
            };
            if let Ok(text) = read_text("/proc/loadavg", &mut text) {
                let mut fields = text.split_ascii_whitespace();
                let load = [fields.next(), fields.next(), fields.next()];
                sample.load = (|| {
                    Some([
                        load[0]?.parse().ok()?,
                        load[1]?.parse().ok()?,
                        load[2]?.parse().ok()?,
                    ])
                })();
                sample.tasks = fields.next().map(str::to_owned);
            }
            sample.uptime = read_text("/proc/uptime", &mut text).ok().and_then(|text| {
                text.split_ascii_whitespace()
                    .next()?
                    .split('.')
                    .next()?
                    .parse()
                    .ok()
            });
            for (value, path) in sample.pressure.iter_mut().zip([
                "/proc/pressure/cpu",
                "/proc/pressure/memory",
                "/proc/pressure/io",
            ]) {
                *value = read_text(path, &mut text).ok().and_then(pressure);
            }
        }
        if let Ok(text) = read_text("/proc/meminfo", &mut text) {
            sample_memory(text, &mut sample);
        }
        if expanded {
            let swap = read_text("/proc/vmstat", &mut text)
                .ok()
                .and_then(|text| counter_pair(text, ["pswpin", "pswpout"]));
            if let Some((old, new)) = previous_swap.zip(swap) {
                sample.swap_rates = std::array::from_fn(|i| {
                    per_second(new[i], old[i], elapsed).map(|v| v * page_size as f64)
                });
            }
            previous_swap = swap;
        } else {
            previous_swap = None;
        }
        sample.disk = sample
            .directory
            .as_deref()
            .and_then(|path| disk_usage(path).ok());
        if expanded {
            (sample.disks, previous_disk) =
                sample_physical_disks(previous_disk, elapsed, &mut text);
        } else {
            previous_disk = HashMap::new();
        }
        let mut network_history = Vec::new();
        if let Ok(text) = read_text("/proc/net/dev", &mut text) {
            sample.networks = sample_physical_networks(text, &mut physical_networks);
            network_history.reserve_exact(sample.networks.len());
            previous_net = sample
                .networks
                .iter_mut()
                .map(|net| {
                    let name = if let Some((name, (rx, tx))) =
                        previous_net.remove_entry(net.name.as_str())
                    {
                        net.rx = per_second(net.received, rx, elapsed);
                        net.tx = per_second(net.transmitted, tx, elapsed);
                        name
                    } else {
                        Arc::from(net.name.as_str())
                    };
                    network_history.push((
                        Arc::clone(&name),
                        [
                            net.rx.map(|v| (v / 1048576.) as f32),
                            net.tx.map(|v| (v / 1048576.) as f32),
                        ],
                    ));
                    (name, (net.received, net.transmitted))
                })
                .collect();
        } else {
            previous_net.clear();
        }
        // Do not retain the /proc buffer through process sampling or the wait.
        drop(text);
        if processes_enabled {
            let (processes, counters) = sample_processes(
                &previous_processes,
                elapsed,
                ticks_per_second,
                page_size,
                process_detail,
            );
            sample.processes = processes;
            previous_processes = counters;
        } else {
            previous_processes = HashMap::new();
        }
        previous_at = Some(start);
        let point = TrendPoint {
            at: start,
            values: sample_values(&sample),
            networks: network_history.into_boxed_slice(),
        };
        state = sampler.state.lock().unwrap();
        if state.closed {
            return;
        }
        record_history(&mut state.history, point);
        let previous = (state.result.replace(sample), state.detail_result.take());
        if let Err(error) = crate::eventfd::notify(sampler.wakeup.as_raw_fd()) {
            eprintln!("notify system statistics: {error}");
            return;
        }
        drop(state);
        drop(previous);
        state = sampler.state.lock().unwrap();
        while !state.closed {
            let remaining = (start + SAMPLE_INTERVAL).saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if state.process_detail != process_detail {
                process_detail = state.process_detail;
                drop(state);
                let detail =
                    process_detail.and_then(|identity| process_details(identity, page_size));
                state = sampler.state.lock().unwrap();
                if let Some(detail) = detail {
                    state.detail_result = Some(detail);
                    if let Err(error) = crate::eventfd::notify(sampler.wakeup.as_raw_fd()) {
                        eprintln!("notify system statistics: {error}");
                        return;
                    }
                }
                continue;
            }
            state = sampler.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

fn per_second(current: u64, previous: u64, elapsed: Option<f64>) -> Option<f64> {
    let seconds = elapsed.filter(|v| *v > 0.)?;
    Some(current.checked_sub(previous)? as f64 / seconds)
}
fn counter_percent(total: u64, old_total: u64, counter: u64, old_counter: u64) -> Option<f32> {
    let total = total.checked_sub(old_total)?;
    let counter = counter.checked_sub(old_counter)?;
    (total > 0).then(|| counter as f32 * 100. / total as f32)
}
fn sample_values(sample: &Sample) -> [Option<f32>; 7] {
    let disk_rates: [Option<f64>; 2] = std::array::from_fn(|i| {
        if sample.disks.is_empty() {
            None
        } else {
            sample.disks.iter().map(|disk| disk.rates[i]).sum()
        }
    });
    [
        sample.cpu,
        sample.memory.map(|u| u.used as f32 * 100. / u.total as f32),
        disk_rates[0].map(|v| (v / 1048576.) as f32),
        disk_rates[1].map(|v| (v / 1048576.) as f32),
        sample
            .swap
            .filter(|u| u.total > 0)
            .map(|u| u.used as f32 * 100. / u.total as f32),
        sample.swap_rates[0].map(|v| (v / 1048576.) as f32),
        sample.swap_rates[1].map(|v| (v / 1048576.) as f32),
    ]
}
fn record_history(history: &mut VecDeque<TrendPoint>, point: TrendPoint) {
    while history
        .front()
        .is_some_and(|old| point.at.duration_since(old.at) > Duration::from_secs(3600))
    {
        history.pop_front();
    }
    if history
        .back()
        .is_some_and(|old| point.at.duration_since(old.at) > Duration::from_secs(2))
    {
        history.push_back(TrendPoint {
            at: point.at,
            values: [None; 7],
            networks: Box::new([]),
        });
    }
    history.push_back(point);
}
fn cpu_counters(line: &str) -> Option<(&str, CpuCounters)> {
    let mut fields = line.split_ascii_whitespace();
    let name = fields.next()?;
    if !name.starts_with("cpu") {
        return None;
    }
    let mut counters = [0u64; 8];
    for counter in &mut counters {
        *counter = fields.next()?.parse().ok()?;
    }
    Some((
        name,
        CpuCounters {
            total: counters.into_iter().try_fold(0u64, u64::checked_add)?,
            idle: counters[3].checked_add(counters[4])?,
            iowait: counters[4],
        },
    ))
}
fn counters<const N: usize>(text: &str, names: [&str; N]) -> [Option<u64>; N] {
    let mut result = [None; N];
    let mut remaining = N;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(index) = fields
            .next()
            .and_then(|name| names.iter().position(|n| *n == name))
        else {
            continue;
        };
        if result[index].is_none()
            && let Some(value) = fields.next().and_then(|value| value.parse().ok())
        {
            result[index] = Some(value);
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
    }
    result
}
fn sample_memory(text: &str, sample: &mut Sample) {
    let [
        total,
        available,
        swap_total,
        swap_free,
        free,
        cached,
        buffers,
    ] = counters(
        text,
        [
            "MemTotal:",
            "MemAvailable:",
            "SwapTotal:",
            "SwapFree:",
            "MemFree:",
            "Cached:",
            "Buffers:",
        ],
    )
    .map(|value| value.and_then(|value| value.checked_mul(1024)));
    let usage = |total: Option<u64>, available: Option<u64>| {
        let total = total?;
        let available = available?;
        Some(Usage {
            used: total.checked_sub(available)?,
            total,
            available,
            inodes: None,
        })
    };
    sample.memory = usage(total, available).filter(|usage| usage.total > 0);
    sample.swap = usage(swap_total, swap_free);
    sample.memory_parts = [free, cached, buffers];
}
fn pressure(text: &str) -> Option<f32> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_ascii_whitespace()
        .find_map(|field| field.strip_prefix("avg10=")?.parse().ok())
}
fn counter_pair(text: &str, names: [&str; 2]) -> Option<[u64; 2]> {
    let [a, b] = counters(text, names);
    Some([a?, b?])
}
fn git_branch(directory: &Path) -> Option<String> {
    for parent in directory.ancestors() {
        let dot_git = parent.join(".git");
        let mut git_dir = match fs::metadata(&dot_git) {
            Ok(metadata) if metadata.is_dir() => dot_git,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => match fs::read_to_string(&dot_git) {
                Ok(text) => parent.join(text.trim().strip_prefix("gitdir: ")?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            },
        };
        git_dir.push("HEAD");
        let head = fs::read_to_string(git_dir).ok()?;
        let head = head.trim();
        return Some(
            head.strip_prefix("ref: refs/heads/")
                .unwrap_or(head)
                .to_owned(),
        );
    }
    None
}

fn sample_physical_networks(text: &str, known: &mut HashMap<String, (bool, bool)>) -> Vec<Network> {
    let mut result: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let mut fields = fields.split_ascii_whitespace().map(str::parse::<u64>);
            let mut values = [0; 12];
            for value in &mut values {
                *value = fields.next()?.ok()?;
            }
            if fields.any(|value| value.is_err()) {
                return None;
            }
            let name = name.trim();
            let physical = if let Some((physical, present)) = known.get_mut(name) {
                *present = true;
                *physical
            } else {
                let physical = Path::new(&format!("/sys/class/net/{name}/device")).exists();
                known.insert(name.to_owned(), (physical, true));
                physical
            };
            if !physical {
                return None;
            }
            Some(Network {
                name: name.to_owned(),
                rx: None,
                tx: None,
                received: values[0],
                transmitted: values[8],
                rx_errors: values[2],
                tx_errors: values[10],
                rx_dropped: values[3],
                tx_dropped: values[11],
            })
        })
        .collect();
    // Interface names may be reused after a device disappears.
    known.retain(|_, (_, present)| std::mem::take(present));
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}
fn sample_physical_disks(
    mut previous: HashMap<String, DiskCounters>,
    elapsed: Option<f64>,
    text: &mut String,
) -> (Vec<Disk>, HashMap<String, DiskCounters>) {
    let mut disks = Vec::with_capacity(previous.len());
    let mut counters = HashMap::with_capacity(previous.len());
    let Ok(text) = read_text("/proc/diskstats", text) else {
        return (disks, counters);
    };
    for (name, disk) in text.lines().filter_map(|line| {
        let mut values = line.split_ascii_whitespace().skip(2);
        let name = values.next()?;
        if !Path::new(&format!("/sys/block/{name}/device")).exists() {
            return None;
        }
        Some((
            name,
            DiskCounters {
                bytes: [
                    values.nth(2)?.parse::<u64>().ok()?.checked_mul(512)?,
                    values.nth(3)?.parse::<u64>().ok()?.checked_mul(512)?,
                ],
                busy_ms: values.nth(2)?.parse().ok()?,
            },
        ))
    }) {
        let (name, old) = previous
            .remove_entry(name)
            .map_or_else(|| (name.to_owned(), None), |(name, old)| (name, Some(old)));
        let old = old.as_ref();
        disks.push(Disk {
            name: name.clone(),
            rates: std::array::from_fn(|i| {
                old.and_then(|old| per_second(disk.bytes[i], old.bytes[i], elapsed))
            }),
            utilization: old
                .and_then(|old| per_second(disk.busy_ms, old.busy_ms, elapsed))
                .map(|v| (v / 10.).min(100.)),
        });
        counters.insert(name, disk);
    }
    (disks, counters)
}
fn disk_usage(path: &Path) -> io::Result<Usage> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid filesystem block counters",
        )
    };
    Ok(Usage {
        used: stats
            .f_blocks
            .checked_sub(stats.f_bfree)
            .and_then(|v| v.checked_mul(stats.f_frsize))
            .ok_or_else(invalid)?,
        total: stats
            .f_blocks
            .checked_mul(stats.f_frsize)
            .filter(|v| *v > 0)
            .ok_or_else(invalid)?,
        available: stats
            .f_bavail
            .checked_mul(stats.f_frsize)
            .ok_or_else(invalid)?,
        inodes: (stats.f_files > 0)
            .then(|| (stats.f_files - stats.f_ffree) as f64 * 100. / stats.f_files as f64),
    })
}
fn process_stat(text: &str, page_size: u64) -> Option<(Process, ProcessCounters)> {
    let (pid, rest) = text.split_once(" (")?;
    let (name, rest) = rest.split_at(memchr::memmem::rfind(rest.as_bytes(), b") ")?);
    let mut values = rest[2..].split_ascii_whitespace();
    let state = values.next()?.chars().next()?;
    let parent = values.next()?.parse::<u64>().ok()? as u32;
    let ticks = values
        .nth(9)?
        .parse::<u64>()
        .ok()?
        .checked_add(values.next()?.parse().ok()?)?;
    let threads = values.nth(4)?.parse::<u64>().ok()? as u32;
    let start = values.nth(1)?.parse().ok()?;
    let rss = values.nth(1)?.parse::<u64>().ok()?.checked_mul(page_size)?;
    Some((
        Process {
            pid: pid.parse().ok()?,
            start,
            name: name.into(),
            uid: 0,
            state,
            cpu: None,
            rss,
            read: None,
            write: None,
            threads,
            parent,
            command: Box::default(),
            directory: None,
        },
        ProcessCounters { ticks, io: None },
    ))
}
// Proc files report zero size; the Read wrapper avoids File's metadata query.
fn read_text(path: impl AsRef<Path>, text: &mut String) -> io::Result<&str> {
    text.clear();
    fs::File::open(path)?.take(u64::MAX).read_to_string(text)?;
    Ok(text)
}

fn sample_processes(
    previous: &HashMap<(u32, u64), ProcessCounters>,
    elapsed: Option<f64>,
    ticks_per_second: f64,
    page_size: u64,
    detail: Option<(u32, u64)>,
) -> (Vec<Process>, HashMap<(u32, u64), ProcessCounters>) {
    let mut processes = Vec::with_capacity(previous.len());
    let mut counters = HashMap::with_capacity(previous.len());
    let root = Path::new("/proc");
    let Ok(entries) = fs::read_dir(root) else {
        return (processes, counters);
    };
    let mut text = String::with_capacity(512);
    let mut path = PathBuf::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        path.clear();
        path.push(root);
        path.push(name);
        path.push("stat");
        let Some((mut process, mut current)) = read_text(&path, &mut text)
            .ok()
            .and_then(|text| process_stat(text, page_size))
        else {
            continue;
        };
        process.uid = match entry.metadata() {
            Ok(meta) => meta.uid(),
            Err(_) => continue,
        };
        path.set_file_name("io");
        current.io = read_text(&path, &mut text)
            .ok()
            .and_then(|text| counter_pair(text, ["read_bytes:", "write_bytes:"]));
        let identity = (pid, process.start);
        if let Some(old) = previous.get(&identity) {
            process.cpu = per_second(current.ticks, old.ticks, elapsed)
                .map(|v| (v * 100. / ticks_per_second) as f32);
            if let Some((old, new)) = old.io.zip(current.io) {
                process.read = per_second(new[0], old[0], elapsed);
                process.write = per_second(new[1], old[1], elapsed);
            }
        }
        if detail == Some(identity)
            && let Some(detail) = process_details(identity, page_size)
        {
            process.command = detail.command.into_boxed_str();
            process.directory = detail.directory.map(PathBuf::into_boxed_path);
        }
        processes.push(process);
        counters.insert(identity, current);
    }
    processes.sort_unstable_by_key(|p| p.pid);
    (processes, counters)
}

fn process_details(identity: (u32, u64), page_size: u64) -> Option<ProcessDetails> {
    let mut path = PathBuf::from(format!("/proc/{}/cmdline", identity.0));
    let command = fs::read(&path)
        .ok()
        .map(|mut bytes| {
            let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            bytes.truncate(end);
            for byte in &mut bytes {
                if *byte == 0 {
                    *byte = b' ';
                }
            }
            String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
        })
        .filter(|v| !v.is_empty());
    path.set_file_name("cwd");
    let directory = fs::read_link(&path).ok();
    path.set_file_name("stat");
    let mut text = String::with_capacity(512);
    let (current, _) = process_stat(read_text(&path, &mut text).ok()?, page_size)?;
    (current.start == identity.1).then_some(ProcessDetails {
        identity,
        command: command.unwrap_or_else(|| format!("[{}]", current.name)),
        directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_classification_forgets_disappeared_interfaces() {
        let name = "nextty-disappeared-interface";
        let text = format!(" {name}: 1024 1 2 3 0 0 0 0 2048 4 5 6 0 0 0 0\n");
        let mut known = HashMap::from([(name.to_owned(), (true, false))]);
        assert_eq!(sample_physical_networks(&text, &mut known).len(), 1);
        assert!(sample_physical_networks("", &mut known).is_empty());
        assert!(known.is_empty());
        assert!(sample_physical_networks(&text, &mut known).is_empty());
        for index in 0..32 {
            let text = format!(" nextty-disappeared-interface-{index}: 0 0 0 0 0 0 0 0 0 0 0 0\n");
            assert!(sample_physical_networks(&text, &mut known).is_empty());
            assert_eq!(known.len(), 1);
        }
    }

    #[test]
    fn counters_and_process_stat_preserve_units_and_identity() {
        let (_, cpu) = cpu_counters("cpu 10 20 30 100 5 6 7 8 9 10").unwrap();
        assert_eq!((cpu.total, cpu.idle, cpu.iowait), (186, 105, 5));
        assert_eq!(counter_percent(200, 100, 70, 20), Some(50.));
        assert_eq!(per_second(1100, 100, Some(2.)), Some(500.));
        assert_eq!(per_second(10, 100, Some(1.)), None);
        let mut sample = Sample::default();
        sample_memory(
            "MemTotal: 1000 kB\nMemAvailable: 666 kB\nSwapTotal: 400 kB\nSwapFree: 100 kB\n",
            &mut sample,
        );
        let mem = sample.memory.unwrap();
        assert_eq!((mem.used, mem.total), (334 * 1024, 1000 * 1024));
        sample_memory("SwapTotal: 400 kB\nSwapFree: 100 kB\n", &mut sample);
        let swap = sample.swap.unwrap();
        assert_eq!((swap.used, swap.total), (300 * 1024, 400 * 1024));
        let mut known = HashMap::from([("eth0".to_owned(), (true, false))]);
        let net = sample_physical_networks(
            "Inter-| Receive\n eth0: 1024 1 2 3 0 0 0 0 2048 4 5 6 0 0 0 0\n",
            &mut known,
        );
        assert_eq!(
            (
                net[0].received,
                net[0].transmitted,
                net[0].rx_errors,
                net[0].tx_dropped
            ),
            (1024, 2048, 2, 6)
        );
        let text =
            "123 (name ) with spaces) R 1 0 0 0 0 0 0 0 0 0 40 10 0 0 20 0 3 0 900 100000 20 0 0";
        let (process, counters) = process_stat(text, 4096).unwrap();
        assert_eq!(
            (process.pid, process.parent, process.threads, process.rss),
            (123, 1, 3, 20 * 4096)
        );
        assert_eq!(process.name.as_ref(), "name ) with spaces");
        assert_eq!((process.start, counters.ticks), (900, 50));
        assert_eq!(
            counter_pair(
                "read_bytes: 42\nwrite_bytes: 84\n",
                ["read_bytes:", "write_bytes:"]
            ),
            Some([42, 84])
        );
        assert_eq!(
            pressure("some avg10=0.25 avg60=0.1\nfull avg10=1.0"),
            Some(0.25)
        );
    }
    #[test]
    fn history_keeps_one_hour_and_breaks_gaps() {
        let start = Instant::now();
        let mut history = VecDeque::new();
        for second in 0..=3610 {
            record_history(
                &mut history,
                TrendPoint {
                    at: start + Duration::from_secs(second),
                    values: [Some(second as f32); 7],
                    networks: Box::new([]),
                },
            );
        }
        assert_eq!(history.len(), 3601);
        assert_eq!(history.front().unwrap().values[0], Some(10.));
        record_history(
            &mut history,
            TrendPoint {
                at: start + Duration::from_secs(3615),
                values: [Some(3615.); 7],
                networks: Box::new([]),
            },
        );
        assert_eq!(history.front().unwrap().values[0], Some(15.));
        assert_eq!(history[history.len() - 2].values, [None; 7]);
    }
    #[test]
    #[cfg(any(feature = "gui", feature = "tui"))]
    fn process_view_keeps_scroll_focus_and_filter_matching() {
        use crate::monitor::{Action, Emphasis, Monitor};
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.processes = (1..=200)
            .map(|pid| Process {
                pid,
                start: 0,
                name: format!("p{pid}").into(),
                uid: 1000,
                state: 'S',
                cpu: Some(0.),
                rss: 0,
                read: None,
                write: None,
                threads: 1,
                parent: 1,
                command: Box::default(),
                directory: None,
            })
            .collect();
        monitor.toggle();
        monitor.action(Action::Tab(6));
        monitor.view(80, 10);
        monitor.scroll = usize::MAX;
        let view = monitor.view(80, 10);
        assert!(
            view.hits
                .iter()
                .any(|hit| hit.action == Action::Process(200))
        );
        assert!(
            view.hits
                .iter()
                .any(|hit| hit.action == Action::Search && hit.y == 0)
        );
        assert!(
            view.hits
                .iter()
                .any(|hit| hit.action == Action::Sort(0) && hit.y == 3)
        );
        assert!(
            view.hits
                .iter()
                .filter(|hit| matches!(hit.action, Action::Process(_)))
                .all(|hit| hit.y >= 4)
        );
        assert!(view.texts.iter().any(|text| text.text == "↑"));
        assert!(view.texts.iter().any(|text| text.text.contains("p200")));
        monitor.action(Action::Process(1));
        let view = monitor.view(80, 10);
        assert!(view.hits.iter().any(|hit| hit.action == Action::Process(1)));
        assert!(view.texts.iter().any(|text| {
            text.text.contains("p1") && text.emphasis(false) == Emphasis::Selected && text.focused
        }));
        monitor.stats.sample.processes[0].uid = 0;
        monitor.stats.sample.processes[99].name = "ΟΣ".into();
        monitor.stats.sample.processes[100].name = "K-worker".into();
        monitor.stats.sample.processes[102].name = "进程１２".into();
        monitor.stats.sample.processes[103].name = "İdaemon".into();
        monitor.stats.sample.processes[199].pid = u32::MAX;
        monitor.action(Action::Search);
        for (query, pid) in [
            ("1000", 2),
            ("0", 1),
            ("ος", 100),
            ("K", 101),
            ("１２", 103),
            ("İ", 104),
            ("4294967295", u32::MAX),
        ] {
            monitor.key(u32::from('u'), "", true);
            monitor.insert_query(query);
            let view = monitor.view(80, 10);
            assert!(
                view.hits
                    .iter()
                    .any(|hit| hit.action == Action::Process(pid))
            );
        }
        monitor.key(u32::from('u'), "", true);
        monitor.stats.sample.processes.truncate(3);
        monitor.stats.sample.processes[0].rss = 30;
        monitor.stats.sample.processes[1].rss = 20;
        monitor.stats.sample.processes[2].rss = 10;
        monitor.action(Action::Sort(1));
        monitor.action(Action::Process(2));
        let view = monitor.view(80, 20);
        assert!(
            view.texts
                .iter()
                .any(|t| { t.emphasis(false) == Emphasis::Selected && t.text.contains("p2") })
        );
        let mut processes = std::mem::take(&mut monitor.stats.sample.processes);
        processes[2].rss = 40;
        processes[1].start += 1;
        let mut state = monitor.stats.sampler.state.lock().unwrap();
        state.result = Some(Sample {
            processes,
            ..Sample::default()
        });
        state.detail_result = Some(ProcessDetails {
            identity: (2, 0),
            command: "stale".into(),
            directory: None,
        });
        drop(state);
        monitor.stats.refresh(None, true).unwrap();
        assert!(monitor.stats.processes()[1].command.is_empty());
        let view = monitor.view(80, 20);
        assert_eq!(
            view.hits
                .iter()
                .filter_map(|hit| match hit.action {
                    Action::Process(pid) => Some(pid),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [3, 1, 2]
        );
        assert!(
            !view
                .texts
                .iter()
                .any(|t| t.emphasis(false) == Emphasis::Selected)
        );
        monitor.key(crate::input::keysym::UP, "", false);
        assert_eq!(monitor.focus, Action::Process(1));
        monitor.action(Action::Sort(1));
        let view = monitor.view(80, 20);
        assert_eq!(
            view.hits
                .iter()
                .filter_map(|hit| match hit.action {
                    Action::Process(pid) => Some(pid),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
    }
    #[test]
    fn trend_windows_keep_recent_ring_order_and_network_gaps() {
        let mut stats = SystemStats::new().unwrap();
        stats.sample.networks = sample_physical_networks(
            "eth0: 1 0 0 0 0 0 0 0 2 0 0 0",
            &mut HashMap::from([("eth0".to_owned(), (true, false))]),
        );
        let now = Instant::now();
        let point = |age| TrendPoint {
            at: now - Duration::from_secs(age),
            values: [
                Some(age as f32),
                None,
                Some(age as f32 * 0.5),
                Some(age as f32),
                None,
                None,
                None,
            ],
            networks: if age == 20 {
                Box::new([])
            } else {
                let selected = ("eth0".into(), [Some(age as f32 * 0.5), Some(age as f32)]);
                let other = ("eth1".into(), [Some(1000.), Some(2000.)]);
                if age == 30 {
                    Box::new([other, selected])
                } else {
                    Box::new([selected, other])
                }
            },
        };
        let mut history = VecDeque::with_capacity(5);
        for age in [1500, 1400, 1300] {
            history.push_back(point(age));
        }
        for _ in 0..3 {
            history.pop_front();
        }
        for age in [900, 120, 30, 20, 10] {
            history.push_back(point(age));
        }
        stats.sampler.state.lock().unwrap().history = history;
        let system = stats.system_trends(60, 0..7);
        assert_eq!(
            system.iter().map(|t| t.name).collect::<Vec<_>>(),
            ["CPU", "MEMORY", "READ", "WRITE", "SWAP", "IN", "OUT"]
        );
        assert_eq!(
            system[0].points.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            [Some(30.), Some(20.), Some(10.)]
        );
        let disk = stats.system_trends(60, 2..4);
        assert_eq!(
            disk.iter()
                .map(|trend| trend
                    .points
                    .iter()
                    .map(|(_, value)| *value)
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            [
                vec![Some(15.), Some(10.), Some(5.)],
                vec![Some(30.), Some(20.), Some(10.)]
            ]
        );
        let network = stats.network_trends(60, &stats.networks()[0]);
        assert_eq!(
            network.iter().map(|t| t.name).collect::<Vec<_>>(),
            ["RX", "TX"]
        );
        assert_eq!(
            network[0]
                .points
                .iter()
                .map(|(_, v)| *v)
                .collect::<Vec<_>>(),
            [Some(15.), None, Some(5.)]
        );
        assert_eq!(network[0].maximum, 32.);
        assert_eq!(network[1].maximum, 32.);
    }
    #[test]
    #[cfg(any(feature = "gui", feature = "tui"))]
    fn network_view_shows_each_sampled_hardware_interface() {
        use crate::monitor::{Action, Monitor};
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.networks = sample_physical_networks(
            "enp1s0: 1024 0 0 0 0 0 0 0 2048 0 0 0
wlp2s0: 4096 0 0 0 0 0 0 0 8192 0 0 0",
            &mut HashMap::from([
                ("enp1s0".to_owned(), (true, false)),
                ("wlp2s0".to_owned(), (true, false)),
            ]),
        );
        monitor.action(Action::Tab(4));
        let view = monitor.view(120, 80);
        for name in ["enp1s0", "wlp2s0"] {
            assert!(view.texts.iter().any(|text| text.text == name));
        }
        assert_eq!(view.plots.len(), 2);
        assert!(view.plots.iter().all(|plot| plot.series.len() == 2));
        let top = monitor.view(120, 3);
        assert_eq!(top.plots.len(), 1);
        assert!(top.plots[0].series[0].summary.starts_with("enp1s0 ·"));
        monitor.scroll = 1000;
        let bottom = monitor.view(120, 3);
        assert_eq!(bottom.plots.len(), 1);
        assert!(bottom.plots[0].series[0].summary.starts_with("wlp2s0 ·"));
        let scrolled = monitor.view(120, 3);
        assert_eq!(scrolled.plots.len(), 1);
        assert!(scrolled.plots[0].series[0].summary.starts_with("wlp2s0 ·"));
    }

    #[test]
    fn async_sampling_publishes_real_rates_and_gates_process_scans() {
        let mut stats = SystemStats::new().unwrap();
        assert!(
            stats
                .system_trends(60, 0..7)
                .iter()
                .all(|t| t.points.is_empty())
        );
        stats.refresh(Some(Path::new("/")), true).unwrap();
        let mut fd = libc::pollfd {
            fd: stats.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        for _ in 0..2 {
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 10000) }, 1);
            stats.refresh(Some(Path::new("/")), true).unwrap();
        }
        assert!(stats.system_trends(60, 0..1)[0].value.is_some());
        assert!(stats.processes().is_empty());
        assert!(!stats.cpu_cores().is_empty());
        assert!(stats.cpu_cores().iter().all(|(_, value)| value.is_some()));
        assert!(stats.sample.memory_parts.iter().all(Option::is_some));
        assert!(
            stats
                .disks()
                .iter()
                .all(|disk| disk.rates.iter().all(Option::is_some))
        );
        assert!(
            stats
                .system_trends(60, 5..7)
                .iter()
                .all(|trend| trend.value.is_some())
        );
        assert!(stats.disk().is_some());
        assert!(stats.networks().iter().all(|net| net.rx.is_some()));
        stats.set_processes_enabled(true);
        for _ in 0..2 {
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 10000) }, 1);
            stats.refresh(Some(Path::new("/")), true).unwrap();
            if !stats.processes().is_empty() {
                break;
            }
        }
        assert!(
            stats
                .processes()
                .iter()
                .any(|p| p.pid == std::process::id())
        );
        assert!(
            stats
                .processes()
                .iter()
                .all(|p| p.command.is_empty() && p.directory.is_none())
        );
        let process = stats
            .processes()
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap();
        let identity = (process.pid, process.start);
        let revision = stats.process_revision();
        stats.set_process_detail(Some(identity));
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 10000) }, 1);
        stats.refresh(Some(Path::new("/")), true).unwrap();
        assert_eq!(stats.process_revision(), revision);
        let process = stats
            .processes()
            .iter()
            .find(|p| p.pid == identity.0)
            .unwrap();
        assert!(!process.command.is_empty());
        assert_eq!(
            process.directory.as_deref(),
            Some(std::env::current_dir().unwrap().as_path())
        );
        assert!(process_details((identity.0, identity.1 + 1), 4096).is_none());
        assert!(stats.refresh(None, true).unwrap());
        assert!(stats.disk().is_none());
    }
}
