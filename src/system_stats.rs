//! Asynchronous local resource samples and fifteen minutes of history.

use std::{
    borrow::Cow,
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
    directory: Option<PathBuf>,
    text: String,
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

pub struct Process {
    pub pid: u32,
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

struct TrendPoint {
    at: Instant,
    values: [Option<f32>; 2],
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
    result: Option<Sample>,
    history: VecDeque<TrendPoint>,
    initialized: bool,
    closed: bool,
}

#[derive(Default)]
struct Sample {
    directory: Option<PathBuf>,
    cpu: Option<f32>,
    iowait: Option<f32>,
    cores: Option<u64>,
    load: Option<[f64; 3]>,
    tasks: Option<String>,
    uptime: Option<u64>,
    pressure: [Option<f32>; 3],
    memory: Option<Usage>,
    swap: Option<Usage>,
    swap_rates: [Option<f64>; 2],
    disk: Option<Usage>,
    disk_name: Option<String>,
    disk_rates: [Option<f64>; 2],
    disk_util: Option<f64>,
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

struct DiskCounters {
    name: String,
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
            directory: None,
            text: "CPU —  MEM —  DSK —".into(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.sampler.wakeup.as_raw_fd()
    }

    pub fn set_processes_enabled(&mut self, enabled: bool) {
        let mut state = self.sampler.state.lock().unwrap();
        if state.processes_enabled != enabled {
            state.processes_enabled = enabled;
            self.sampler.changed.notify_one();
        }
    }

    pub fn refresh(&mut self, directory: Option<&Path>, expanded: bool) -> io::Result<bool> {
        let mut state = self.sampler.state.lock().unwrap();
        let directory_changed = state.directory.as_deref() != directory;
        if directory_changed {
            state.directory = directory.map(Path::to_path_buf);
            self.directory = state.directory.clone();
        }
        if !state.initialized {
            state.initialized = true;
            self.sampler.changed.notify_one();
        }
        if state.result.is_some() {
            crate::eventfd::consume_notification(self.fd())?;
        }
        let result = state.result.take();
        drop(state);
        if result.is_none() && !directory_changed {
            return Ok(false);
        }
        let sampled = result.is_some();
        if let Some(sample) = result {
            self.sample = sample;
        }
        let disk = self
            .disk()
            .and_then(|u| filesystem_percent(u.total, u.total - u.used, u.available));
        let text = format!(
            "CPU {}  MEM {}  DSK {}",
            percent(self.sample.cpu),
            percent(sample_values(&self.sample)[1]),
            percent(disk)
        );
        let changed = text != self.text || expanded && (sampled || directory_changed);
        self.text = text;
        Ok(changed)
    }

    fn disk(&self) -> Option<Usage> {
        (self.sample.directory == self.directory)
            .then_some(self.sample.disk)
            .flatten()
    }

    fn network(&self, interface: Option<&str>) -> Option<&Network> {
        match interface {
            Some(name) => self.sample.networks.iter().find(|net| net.name == name),
            None => self
                .sample
                .networks
                .iter()
                .find(|net| net.name != "lo")
                .or_else(|| self.sample.networks.first()),
        }
    }

    pub fn networks(&self) -> &[Network] {
        &self.sample.networks
    }
    pub fn processes(&self) -> &[Process] {
        &self.sample.processes
    }

    pub fn system_trends(&self, seconds: u32) -> Vec<ResourceTrend> {
        let state = self.sampler.state.lock().unwrap();
        let now = Instant::now();
        let start = state.history.partition_point(|point| {
            now.saturating_duration_since(point.at).as_secs_f32() > seconds as f32
        });
        let history = state.history.range(start..);
        let values = sample_values(&self.sample);
        let mut result = Vec::with_capacity(2);
        for index in 0..2 {
            result.push(ResourceTrend {
                name: ["CPU", "MEM"][index],
                value: values[index],
                summary: if index == 0 {
                    format!(
                        "{} cores · load {}",
                        number(self.sample.cores),
                        self.sample.load.map_or_else(
                            || "—".into(),
                            |v| format!("{:.2} / {:.2} / {:.2}", v[0], v[1], v[2])
                        )
                    )
                } else {
                    capacity(self.sample.memory)
                },
                points: history
                    .clone()
                    .map(|point| {
                        let age = now.saturating_duration_since(point.at).as_secs_f32();
                        (age, point.values[index])
                    })
                    .collect(),
                maximum: 100.,
                unit: "%",
            });
        }
        result
    }

    pub fn network_trends(&self, seconds: u32, interface: &str) -> Vec<ResourceTrend> {
        let Some(network) = self.network(Some(interface)) else {
            return Vec::new();
        };
        let state = self.sampler.state.lock().unwrap();
        let now = Instant::now();
        let start = state.history.partition_point(|point| {
            now.saturating_duration_since(point.at).as_secs_f32() > seconds as f32
        });
        let history = state.history.range(start..);
        let mut result = Vec::with_capacity(2);
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
        let peak = points
            .iter()
            .flatten()
            .filter_map(|(_, v)| *v)
            .fold(0.125f32, f32::max);
        let maximum = 2f32.powf(peak.log2().ceil());
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
        let mut result = vec![
            (
                "Load · 1/5/15 min".into(),
                self.sample.load.map_or_else(
                    || "—".into(),
                    |v| format!("{:.2} / {:.2} / {:.2}", v[0], v[1], v[2]),
                ),
            ),
            (
                "CPU cores / I/O wait".into(),
                format!(
                    "{} / {}",
                    number(self.sample.cores),
                    percent(self.sample.iowait)
                ),
            ),
            (
                "MEM available".into(),
                self.sample
                    .memory
                    .map_or_else(|| "—".into(), |u| bytes(u.available)),
            ),
            (
                "Tasks · running/total".into(),
                self.sample.tasks.clone().unwrap_or_else(|| "—".into()),
            ),
            (
                "Uptime".into(),
                self.sample.uptime.map_or_else(
                    || "—".into(),
                    |s| format!("{}d {:02}h {:02}m", s / 86400, s / 3600 % 24, s / 60 % 60),
                ),
            ),
            (
                "PSI some 10s · C/M/I".into(),
                self.sample
                    .pressure
                    .map(|v| v.map_or_else(|| "—".into(), |v| format!("{v:.1}%")))
                    .join(" / "),
            ),
        ];
        result.extend(self.storage());
        result
    }

    pub fn storage(&self) -> Vec<(String, String)> {
        let valid = self.sample.directory == self.directory;
        vec![
            (
                "Filesystem path".into(),
                self.directory
                    .as_ref()
                    .map_or_else(|| "—".into(), |p| p.display().to_string()),
            ),
            ("DSK used / total".into(), capacity(self.disk())),
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
            (
                format!(
                    "DSK I/O · {}",
                    if valid {
                        self.sample.disk_name.as_deref().unwrap_or("—")
                    } else {
                        "—"
                    }
                ),
                if valid {
                    format!(
                        "{} / {}",
                        rate(self.sample.disk_rates[0]),
                        rate(self.sample.disk_rates[1])
                    )
                } else {
                    "—".into()
                },
            ),
            (
                "DSK utilization".into(),
                if valid {
                    self.sample
                        .disk_util
                        .map_or_else(|| "—".into(), |v| format!("{v:.1}%"))
                } else {
                    "—".into()
                },
            ),
            ("Swap used / total".into(), capacity(self.sample.swap)),
            (
                "Swap in / out".into(),
                format!(
                    "{} / {}",
                    rate(self.sample.swap_rates[0]),
                    rate(self.sample.swap_rates[1])
                ),
            ),
        ]
    }

    pub fn network_details(&self, interface: Option<&str>) -> Vec<(String, String)> {
        let Some(net) = self.network(interface) else {
            return vec![("Interface".into(), "—".into())];
        };
        vec![
            ("Interface".into(), net.name.clone()),
            (
                "Receive / transmit".into(),
                format!("{} / {}", rate(net.rx), rate(net.tx)),
            ),
            (
                "Total RX / TX".into(),
                format!("{} / {}", bytes(net.received), bytes(net.transmitted)),
            ),
            (
                "Errors RX / TX".into(),
                format!("{} / {}", net.rx_errors, net.tx_errors),
            ),
            (
                "Dropped RX / TX".into(),
                format!("{} / {}", net.rx_dropped, net.tx_dropped),
            ),
        ]
    }

    pub fn label<'a>(
        &'a self,
        controller: bool,
        status: Option<&'a str>,
        remote: bool,
    ) -> Cow<'a, str> {
        let status = status.filter(|text| !text.is_empty());
        let text = status.unwrap_or(&self.text);
        let prefix = match (controller, remote && status.is_none()) {
            (true, false) => return Cow::Borrowed(text),
            (true, true) => "Local  ",
            (false, false) => "Read-only  ",
            (false, true) => "Read-only  Local  ",
        };
        [prefix, text].concat().into()
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
    let mut previous_net: HashMap<String, (u64, u64)> = HashMap::new();
    let mut previous_disk: Option<DiskCounters> = None;
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
        let processes_enabled = state.processes_enabled;
        drop(state);
        let mut sample = Sample {
            directory: directory.clone(),
            ..Sample::default()
        };
        let cpu = fs::File::open("/proc/stat").ok().and_then(|file| {
            let mut text = String::new();
            io::BufReader::with_capacity(256, file)
                .read_line(&mut text)
                .ok()?;
            cpu_counters(&text)
        });
        if let Some((old, new)) = previous_cpu.zip(cpu) {
            sample.cpu =
                counter_percent(new.total, old.total, new.idle, old.idle).map(|idle| 100. - idle);
            sample.iowait = counter_percent(new.total, old.total, new.iowait, old.iowait);
        }
        previous_cpu = cpu;
        sample.cores = match unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } {
            n if n > 0 => Some(n as u64),
            _ => None,
        };
        if let Ok(text) = fs::read_to_string("/proc/loadavg") {
            let fields: Vec<_> = text.split_whitespace().collect();
            sample.load = (|| {
                Some([
                    fields.first()?.parse().ok()?,
                    fields.get(1)?.parse().ok()?,
                    fields.get(2)?.parse().ok()?,
                ])
            })();
            sample.tasks = fields.get(3).map(|text| text.to_string());
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
        if let Ok(text) = fs::read_to_string("/proc/meminfo") {
            sample.memory = memory_usage(&text);
            sample.swap = swap_usage(&text);
        }
        let swap = fs::read_to_string("/proc/vmstat")
            .ok()
            .and_then(|text| vm_swap(&text));
        if let Some((old, new)) = previous_swap.zip(swap) {
            sample.swap_rates = std::array::from_fn(|i| {
                per_second(new[i], old[i], elapsed).map(|v| v * page_size as f64)
            });
        }
        previous_swap = swap;
        sample.disk = directory.as_deref().and_then(|path| disk_usage(path).ok());
        let disk = directory.as_deref().and_then(disk_counters);
        if let Some(disk) = &disk {
            sample.disk_name = Some(disk.name.clone());
            if let Some(previous) = &previous_disk
                && previous.name == disk.name
            {
                sample.disk_rates =
                    std::array::from_fn(|i| per_second(disk.bytes[i], previous.bytes[i], elapsed));
                sample.disk_util = per_second(disk.busy_ms, previous.busy_ms, elapsed)
                    .map(|v| (v / 10.).min(100.));
            }
        }
        previous_disk = disk;
        if let Ok(text) = fs::read_to_string("/proc/net/dev") {
            sample.networks = network_counters(&text);
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
            let (processes, counters) =
                sample_processes(&previous_processes, elapsed, ticks_per_second, page_size);
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
        let previous = state.result.replace(sample);
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
fn sample_values(sample: &Sample) -> [Option<f32>; 2] {
    [
        sample.cpu,
        sample.memory.map(|u| u.used as f32 * 100. / u.total as f32),
    ]
}
fn record_history(history: &mut VecDeque<TrendPoint>, point: TrendPoint) {
    while history
        .front()
        .is_some_and(|old| point.at.duration_since(old.at) > Duration::from_secs(900))
    {
        history.pop_front();
    }
    if history
        .back()
        .is_some_and(|old| point.at.duration_since(old.at) > Duration::from_secs(2))
    {
        history.push_back(TrendPoint {
            at: point.at,
            values: [None; 2],
            networks: Vec::new(),
        });
    }
    history.push_back(point);
    while history.len() > 902 {
        history.pop_front();
    }
}
fn cpu_counters(text: &str) -> Option<CpuCounters> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
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
fn vm_swap(text: &str) -> Option<[u64; 2]> {
    let value = |name| {
        text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == name)
                .then(|| fields.next()?.parse().ok())
                .flatten()
        })
    };
    Some([value("pswpin")?, value("pswpout")?])
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
fn mount_source<'a>(text: &'a str, path: &Path) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mount = mount.split_whitespace().nth(4)?;
            let decoded = mount.contains('\\').then(|| {
                mount
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\")
            });
            let mount = decoded.as_deref().unwrap_or(mount);
            if !path.starts_with(mount) {
                return None;
            }
            let source = filesystem.split_whitespace().nth(1)?;
            Some((mount.len(), source))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, source)| source)
        .filter(|source| source.starts_with("/dev/"))
}
fn disk_counters(path: &Path) -> Option<DiskCounters> {
    let mut device = fs::metadata(path).ok()?.dev();
    if libc::major(device) == 0 {
        // Filesystems such as Btrfs expose an anonymous st_dev; mountinfo names the block device.
        let path = fs::canonicalize(path).ok()?;
        let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
        let source = mount_source(&mounts, &path)?;
        device = fs::metadata(source).ok()?.rdev();
    }
    let major = libc::major(device);
    let minor = libc::minor(device);
    let text = fs::read_to_string("/proc/diskstats").ok()?;
    text.lines().find_map(|line| {
        let mut values = line.split_whitespace();
        if values.next()?.parse::<u32>().ok()? != major
            || values.next()?.parse::<u32>().ok()? != minor
        {
            return None;
        }
        Some(DiskCounters {
            name: values.next()?.to_string(),
            bytes: [
                values.nth(2)?.parse::<u64>().ok()?.checked_mul(512)?,
                values.nth(3)?.parse::<u64>().ok()?.checked_mul(512)?,
            ],
            busy_ms: values.nth(2)?.parse().ok()?,
        })
    })
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
fn filesystem_percent(blocks: u64, free: u64, available: u64) -> Option<f32> {
    let used = blocks.checked_sub(free)?;
    let total = used.checked_add(available)?;
    (total > 0).then(|| used as f32 * 100. / total as f32)
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
fn process_io(text: &str) -> Option<[u64; 2]> {
    let value = |name| {
        text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == name)
                .then(|| fields.next()?.parse().ok())
                .flatten()
        })
    };
    Some([value("read_bytes:")?, value("write_bytes:")?])
}

