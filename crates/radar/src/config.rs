use std::collections::HashMap;
use std::path::PathBuf;

use radar_core::{Sensor, SensorKind};

/// Explicit sensor visibility choices; sensors without one use `default_visible`.
#[derive(Debug, Default)]
pub struct Config {
    sensors: HashMap<String, bool>,
}

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("radar/viewer.conf")
}

pub fn default_visible(s: &Sensor) -> bool {
    if s.kind != SensorKind::Temp {
        return true;
    }
    let chip = s.chip.split(':').next().unwrap_or_default();
    matches!(
        (chip, s.label.as_str()),
        ("k10temp", "Tctl")
            | ("coretemp", "Package id 0")
            | ("amdgpu", "edge")
            | ("nvme", "Composite")
    )
}

impl Config {
    pub fn load() -> Self {
        let text = std::fs::read_to_string(path()).unwrap_or_default();
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let sensors = text
            .lines()
            .filter_map(|l| match l.split_at_checked(1)? {
                ("+", key) => Some((key.to_string(), true)),
                ("-", key) => Some((key.to_string(), false)),
                _ => None,
            })
            .collect();
        Config { sensors }
    }

    fn serialize(&self) -> String {
        let mut lines: Vec<String> = self
            .sensors
            .iter()
            .map(|(k, &v)| format!("{}{k}", if v { '+' } else { '-' }))
            .collect();
        lines.sort();
        lines.join("\n") + "\n"
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, self.serialize()) {
            eprintln!("radar: saving {}: {e}", path.display());
        }
    }

    pub fn visible(&self, s: &Sensor) -> bool {
        self.sensors
            .get(&s.key())
            .copied()
            .unwrap_or_else(|| default_visible(s))
    }

    pub fn set_visible(&mut self, s: &Sensor, visible: bool) {
        self.sensors.insert(s.key(), visible);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(chip: &str, label: &str) -> Sensor {
        Sensor {
            id: 1,
            kind: SensorKind::Temp,
            chip: chip.into(),
            label: label.into(),
            unit: "°C".into(),
        }
    }

    #[test]
    fn defaults_and_overrides_round_trip() {
        let mut c = Config::default();
        assert!(c.visible(&temp("k10temp", "Tctl")));
        assert!(c.visible(&temp("nvme:nvme1", "Composite")));
        assert!(!c.visible(&temp("k10temp", "Tccd1")));

        c.set_visible(&temp("k10temp", "Tccd1"), true);
        c.set_visible(&temp("nvme:nvme1", "Composite"), false);
        let c = Config::parse(&c.serialize());
        assert!(c.visible(&temp("k10temp", "Tccd1")));
        assert!(!c.visible(&temp("nvme:nvme1", "Composite")));
    }
}
