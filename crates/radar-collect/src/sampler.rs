use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::discover::{Discovery, discover};
use crate::parse::{self, CpuTimes, PidStat};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub dt_ms: i64,
    pub cpu_busy: Option<f64>,
    pub cpu_iowait: Option<f64>,
    pub load1: f64,
    pub mem_used: i64,
    pub mem_total: i64,
    pub swap_used: i64,
    pub net_rx: Option<i64>,
    pub net_tx: Option<i64>,
    pub disk_read: Option<i64>,
    pub disk_write: Option<i64>,
    /// (index into `Discovery::sensors`, value)
    pub sensors: Vec<(usize, f64)>,
}

pub fn cpu_percentages(prev: CpuTimes, cur: CpuTimes) -> Option<(f64, f64)> {
    let total = cur.total.checked_sub(prev.total)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    let iowait = cur.iowait.checked_sub(prev.iowait)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle + iowait);
    Some((
        busy as f64 * 100.0 / total as f64,
        iowait as f64 * 100.0 / total as f64,
    ))
}

/// Byte counters for a set of named devices, summed into one delta per sample.
#[derive(Debug, Default)]
pub struct PairCounters {
    names: Vec<String>,
    prev: Vec<Option<(u64, u64)>>,
    cur: Vec<Option<(u64, u64)>>,
}

impl PairCounters {
    pub fn set_names(&mut self, names: &[String]) {
        let prev = names
            .iter()
            .map(|n| {
                self.names
                    .iter()
                    .position(|o| o == n)
                    .and_then(|i| self.prev[i])
            })
            .collect();
        self.names = names.to_vec();
        self.prev = prev;
        self.cur = vec![None; names.len()];
    }

    pub fn reset(&mut self) {
        self.prev.fill(None);
    }

    pub fn record(&mut self, name: &[u8], a: u64, b: u64) {
        if let Some(i) = self.names.iter().position(|n| n.as_bytes() == name) {
            self.cur[i] = Some((a, b));
        }
    }

    /// None when no device has a baseline, or when any counter went backwards.
    pub fn finish(&mut self) -> Option<(u64, u64)> {
        let mut total = (0u64, 0u64);
        let mut paired = false;
        let mut reset = false;
        for (p, c) in self.prev.iter().zip(&self.cur) {
            if let (Some(p), Some(c)) = (p, c) {
                match (c.0.checked_sub(p.0), c.1.checked_sub(p.1)) {
                    (Some(da), Some(db)) => {
                        total.0 += da;
                        total.1 += db;
                        paired = true;
                    }
                    _ => reset = true,
                }
            }
        }
        std::mem::swap(&mut self.prev, &mut self.cur);
        self.cur.fill(None);
        (paired && !reset).then_some(total)
    }
}

/// Kernel worker names carry a per-thread id and workqueue suffix; group them as one.
pub fn process_name(comm: &[u8]) -> &[u8] {
    if comm.starts_with(b"kworker/") {
        b"kworker"
    } else {
        comm
    }
}

#[derive(Debug, Default)]
pub struct ProcTracker {
    seen: HashMap<(u32, u64), (u64, u32)>,
    generation: u32,
    prev_uptime_ticks: Option<u64>,
    /// CPU ticks per process name for the current sample; zeroed entries are kept for reuse.
    pub totals: HashMap<Box<[u8]>, u64>,
}

impl ProcTracker {
    pub fn begin(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.totals.values_mut().for_each(|v| *v = 0);
    }

    pub fn observe(&mut self, pid: u32, stat: &PidStat) {
        let generation = self.generation;
        let delta = match self.seen.get_mut(&(pid, stat.starttime)) {
            Some(entry) => {
                let d = stat.ticks.saturating_sub(entry.0);
                *entry = (stat.ticks, generation);
                d
            }
            None => {
                self.seen
                    .insert((pid, stat.starttime), (stat.ticks, generation));
                match self.prev_uptime_ticks {
                    Some(prev) if stat.starttime >= prev => stat.ticks,
                    _ => 0,
                }
            }
        };
        if delta == 0 {
            return;
        }
        let name = process_name(stat.comm);
        match self.totals.get_mut(name) {
            Some(v) => *v += delta,
            None => {
                self.totals.insert(name.into(), delta);
            }
        }
    }