fn sample_processes(
    previous: &HashMap<u32, ProcessCounters>,
    elapsed: Option<f64>,
    ticks_per_second: f64,
    page_size: u64,
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
        let Some((mut process, mut current)) = fs::read_to_string(path.join("stat"))
            .ok()
            .and_then(|text| process_stat(&text, page_size))
        else {
            continue;
        };
        process.uid = match entry.metadata() {
            Ok(meta) => meta.uid(),
            Err(_) => continue,
        };
        current.io = fs::read_to_string(path.join("io"))
            .ok()
            .and_then(|text| process_io(&text));
        if let Some(old) = previous.get(&pid).filter(|old| old.start == current.start) {
            process.cpu = per_second(current.ticks, old.ticks, elapsed)
                .map(|v| (v * 100. / ticks_per_second) as f32);
            if let Some((old, new)) = old.io.zip(current.io) {
                process.read = per_second(new[0], old[0], elapsed);
                process.write = per_second(new[1], old[1], elapsed);
            }
        }
        process.command = fs::read(path.join("cmdline"))
            .ok()
            .map(|v| {
                String::from_utf8_lossy(&v)
                    .trim_end_matches('\0')
                    .replace('\0', " ")
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| format!("[{}]", process.name));
        process.directory = fs::read_link(path.join("cwd")).ok();
        processes.push(process);
        counters.insert(pid, current);
    }
    processes.sort_by_key(|p| p.pid);
    (processes, counters)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anonymous_filesystem_maps_to_its_longest_mount() {
        let mounts = "1 0 0:1 / / rw - btrfs /dev/root rw\n2 1 0:1 /home /home rw - btrfs /dev/data rw\n3 1 0:2 / /home/a\\040b rw - btrfs /dev/work rw\n";
        assert_eq!(
            mount_source(mounts, Path::new("/home/a b/project")),
            Some("/dev/work")
        );
        assert_eq!(
            mount_source(mounts, Path::new("/home/abc/project")),
            Some("/dev/data")
        );
        assert_eq!(
            mount_source("1 0 0:1 / / rw - tmpfs tmpfs rw", Path::new("/tmp")),
            None
        );
        assert_eq!(
            mount_source(
                "1 0 0:1 / / rw - btrfs /dev/root rw\n2 1 0:2 / /tmp rw - tmpfs tmpfs rw",
                Path::new("/tmp/project")
            ),
            None
        );
    }
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
            process_io("read_bytes: 42\nwrite_bytes: 84\n"),
            Some([42, 84])
        );
        assert_eq!(
            pressure("some avg10=0.25 avg60=0.1\nfull avg10=1.0"),
            Some(0.25)
        );
    }
    #[test]
    fn history_keeps_fifteen_minutes_and_breaks_gaps() {
        let start = Instant::now();
        let mut history = VecDeque::new();
        for second in 0..=910 {
            record_history(
                &mut history,
                TrendPoint {
                    at: start + Duration::from_secs(second),
                    values: [Some(second as f32); 2],
                    networks: Vec::new(),
                },
            );
        }
        assert_eq!(history.len(), 901);
        assert_eq!(history.front().unwrap().values[0], Some(10.));
        record_history(
            &mut history,
            TrendPoint {
                at: start + Duration::from_secs(915),
                values: [Some(915.); 2],
                networks: Vec::new(),
            },
        );
        assert_eq!(history.front().unwrap().values[0], Some(15.));
        assert_eq!(history[history.len() - 2].values, [None; 2]);
    }
    #[test]
    fn process_view_keeps_scroll_focus_and_filter_matching() {
        use crate::monitor::{Action, Monitor};
        let mut monitor = Monitor::new().unwrap();
        monitor.stats.sample.processes = (1..=200)
            .map(|pid| Process {
                pid,
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
        monitor.action(Action::Tab(2));
        monitor.view(80, 10);
        monitor.scroll = usize::MAX;
        let view = monitor.view(80, 10);
        assert!(
            view.hits
                .iter()
                .any(|hit| hit.action == Action::Process(200))
        );
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
            monitor.bytes(&[21]);
            monitor.bytes(query.as_bytes());
            let view = monitor.view(80, 10);
            assert!(
                view.hits
                    .iter()
                    .any(|hit| hit.action == Action::Process(pid))
            );
        }
    }
    #[test]
    fn trend_windows_keep_recent_ring_order_and_network_gaps() {
        let mut stats = SystemStats::new().unwrap();
        stats.sample.networks = network_counters("eth0: 1 0 0 0 0 0 0 0 2 0 0 0");
        let now = Instant::now();
        let point = |age| TrendPoint {
            at: now - Duration::from_secs(age),
            values: [Some(age as f32), None],
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
        let system = stats.system_trends(60);
        assert_eq!(
            system.iter().map(|t| t.name).collect::<Vec<_>>(),
            ["CPU", "MEM"]
        );
        assert_eq!(
            system[0].points.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
            [Some(30.), Some(20.), Some(10.)]
        );
        let network = stats.network_trends(60, "eth0");
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
        assert!(stats.network_trends(60, "missing").is_empty());
    }
    #[test]
    fn async_sampling_publishes_real_rates_and_gates_process_scans() {
        let mut stats = SystemStats::new().unwrap();
        assert!(stats.system_trends(60).iter().all(|t| t.points.is_empty()));
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
        assert!(stats.system_trends(60)[0].value.is_some());
        assert!(stats.processes().is_empty());
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
        assert!(stats.refresh(None, true).unwrap());
        assert!(stats.disk().is_none());
        assert_eq!(
            stats.label(false, Some("copied"), false),
            "Read-only  copied"
        );
    }
}
