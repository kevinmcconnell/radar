use std::error::Error;
use std::path::Path;

use radar_core::{SensorKind, db};
use rusqlite::Connection;

use crate::discover::{Reading, SensorSource};
use crate::sampler::Sample;
use crate::sys;
use crate::writer::Writer;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;
const CLK_TCK: u64 = 100;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next()
    }

    fn chance(&mut self, p: f64) -> bool {
        self.next() < p
    }
}

fn sensor(kind: SensorKind, chip: &str, label: &str) -> SensorSource {
    SensorSource {
        kind,
        chip: chip.into(),
        label: label.into(),
        reading: Reading::File {
            path: Default::default(),
            scale: 1.0,
        },
    }
}

fn utc_offset_secs(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT unixepoch('now', 'localtime') - unixepoch('now')",
        [],
        |row| row.get(0),
    )
}

/// Daytime activity level between 0 and 1.
fn activity(local_hour: f64) -> f64 {
    match local_hour {
        h if (9.0..18.0).contains(&h) => 1.0,
        h if (18.0..23.5).contains(&h) => 0.6,
        h if (7.5..9.0).contains(&h) => 0.4,
        _ => 0.03,
    }
}

const PROCS: &[(&str, f64)] = &[
    ("firefox", 0.22),
    ("Web Content", 0.18),
    ("chromium", 0.12),
    ("Hyprland", 0.10),
    ("alacritty", 0.05),
    ("nvim", 0.04),
    ("pipewire", 0.03),
    ("Xwayland", 0.02),
    ("kworker/u64:2", 0.03),
    ("systemd-journal", 0.01),
    ("waybar", 0.02),
];

/// A coding agent that works in sessions of a few turns, each turn a burst of tokens that
/// fills its rolling quota windows.
#[derive(Default)]
struct AgentDemo {
    turns_left: i64,
    five_hour: f64,
    seven_day: f64,
}

impl AgentDemo {
    /// Quota used and tokens per minute over `interval` seconds.
    fn step(&mut self, ts: i64, interval: i64, act: f64, rng: &mut Rng) -> (f64, f64) {
        if ts % (5 * 3600) < interval {
            self.five_hour = 0.0;
        }
        if ts % (7 * 86400) < interval {
            self.seven_day = 0.0;
        }
        if self.turns_left == 0 && rng.chance(act / 120.0) {
            self.turns_left = (rng.range(2.0, 12.0) * 60.0) as i64 / interval;
        }
        if self.turns_left == 0 || !rng.chance(0.35) {
            self.turns_left = (self.turns_left - 1).max(0);
            return (0.0, 0.0);
        }
        self.turns_left -= 1;
        let input = rng.range(20_000.0, 120_000.0);
        let output = rng.range(200.0, 3_000.0);
        let share = (input + 5.0 * output) / 4e7;
        self.five_hour = (self.five_hour + share * 100.0).min(100.0);
        self.seven_day = (self.seven_day + share * 100.0 / 6.0).min(100.0);
        let per_min = 60.0 / interval as f64;
        (input * per_min, output * per_min)
    }
}

