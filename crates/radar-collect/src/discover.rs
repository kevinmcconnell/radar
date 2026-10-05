use std::fs;
use std::path::{Path, PathBuf};

use radar_core::SensorKind;

use crate::agents::{Agent, Direction};

const MIN_PLAUSIBLE_MILLIDEGREES: i64 = -40_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    File {
        path: PathBuf,
        scale: f64,
    },
    BusyFreq,
    FastestFreq,
    /// Percent left of the `window`th rate-limit window the agent has reported.
    Quota {
        agent: Agent,
        window: usize,
    },
    Tokens {
        agent: Agent,
        direction: Direction,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SensorSource {
    pub kind: SensorKind,
    pub chip: String,
    pub label: String,
    pub reading: Reading,
}

/// Clock speed in kHz of one CPU thread. `core` numbers the physical cores from 0.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreFreq {
    pub cpu: usize,
    pub core: usize,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovery {
    pub sensors: Vec<SensorSource>,
    pub core_freqs: Vec<CoreFreq>,
    pub net_ifaces: Vec<String>,
    pub disks: Vec<String>,
}

pub fn discover(root: &Path) -> Discovery {
    let mut sensors = hwmon_sensors(root);
    sensors.extend(gpu_busy_sensors(root));
    sensors.extend(battery_sensors(root));
    let core_freqs = core_freqs(root);
    if !core_freqs.is_empty() {
        sensors.extend(cpu_freq_sensors());
    }
    Discovery {
        sensors,
        core_freqs,
        net_ifaces: entries_with_device(&root.join("sys/class/net")),
        disks: entries_with_device(&root.join("sys/block")),
    }
}

fn sorted_entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = rd
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    out.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    out
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let i = s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        (s[..i].to_string(), s[i..].parse::<u64>().unwrap_or(0))
    };
    split(a).cmp(&split(b)).then_with(|| a.cmp(b))
}

