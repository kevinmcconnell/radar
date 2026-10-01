use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use rustix::io::Errno;
use rustix::thread::clock_nanosleep_absolute;
use rustix::time::{ClockId, Timespec, clock_gettime};
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::parse;

static STOP: LazyLock<Arc<AtomicBool>> = LazyLock::new(Arc::default);

pub fn install_signal_handlers() -> std::io::Result<()> {
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, STOP.clone())?;
    }
    Ok(())
}

pub fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}

pub fn wall_secs() -> i64 {
    clock_gettime(ClockId::Realtime).tv_sec
}

/// Monotonic milliseconds that keep counting during suspend.
pub fn boottime_ms() -> i64 {
    let ts = clock_gettime(ClockId::Boottime);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

/// Returns false if interrupted by a signal.
pub fn sleep_until_wall(secs: i64) -> bool {
    let target = Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    loop {
        match clock_nanosleep_absolute(ClockId::Realtime, &target) {
            Err(Errno::INTR) if stopping() => return false,
            Err(Errno::INTR) => continue,
            _ => return true,
        }
    }
}

pub fn clk_tck() -> u64 {
    rustix::param::clock_ticks_per_second()
}

pub fn ncpus(root: &Path) -> u64 {
    std::fs::read(root.join("proc/stat"))
        .map_or(0, |buf| parse::count_cpus(&buf))
        .max(1)
}
