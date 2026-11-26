//! Asynchronous local resource samples and one hour of history.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::ops::Range;
use std::os::fd::RawFd;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, io};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const GIB: f64 = 1073741824.;
pub(crate) const MIB: f64 = 1048576.;
/// The series of `Sample::values` and the history.
const NAMES: [&str; 7] = ["CPU", "MEMORY", "READ", "WRITE", "SWAP", "IN", "OUT"];

/// A process across samples: its pid and start time.
type Identity = (u32, u64);
/// CPU time in clock ticks and, when readable, bytes read and written.
type ProcessCounters = (u64, Option<[u64; 2]>);

pub struct SystemStats {
    worker: crate::worker::Worker<Request, (Sample, TrendPoint)>,
    pub(crate) sample: Sample,
    history: VecDeque<TrendPoint>,
    pub(crate) process_revision: u64,
    directory: Option<PathBuf>,
    processes: bool,
    detail: Option<Identity>,
    next_sample: Option<Instant>,
    pub summary: [String; 4],
}

pub struct ResourceTrend {
    pub name: &'static str,
    pub value: Option<f32>,
    pub summary: String,
    /// Age in seconds and value, oldest first.
    pub points: Vec<(f32, Option<f32>)>,
    pub unit: &'static str,
}

pub struct Network {
    pub name: Arc<str>,
    /// Receive and transmit rates in MiB/s; the counters below are in the same order.
    pub rates: [Option<f32>; 2],
    bytes: [u64; 2],
    errors: [u64; 2],
    dropped: [u64; 2],
}

pub struct Disk {
    pub name: String,
    /// Read and write rates in MiB/s.
    pub rates: [Option<f32>; 2],
    pub utilization: Option<f32>,
}

#[derive(Default)]
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

struct TrendPoint {
    at: Instant,
    values: [Option<f32>; 7],
    networks: Vec<(Arc<str>, [Option<f32>; 2])>,
}

struct Request {
    directory: Option<PathBuf>,
    expanded: bool,
    processes: bool,
    /// The process whose command line and directory are read with each sample.
    detail: Option<Identity>,
}

/// The previous sample's counters, from which the next one computes rates.
#[derive(Default)]
struct Sampler {
    at: Option<Instant>,
    cpu: Option<[u64; 3]>,
    cores: HashMap<String, [u64; 3]>,
    swap: Option<[u64; 2]>,
    disks: HashMap<String, [u64; 3]>,
    networks: HashMap<Arc<str>, [u64; 2]>,
    processes: HashMap<Identity, ProcessCounters>,
}

