//! Asynchronous local resource samples and one hour of history.

use std::{
    collections::{HashMap, VecDeque},
    ffi::CString,
    fs,
    io::{self, BufRead},
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
    pub fn details(&self) -> [(String, String); 3] {
        [
            (
                "Total RX / TX".into(),
                format!("{} / {}", bytes(self.received), bytes(self.transmitted)),
            ),
            (
                "Errors RX / TX".into(),
                format!("{} / {}", self.rx_errors, self.tx_errors),
            ),
            (
                "Dropped RX / TX".into(),
                format!("{} / {}", self.rx_dropped, self.tx_dropped),
            ),
        ]
    }
}

pub struct Process {
    pub pid: u32,
    pub(crate) start: u64,
    pub name: String,
    pub uid: u32,
    pub state: char,
    pub cpu: Option<f32>,
    pub rss: u64,
    pub read: Option<f64>,
    pub write: Option<f64>,
    pub threads: u32,
    pub parent: u32,
    pub command: String,
    pub directory: Option<PathBuf>,
}

struct ProcessDetails {
    identity: (u32, u64),
    command: String,
    directory: Option<PathBuf>,
}

struct TrendPoint {
    at: Instant,
    values: [Option<f32>; 7],
    networks: Vec<(String, [Option<f32>; 2])>,
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
    cpu_cores: Vec<(String, Option<f32>)>,
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
    start: u64,
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

fn number<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "—".into(), |value| value.to_string())
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
fn rate(value: Option<f64>) -> String {
    value.map_or_else(
        || "—".into(),
        |n| {
            if n >= 1048576. {
                format!("{:.2} MiB/s", n / 1048576.)
            } else {
                format!("{:.1} KiB/s", n / 1024.)
            }
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

    pub(crate) fn process_revision(&self) -> u64 {
        self.process_revision
    }

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
        let branch_changed = result
            .as_ref()
            .is_some_and(|sample| sample.branch != self.sample.branch);
        if let Some(sample) = result {
            self.sample = sample;
            self.process_revision += 1;
        }
        if let Some(detail) = detail
            && let Some(process) = self
                .sample
                .processes
                .iter_mut()
                .find(|p| (p.pid, p.start) == detail.identity)
        {
            process.command = detail.command;
            process.directory = detail.directory;
        }
        let values = sample_values(&self.sample);
        let disk = self.disk().and_then(|u| {
            let total = u.used + u.available;
            (total > 0).then(|| u.used as f32 * 100. / total as f32)
        });
        let summary = [
            format!(" {}", percent(values[0])),
            format!(" {}", percent(values[1])),
            format!(" {}", percent(disk)),
            format!(" {}", percent(values[4])),
        ];
        let changed =
            branch_changed || directory_changed || summary != self.summary || expanded && sampled;
        self.summary = summary;
        Ok(changed)
    }

    fn disk(&self) -> Option<Usage> {
        (self.sample.directory == self.directory)
            .then_some(self.sample.disk)
            .flatten()
    }

    pub fn branch(&self) -> Option<&str> {
        (self.sample.directory == self.directory)
            .then_some(self.sample.branch.as_deref())
            .flatten()
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
                    0 => format!("{} logical cores", number(self.sample.cores)),
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
            for (index, trend) in indices.zip(&mut result) {
                trend.points = history
                    .clone()
                    .map(|point| {
                        (
                            now.saturating_duration_since(point.at).as_secs_f32(),
                            point.values[index],
                        )
                    })
                    .collect();
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
        for point in history {
            let age = now.saturating_duration_since(point.at).as_secs_f32();
            let values = point
                .networks
                .iter()
                .find(|(name, _)| *name == network.name)
                .map(|(_, values)| *values)
                .unwrap_or([None; 2]);
            for index in 0..2 {
                points[index].push((age, values[index]));
            }
        }
        drop(state);
        let peak = points
            .iter()
            .flatten()
            .filter_map(|(_, v)| *v)
            .fold(0.125f32, f32::max);
        let maximum = 2f32.powf(peak.log2().ceil());
        let mut result = Vec::with_capacity(2);
        for (index, points) in points.into_iter().enumerate() {
            result.push(ResourceTrend {
                name: ["RX", "TX"][index],
                value: [network.rx, network.tx][index].map(|v| (v / 1048576.) as f32),
                summary: format!(
                    "{} · {}",
                    network.name,
                    rate([network.rx, network.tx][index])
                ),
                points,
                maximum,
                unit: "MiB/s",
            });
        }
        result
    }

    pub fn overview(&self) -> Vec<(String, String)> {
        vec![
            ("Load · 1/5/15 min".into(), self.load()),
            (
                "Uptime".into(),
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
    pub fn cpu_details(&self) -> Vec<(String, String)> {
        vec![
            ("Load · 1/5/15 min".into(), self.load()),
            ("I/O wait".into(), percent(self.sample.iowait)),
            (
                "CPU pressure · 10s".into(),
                percent(self.sample.pressure[0]),
            ),
            (
                "Tasks · running/total".into(),
                self.sample.tasks.clone().unwrap_or_else(|| "—".into()),
            ),
        ]
    }
    pub fn cpu_cores(&self) -> &[(String, Option<f32>)] {
        &self.sample.cpu_cores
    }
    pub fn memory_details(&self) -> Vec<(String, String)> {
        let mut rows = vec![(
            "Available".into(),
            self.sample
                .memory
                .map_or_else(|| "—".into(), |u| bytes(u.available)),
        )];
        for (index, label) in ["Free", "Cached", "Buffers"].into_iter().enumerate() {
            rows.push((
                label.into(),
                self.sample.memory_parts[index].map_or_else(|| "—".into(), bytes),
            ));
        }
        rows.push((
            "Memory pressure · 10s".into(),
            percent(self.sample.pressure[1]),
        ));
        rows
    }
    pub fn disks(&self) -> &[Disk] {
        &self.sample.disks
    }
    pub fn disk_details(&self) -> Vec<(String, String)> {
        vec![
            (
                "Filesystem path".into(),
                self.directory
                    .as_ref()
                    .map_or_else(|| "—".into(), |p| p.display().to_string()),
            ),
            ("Used / total".into(), capacity(self.disk())),
            (
                "I/O pressure · 10s".into(),
                percent(self.sample.pressure[2]),
            ),
            (
                "Available / inodes".into(),
                self.disk().map_or_else(
                    || "—".into(),
                    |u| {
                        format!(
                            "{} / {}",
                            bytes(u.available),
                            u.inodes.map_or_else(|| "—".into(), |v| format!("{v:.1}%"))
                        )
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
    let mut previous_net: HashMap<String, (u64, u64)> = HashMap::new();
    let mut previous_disk: HashMap<String, DiskCounters> = HashMap::new();
    let mut previous_cores: HashMap<String, CpuCounters> = HashMap::new();
    let mut previous_swap: Option<[u64; 2]> = None;
    let mut previous_processes: HashMap<u32, ProcessCounters> = HashMap::new();
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
        let cpu_text = fs::read_to_string("/proc/stat").ok();
        let cpu = cpu_text.as_deref().and_then(cpu_counters);
        if expanded {
            if let Some(text) = &cpu_text {
                let mut counters = HashMap::new();
                for line in text
                    .lines()
                    .skip(1)
                    .take_while(|line| line.starts_with("cpu"))
                {
                    if let Some(new) = cpu_counters(line) {
                        let name = line.split_whitespace().next().unwrap().to_owned();
                        let value = previous_cores.get(&name).and_then(|old| {
                            counter_percent(new.total, old.total, new.idle, old.idle)
                                .map(|idle| 100. - idle)
                        });
                        sample.cpu_cores.push((name.clone(), value));
                        counters.insert(name, new);
                    }
                }
                previous_cores = counters;
            }
        } else {
            previous_cores.clear();
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
            if let Ok(text) = fs::read_to_string("/proc/loadavg") {
                let mut fields = text.split_whitespace();
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
            sample.uptime = fs::read_to_string("/proc/uptime").ok().and_then(|text| {
                text.split_whitespace()
                    .next()?
                    .split('.')
                    .next()?
                    .parse()
                    .ok()
            });
            for (index, name) in ["cpu", "memory", "io"].into_iter().enumerate() {
                sample.pressure[index] = fs::read_to_string(format!("/proc/pressure/{name}"))
                    .ok()
                    .and_then(|text| pressure(&text));
            }
        }
        if let Ok(text) = fs::read_to_string("/proc/meminfo") {
            sample.memory = memory_usage(&text);
            sample.swap = swap_usage(&text);
            sample.memory_parts =
                ["MemFree:", "Cached:", "Buffers:"].map(|name| memory_field(&text, name));
        }
        if expanded {
            let swap = fs::read_to_string("/proc/vmstat")
                .ok()
                .and_then(|text| counter_pair(&text, ["pswpin", "pswpout"]));
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
            let disks = physical_disk_counters();
            for (name, disk) in &disks {
                let previous = previous_disk.get(name);
                sample.disks.push(Disk {
                    name: name.clone(),
                    rates: std::array::from_fn(|i| {
                        previous.and_then(|old| per_second(disk.bytes[i], old.bytes[i], elapsed))
                    }),
                    utilization: previous
                        .and_then(|old| per_second(disk.busy_ms, old.busy_ms, elapsed))
                        .map(|v| (v / 10.).min(100.)),
                });
            }
            previous_disk = disks.into_iter().collect();
        } else {
            previous_disk.clear();
        }
        if let Ok(text) = fs::read_to_string("/proc/net/dev") {
            sample.networks = network_counters(&text);
            sample.networks.retain(|net| {
                *physical_networks
                    .entry(net.name.clone())
                    .or_insert_with(|| {
                        Path::new("/sys/class/net")
                            .join(&net.name)
                            .join("device")
                            .exists()
                    })
            });
            for net in &mut sample.networks {
                if let Some((rx, tx)) = previous_net.get(&net.name) {
                    net.rx = per_second(net.received, *rx, elapsed);
                    net.tx = per_second(net.transmitted, *tx, elapsed);
                }
            }
            previous_net = sample
                .networks
                .iter()
                .map(|net| (net.name.clone(), (net.received, net.transmitted)))
                .collect();
        } else {
            previous_net.clear();
        }
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
            previous_processes.clear();
        }
        previous_at = Some(start);
        let point = TrendPoint {
            at: start,
            values: sample_values(&sample),
            networks: sample
                .networks
                .iter()
                .map(|net| {
                    (
                        net.name.clone(),
                        [
                            net.rx.map(|v| (v / 1048576.) as f32),
                            net.tx.map(|v| (v / 1048576.) as f32),
                        ],
                    )
                })
                .collect(),
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
    (total > 0)
        .then(|| {
            counter
                .checked_sub(old_counter)
                .map(|v| v as f32 * 100. / total as f32)
        })
        .flatten()
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
            networks: Vec::new(),
        });
    }
    history.push_back(point);
    while history.len() > 3602 {
        history.pop_front();
    }
}
fn cpu_counters(text: &str) -> Option<CpuCounters> {
    let mut fields = text.lines().next()?.split_whitespace();
    if !fields.next()?.starts_with("cpu") {
        return None;
    }
    let mut counters = [0u64; 8];
    for counter in &mut counters {
        *counter = fields.next()?.parse().ok()?;
    }
    Some(CpuCounters {
        total: counters.into_iter().try_fold(0u64, u64::checked_add)?,
        idle: counters[3].checked_add(counters[4])?,
        iowait: counters[4],
    })
}
fn memory_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == name)
            .then(|| fields.next()?.parse::<u64>().ok()?.checked_mul(1024))
            .flatten()
    })
}
fn memory_usage(text: &str) -> Option<Usage> {
    let total = memory_field(text, "MemTotal:").filter(|&v| v > 0)?;
    let available = memory_field(text, "MemAvailable:")?;
    Some(Usage {
        used: total.checked_sub(available)?,
        total,
        available,
        inodes: None,
    })
}
fn swap_usage(text: &str) -> Option<Usage> {
    let total = memory_field(text, "SwapTotal:")?;
    let available = memory_field(text, "SwapFree:")?;
    Some(Usage {
        used: total.checked_sub(available)?,
        total,
        available,
        inodes: None,
    })
}
fn pressure(text: &str) -> Option<f32> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10=")?.parse().ok())
}
fn counter_pair(text: &str, names: [&str; 2]) -> Option<[u64; 2]> {
    let value = |name| {
        text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == name)
                .then(|| fields.next()?.parse().ok())
                .flatten()
        })
    };
    Some([value(names[0])?, value(names[1])?])
}
fn git_branch(directory: &Path) -> Option<String> {
    for parent in directory.ancestors() {
        let dot_git = parent.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            match fs::read_to_string(&dot_git) {
                Ok(text) => parent.join(text.trim().strip_prefix("gitdir: ")?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            }
        };
        let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        return Some(
            head.strip_prefix("ref: refs/heads/")
                .unwrap_or(head)
                .to_owned(),
        );
    }
    None
}