fn entries_with_device(dir: &Path) -> Vec<String> {
    sorted_entries(dir)
        .into_iter()
        .filter(|(_, p)| p.join("device").exists())
        .map(|(n, _)| n)
        .collect()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn read_number(path: &Path) -> Option<i64> {
    read_trimmed(path)?.parse().ok()
}

fn link_name(path: &Path) -> Option<String> {
    let target = fs::read_link(path).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

fn hwmon_sensors(root: &Path) -> Vec<SensorSource> {
    let chips: Vec<(String, PathBuf, Option<String>)> =
        sorted_entries(&root.join("sys/class/hwmon"))
            .into_iter()
            .filter_map(|(_, dir)| {
                let name = read_trimmed(&dir.join("name"))?;
                let device = link_name(&dir.join("device"));
                Some((name, dir, device))
            })
            .collect();

    // hwmonN numbering is unstable across boots and hotplug, so chips are keyed by their device
    let mut named: Vec<(String, &PathBuf)> = chips
        .iter()
        .map(|(name, dir, device)| {
            let chip = match device {
                Some(dev) => format!("{name}:{dev}"),
                None => name.clone(),
            };
            (chip, dir)
        })
        .collect();
    named.sort_by(|a, b| natural_cmp(&a.0, &b.0));

    let mut out = Vec::new();
    for (chip, dir) in named {
        let mut chip_sensors = Vec::new();
        for (file, path) in sorted_entries(dir) {
            let (prefix, kind, scale) = if file.starts_with("temp") {
                ("temp", SensorKind::Temp, 0.001)
            } else if file.starts_with("fan") {
                ("fan", SensorKind::Fan, 1.0)
            } else {
                continue;
            };
            let Some(index) = file
                .strip_prefix(prefix)
                .and_then(|s| s.strip_suffix("_input"))
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            let Some(value) = read_number(&path) else {
                continue;
            };
            let disconnected = match kind {
                SensorKind::Fan => value == 0,
                _ => value < MIN_PLAUSIBLE_MILLIDEGREES,
            };
            if disconnected {
                continue;
            }
            let label = read_trimmed(&dir.join(format!("{prefix}{index}_label")))
                .unwrap_or_else(|| format!("{prefix}{index}"));
            let order = (prefix, index.parse::<u32>().unwrap_or(0));
            chip_sensors.push((
                order,
                SensorSource {
                    kind,
                    chip: chip.clone(),
                    label,
                    reading: Reading::File { path, scale },
                },
            ));
        }
        chip_sensors.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(chip_sensors.into_iter().map(|(_, s)| s));
    }
    out
}

fn gpu_busy_sensors(root: &Path) -> Vec<SensorSource> {
    sorted_entries(&root.join("sys/class/drm"))
        .into_iter()
        .filter(|(name, _)| name.starts_with("card") && !name.contains('-'))
        .filter_map(|(name, dir)| {
            let path = dir.join("device/gpu_busy_percent");
            read_number(&path)?;
            let chip = link_name(&dir.join("device/driver")).unwrap_or_else(|| "gpu".into());
            Some(SensorSource {
                kind: SensorKind::GpuBusy,
                chip,
                label: name,
                reading: Reading::File { path, scale: 1.0 },
            })
        })
        .collect()
}

fn battery_sensors(root: &Path) -> Vec<SensorSource> {
    sorted_entries(&root.join("sys/class/power_supply"))
        .into_iter()
        .filter(|(name, _)| name.starts_with("BAT"))
        .filter_map(|(name, dir)| {
            let path = dir.join("power_now");
            read_number(&path)?;
            Some(SensorSource {
                kind: SensorKind::Power,
                chip: "battery".into(),
                label: name,
                reading: Reading::File { path, scale: 1e-6 },
            })
        })
        .collect()
}

fn core_freqs(root: &Path) -> Vec<CoreFreq> {
    let mut physical: Vec<(i64, i64)> = Vec::new();
    sorted_entries(&root.join("sys/devices/system/cpu"))
        .into_iter()
        .filter_map(|(name, dir)| {
            let cpu: usize = name.strip_prefix("cpu")?.parse().ok()?;
            let path = dir.join("cpufreq/scaling_cur_freq");
            read_number(&path)?;
            let topology = |file: &str| read_number(&dir.join("topology").join(file));
            let key = match (topology("physical_package_id"), topology("core_id")) {
                (Some(package), Some(core)) => (package, core),
                _ => (-1, cpu as i64),
            };
            let core = physical.iter().position(|k| *k == key).unwrap_or_else(|| {
                physical.push(key);
                physical.len() - 1
            });
            Some(CoreFreq { cpu, core, path })
        })
        .collect()
}

fn cpu_freq_sensors() -> [SensorSource; 2] {
    [
        ("busy cores", Reading::BusyFreq),
        ("fastest core", Reading::FastestFreq),
    ]
    .map(|(label, reading)| SensorSource {
        kind: SensorKind::Freq,
        chip: "cpu".into(),
        label: label.into(),
        reading,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amd() -> Discovery {
        discover(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amd"))
    }

    fn names(d: &Discovery) -> Vec<(SensorKind, String, String)> {
        d.sensors
            .iter()
            .map(|s| (s.kind, s.chip.clone(), s.label.clone()))
            .collect()
    }

    #[test]
    fn discovers_hwmon_with_stable_names() {
        let d = amd();
        let n = names(&d);
        let t = |c: &str, l: &str| (SensorKind::Temp, c.to_string(), l.to_string());
        assert_eq!(
            n,
            vec![
                t("amdgpu:0000:03:00.0", "edge"),
                t("amdgpu:0000:03:00.0", "junction"),
                t("amdgpu:0000:03:00.0", "mem"),
                (
                    SensorKind::Fan,
                    "asusec:asus-ec-sensors".into(),
                    "CPU_Opt".into()
                ),
                t("asusec:asus-ec-sensors", "Chipset"),
                t("k10temp:0000:00:18.3", "Tctl"),
                t("k10temp:0000:00:18.3", "Tccd1"),
                t("k10temp:0000:00:18.3", "Tccd2"),
                t("nvme:nvme0", "Composite"),
                t("nvme:nvme1", "Composite"),
                t("spd5118:8-0051", "temp1"),
                t("spd5118:8-0053", "temp1"),
                (SensorKind::GpuBusy, "amdgpu".into(), "card1".into()),
                (SensorKind::Power, "battery".into(), "BAT0".into()),
                (SensorKind::Freq, "cpu".into(), "busy cores".into()),
                (SensorKind::Freq, "cpu".into(), "fastest core".into()),
            ]
        );
    }

    #[test]
    fn discovers_physical_interfaces_and_disks() {
        let d = amd();
        assert_eq!(d.net_ifaces, vec!["enp12s0", "enp13s0"]);
        assert_eq!(d.disks, vec!["nvme0n1", "nvme2n1"]);
    }

    #[test]
    fn discovers_core_clock_speeds() {
        let cores: Vec<(usize, usize)> = amd().core_freqs.iter().map(|c| (c.cpu, c.core)).collect();
        assert_eq!(cores, vec![(0, 0), (1, 0), (2, 1)]);
    }

    #[test]
    fn missing_root_is_empty() {
        assert_eq!(discover(Path::new("/nonexistent")), Discovery::default());
    }
}
