//! Asynchronous local resource samples and one hour of history.

use std::{
    collections::{HashMap, VecDeque},
    ffi::CString,
    fs, io,
    ops::Range,
    os::{
        fd::RawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const GIB: f64 = 1073741824.;
pub(crate) const MIB: f64 = 1048576.;
const NAMES: [&str; 7] = ["CPU", "MEMORY", "READ", "WRITE", "SWAP", "IN", "OUT"];

type Identity = (u32, u64);

pub struct SystemStats {
    worker: crate::worker::Worker<Request, Output>,
    pub(crate) sample: Sample,
    history: VecDeque<TrendPoint>,
    pub(crate) process_revision: u64,
    directory: Option<PathBuf>,
    processes_enabled: bool,
    process_detail: Option<Identity>,
    next_sample: Option<Instant>,
    pub summary: [String; 4],
}

pub struct ResourceTrend {
    pub name: &'static str,
    pub value: Option<f32>,
    pub summary: String,
    pub points: Vec<(f32, Option<f32>)>,
    pub unit: &'static str,
}

pub struct Network {
    pub name: Arc<str>,
    pub rx: Option<f32>,
    pub tx: Option<f32>,
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
                    self.received as f64 / GIB,
                    self.transmitted as f64 / GIB
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
    identity: Identity,
    command: String,
    directory: Option<PathBuf>,
}

struct TrendPoint {
    at: Instant,
    values: [Option<f32>; 7],
    networks: Vec<(Arc<str>, [Option<f32>; 2])>,
}

enum Request {
    Sample {
        directory: Option<PathBuf>,
        expanded: bool,
        processes: bool,
        detail: Option<Identity>,
    },
    Detail(Identity),
}

enum Output {
    Sample(Box<Sample>, TrendPoint),
    Detail(ProcessDetails),
}

#[derive(Default)]
struct Sampler {
    previous_at: Option<Instant>,
    previous_cpu: Option<CpuCounters>,
    previous_cores: HashMap<String, CpuCounters>,
    previous_swap: Option<[u64; 2]>,
    previous_disk: HashMap<String, DiskCounters>,
    previous_net: HashMap<Arc<str>, (u64, u64)>,
    previous_processes: HashMap<Identity, ProcessCounters>,
}

#[derive(Default)]
pub(crate) struct Sample {
    directory: Option<PathBuf>,
    branch: Option<String>,
    values: [Option<f32>; 7],
    iowait: Option<f32>,
    load: Option<String>,
    tasks: Option<String>,
    uptime: Option<u64>,
    pressure: [Option<f32>; 3],
    memory: Option<Usage>,
    memory_parts: [Option<u64>; 3],
    pub cpu_cores: Vec<(String, Option<f32>)>,
    swap: Option<Usage>,
    disk: Option<Usage>,
    pub disks: Vec<Disk>,
    pub networks: Vec<Network>,
    pub processes: Vec<Process>,
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
    pub rates: [Option<f32>; 2],
    pub utilization: Option<f64>,
}

#[derive(Clone, Copy)]
struct DiskCounters {
    bytes: [u64; 2],
    busy_ms: u64,
}

pub(crate) fn dash<T>(value: Option<T>, format: impl FnOnce(T) -> String) -> String {
    value.map_or_else(|| "—".into(), format)
}
fn percent(value: Option<f32>) -> String {
    dash(value, |value| format!("{value:.0}%"))
}
fn capacity(usage: Option<Usage>) -> String {
    dash(usage, |u| {
        if u.total == 0 {
            "disabled".into()
        } else {
            format!(
                "{:.1} / {:.1} GiB",
                u.used as f64 / GIB,
                u.total as f64 / GIB
            )
        }
    })
}
fn gib(value: u64) -> String {
    format!("{:.2} GiB", value as f64 / GIB)
}
fn mib(rate: f64) -> f32 {
    (rate / MIB) as f32
}

impl SystemStats {
    pub fn new() -> io::Result<Self> {
        let notification = Arc::new(crate::sys::new()?);
        let mut sampler = Sampler::default();
        let worker =
            crate::worker::Worker::new("nextty-stats", notification, move |request, publish| {
                match request {
                    Request::Sample {
                        directory,
                        expanded,
                        processes,
                        detail,
                    } => {
                        let (sample, point) =
                            sampler.sample(directory, expanded, processes, detail);
                        publish(Output::Sample(Box::new(sample), point));
                    }
                    Request::Detail(identity) => {
                        if let Some(detail) = process_details(identity) {
                            publish(Output::Detail(detail));
                        }
                    }
                }
            })?;
        Ok(Self {
            worker,
            sample: Sample::default(),
            history: VecDeque::new(),
            process_revision: 0,
            directory: None,
            processes_enabled: false,
            process_detail: None,
            next_sample: None,
            summary: [" —", " —", " —", " —"].map(str::to_owned),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.worker.fd()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.next_sample
    }

    pub fn set_processes_enabled(&mut self, enabled: bool) {
        if self.processes_enabled != enabled {
            self.processes_enabled = enabled;
            self.next_sample = Some(Instant::now());
        }
    }

    pub(crate) fn set_process_detail(&mut self, identity: Option<Identity>) {
        if self.process_detail != identity {
            self.process_detail = identity;
            if let Some(identity) = identity {
                self.worker.submit(Request::Detail(identity));
            }
        }
    }

    /// Applies finished background work and starts the next sample when due;
    /// returns whether anything visible changed.
    pub fn refresh(&mut self, directory: Option<&Path>, expanded: bool) -> io::Result<bool> {
        crate::sys::consume_notification(self.fd())?;
        let mut changed = false;
        let mut summary_stale = false;
        match self.worker.take() {
            Some(Output::Sample(sample, point)) => {
                changed = expanded || sample.branch != self.sample.branch;
                summary_stale = true;
                record_history(&mut self.history, point);
                self.sample = *sample;
                self.process_revision += 1;
            }
            Some(Output::Detail(detail)) => {
                changed = expanded;
                if let Ok(index) = self
                    .sample
                    .processes
                    .binary_search_by_key(&detail.identity, |p| (p.pid, p.start))
                {
                    let process = &mut self.sample.processes[index];
                    process.command = detail.command;
                    process.directory = detail.directory;
                }
            }
            None => {}
        }
        if self.directory.as_deref() != directory {
            self.directory = directory.map(Path::to_path_buf);
            self.next_sample = Some(Instant::now());
            changed = true;
            summary_stale = true;
        }
        let now = Instant::now();
        if self.next_sample.is_none_or(|deadline| now >= deadline) {
            self.next_sample = Some(now + SAMPLE_INTERVAL);
            self.worker.submit(Request::Sample {
                directory: self.directory.clone(),
                expanded,
                processes: self.processes_enabled,
                detail: self.process_detail,
            });
        }
        if summary_stale {
            let values = self.sample.values;
            let disk = self
                .disk()
                .filter(|u| u.used + u.available > 0)
                .map(|u| u.used as f32 * 100. / (u.used + u.available) as f32);
            let summary =
                [values[0], values[1], disk, values[4]].map(|v| format!(" {}", percent(v)));
            changed |= summary != self.summary;
            self.summary = summary;
        }
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

    pub fn swap_disabled(&self) -> bool {
        self.sample.swap.is_some_and(|usage| usage.total == 0)
    }

    /// History within `seconds`, oldest first, with each point's age in seconds.
    fn recent(&self, seconds: u32) -> impl Iterator<Item = (f32, &TrendPoint)> {
        let now = Instant::now();
        let age = move |point: &TrendPoint| now.saturating_duration_since(point.at).as_secs_f32();
        let start = self
            .history
            .partition_point(|point| age(point) > seconds as f32);
        self.history
            .range(start..)
            .map(move |point| (age(point), point))
    }

    pub fn system_trends(&self, seconds: u32, indices: Range<usize>) -> Vec<ResourceTrend> {
        let mut result: Vec<_> = indices
            .clone()
            .map(|index| ResourceTrend {
                name: NAMES[index],
                value: self.sample.values[index],
                summary: match index {
                    0 => match self.sample.cpu_cores.len() {
                        0 => "— logical cores".into(),
                        cores => format!("{cores} logical cores"),
                    },
                    1 => capacity(self.sample.memory),
                    4 => capacity(self.sample.swap),
                    _ => String::new(),
                },
                points: Vec::new(),
                unit: if matches!(index, 2 | 3 | 5 | 6) {
                    "MiB/s"
                } else {
                    "%"
                },
            })
            .collect();
        for (age, point) in self.recent(seconds) {
            for (index, trend) in indices.clone().zip(&mut result) {
                trend.points.push((age, point.values[index]));
            }
        }
        result
    }

    pub fn network_trends(&self, seconds: u32, network: &Network) -> Vec<ResourceTrend> {
        let mut result =
            [("RX", network.rx), ("TX", network.tx)].map(|(name, rate)| ResourceTrend {
                name,
                value: rate,
                summary: network.name.to_string(),
                points: Vec::new(),
                unit: "MiB/s",
            });
        for (age, point) in self.recent(seconds) {
            let rates = point
                .networks
                .iter()
                .find(|(name, _)| *name == network.name)
                .map_or([None; 2], |(_, rates)| *rates);
            for (trend, rate) in result.iter_mut().zip(rates) {
                trend.points.push((age, rate));
            }
        }
        result.into()
    }

    fn load(&self) -> (&'static str, String) {
        (
            "Load · 1/5/15 min",
            self.sample.load.clone().unwrap_or_else(|| "—".into()),
        )
    }
    pub fn overview(&self) -> [(&'static str, String); 2] {
        [
            self.load(),
            (
                "Uptime",
                dash(self.sample.uptime, |s| {
                    format!("{}d {:02}h {:02}m", s / 86400, s / 3600 % 24, s / 60 % 60)
                }),
            ),
        ]
    }
    pub fn cpu_details(&self) -> [(&'static str, String); 4] {
        [
            self.load(),
            ("I/O wait", percent(self.sample.iowait)),
            ("CPU pressure · 10s", percent(self.sample.pressure[0])),
            (
                "Tasks · running/total",
                self.sample.tasks.clone().unwrap_or_else(|| "—".into()),
            ),
        ]
    }
    pub fn memory_details(&self) -> [(&'static str, String); 5] {
        let [free, cached, buffers] = self.sample.memory_parts.map(|part| dash(part, gib));
        [
            ("Available", dash(self.sample.memory, |u| gib(u.available))),
            ("Free", free),
            ("Cached", cached),
            ("Buffers", buffers),
            ("Memory pressure · 10s", percent(self.sample.pressure[1])),
        ]
    }
    pub fn disk_details(&self) -> [(&'static str, String); 4] {
        [
            (
                "Filesystem path",
                dash(self.directory.as_deref(), |p| p.display().to_string()),
            ),
            ("Used / total", capacity(self.disk())),
            ("I/O pressure · 10s", percent(self.sample.pressure[2])),
            (
                "Available / inodes",
                dash(self.disk(), |u| {
                    format!(
                        "{} / {}",
                        gib(u.available),
                        dash(u.inodes, |inodes| format!("{inodes:.1}%"))
                    )
                }),
            ),
        ]
    }
}

impl Sampler {
    fn sample(
        &mut self,
        directory: Option<PathBuf>,
        expanded: bool,
        processes: bool,
        detail: Option<Identity>,
    ) -> (Sample, TrendPoint) {
        let now = Instant::now();
        let elapsed = self
            .previous_at
            .replace(now)
            .map(|at| now.duration_since(at).as_secs_f64());
        let mut sample = Sample {
            branch: directory.as_deref().and_then(git_branch),
            disk: directory.as_deref().and_then(disk_usage),
            directory,
            ..Sample::default()
        };
        let busy = |new: CpuCounters, old: CpuCounters| {
            counter_percent(new.total, old.total, new.idle, old.idle).map(|idle| 100. - idle)
        };
        let stat = read("/proc/stat").unwrap_or_default();
        let mut lines = stat.lines();
        let cpu = lines.next().and_then(cpu_counters).map(|(_, cpu)| cpu);
        let mut cpu_percent = None;
        if let Some((old, new)) = self.previous_cpu.zip(cpu) {
            cpu_percent = busy(new, old);
            sample.iowait = counter_percent(new.total, old.total, new.iowait, old.iowait);
        }
        self.previous_cpu = cpu;
        let previous_cores = std::mem::take(&mut self.previous_cores);
        if expanded {
            for (name, new) in lines.map_while(cpu_counters) {
                let value = previous_cores.get(name).and_then(|old| busy(new, *old));
                sample.cpu_cores.push((name.to_owned(), value));
                self.previous_cores.insert(name.to_owned(), new);
            }
            if let Some(text) = read("/proc/loadavg") {
                let mut fields = text.split_ascii_whitespace();
                sample.load = Some(fields.by_ref().take(3).collect::<Vec<_>>().join(" / "));
                sample.tasks = fields.next().map(str::to_owned);
            }
            sample.uptime =
                read("/proc/uptime").and_then(|text| text.split('.').next()?.parse().ok());
            for (value, resource) in sample.pressure.iter_mut().zip(["cpu", "memory", "io"]) {
                *value =
                    read(format!("/proc/pressure/{resource}")).and_then(|text| pressure(&text));
            }
        }
        if let Some(text) = read("/proc/meminfo") {
            sample_memory(&text, &mut sample);
        }
        let swap = expanded
            .then(|| {
                read("/proc/vmstat").and_then(|text| counter_pair(&text, ["pswpin", "pswpout"]))
            })
            .flatten();
        let mut swap_rates = [None; 2];
        if let Some((old, new)) = self.previous_swap.zip(swap) {
            swap_rates = [0, 1]
                .map(|i| per_second(new[i], old[i], elapsed).map(|v| mib(v * page_size() as f64)));
        }
        self.previous_swap = swap;
        let previous_disk = std::mem::take(&mut self.previous_disk);
        if expanded {
            let text = read("/proc/diskstats").unwrap_or_default();
            for (name, new) in text.lines().filter_map(disk_counters) {
                let old = previous_disk.get(name);
                sample.disks.push(Disk {
                    name: name.to_owned(),
                    rates: [0, 1].map(|i| {
                        old.and_then(|old| per_second(new.bytes[i], old.bytes[i], elapsed))
                            .map(mib)
                    }),
                    utilization: old
                        .and_then(|old| per_second(new.busy_ms, old.busy_ms, elapsed))
                        .map(|v| (v / 10.).min(100.)),
                });
                self.previous_disk.insert(name.to_owned(), new);
            }
        }
        let mut previous_net = std::mem::take(&mut self.previous_net);
        sample.networks = parse_networks(&read("/proc/net/dev").unwrap_or_default());
        sample
            .networks
            .retain(|net| Path::new(&format!("/sys/class/net/{}/device", net.name)).exists());
        for net in &mut sample.networks {
            if let Some((name, (rx, tx))) = previous_net.remove_entry(&net.name) {
                // Reuse the allocation shared with the history.
                net.name = name;
                net.rx = per_second(net.received, rx, elapsed).map(mib);
                net.tx = per_second(net.transmitted, tx, elapsed).map(mib);
            }
            self.previous_net
                .insert(net.name.clone(), (net.received, net.transmitted));
        }
        sample.processes = if processes {
            self.sample_processes(elapsed, detail)
        } else {
            self.previous_processes.clear();
            Vec::new()
        };
        let disk_rates = [0, 1].map(|i| {
            (!sample.disks.is_empty())
                .then(|| {
                    sample
                        .disks
                        .iter()
                        .map(|disk| disk.rates[i])
                        .sum::<Option<f32>>()
                })
                .flatten()
        });
        let percent_of = |usage: Option<Usage>| {
            usage
                .filter(|u| u.total > 0)
                .map(|u| u.used as f32 * 100. / u.total as f32)
        };
        sample.values = [
            cpu_percent,
            percent_of(sample.memory),
            disk_rates[0],
            disk_rates[1],
            percent_of(sample.swap),
            swap_rates[0],
            swap_rates[1],
        ];
        let point = TrendPoint {
            at: now,
            values: sample.values,
            networks: sample
                .networks
                .iter()
                .map(|net| (net.name.clone(), [net.rx, net.tx]))
                .collect(),
        };
        (sample, point)
    }

    fn sample_processes(&mut self, elapsed: Option<f64>, detail: Option<Identity>) -> Vec<Process> {
        let previous = std::mem::take(&mut self.previous_processes);
        let mut processes = Vec::with_capacity(previous.len());
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                continue;
            }
            let Some((mut process, counters)) = read_process(&entry) else {
                continue;
            };
            let identity = (process.pid, process.start);
            if let Some(old) = previous.get(&identity) {
                process.cpu = per_second(counters.ticks, old.ticks, elapsed)
                    .map(|v| (v * 100. / clock_ticks()) as f32);
                if let Some((old, new)) = old.io.zip(counters.io) {
                    process.read = per_second(new[0], old[0], elapsed);
                    process.write = per_second(new[1], old[1], elapsed);
                }
            }
            if detail == Some(identity)
                && let Some(detail) = process_details(identity)
            {
                process.command = detail.command;
                process.directory = detail.directory;
            }
            self.previous_processes.insert(identity, counters);
            processes.push(process);
        }
        processes.sort_unstable_by_key(|p| p.pid);
        processes
    }
}

fn page_size() -> u64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}
fn clock_ticks() -> f64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

/// A missing /proc file is a normal state: old kernels lack pressure files, a process may exit.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
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
fn record_history(history: &mut VecDeque<TrendPoint>, point: TrendPoint) {
    while history
        .front()
        .is_some_and(|old| point.at.duration_since(old.at) > Duration::from_secs(3600))
    {
        history.pop_front();
    }
    // Break plotted lines across a pause in sampling.
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
}
fn cpu_counters(line: &str) -> Option<(&str, CpuCounters)> {
    let mut fields = line.split_ascii_whitespace();
    let name = fields.next().filter(|name| name.starts_with("cpu"))?;
    let mut counters = [0u64; 8];
    for counter in &mut counters {
        *counter = fields.next()?.parse().ok()?;
    }
    Some((
        name,
        CpuCounters {
            total: counters.iter().sum(),
            idle: counters[3] + counters[4],
            iowait: counters[4],
        },
    ))
}
fn counters<const N: usize>(text: &str, names: [&str; N]) -> [Option<u64>; N] {
    let mut result = [None; N];
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        if let Some(index) = fields
            .next()
            .and_then(|name| names.iter().position(|n| *n == name))
        {
            result[index] = result[index].or_else(|| fields.next()?.parse().ok());
        }
    }
    result
}
fn counter_pair(text: &str, names: [&str; 2]) -> Option<[u64; 2]> {
    let [a, b] = counters(text, names);
    Some([a?, b?])
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
    .map(|kib| Some(kib? * 1024));
    let usage = |total: Option<u64>, available: Option<u64>| {
        let (total, available) = (total?, available?);
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
fn git_branch(directory: &Path) -> Option<String> {
    let (parent, dot_git) = directory
        .ancestors()
        .map(|parent| (parent, parent.join(".git")))
        .find(|(_, dot_git)| dot_git.exists())?;
    // A worktree or submodule has a .git file pointing at its directory.
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        parent.join(read(&dot_git)?.trim().strip_prefix("gitdir: ")?)
    };
    let head = read(git_dir.join("HEAD"))?;
    let head = head.trim();
    Some(
        head.strip_prefix("ref: refs/heads/")
            .unwrap_or(head)
            .to_owned(),
    )
}

/// Parses /proc/net/dev; the fields are rx bytes, packets, errors, drops, ... then the same for tx.
fn parse_networks(text: &str) -> Vec<Network> {
    let mut result: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let values: Vec<u64> = fields
                .split_ascii_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            (values.len() >= 12).then(|| Network {
                name: name.trim().into(),
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
    result.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    result
}
fn disk_counters(line: &str) -> Option<(&str, DiskCounters)> {
    let mut values = line.split_ascii_whitespace().skip(2);
    let name = values.next()?;
    if !Path::new(&format!("/sys/block/{name}/device")).exists() {
        return None;
    }
    let sectors = |value: &str| Some(value.parse::<u64>().ok()? * 512);
    Some((
        name,
        DiskCounters {
            bytes: [sectors(values.nth(2)?)?, sectors(values.nth(3)?)?],
            busy_ms: values.nth(2)?.parse().ok()?,
        },
    ))
}
fn disk_usage(path: &Path) -> Option<Usage> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    (stats.f_blocks > 0).then(|| Usage {
        used: stats.f_blocks.saturating_sub(stats.f_bfree) * stats.f_frsize,
        total: stats.f_blocks * stats.f_frsize,
        available: stats.f_bavail * stats.f_frsize,
        inodes: (stats.f_files > 0).then(|| {
            stats.f_files.saturating_sub(stats.f_ffree) as f64 * 100. / stats.f_files as f64
        }),
    })
}
fn process_stat(text: &str) -> Option<(Process, ProcessCounters)> {
    // The name is in parentheses and may itself contain ") ".
    let (pid, rest) = text.split_once(" (")?;
    let (name, rest) = rest.split_at(rest.rfind(") ")?);
    let mut values = rest[2..].split_ascii_whitespace();
    let state = values.next()?.chars().next()?;
    let parent = values.next()?.parse().ok()?;
    let ticks = values.nth(9)?.parse::<u64>().ok()? + values.next()?.parse::<u64>().ok()?;
    let threads = values.nth(4)?.parse().ok()?;
    let start = values.nth(1)?.parse().ok()?;
    let rss = values.nth(1)?.parse::<u64>().ok()? * page_size();
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
            command: String::new(),
            directory: None,
        },
        ProcessCounters { ticks, io: None },
    ))
}
fn read_process(entry: &fs::DirEntry) -> Option<(Process, ProcessCounters)> {
    let (mut process, mut counters) = process_stat(&read(entry.path().join("stat"))?)?;
    process.uid = entry.metadata().ok()?.uid();
    counters.io = read(entry.path().join("io"))
        .and_then(|text| counter_pair(&text, ["read_bytes:", "write_bytes:"]));
    Some((process, counters))
}

fn process_details(identity: Identity) -> Option<ProcessDetails> {
    let dir = PathBuf::from(format!("/proc/{}", identity.0));
    let (process, _) = process_stat(&read(dir.join("stat"))?)?;
    if process.start != identity.1 {
        return None;
    }
    let command = fs::read(dir.join("cmdline")).ok().map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .replace('\0', " ")
    });
    Some(ProcessDetails {
        identity,
        command: command
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| format!("[{}]", process.name)),
        directory: fs::read_link(dir.join("cwd")).ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_text_parses_to_units_and_identity() {
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
        let (memory, swap) = (sample.memory.unwrap(), sample.swap.unwrap());
        assert_eq!((memory.used, memory.total), (334 * 1024, 1000 * 1024));
        assert_eq!((swap.used, swap.total), (300 * 1024, 400 * 1024));
        let net = parse_networks("Inter-| Receive\n eth0: 1024 1 2 3 0 0 0 0 2048 4 5 6 0 0 0 0\n");
        assert_eq!(net.len(), 1);
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
        let (process, counters) = process_stat(text).unwrap();
        assert_eq!(
            (process.pid, process.parent, process.threads, process.rss),
            (123, 1, 3, 20 * page_size())
        );
        assert_eq!(process.name, "name ) with spaces");
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
        let point = |second| TrendPoint {
            at: start + Duration::from_secs(second),
            values: [Some(second as f32); 7],
            networks: Vec::new(),
        };
        let mut history = VecDeque::new();
        for second in 0..=3610 {
            record_history(&mut history, point(second));
        }
        assert_eq!(history.len(), 3601);
        assert_eq!(history.front().unwrap().values[0], Some(10.));
        record_history(&mut history, point(3615));
        assert_eq!(history.front().unwrap().values[0], Some(15.));
        assert_eq!(history[history.len() - 2].values, [None; 7]);
    }

    #[test]
    fn trends_keep_the_window_and_follow_one_interface_across_samples() {
        let mut stats = SystemStats::new().unwrap();
        let eth0: Arc<str> = "eth0".into();
        stats.sample.networks = parse_networks("eth0: 1 0 0 0 0 0 0 0 2 0 0 0");
        let now = Instant::now();
        let point = |age: u64| {
            let selected = (Arc::clone(&eth0), [Some(age as f32 / 2.), Some(age as f32)]);
            let other = ("eth1".into(), [Some(1000.), Some(2000.)]);
            TrendPoint {
                at: now - Duration::from_secs(age),
                values: [
                    Some(age as f32),
                    None,
                    Some(age as f32 / 2.),
                    Some(age as f32),
                    None,
                    None,
                    None,
                ],
                networks: match age {
                    20 => Vec::new(),
                    30 => vec![other, selected],
                    _ => vec![selected, other],
                },
            }
        };
        stats.history = [900, 120, 30, 20, 10].into_iter().map(point).collect();
        let values =
            |trend: &ResourceTrend| trend.points.iter().map(|(_, v)| *v).collect::<Vec<_>>();
        let system = stats.system_trends(60, 2..4);
        assert_eq!(values(&system[0]), [Some(15.), Some(10.), Some(5.)]);
        assert_eq!(values(&system[1]), [Some(30.), Some(20.), Some(10.)]);
        let network = stats.network_trends(60, &stats.sample.networks[0]);
        assert_eq!(values(&network[0]), [Some(15.), None, Some(5.)]);
        assert_eq!(values(&network[1]), [Some(30.), None, Some(10.)]);
    }

    #[test]
    fn sampling_reads_this_process_and_its_details() {
        let mut sampler = Sampler::default();
        let (sample, _) = sampler.sample(None, true, true, None);
        let pid = std::process::id();
        let identity = sample
            .processes
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| (p.pid, p.start))
            .unwrap();
        let (sample, point) = sampler.sample(Some("/".into()), true, true, Some(identity));
        assert!(!sample.cpu_cores.is_empty());
        assert_eq!(point.values, sample.values);
        assert!(sample.disk.is_some());
        let process = sample.processes.iter().find(|p| p.pid == pid).unwrap();
        assert!(process.cpu.is_some() && !process.command.is_empty());
        assert_eq!(
            process.directory.as_deref(),
            Some(std::env::current_dir().unwrap().as_path())
        );
        assert!(process_details((pid, identity.1 + 1)).is_none());
    }

    #[test]
    fn process_view_keeps_the_selection_across_resorts_and_filters_unicode() {
        use crate::monitor::{Action, Emphasis, Monitor};
        let process = |pid: u32, name: &str| Process {
            pid,
            start: 0,
            name: name.into(),
            uid: 1000,
            state: 'S',
            cpu: Some(0.),
            rss: u64::from(pid),
            read: None,
            write: None,
            threads: 1,
            parent: 1,
            command: String::new(),
            directory: None,
        };
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.processes = vec![
            process(1, "init"),
            process(2, "ΟΣ"),
            process(3, "K-worker"),
            process(4, "İdaemon"),
        ];
        monitor.toggle();
        monitor.action(Action::Tab(6));
        let pids = |view: &crate::monitor::View| {
            view.hits
                .iter()
                .filter_map(|hit| match hit.action {
                    Action::Process(pid) => Some(pid),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        monitor.action(Action::Search);
        for (query, pid) in [("1000", 1), ("ος", 2), ("k", 3), ("İ", 4), ("3", 3)] {
            monitor.key(u32::from('u'), "", true);
            monitor.insert_query(query);
            assert!(pids(&monitor.view(80, 20)).contains(&pid), "{query}");
        }
        monitor.key(u32::from('u'), "", true);
        monitor.action(Action::Sort(1));
        assert_eq!(pids(&monitor.view(80, 20)), [4, 3, 2, 1]);
        monitor.action(Action::Process(2));
        // A new sample reorders the rows and replaces process 2 by a new process with the same pid.
        let mut processes = std::mem::take(&mut monitor.stats.sample.processes);
        processes[0].rss = 40;
        processes[1].start = 1;
        monitor.stats.sample.processes = processes;
        monitor.stats.process_revision += 1;
        let view = monitor.view(80, 20);
        assert_eq!(pids(&view), [1, 4, 3, 2]);
        assert!(
            !view
                .texts
                .iter()
                .any(|text| text.emphasis(false) == Emphasis::Selected)
        );
    }
}