pub fn seed(path: &Path, root: &Path, days: u64, interval: i64) -> Result<(), Box<dyn Error>> {
    let conn = db::open_rw(path)?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    let offset = utc_offset_secs(&conn)?;
    let mut writer = Writer::new(conn);
    let ncpus = sys::ncpus(root);
    writer.write_meta(CLK_TCK, ncpus, "demo")?;

    let sensors = [
        sensor(SensorKind::Temp, "k10temp:0000:00:18.3", "Tctl"),
        sensor(SensorKind::Temp, "k10temp:0000:00:18.3", "Tccd1"),
        sensor(SensorKind::Temp, "k10temp:0000:00:18.3", "Tccd2"),
        sensor(SensorKind::Temp, "amdgpu:0000:03:00.0", "edge"),
        sensor(SensorKind::Temp, "amdgpu:0000:03:00.0", "junction"),
        sensor(SensorKind::Temp, "nvme:nvme0", "Composite"),
        sensor(SensorKind::Fan, "asusec:asus-ec-sensors", "CPU_Opt"),
        sensor(SensorKind::GpuBusy, "amdgpu", "card1"),
        sensor(SensorKind::Freq, "cpu", "busy cores"),
        sensor(SensorKind::Freq, "cpu", "fastest core"),
        sensor(SensorKind::Quota, "claude", "5h"),
        sensor(SensorKind::Quota, "claude", "7d"),
        sensor(SensorKind::Tokens, "claude", "input"),
        sensor(SensorKind::Tokens, "claude", "output"),
        sensor(SensorKind::Quota, "codex", "5h"),
        sensor(SensorKind::Quota, "codex", "7d"),
        sensor(SensorKind::Tokens, "codex", "input"),
        sensor(SensorKind::Tokens, "codex", "output"),
    ];
    writer.register_sensors(&sensors)?;

    let end = sys::wall_secs() / interval * interval;
    let start = end - days as i64 * 86400;
    let gap_start = end - (days as i64 * 86400) / 2;
    let gap_end = gap_start + (6 * 3600).min(days as i64 * 86400 / 10);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    let mut burst = 0i64;
    let mut download = 0i64;
    let mut download_rate = 0.0;
    let mut mem = 10.0 * GIB;
    let mut tctl = 40.0;
    let mut nvme = 38.0;
    let mut load = 0.5;
    let mut agents = [AgentDemo::default(), AgentDemo::default()];
    let mut after_gap = true;
    let mut sample = Sample {
        mem_total: (64.0 * GIB) as i64,
        ..Default::default()
    };
    let mut procs: Vec<(&[u8], u64)> = Vec::with_capacity(PROCS.len() + 2);

    let mut ts = start;
    while ts <= end {
        if (gap_start..gap_end).contains(&ts) {
            ts = gap_end;
            after_gap = true;
            continue;
        }
        let hour = (ts + offset).rem_euclid(86400) as f64 / 3600.0;
        let act = activity(hour);

        if burst == 0 && rng.chance(act / 900.0) {
            burst = (rng.range(5.0, 30.0) * 60.0) as i64 / interval;
        }
        if download == 0 && rng.chance(act / 2500.0) {
            download = (rng.range(1.0, 10.0) * 60.0) as i64 / interval;
            download_rate = rng.range(20.0, 90.0) * MIB;
        }
        let compiling = burst > 0;
        let downloading = download > 0;
        burst = (burst - 1).max(0);
        download = (download - 1).max(0);

        let cpu = (2.0
            + 14.0 * act
            + rng.range(-1.5, 3.0)
            + if compiling {
                rng.range(55.0, 85.0)
            } else {
                0.0
            })
        .clamp(0.3, 100.0);
        let gpu = (act * 8.0 + rng.range(0.0, 6.0)).min(100.0);
        load += (cpu / 100.0 * ncpus as f64 * 0.9 - load) * 0.05;
        let mem_target = (8.0 + 12.0 * act + if compiling { 8.0 } else { 0.0 }) * GIB;
        mem += (mem_target - mem) * 0.01 + rng.range(-20.0, 20.0) * MIB;
        tctl += (38.0 + cpu * 0.5 - tctl) * 0.25 + rng.range(-0.4, 0.4);
        let disk_busy = compiling || downloading;
        nvme += (37.0 + if disk_busy { 12.0 } else { 0.0 } - nvme) * 0.05;

        let secs = interval as f64;
        let rx =
            (2048.0 * act + rng.range(0.0, 1500.0) + if downloading { download_rate } else { 0.0 })
                * secs;
        let tx = (800.0 * act + rng.range(0.0, 600.0)) * secs + rx * 0.02;
        let read = (rng.range(0.0, 200.0) * 1024.0 * act
            + if compiling {
                rng.range(5.0, 60.0) * MIB
            } else {
                0.0
            })
            * secs;
        let write = (rng.range(50.0, 400.0) * 1024.0
            + if compiling {
                rng.range(20.0, 150.0) * MIB
            } else {
                0.0
            }
            + if downloading { download_rate } else { 0.0 })
            * secs;

        sample.ts = ts;
        sample.dt_ms = interval * 1000 + rng.range(-3.0, 3.0) as i64;
        sample.load1 = load;
        sample.mem_used = mem as i64;
        sample.swap_used = if ts > gap_end {
            (310.0 * MIB) as i64
        } else {
            0
        };
        if after_gap {
            sample.cpu_busy = None;
            sample.cpu_iowait = None;
            sample.net_rx = None;
            sample.net_tx = None;
            sample.disk_read = None;
            sample.disk_write = None;
        } else {
            sample.cpu_busy = Some(cpu);
            sample.cpu_iowait = Some(if disk_busy {
                rng.range(0.5, 3.0)
            } else {
                rng.range(0.0, 0.3)
            });
            sample.net_rx = Some(rx as i64);
            sample.net_tx = Some(tx as i64);
            sample.disk_read = Some(read as i64);
            sample.disk_write = Some(write as i64);
        }
        let edge = 38.0 + gpu * 0.4 + rng.range(-0.3, 0.3);
        let throttle = (tctl - 70.0).max(0.0) * 25.0;
        sample.sensors.clear();
        sample.sensors.extend([
            (0, tctl),
            (1, tctl - 3.0 + rng.range(-0.5, 0.5)),
            (2, tctl - 4.5 + rng.range(-0.5, 0.5)),
            (3, edge),
            (4, edge + 6.0 + rng.range(-0.5, 0.5)),
            (5, nvme + rng.range(-0.3, 0.3)),
            (6, 550.0 + cpu * 9.0 + rng.range(-20.0, 20.0)),
            (7, gpu),
            (9, 5700.0 - throttle * 0.5 + rng.range(-40.0, 40.0)),
        ]);
        if !after_gap {
            sample
                .sensors
                .push((8, 5300.0 - cpu * 5.0 - throttle + rng.range(-80.0, 80.0)));
        }
        for (n, agent) in agents.iter_mut().enumerate() {
            let (input, output) = agent.step(ts, interval, act, &mut rng);
            let base = 10 + n * 4;
            sample
                .sensors
                .extend([(base, agent.five_hour), (base + 1, agent.seven_day)]);
            if !after_gap {
                sample
                    .sensors
                    .extend([(base + 2, input), (base + 3, output)]);
            }
        }

        procs.clear();
        if !after_gap {
            let total = cpu / 100.0 * ncpus as f64 * CLK_TCK as f64 * secs;
            let (desktop, build) = if compiling {
                (total * 0.15, total * 0.85)
            } else {
                (total, 0.0)
            };
            for &(name, weight) in PROCS {
                let ticks = (desktop * weight * rng.range(0.6, 1.4)) as u64;
                if ticks > 0 {
                    procs.push((name.as_bytes(), ticks));
                }
            }
            if compiling {
                procs.push((b"rustc", (build * 0.95) as u64));
                procs.push((b"cargo", (build * 0.05) as u64));
            }
            if rng.chance(0.02) {
                procs.push((b"radar-collect", 1));
            }
        }
        writer.write(&sample, procs.iter().copied())?;

        after_gap = false;
        ts += interval;
    }
    println!("seeded {days} days into {}", path.display());
    Ok(())
}