fn network_counters(text: &str) -> Vec<Network> {
    let mut result: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let mut fields = fields.split_whitespace().map(str::parse::<u64>);
            let mut values = [0; 12];
            for value in &mut values {
                *value = fields.next()?.ok()?;
            }
            if fields.any(|value| value.is_err()) {
                return None;
            }
            Some(Network {
                name: name.trim().to_owned(),
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
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}
fn physical_disk_counters() -> Vec<(String, DiskCounters)> {
    let Ok(text) = fs::read_to_string("/proc/diskstats") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut values = line.split_whitespace().skip(2);
            let name = values.next()?;
            if !Path::new("/sys/block").join(name).join("device").exists() {
                return None;
            }
            Some((
                name.to_owned(),
                DiskCounters {
                    bytes: [
                        values.nth(2)?.parse::<u64>().ok()?.checked_mul(512)?,
                        values.nth(3)?.parse::<u64>().ok()?.checked_mul(512)?,
                    ],
                    busy_ms: values.nth(2)?.parse().ok()?,
                },
            ))
        })
        .collect()
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
    let (name, rest) = rest.rsplit_once(") ")?;
    let mut values = rest.split_whitespace();
    let mut fields = [""; 22];
    for field in &mut fields {
        *field = values.next()?;
    }
    let value = |i: usize| fields.get(i)?.parse::<u64>().ok();
    let counters = ProcessCounters {
        start: value(19)?,
        ticks: value(11)?.checked_add(value(12)?)?,
        io: None,
    };
    Some((
        Process {
            pid: pid.parse().ok()?,
            start: counters.start,
            name: name.into(),
            uid: 0,
            state: fields.first()?.chars().next()?,
            cpu: None,
            rss: value(21)?.checked_mul(page_size)?,
            read: None,
            write: None,
            threads: value(17)? as u32,
            parent: value(1)? as u32,
            command: String::new(),
            directory: None,
        },
        counters,
    ))
}
// Proc files report zero size; buffered reads avoid a metadata query for each one.
fn process_text(path: PathBuf) -> io::Result<String> {
    let mut reader = io::BufReader::with_capacity(512, fs::File::open(path)?);
    let mut text = String::new();
    while reader.read_line(&mut text)? != 0 {}
    Ok(text)
}

