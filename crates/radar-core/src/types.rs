#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SysPoint {
    pub t: i64,
    pub cpu_busy: Option<f64>,
    pub cpu_iowait: Option<f64>,
    pub load1: Option<f64>,
    pub mem_used: Option<f64>,
    pub mem_total: Option<f64>,
    pub swap_used: Option<f64>,
    pub net_rx: Option<f64>,
    pub net_tx: Option<f64>,
    pub disk_read: Option<f64>,
    pub disk_write: Option<f64>,
    /// The bucket holds the first sample after a collection gap.
    pub gap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    pub avg: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SysStats {
    pub cpu_busy: Option<Stats>,
    pub mem_used: Option<Stats>,
    pub net_rx: Option<Stats>,
    pub disk_read: Option<Stats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorKind {
    Temp,
    Fan,
    GpuBusy,
    Power,
    Freq,
    Quota,
    Tokens,
}

impl SensorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SensorKind::Temp => "temp",
            SensorKind::Fan => "fan",
            SensorKind::GpuBusy => "gpu_busy",
            SensorKind::Power => "power",
            SensorKind::Freq => "freq",
            SensorKind::Quota => "quota",
            SensorKind::Tokens => "tokens",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "temp" => SensorKind::Temp,
            "fan" => SensorKind::Fan,
            "gpu_busy" => SensorKind::GpuBusy,
            "power" => SensorKind::Power,
            "freq" => SensorKind::Freq,
            "quota" => SensorKind::Quota,
            "tokens" => SensorKind::Tokens,
            _ => return None,
        })
    }

    pub fn unit(self) -> &'static str {
        match self {
            SensorKind::Temp => "°C",
            SensorKind::Fan => "rpm",
            SensorKind::GpuBusy => "%",
            SensorKind::Power => "W",
            SensorKind::Freq => "MHz",
            SensorKind::Quota => "%",
            SensorKind::Tokens => "tok/min",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub id: i64,
    pub kind: SensorKind,
    pub chip: String,
    pub label: String,
    pub unit: String,
}

impl Sensor {
    pub fn key(&self) -> String {
        format!("{}/{}", self.chip, self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorPoint {
    pub t: i64,
    pub sensor_id: i64,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcUsage {
    pub name: String,
    pub ticks: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub clk_tck: i64,
    pub ncpus: i64,
    pub hostname: String,
}

impl Default for Meta {
    fn default() -> Self {
        Meta {
            clk_tck: 100,
            ncpus: 1,
            hostname: String::new(),
        }
    }
}