    pub fn finish(&mut self, uptime_ticks_at_begin: u64) {
        let generation = self.generation;
        self.seen.retain(|_, v| v.1 == generation);
        self.prev_uptime_ticks = Some(uptime_ticks_at_begin);
    }

    pub fn reset(&mut self) {
        self.seen.clear();
        self.prev_uptime_ticks = None;
    }

    pub fn nonzero(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.totals
            .iter()
            .filter(|e| *e.1 > 0)
            .map(|(k, v)| (&**k, *v))
    }
}

pub struct Sampler {
    root: PathBuf,
    proc_dir: PathBuf,
    stat_path: PathBuf,
    loadavg_path: PathBuf,
    meminfo_path: PathBuf,
    net_dev_path: PathBuf,
    diskstats_path: PathBuf,
    uptime_path: PathBuf,
    buf: Vec<u8>,
    pid_path: String,
    clk_tck: u64,
    cpu_prev: Option<CpuTimes>,
    net: PairCounters,
    disk: PairCounters,
    pub discovery: Discovery,
    pub procs: ProcTracker,
    pub sample: Sample,
}

pub fn read_into(path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    File::open(path)?.read_to_end(buf)?;
    Ok(())
}

impl Sampler {
    pub fn new(root: &Path, clk_tck: u64) -> Self {
        let proc_dir = root.join("proc");
        let mut s = Sampler {
            root: root.to_path_buf(),
            stat_path: proc_dir.join("stat"),
            loadavg_path: proc_dir.join("loadavg"),
            meminfo_path: proc_dir.join("meminfo"),
            net_dev_path: proc_dir.join("net/dev"),
            diskstats_path: proc_dir.join("diskstats"),
            uptime_path: proc_dir.join("uptime"),
            proc_dir,
            buf: Vec::with_capacity(16 * 1024),
            pid_path: String::with_capacity(64),
            clk_tck,
            cpu_prev: None,
            net: PairCounters::default(),
            disk: PairCounters::default(),
            discovery: Discovery::default(),
            procs: ProcTracker::default(),
            sample: Sample::default(),
        };
        s.rediscover();
        s
    }

    pub fn rediscover(&mut self) {
        self.discovery = discover(&self.root);
        self.net.set_names(&self.discovery.net_ifaces);
        self.disk.set_names(&self.discovery.disks);
        self.procs.totals.clear();
    }

    pub fn reset_baselines(&mut self) {
        self.cpu_prev = None;
        self.net.reset();
        self.disk.reset();
        self.procs.reset();
    }

    pub fn take(&mut self, ts: i64, dt_ms: i64) -> io::Result<()> {
        let s = &mut self.sample;
        s.ts = ts;
        s.dt_ms = dt_ms;

        read_into(&self.stat_path, &mut self.buf)?;
        let cpu = parse::parse_cpu_stat(&self.buf);
        let pcts = self
            .cpu_prev
            .zip(cpu)
            .and_then(|(p, c)| cpu_percentages(p, c));
        s.cpu_busy = pcts.map(|p| p.0);
        s.cpu_iowait = pcts.map(|p| p.1);
        self.cpu_prev = cpu;

        read_into(&self.loadavg_path, &mut self.buf)?;
        s.load1 = parse::parse_loadavg(&self.buf).unwrap_or(0.0);

        read_into(&self.meminfo_path, &mut self.buf)?;
        let mem = parse::parse_meminfo(&self.buf).unwrap_or_default();
        s.mem_total = mem.total as i64;
        s.mem_used = mem.total.saturating_sub(mem.available) as i64;
        s.swap_used = mem.swap_total.saturating_sub(mem.swap_free) as i64;

        read_into(&self.net_dev_path, &mut self.buf)?;
        parse::parse_net_dev(&self.buf, |name, rx, tx| self.net.record(name, rx, tx));
        let net = self.net.finish();
        s.net_rx = net.map(|n| n.0 as i64);
        s.net_tx = net.map(|n| n.1 as i64);

        read_into(&self.diskstats_path, &mut self.buf)?;
        parse::parse_diskstats(&self.buf, |name, r, w| self.disk.record(name, r, w));
        let disk = self.disk.finish();
        s.disk_read = disk.map(|d| d.0 as i64);
        s.disk_write = disk.map(|d| d.1 as i64);

        s.sensors.clear();
        for (i, src) in self.discovery.sensors.iter().enumerate() {
            if read_into(&src.path, &mut self.buf).is_ok()
                && let Some(v) = parse::parse_i64(parse::trim(&self.buf))
            {
                s.sensors.push((i, v as f64 * src.scale));
            }
        }

        self.sample_procs()
    }

