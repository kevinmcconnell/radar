mod cli;
mod demo;
mod discover;
mod parse;
mod sampler;
mod sys;
mod writer;

use std::error::Error;
use std::path::Path;
use std::time::Duration;

use radar_core::db;

use crate::cli::{Args, Mode};
use crate::discover::Reading;
use crate::sampler::Sampler;
use crate::writer::Writer;

const REDISCOVER_SECS: i64 = 300;
const TRIM_SECS: i64 = 3600;

fn main() {
    let args = match cli::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("radar-collect: {e}");
            std::process::exit(2);
        }
    };
    let result = match args.mode {
        Mode::Run => run(&args),
        Mode::Once => once(&args),
        Mode::ListSensors => {
            list_sensors(&args.root);
            Ok(())
        }
        Mode::SeedDemo(days) => demo::seed(&args.db, &args.root, days, args.interval as i64),
    };
    if let Err(e) = result {
        eprintln!("radar-collect: {e}");
        std::process::exit(1);
    }
}

fn hostname(root: &Path) -> String {
    std::fs::read_to_string(root.join("proc/sys/kernel/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn run(args: &Args) -> Result<(), Box<dyn Error>> {
    sys::install_signal_handlers()?;
    let clk_tck = sys::clk_tck();
    let mut writer = Writer::new(db::open_rw(&args.db)?);
    writer.write_meta(clk_tck, sys::ncpus(&args.root), &hostname(&args.root))?;
    let mut sampler = Sampler::new(&args.root, clk_tck);
    writer.register_sensors(&sampler.discovery.sensors)?;

    let interval = args.interval as i64;
    let retention = args.retention_days as i64 * 86400;
    let mut last: Option<(i64, i64)> = None;
    let mut next_discover = sys::wall_secs() + REDISCOVER_SECS;
    let mut next_trim = 0;

    while !sys::stopping() {
        let ts = (sys::wall_secs() / interval + 1) * interval;
        if !sys::sleep_until_wall(ts) {
            break;
        }
        let boot = sys::boottime_ms();
        let (dt_ms, gap) = match last {
            Some((prev_ts, prev_boot)) => {
                let dt = boot - prev_boot;
                (
                    dt,
                    dt > 3 * interval * 1000 || ts <= prev_ts || ts - prev_ts > 3 * interval,
                )
            }
            None => (0, true),
        };
        if gap {
            sampler.reset_baselines();
        }
        if ts >= next_discover || (gap && last.is_some()) {
            sampler.rediscover();
            if let Err(e) = writer.register_sensors(&sampler.discovery.sensors) {
                eprintln!("radar-collect: registering sensors: {e}");
            }
            next_discover = ts + REDISCOVER_SECS;
        }
        let result = match sampler.take(ts, dt_ms) {
            Ok(()) => writer
                .write(&sampler.sample, sampler.procs.nonzero())
                .map_err(|e| format!("writing sample: {e}")),
            Err(e) => Err(format!("sampling: {e}")),
        };
        // a lost sample must not be folded into the next interval's deltas
        last = match result {
            Ok(()) => Some((ts, boot)),
            Err(e) => {
                eprintln!("radar-collect: {e}");
                None
            }
        };

        if ts >= next_trim {
            if let Err(e) = writer.trim(ts - retention) {
                eprintln!("radar-collect: trimming: {e}");
            }
            next_trim = ts + TRIM_SECS;
        }
    }
    Ok(())
}

fn once(args: &Args) -> Result<(), Box<dyn Error>> {
    let clk_tck = sys::clk_tck();
    let mut sampler = Sampler::new(&args.root, clk_tck);
    let start = sys::boottime_ms();
    sampler.take(sys::wall_secs(), 0)?;
    print_sample(&sampler, clk_tck);
    std::thread::sleep(Duration::from_secs(args.interval));
    sampler.take(sys::wall_secs(), sys::boottime_ms() - start)?;
    println!();
    print_sample(&sampler, clk_tck);
    Ok(())
}

fn print_sample(sampler: &Sampler, clk_tck: u64) {
    let s = &sampler.sample;
    let secs = s.dt_ms as f64 / 1000.0;
    let pct = |v: Option<f64>| v.map_or("-".into(), |v| format!("{v:.1}%"));
    let rate = |v: Option<i64>| match v {
        Some(v) if secs > 0.0 => format!("{}/s", human_bytes(v as f64 / secs)),
        _ => "-".into(),
    };
    println!("ts {}  dt {} ms", s.ts, s.dt_ms);
    println!(
        "cpu busy {}  iowait {}  load {:.2}",
        pct(s.cpu_busy),
        pct(s.cpu_iowait),
        s.load1
    );
    println!(
        "mem {} / {}  swap {}",
        human_bytes(s.mem_used as f64),
        human_bytes(s.mem_total as f64),
        human_bytes(s.swap_used as f64)
    );
    println!("net rx {}  tx {}", rate(s.net_rx), rate(s.net_tx));
    println!(
        "disk read {}  write {}",
        rate(s.disk_read),
        rate(s.disk_write)
    );
    for &(i, v) in &s.sensors {
        let src = &sampler.discovery.sensors[i];
        println!(
            "  {:<10} {:<22} {:<12} {v:.1} {}",
            src.kind.as_str(),
            src.chip,
            src.label,
            src.kind.unit()
        );
    }
    let mut procs: Vec<_> = sampler.procs.nonzero().collect();
    procs.sort_by_key(|p| std::cmp::Reverse(p.1));
    for (name, ticks) in procs.iter().take(10) {
        println!(
            "  {:<20} {:.2} cpu-s",
            String::from_utf8_lossy(name),
            *ticks as f64 / clk_tck as f64
        );
    }
}

fn human_bytes(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = v;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn list_sensors(root: &Path) {
    let d = discover::discover(root);
    println!("sensors:");
    for s in &d.sensors {
        let source = match &s.reading {
            Reading::File { path, .. } => path.display().to_string(),
            Reading::BusyFreq | Reading::FastestFreq => {
                format!("{} cpus, cpufreq/scaling_cur_freq", d.core_freqs.len())
            }
        };
        println!(
            "  {:<10} {:<22} {:<12} {}",
            s.kind.as_str(),
            s.chip,
            s.label,
            source
        );
    }
    println!("network interfaces: {}", d.net_ifaces.join(" "));
    println!("disks: {}", d.disks.join(" "));
}