fn sample_processes(
    previous: &HashMap<u32, ProcessCounters>,
    elapsed: Option<f64>,
    ticks_per_second: f64,
    page_size: u64,
    detail: Option<(u32, u64)>,
) -> (Vec<Process>, HashMap<u32, ProcessCounters>) {
    let mut processes = Vec::new();
    let mut counters = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return (processes, counters);
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        let Some((mut process, mut current)) = process_text(path.join("stat"))
            .ok()
            .and_then(|text| process_stat(&text, page_size))
        else {
            continue;
        };
        process.uid = match entry.metadata() {
            Ok(meta) => meta.uid(),
            Err(_) => continue,
        };
        current.io = process_text(path.join("io"))
            .ok()
            .and_then(|text| counter_pair(&text, ["read_bytes:", "write_bytes:"]));
        if let Some(old) = previous.get(&pid).filter(|old| old.start == current.start) {
            process.cpu = per_second(current.ticks, old.ticks, elapsed)
                .map(|v| (v * 100. / ticks_per_second) as f32);
            if let Some((old, new)) = old.io.zip(current.io) {
                process.read = per_second(new[0], old[0], elapsed);
                process.write = per_second(new[1], old[1], elapsed);
            }
        }
        if detail == Some((pid, current.start))
            && let Some(detail) = process_details((pid, current.start), page_size)
        {
            process.command = detail.command;
            process.directory = detail.directory;
        }
        processes.push(process);
        counters.insert(pid, current);
    }
    processes.sort_by_key(|p| p.pid);
    (processes, counters)
}