    fn sample_procs(&mut self) -> io::Result<()> {
        read_into(&self.uptime_path, &mut self.buf)?;
        let uptime = parse::parse_uptime_secs(&self.buf).unwrap_or(0.0);
        let uptime_ticks = (uptime * self.clk_tck as f64) as u64;

        self.procs.begin();
        let root_len = {
            self.pid_path.clear();
            self.pid_path.push_str(&self.proc_dir.to_string_lossy());
            self.pid_path.len()
        };
        for entry in fs::read_dir(&self.proc_dir)?.flatten() {
            let name = entry.file_name();
            let Some(pid) = parse::parse_u64(name.as_encoded_bytes()) else {
                continue;
            };
            self.pid_path.truncate(root_len);
            let _ = write!(self.pid_path, "/{pid}/stat");
            if read_into(Path::new(&self.pid_path), &mut self.buf).is_err() {
                continue;
            }
            if let Some(stat) = parse::parse_pid_stat(&self.buf) {
                self.procs.observe(pid as u32, &stat);
            }
        }
        self.procs.finish(uptime_ticks);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(comm: &[u8], ticks: u64, starttime: u64) -> PidStat<'_> {
        PidStat {
            comm,
            ticks,
            starttime,
        }
    }

    fn totals(t: &ProcTracker) -> Vec<(String, u64)> {
        let mut v: Vec<_> = t
            .nonzero()
            .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn cpu_percent() {
        let p = CpuTimes {
            total: 1000,
            idle: 800,
            iowait: 50,
        };
        let c = CpuTimes {
            total: 2000,
            idle: 1600,
            iowait: 100,
        };
        assert_eq!(cpu_percentages(p, c), Some((15.0, 5.0)));
        assert_eq!(cpu_percentages(p, p), None);
        assert_eq!(cpu_percentages(c, p), None);
    }

    #[test]
    fn counters_sum_and_detect_reset() {
        let mut c = PairCounters::default();
        c.set_names(&["eth0".into(), "wlan0".into()]);
        c.record(b"eth0", 100, 10);
        c.record(b"lo", 999, 999);
        assert_eq!(c.finish(), None);

        c.record(b"eth0", 150, 20);
        c.record(b"wlan0", 5, 5);
        assert_eq!(c.finish(), Some((50, 10)));

        c.record(b"eth0", 10, 25);
        c.record(b"wlan0", 6, 6);
        assert_eq!(c.finish(), None, "counter reset");

        c.record(b"eth0", 20, 30);
        c.record(b"wlan0", 8, 8);
        assert_eq!(c.finish(), Some((12, 7)));
    }

    #[test]
    fn counters_keep_baselines_across_rediscovery() {
        let mut c = PairCounters::default();
        c.set_names(&["eth0".into()]);
        c.record(b"eth0", 100, 100);
        c.finish();
        c.set_names(&["usb0".into(), "eth0".into()]);
        c.record(b"eth0", 200, 300);
        c.record(b"usb0", 1, 1);
        assert_eq!(c.finish(), Some((100, 200)));
    }

    #[test]
    fn counters_reset_after_gap() {
        let mut c = PairCounters::default();
        c.set_names(&["eth0".into()]);
        c.record(b"eth0", 100, 100);
        c.finish();
        c.reset();
        c.record(b"eth0", 200, 200);
        assert_eq!(c.finish(), None);
    }

    #[test]
    fn procs_baseline_then_deltas() {
        let mut t = ProcTracker::default();
        t.begin();
        t.observe(1, &stat(b"firefox", 1000, 50));
        t.observe(2, &stat(b"firefox", 500, 60));
        t.finish(10_000);
        assert!(totals(&t).is_empty(), "first sample only records baselines");

        t.begin();
        t.observe(1, &stat(b"firefox", 1100, 50));
        t.observe(2, &stat(b"firefox", 530, 60));
        t.finish(10_500);
        assert_eq!(totals(&t), vec![("firefox".into(), 130)]);
    }

    #[test]
    fn procs_started_mid_interval_count_fully() {
        let mut t = ProcTracker::default();
        t.begin();
        t.finish(10_000);
        t.begin();
        t.observe(7, &stat(b"rustc", 300, 10_200));
        t.observe(8, &stat(b"old", 900, 9_000));
        t.finish(10_500);
        assert_eq!(totals(&t), vec![("rustc".into(), 300)]);
    }

    #[test]
    fn procs_pid_reuse_is_a_new_process() {
        let mut t = ProcTracker::default();
        t.begin();
        t.observe(42, &stat(b"make", 5000, 100));
        t.finish(10_000);

        t.begin();
        t.observe(42, &stat(b"sh", 20, 10_100));
        t.finish(10_500);
        assert_eq!(totals(&t), vec![("sh".into(), 20)]);

        t.begin();
        t.observe(42, &stat(b"sh", 25, 10_100));
        t.finish(11_000);
        assert_eq!(totals(&t), vec![("sh".into(), 5)]);
        assert_eq!(t.seen.len(), 1, "exited process dropped");
    }

    #[test]
    fn kworkers_are_grouped() {
        let mut t = ProcTracker::default();
        t.begin();
        t.finish(0);
        t.begin();
        t.observe(1, &stat(b"kworker/u129:8-blkcg_punt_bio", 3, 10));
        t.observe(2, &stat(b"kworker/3:1H-kblockd", 4, 10));
        t.finish(100);
        assert_eq!(totals(&t), vec![("kworker".into(), 7)]);
    }

    #[test]
    fn procs_reset_after_gap_only_baselines() {
        let mut t = ProcTracker::default();
        t.begin();
        t.observe(1, &stat(b"a", 100, 50));
        t.finish(10_000);
        t.reset();
        t.begin();
        t.observe(1, &stat(b"a", 900, 50));
        t.observe(2, &stat(b"b", 900, 20_000));
        t.finish(30_000);
        assert!(totals(&t).is_empty());
    }

    #[test]
    fn sampler_reads_fixture_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amd");
        let mut s = Sampler::new(&root, 100);
        s.take(1000, 0).unwrap();
        let first = s.sample.clone();
        assert_eq!(first.cpu_busy, None);
        assert_eq!(first.net_rx, None);
        assert_eq!(first.load1, 0.20);
        assert_eq!(first.mem_total, 65456840 * 1024);
        assert_eq!(first.swap_used, 944 * 1024);
        let tctl = s
            .discovery
            .sensors
            .iter()
            .position(|x| x.label == "Tctl")
            .unwrap();
        assert!(first.sensors.contains(&(tctl, 52.125)));

        s.take(1005, 5000).unwrap();
        let second = &s.sample;
        assert_eq!(
            second.cpu_busy, None,
            "identical counters give no cpu delta"
        );
        assert_eq!(second.net_rx, Some(0));
        assert_eq!(second.disk_write, Some(0));
    }
}
