#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
    pub iowait: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidStat<'a> {
    pub comm: &'a [u8],
    pub ticks: u64,
    pub starttime: u64,
}

pub fn fields(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    buf.split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty())
}

pub fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(n)
}

pub fn parse_i64(s: &[u8]) -> Option<i64> {
    match s.split_first() {
        Some((b'-', rest)) => parse_u64(rest).map(|n| -(n as i64)),
        _ => parse_u64(s).map(|n| n as i64),
    }
}

pub fn trim(s: &[u8]) -> &[u8] {
    s.trim_ascii()
}

pub fn parse_cpu_stat(buf: &[u8]) -> Option<CpuTimes> {
    let line = buf.split(|&b| b == b'\n').next()?;
    let mut it = fields(line);
    if it.next()? != b"cpu" {
        return None;
    }
    // user nice system idle iowait irq softirq steal; guest time is already in user/nice
    let mut vals = [0u64; 8];
    let mut n = 0;
    for (slot, f) in vals.iter_mut().zip(it) {
        *slot = parse_u64(f)?;
        n += 1;
    }
    if n < 5 {
        return None;
    }
    Some(CpuTimes {
        total: vals.iter().sum(),
        idle: vals[3],
        iowait: vals[4],
    })
}

pub fn count_cpus(buf: &[u8]) -> u64 {
    buf.split(|&b| b == b'\n')
        .filter(|line| line.starts_with(b"cpu") && line.get(3).is_some_and(u8::is_ascii_digit))
        .count() as u64
}

pub fn parse_loadavg(buf: &[u8]) -> Option<f64> {
    let f = fields(buf).next()?;
    std::str::from_utf8(f).ok()?.parse().ok()
}

pub fn parse_meminfo(buf: &[u8]) -> Option<MemInfo> {
    let mut m = MemInfo::default();
    let mut found = 0;
    for line in buf.split(|&b| b == b'\n') {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let slot = match &line[..colon] {
            b"MemTotal" => &mut m.total,
            b"MemAvailable" => &mut m.available,
            b"SwapTotal" => &mut m.swap_total,
            b"SwapFree" => &mut m.swap_free,
            _ => continue,
        };
        *slot = parse_u64(fields(&line[colon + 1..]).next()?)? * 1024;
        found += 1;
        if found == 4 {
            return Some(m);
        }
    }
    None
}

pub fn parse_net_dev(buf: &[u8], mut f: impl FnMut(&[u8], u64, u64)) {
    for line in buf.split(|&b| b == b'\n').skip(2) {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let iface = trim(&line[..colon]);
        let mut it = fields(&line[colon + 1..]);
        let rx = it.next().and_then(parse_u64);
        let tx = it.nth(7).and_then(parse_u64);
        if let (Some(rx), Some(tx)) = (rx, tx) {
            f(iface, rx, tx);
        }
    }
}

pub fn parse_diskstats(buf: &[u8], mut f: impl FnMut(&[u8], u64, u64)) {
    for line in buf.split(|&b| b == b'\n') {
        let mut it = fields(line).skip(2);
        let Some(name) = it.next() else { continue };
        let read = it.nth(2).and_then(parse_u64);
        let written = it.nth(3).and_then(parse_u64);
        if let (Some(r), Some(w)) = (read, written) {
            f(name, r * 512, w * 512);
        }
    }
}

pub fn parse_pid_stat(buf: &[u8]) -> Option<PidStat<'_>> {
    let open = buf.iter().position(|&b| b == b'(')?;
    let close = buf.iter().rposition(|&b| b == b')')?;
    if close < open {
        return None;
    }
    let comm = &buf[open + 1..close];
    // fields after comm start at field 3 (state); utime=14, stime=15, starttime=22
    let mut it = fields(&buf[close + 1..]);
    let utime = parse_u64(it.nth(11)?)?;
    let stime = parse_u64(it.next()?)?;
    let starttime = parse_u64(it.nth(6)?)?;
    Some(PidStat {
        comm,
        ticks: utime + stime,
        starttime,
    })
}

pub fn parse_uptime_secs(buf: &[u8]) -> Option<f64> {
    std::str::from_utf8(fields(buf).next()?).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES}/{name}")).unwrap()
    }

    #[test]
    fn cpu_stat() {
        let t = parse_cpu_stat(&fixture("amd/proc/stat")).unwrap();
        assert_eq!(
            t,
            CpuTimes {
                total: 19936 + 972 + 4921 + 4648247 + 74994 + 1154 + 743,
                idle: 4648247,
                iowait: 74994
            }
        );
        assert_eq!(parse_cpu_stat(b"intr 1 2 3\n"), None);
    }

    #[test]
    fn cpu_count() {
        assert_eq!(count_cpus(&fixture("amd/proc/stat")), 1);
        assert_eq!(
            count_cpus(b"cpu  1 2\ncpu0 1 1\ncpu1 0 1\ncpu10 0 0\nintr 1\n"),
            3
        );
        assert_eq!(count_cpus(b""), 0);
    }

    #[test]
    fn loadavg() {
        assert_eq!(parse_loadavg(&fixture("amd/proc/loadavg")), Some(0.20));
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo(&fixture("amd/proc/meminfo")).unwrap();
        assert_eq!(m.total, 65456840 * 1024);
        assert_eq!(m.available, 58148148 * 1024);
        assert_eq!(m.swap_total, 130913944 * 1024);
        assert_eq!(m.swap_free, 130913000 * 1024);
    }

    #[test]
    fn net_dev() {
        let mut out = Vec::new();
        parse_net_dev(&fixture("amd/proc/net/dev"), |i, rx, tx| {
            out.push((i.to_vec(), rx, tx))
        });
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], (b"lo".to_vec(), 34862, 34862));
        assert_eq!(out[2], (b"enp13s0".to_vec(), 987654321, 12345678));
    }

    #[test]
    fn diskstats() {
        let mut out = Vec::new();
        parse_diskstats(&fixture("amd/proc/diskstats"), |n, r, w| {
            out.push((n.to_vec(), r, w))
        });
        assert_eq!(out[2], (b"nvme0n1".to_vec(), 350851 * 512, 306848 * 512));
    }

    #[test]
    fn pid_stat_plain() {
        let buf = fixture("amd/proc/11992/stat");
        let s = parse_pid_stat(&buf).unwrap();
        assert_eq!(s.comm, b"cat");
        assert_eq!(s.ticks, 7 + 3);
        assert_eq!(s.starttime, 148302);
    }

    #[test]
    fn pid_stat_comm_with_spaces_and_parens() {
        let buf = fixture("amd/proc/4242/stat");
        let s = parse_pid_stat(&buf).unwrap();
        assert_eq!(s.comm, b"Web Content) (x");
        assert_eq!(s.ticks, 1500 + 250);
        assert_eq!(s.starttime, 99000);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_i64(b"-5000"), Some(-5000));
        assert_eq!(parse_u64(b"12a"), None);
        assert_eq!(parse_u64(b""), None);
    }
}