fn process_details(identity: (u32, u64), page_size: u64) -> Option<ProcessDetails> {
    let path = PathBuf::from(format!("/proc/{}", identity.0));
    let (process, _) = process_stat(&process_text(path.join("stat")).ok()?, page_size)?;
    if process.start != identity.1 {
        return None;
    }
    let command = fs::read(path.join("cmdline"))
        .ok()
        .map(|v| {
            String::from_utf8_lossy(&v)
                .trim_end_matches('\0')
                .replace('\0', " ")
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("[{}]", process.name));
    let directory = fs::read_link(path.join("cwd")).ok();
    let (current, _) = process_stat(&process_text(path.join("stat")).ok()?, page_size)?;
    (current.start == identity.1).then_some(ProcessDetails {
        identity,
        command,
        directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_and_process_stat_preserve_units_and_identity() {
        let cpu = cpu_counters("cpu 10 20 30 100 5 6 7 8 9 10\ncpu0 1").unwrap();
        assert_eq!((cpu.total, cpu.idle, cpu.iowait), (186, 105, 5));
        assert_eq!(counter_percent(200, 100, 70, 20), Some(50.));
        assert_eq!(per_second(1100, 100, Some(2.)), Some(500.));
        assert_eq!(per_second(10, 100, Some(1.)), None);
        let mem = memory_usage(
            "MemTotal: 1000 kB\nMemAvailable: 666 kB\nSwapTotal: 400 kB\nSwapFree: 100 kB\n",
        )
        .unwrap();
        assert_eq!((mem.used, mem.total), (334 * 1024, 1000 * 1024));
        let swap = swap_usage("SwapTotal: 400 kB\nSwapFree: 100 kB\n").unwrap();
        assert_eq!((swap.used, swap.total), (300 * 1024, 400 * 1024));
        let net =
            network_counters("Inter-| Receive\n eth0: 1024 1 2 3 0 0 0 0 2048 4 5 6 0 0 0 0\n");
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
        assert_eq!(process.name, "name ) with spaces");
        assert_eq!((counters.start, counters.ticks), (900, 50));
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
                    networks: Vec::new(),
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
                networks: Vec::new(),
            },
        );
        assert_eq!(history.front().unwrap().values[0], Some(15.));
        assert_eq!(history[history.len() - 2].values, [None; 7]);
    }
    #[test]
    fn process_view_keeps_scroll_focus_and_filter_matching() {
        use crate::monitor::{Action, Monitor};
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.processes = (1..=200)
            .map(|pid| Process {
                pid,
                start: 0,
                name: format!("p{pid}"),
                uid: 1000,
                state: 'S',
                cpu: Some(0.),
                rss: 0,
                read: None,
                write: None,
                threads: 1,
                parent: 1,
                command: String::new(),
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
        assert!(
            view.texts
                .iter()
                .any(|text| { text.text.contains("p1") && text.selected && text.focused })
        );
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
                .any(|t| t.selected && t.text.contains("p2"))
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
        assert!(!view.texts.iter().any(|t| t.selected));
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
        stats.sample.networks = network_counters("eth0: 1 0 0 0 0 0 0 0 2 0 0 0");
        let now = Instant::now();
        let point = |age| TrendPoint {
            at: now - Duration::from_secs(age),
            values: [Some(age as f32), None, None, None, None, None, None],
            networks: if age == 20 {
                Vec::new()
            } else {
                vec![("eth0".into(), [Some(age as f32 * 0.5), Some(age as f32)])]
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
    fn network_view_shows_each_sampled_hardware_interface() {
        use crate::monitor::{Action, Monitor};
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.networks = network_counters(
            "enp1s0: 1024 0 0 0 0 0 0 0 2048 0 0 0
wlp2s0: 4096 0 0 0 0 0 0 0 8192 0 0 0",
        );
        monitor.action(Action::Tab(4));
        let view = monitor.view(120, 80);
        for name in ["enp1s0", "wlp2s0"] {
            assert!(view.texts.iter().any(|text| text.text == name));
        }
        assert_eq!(view.plots.len(), 2);
        assert!(view.plots.iter().all(|plot| plot.series.len() == 2));
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
