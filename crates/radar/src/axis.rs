#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Percent,
    Celsius,
    Bytes,
    BytesPerSec,
    Rpm,
    Watts,
    Frequency,
}

impl Format {
    pub fn binary(self) -> bool {
        matches!(self, Format::Bytes | Format::BytesPerSec)
    }

    pub fn format(self, v: f64) -> String {
        match self {
            Format::Percent => format!("{}%", trim_float(v, if v < 10.0 { 1 } else { 0 })),
            Format::Celsius => format!("{}°C", trim_float(v, 0)),
            Format::Bytes => human_bytes(v),
            Format::BytesPerSec => format!("{}/s", human_bytes(v)),
            Format::Rpm => format!("{} rpm", trim_float(v, 0)),
            Format::Watts => format!("{} W", trim_float(v, 1)),
            Format::Frequency if v >= 1000.0 => format!("{} GHz", trim_float(v / 1000.0, 2)),
            Format::Frequency => format!("{} MHz", trim_float(v, 0)),
        }
    }
}

fn trim_float(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

pub fn human_bytes(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = v;
    let mut unit = 0;
    while v.abs() >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    let decimals = if unit == 0 || v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    };
    format!("{} {}", trim_float(v, decimals), UNITS[unit])
}

fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    let f = raw / mag;
    let nice = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .find(|&n| n >= f - 1e-9)
        .unwrap_or(10.0);
    nice * mag
}

/// Returns (axis max, tick values from 0) for an axis starting at zero.
pub fn y_ticks(max: f64, format: Format) -> (f64, Vec<f64>) {
    let max = if max > 0.0 && max.is_finite() {
        max
    } else {
        1.0
    };
    let unit = if format.binary() {
        1024f64.powf(max.log(1024.0).floor().max(0.0))
    } else {
        1.0
    };
    let step = nice_step(max / unit / 3.0) * unit;
    let top = (max / step - 1e-9).ceil().max(1.0) * step;
    let n = (top / step).round() as usize;
    (top, (0..=n).map(|i| i as f64 * step).collect())
}

pub fn fixed_ticks(max: f64) -> Vec<f64> {
    let step = nice_step(max / 4.0);
    let n = (max / step).round() as usize;
    (0..=n).map(|i| i as f64 * step).collect()
}

const TIME_STEPS: &[i64] = &[
    60,
    120,
    300,
    600,
    900,
    1800,
    3600,
    2 * 3600,
    3 * 3600,
    6 * 3600,
    12 * 3600,
    86400,
    2 * 86400,
];

/// Smallest step giving at most `max_ticks` ticks over the range.
pub fn time_step(range_secs: i64, max_ticks: i64) -> i64 {
    let max_ticks = max_ticks.max(1);
    TIME_STEPS
        .iter()
        .copied()
        .find(|&s| range_secs / s <= max_ticks)
        .unwrap_or(*TIME_STEPS.last().unwrap())
}

/// Tick times aligned to local wall-clock multiples of `step`.
pub fn time_ticks(from: i64, to: i64, step: i64, utc_offset: i64) -> Vec<i64> {
    let first = (from + utc_offset).div_euclid(step) * step - utc_offset;
    let mut t = if first < from { first + step } else { first };
    let mut out = Vec::new();
    while t <= to {
        out.push(t);
        t += step;
    }
    out
}

pub fn duration(secs: f64) -> String {
    let s = secs.round() as i64;
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
            .trim_start_matches("0m ")
            .to_string()
    } else if s < 86400 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86400, (s % 86400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y_ticks_are_nice() {
        assert_eq!(
            y_ticks(73.0, Format::Celsius),
            (75.0, vec![0.0, 25.0, 50.0, 75.0])
        );
        assert_eq!(y_ticks(0.0, Format::Percent).0, 1.0);
        let (top, ticks) = y_ticks(50.0 * 1024.0 * 1024.0, Format::BytesPerSec);
        assert_eq!(top, 60.0 * 1024.0 * 1024.0);
        assert_eq!(ticks.len(), 4);
        assert_eq!(fixed_ticks(100.0), vec![0.0, 25.0, 50.0, 75.0, 100.0]);
    }

    #[test]
    fn formats() {
        assert_eq!(Format::BytesPerSec.format(1536.0), "1.5 KiB/s");
        assert_eq!(
            Format::Bytes.format(64.0 * 1024.0 * 1024.0 * 1024.0),
            "64 GiB"
        );
        assert_eq!(Format::Percent.format(2.5), "2.5%");
        assert_eq!(Format::Percent.format(50.0), "50%");
        assert_eq!(Format::Celsius.format(52.4), "52°C");
        assert_eq!(Format::Frequency.format(4853.2), "4.85 GHz");
        assert_eq!(Format::Frequency.format(4000.0), "4 GHz");
        assert_eq!(Format::Frequency.format(624.194), "624 MHz");
    }

    #[test]
    fn time_ticks_align_to_local_time() {
        assert_eq!(time_step(3600, 8), 600);
        assert_eq!(time_step(5 * 86400, 8), 86400);
        assert_eq!(time_ticks(1000, 5000, 1800, 0), vec![1800, 3600]);
        assert_eq!(time_ticks(0, 7200, 3600, 1800), vec![1800, 5400]);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0.42), "0.4s");
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(125.0), "2m 5s");
        assert_eq!(duration(2.0 * 3600.0 + 14.0 * 60.0), "2h 14m");
        assert_eq!(duration(90000.0), "1d 1h");
    }
}