#[derive(Default)]
pub(crate) struct Sample {
    directory: Option<PathBuf>,
    branch: Option<String>,
    values: [Option<f32>; 7],
    pub overview: [(&'static str, String); 2],
    pub cpu_details: [(&'static str, String); 4],
    pub memory_details: [(&'static str, String); 5],
    io_pressure: Option<f32>,
    memory: Option<Usage>,
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

pub(crate) fn dash<T>(value: Option<T>, format: impl FnOnce(T) -> String) -> String {
    value.map_or_else(|| "—".into(), format)
}
pub(crate) fn percent(value: Option<f32>) -> String {
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

impl Network {
    pub fn details(&self) -> [(&'static str, String); 3] {
        let pair = |[rx, tx]: [String; 2]| format!("{rx} / {tx}");
        [
            ("Total RX / TX", pair(self.bytes.map(gib))),
            ("Errors RX / TX", pair(self.errors.map(|n| n.to_string()))),
            ("Dropped RX / TX", pair(self.dropped.map(|n| n.to_string()))),
        ]
    }
}

impl SystemStats {
    pub fn new() -> io::Result<Self> {
        let mut sampler = Sampler::default();
        let worker = crate::worker::Worker::new(
            "nextty-stats",
            Arc::new(crate::sys::new()?),
            move |request, publish| publish(sampler.sample(request)),
        )?;
        let mut stats = Self {
            worker,
            sample: Sample::default(),
            history: VecDeque::new(),
            process_revision: 0,
            directory: None,
            processes: false,
            detail: None,
            next_sample: None,
            summary: Default::default(),
        };
        stats.summary = stats.summarize();
        Ok(stats)
    }

    pub fn fd(&self) -> RawFd {
        self.worker.fd()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.next_sample
    }

    /// Whether processes are scanned and which one's details are read; a change samples at once.
    pub fn set_sampling(&mut self, processes: bool, detail: Option<Identity>) {
        if (self.processes, self.detail) != (processes, detail) {
            (self.processes, self.detail) = (processes, detail);
            self.next_sample = Some(Instant::now());
        }
    }

    /// Applies finished background work and starts the next sample when due;
    /// returns whether anything visible changed.
    pub fn refresh(
        &mut self,
        directory: Option<&Path>,
        expanded: bool,
        ready: bool,
    ) -> io::Result<bool> {
        let mut changed = false;
        let mut updated = false;
        if ready
            && crate::sys::consume_notification(self.fd())?
            && let Some((sample, point)) = self.worker.take()
        {
            changed = expanded || sample.branch != self.sample.branch;
            updated = true;
            record_history(&mut self.history, point);
            self.sample = sample;
            self.process_revision += 1;
        }
        if self.directory.as_deref() != directory {
            self.directory = directory.map(Path::to_path_buf);
            self.next_sample = None;
            (changed, updated) = (true, true);
        }
        let now = Instant::now();
        if self.next_sample.is_none_or(|deadline| now >= deadline) {
            self.next_sample = Some(now + SAMPLE_INTERVAL);
            self.worker.submit(Request {
                directory: self.directory.clone(),
                expanded,
                processes: self.processes,
                detail: self.detail,
            });
        }
        if updated {
            let summary = self.summarize();
            changed |= summary != self.summary;
            self.summary = summary;
        }
        Ok(changed)
    }

    fn summarize(&self) -> [String; 4] {
        let values = self.sample.values;
        let disk = self
            .disk()
            .filter(|u| u.used + u.available > 0)
            .map(|u| u.used as f32 * 100. / (u.used + u.available) as f32);
        [
            ("\u{f2db}", values[0]),
            ("\u{efc5}", values[1]),
            ("\u{f0a0}", disk),
            ("\u{f0ec}", values[4]),
        ]
        .map(|(icon, value)| format!("{icon} {}", percent(value)))
    }

    /// Usage of the filesystem holding the directory, unless the sample is for another one.
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

    /// The history within `seconds` as (age in seconds, value), oldest first.
    fn points(
        &self,
        seconds: u32,
        value: impl Fn(&TrendPoint) -> Option<f32>,
    ) -> Vec<(f32, Option<f32>)> {
        let now = Instant::now();
        let age = |point: &TrendPoint| now.saturating_duration_since(point.at).as_secs_f32();
        let start = self
            .history
            .partition_point(|point| age(point) > seconds as f32);
        self.history
            .range(start..)
            .map(|point| (age(point), value(point)))
            .collect()
    }

    pub fn system_trends(&self, seconds: u32, indices: Range<usize>) -> Vec<ResourceTrend> {
        indices
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
                points: self.points(seconds, |point| point.values[index]),
                unit: if matches!(index, 0 | 1 | 4) {
                    "%"
                } else {
                    "MiB/s"
                },
            })
            .collect()
    }

    pub fn network_trends(&self, seconds: u32, network: &Network) -> Vec<ResourceTrend> {
        ["RX", "TX"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| ResourceTrend {
                name,
                value: network.rates[index],
                summary: network.name.to_string(),
                points: self.points(seconds, |point| {
                    let (_, rates) = point.networks.iter().find(|(n, _)| *n == network.name)?;
                    rates[index]
                }),
                unit: "MiB/s",
            })
            .collect()
    }

    pub fn disk_details(&self) -> [(&'static str, String); 4] {
        [
            (
                "Filesystem path",
                dash(self.directory.as_deref(), |p| p.display().to_string()),
            ),
            ("Used / total", capacity(self.disk())),
            ("I/O pressure · 10s", percent(self.sample.io_pressure)),
            (
                "Available / inodes",
                dash(self.disk(), |u| {
                    let inodes = dash(u.inodes, |inodes| format!("{inodes:.1}%"));
                    format!("{} / {inodes}", gib(u.available))
                }),
            ),
        ]
    }
}

impl Sampler {
    fn sample(&mut self, request: Request) -> (Sample, TrendPoint) {
        let Request {
            directory,
            expanded,
            processes,
            detail,
        } = request;
        let now = Instant::now();
        let elapsed = self
            .at
            .replace(now)
            .map(|at| now.duration_since(at).as_secs_f64());
        let rate = |new: u64, old: u64| per_second(new, old, elapsed);
        let mut sample = Sample {
            branch: directory.as_deref().and_then(git_branch),
            disk: directory.as_deref().and_then(disk_usage),
            directory,
            ..Sample::default()
        };
        let stat = read("/proc/stat").unwrap_or_default();
        let mut cpus = stat.lines().map_while(cpu_counters);
        let cpu = cpus.next().map(|(_, cpu)| cpu);
        let old_cpu = std::mem::replace(&mut self.cpu, cpu);
        let busy = |new, old| share(new, old, 1).map(|idle| 100. - idle);
        let cpu_percent = old_cpu.zip(cpu).and_then(|(old, new)| busy(new, old));
        let iowait = old_cpu.zip(cpu).and_then(|(old, new)| share(new, old, 2));
        let cores = std::mem::take(&mut self.cores);
        let (mut load, mut tasks, mut uptime, mut pressure) = (None, None, None, [None; 3]);
        if expanded {
            for (name, new) in cpus {
                let value = cores.get(name).and_then(|old| busy(new, *old));
                sample.cpu_cores.push((name.to_owned(), value));
                self.cores.insert(name.to_owned(), new);
            }
            if let Some(text) = read("/proc/loadavg") {
                let mut fields = text.split_ascii_whitespace();
                load = Some(fields.by_ref().take(3).collect::<Vec<_>>().join(" / "));
                tasks = fields.next().map(str::to_owned);
            }
            uptime = read("/proc/uptime").and_then(|text| text.split('.').next()?.parse().ok());
            pressure = ["cpu", "memory", "io"].map(|resource| {
                let text = read(format!("/proc/pressure/{resource}"))?;
                text.strip_prefix("some avg10=")?
                    .split(' ')
                    .next()?
                    .parse()
                    .ok()
            });
        }
        let meminfo = read("/proc/meminfo").unwrap_or_default();
        let kib = |name| Some(field(&meminfo, name)? * 1024);
        let usage = |total: Option<u64>, available: Option<u64>| {
            let (total, available) = (total?, available?);
            Some(Usage {
                used: total.checked_sub(available)?,
                total,
                available,
                inodes: None,
            })
        };
        sample.memory = usage(kib("MemTotal:"), kib("MemAvailable:"));
        sample.swap = usage(kib("SwapTotal:"), kib("SwapFree:"));
        let load = ("Load · 1/5/15 min", dash(load, |load| load));
        let uptime = dash(uptime, |s: u64| {
            format!("{}d {:02}h {:02}m", s / 86400, s / 3600 % 24, s / 60 % 60)
        });
        sample.overview = [load.clone(), ("Uptime", uptime)];
        sample.cpu_details = [
            load,
            ("I/O wait", percent(iowait)),
            ("CPU pressure · 10s", percent(pressure[0])),
            ("Tasks · running/total", dash(tasks, |tasks| tasks)),
        ];
        let [free, cached, buffers] =
            ["MemFree:", "Cached:", "Buffers:"].map(|n| dash(kib(n), gib));
        sample.memory_details = [
            ("Available", dash(sample.memory, |u| gib(u.available))),
            ("Free", free),
            ("Cached", cached),
            ("Buffers", buffers),
            ("Memory pressure · 10s", percent(pressure[1])),
        ];
        sample.io_pressure = pressure[2];
        let swap = expanded
            .then(|| read("/proc/vmstat"))
            .flatten()
            .and_then(|text| Some([field(&text, "pswpin")?, field(&text, "pswpout")?]));
        let swap_rates = [0, 1].map(|i| {
            let (old, new) = self.swap.zip(swap)?;
            Some(mib(rate(new[i], old[i])? * page_size() as f64))
        });
        self.swap = swap;
        let disks = std::mem::take(&mut self.disks);
        if expanded {
            let text = read("/proc/diskstats").unwrap_or_default();
            for (name, new) in text.lines().filter_map(disk_counters) {
                let disk_rate = |i: usize| rate(new[i], disks.get(name)?[i]);
                sample.disks.push(Disk {
                    name: name.to_owned(),
                    rates: [0, 1].map(|i| disk_rate(i).map(mib)),
                    utilization: disk_rate(2).map(|busy_ms| (busy_ms / 10.).min(100.) as f32),
                });
                self.disks.insert(name.to_owned(), new);
            }
        }
        let mut networks = std::mem::take(&mut self.networks);
        sample.networks = read_networks(&read("/proc/net/dev").unwrap_or_default());
        for network in &mut sample.networks {
            if let Some((name, old)) = networks.remove_entry(&network.name) {
                // Share the name's allocation with the history.
                network.name = name;
                network.rates = [0, 1].map(|i| rate(network.bytes[i], old[i]).map(mib));
            }
            self.networks.insert(network.name.clone(), network.bytes);
        }
        sample.processes = if processes {
            self.sample_processes(elapsed, detail)
        } else {
            self.processes.clear();
            Vec::new()
        };
        let disks_rate = |i: usize| {
            let rates = sample.disks.iter().map(|disk| disk.rates[i]);
            rates
                .sum::<Option<f32>>()
                .filter(|_| !sample.disks.is_empty())
        };
        let percent_of = |usage: Option<Usage>| {
            usage
                .filter(|u| u.total > 0)
                .map(|u| u.used as f32 * 100. / u.total as f32)
        };
        sample.values = [
            cpu_percent,
            percent_of(sample.memory),
            disks_rate(0),
            disks_rate(1),
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
                .map(|network| (network.name.clone(), network.rates))
                .collect(),
        };
        (sample, point)
    }

    fn sample_processes(&mut self, elapsed: Option<f64>, detail: Option<Identity>) -> Vec<Process> {
        let previous = std::mem::take(&mut self.processes);
        let mut processes = Vec::with_capacity(previous.len());
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            // Skips the other entries, among them `self`, which would repeat this process.
            if !entry.file_name().as_bytes()[0].is_ascii_digit() {
                continue;
            }
            let Some((mut process, (ticks, io))) = read_process(&entry, detail) else {
                continue;
            };
            let identity = (process.pid, process.start);
            if let Some(&(old_ticks, old_io)) = previous.get(&identity) {
                process.cpu = per_second(ticks, old_ticks, elapsed)
                    .map(|v| (v * 100. / clock_ticks()) as f32);
                if let Some((old, new)) = old_io.zip(io) {
                    process.read = per_second(new[0], old[0], elapsed);
                    process.write = per_second(new[1], old[1], elapsed);
                }
            }
            self.processes.insert(identity, (ticks, io));
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

/// The number after `name` on the line starting with it, as in /proc/meminfo or /proc/vmstat.
fn field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.split_ascii_whitespace().next()?;
        value.parse().ok()
    })
}

fn per_second(new: u64, old: u64, elapsed: Option<f64>) -> Option<f64> {
    Some(new.checked_sub(old)? as f64 / elapsed?)
}

/// The percentage of the CPU time between two [total, idle, iowait] counters spent in `index`.
fn share(new: [u64; 3], old: [u64; 3], index: usize) -> Option<f32> {
    let total = new[0].checked_sub(old[0])?;
    let part = new[index].checked_sub(old[index])?;
    (total > 0).then(|| part as f32 * 100. / total as f32)
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

/// A /proc/stat `cpu` line as its name and [total, idle, iowait] time.
fn cpu_counters(line: &str) -> Option<(&str, [u64; 3])> {
    let mut fields = line.split_ascii_whitespace();
    let name = fields.next().filter(|name| name.starts_with("cpu"))?;
    let mut counters = [0u64; 8];
    for counter in &mut counters {
        *counter = fields.next()?.parse().ok()?;
    }
    let total = counters.iter().sum();
    Some((name, [total, counters[3] + counters[4], counters[4]]))
}

/// A /proc/diskstats line of a physical disk as its name and [read bytes, written bytes, busy ms].
fn disk_counters(line: &str) -> Option<(&str, [u64; 3])> {
    let mut fields = line.split_ascii_whitespace().skip(2);
    let name = fields.next()?;
    if !Path::new(&format!("/sys/block/{name}/device")).exists() {
        return None;
    }
    let mut number = |skip| fields.nth(skip)?.parse::<u64>().ok();
    Some((name, [number(2)? * 512, number(3)? * 512, number(2)?]))
}

/// The physical interfaces of /proc/net/dev, sorted by name; each line holds the receive
/// bytes, packets, errors, drops and four more counters, then the same for transmission.
fn read_networks(text: &str) -> Vec<Network> {
    let mut networks: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let name = name.trim();
            let v: Vec<u64> = fields
                .split_ascii_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let physical = Path::new(&format!("/sys/class/net/{name}/device")).exists();
            (physical && v.len() >= 12).then(|| Network {
                name: name.into(),
                rates: [None; 2],
                bytes: [v[0], v[8]],
                errors: [v[2], v[10]],
                dropped: [v[3], v[11]],
            })
        })
        .collect();
    networks.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    networks
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
    Some(head.strip_prefix("ref: refs/heads/").unwrap_or(head).into())
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

/// /proc/<pid>/stat as the process and its CPU time in clock ticks.
fn process_stat(text: &str) -> Option<(Process, u64)> {
    // The name is in parentheses and may itself contain ") ".
    let (pid, rest) = text.split_once(" (")?;
    let (name, rest) = rest.rsplit_once(") ")?;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    let number = |index: usize| fields.get(index)?.parse::<u64>().ok();
    let process = Process {
        pid: pid.parse().ok()?,
        start: number(19)?,
        name: name.into(),
        state: fields.first()?.chars().next()?,
        rss: number(21)? * page_size(),
        threads: number(17)? as u32,
        parent: number(1)? as u32,
        ..Process::default()
    };
    Some((process, number(11)? + number(12)?))
}

/// Reads one `/proc/<pid>`; a matching `detail` also reads the command line and directory.
fn read_process(
    entry: &fs::DirEntry,
    detail: Option<Identity>,
) -> Option<(Process, ProcessCounters)> {
    let dir = entry.path();
    let (mut process, ticks) = process_stat(&read(dir.join("stat"))?)?;
    process.uid = entry.metadata().ok()?.uid();
    let io = read(dir.join("io"))
        .and_then(|text| Some([field(&text, "read_bytes:")?, field(&text, "write_bytes:")?]));
    if detail == Some((process.pid, process.start)) {
        let command = fs::read(dir.join("cmdline")).unwrap_or_default();
        let command = String::from_utf8_lossy(&command)
            .trim_end_matches('\0')
            .replace('\0', " ");
        process.command = if command.is_empty() {
            format!("[{}]", process.name)
        } else {
            command
        };
        process.directory = fs::read_link(dir.join("cwd")).ok();
    }
    Some((process, (ticks, io)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_text_parses_to_counters_and_rates() {
        let (_, old) = cpu_counters("cpu 10 20 30 100 5 6 7 8 9 10").unwrap();
        assert_eq!(old, [186, 105, 5]);
        let (_, new) = cpu_counters("cpu 60 20 30 140 15 6 7 8").unwrap();
        assert_eq!(
            (share(new, old, 1), share(new, old, 2)),
            (Some(50.), Some(10.))
        );
        assert_eq!(per_second(1100, 100, Some(2.)), Some(500.));
        assert_eq!(per_second(10, 100, Some(1.)), None);
        let meminfo = "MemTotal: 1000 kB\nSwapCached: 7 kB\nCached: 666 kB\n";
        assert_eq!(field(meminfo, "Cached:"), Some(666));
        let text =
            "123 (name ) with spaces) R 1 0 0 0 0 0 0 0 0 0 40 10 0 0 20 0 3 0 900 100000 20 0 0";
        let (process, ticks) = process_stat(text).unwrap();
        assert_eq!(process.name, "name ) with spaces");
        assert_eq!(
            (process.pid, process.parent, process.threads, process.start),
            (123, 1, 3, 900)
        );
        assert_eq!((process.rss, ticks), (20 * page_size(), 50));
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
        assert_eq!(history[history.len() - 2].values, [None; 7]);
    }

    #[test]
    fn trends_keep_the_window_and_follow_one_interface_across_samples() {
        let mut stats = SystemStats::new().unwrap();
        let eth0: Arc<str> = "eth0".into();
        let now = Instant::now();
        let point = |age: u64| {
            let selected = (eth0.clone(), [Some(age as f32), None]);
            let other = ("eth1".into(), [Some(1000.), None]);
            TrendPoint {
                at: now - Duration::from_secs(age),
                values: [Some(age as f32); 7],
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
        let system = stats.system_trends(60, 2..3);
        assert_eq!(values(&system[0]), [Some(30.), Some(20.), Some(10.)]);
        let network = Network {
            name: eth0.clone(),
            rates: [None; 2],
            bytes: [0; 2],
            errors: [0; 2],
            dropped: [0; 2],
        };
        let network = stats.network_trends(60, &network);
        assert_eq!(values(&network[0]), [Some(30.), None, Some(10.)]);
    }
}
