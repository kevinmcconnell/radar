use std::collections::HashMap;

use radar_core::db;
use radar_core::schema::SENSOR_ROLLUP_SECS;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::discover::SensorSource;
use crate::sampler::Sample;

pub struct Writer {
    conn: Connection,
    sensor_ids: Vec<i64>,
    name_ids: HashMap<Box<[u8]>, i64>,
}

impl Writer {
    pub fn new(conn: Connection) -> Self {
        Writer {
            conn,
            sensor_ids: Vec::new(),
            name_ids: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn write_meta(&self, clk_tck: u64, ncpus: u64, hostname: &str) -> rusqlite::Result<()> {
        db::set_meta(&self.conn, "clk_tck", &clk_tck.to_string())?;
        db::set_meta(&self.conn, "ncpus", &ncpus.to_string())?;
        db::set_meta(&self.conn, "hostname", hostname)?;
        db::set_meta(&self.conn, "collector_version", env!("CARGO_PKG_VERSION"))
    }

    pub fn register_sensors(&mut self, sensors: &[SensorSource]) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO sensors (kind, chip, label, unit) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (kind, chip, label) DO UPDATE SET unit = excluded.unit
             RETURNING id",
        )?;
        self.sensor_ids = sensors
            .iter()
            .map(|s| {
                stmt.query_row((s.kind.as_str(), &s.chip, &s.label, s.kind.unit()), |r| {
                    r.get(0)
                })
            })
            .collect::<rusqlite::Result<_>>()?;
        Ok(())
    }

    pub fn write<'a>(
        &mut self,
        s: &Sample,
        procs: impl Iterator<Item = (&'a [u8], u64)>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        let result = write_sample(&tx, &self.sensor_ids, &mut self.name_ids, s, procs)
            .and_then(|()| tx.commit());
        if result.is_err() {
            self.name_ids.clear();
        }
        result
    }

    pub fn trim(&mut self, cutoff: i64) -> rusqlite::Result<()> {
        self.name_ids.clear();
        db::trim(&mut self.conn, cutoff)
    }
}

fn write_sample<'a>(
    tx: &Transaction,
    sensor_ids: &[i64],
    name_ids: &mut HashMap<Box<[u8]>, i64>,
    s: &Sample,
    procs: impl Iterator<Item = (&'a [u8], u64)>,
) -> rusqlite::Result<()> {
    let inserted = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO sys_samples
           (ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used, mem_total, swap_used,
            net_rx, net_tx, disk_read, disk_write)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?
        .execute(rusqlite::params![
            s.ts,
            s.dt_ms,
            s.cpu_busy,
            s.cpu_iowait,
            s.load1,
            s.mem_used,
            s.mem_total,
            s.swap_used,
            s.net_rx,
            s.net_tx,
            s.disk_read,
            s.disk_write,
        ])?;
    if inserted == 0 {
        return Ok(());
    }

    let mut sensor_stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO sensor_samples (ts, sensor_id, value) VALUES (?1, ?2, ?3)",
    )?;
    let rollup = s.ts.div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS;
    let mut rollup_stmt = tx.prepare_cached(
        "INSERT INTO sensor_rollups (ts, sensor_id, total, peak, samples) VALUES (?1, ?2, ?3, ?3, 1)
         ON CONFLICT (ts, sensor_id) DO UPDATE SET
           total = total + excluded.total,
           peak = max(peak, excluded.peak),
           samples = samples + 1",
    )?;
    for &(i, value) in &s.sensors {
        if let Some(&id) = sensor_ids.get(i)
            && sensor_stmt.execute((s.ts, id, value))? > 0
        {
            rollup_stmt.execute((rollup, id, value))?;
        }
    }

    let minute = s.ts.div_euclid(60) * 60;
    let mut proc_stmt = tx.prepare_cached(
        "INSERT INTO proc_minutes (ts, name_id, cpu_ticks) VALUES (?1, ?2, ?3)
         ON CONFLICT (ts, name_id) DO UPDATE SET cpu_ticks = cpu_ticks + excluded.cpu_ticks",
    )?;
    for (name, ticks) in procs {
        let id = match name_ids.get(name) {
            Some(&id) => id,
            None => {
                let id = name_id(tx, name)?;
                name_ids.insert(name.into(), id);
                id
            }
        };
        proc_stmt.execute((minute, id, ticks as i64))?;
    }
    Ok(())
}

fn name_id(tx: &Transaction, name: &[u8]) -> rusqlite::Result<i64> {
    let name = String::from_utf8_lossy(name);
    let existing = tx
        .prepare_cached("SELECT id FROM proc_names WHERE name = ?1")?
        .query_row([&name], |r| r.get(0))
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => tx
            .prepare_cached("INSERT INTO proc_names (name) VALUES (?1) RETURNING id")?
            .query_row([&name], |r| r.get(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{SensorKind, query};

    fn sensor(chip: &str, label: &str) -> SensorSource {
        SensorSource {
            kind: SensorKind::Temp,
            chip: chip.into(),
            label: label.into(),
            path: "/x".into(),
            scale: 1.0,
        }
    }

    #[test]
    fn writes_samples_and_accumulates_minute_buckets() {
        let mut w = Writer::new(db::open_rw_in_memory().unwrap());
        w.register_sensors(&[sensor("k10temp", "Tctl"), sensor("nvme", "Composite")])
            .unwrap();
        w.register_sensors(&[sensor("nvme", "Composite")]).unwrap();
        assert_eq!(w.sensor_ids, vec![2]);

        let mut s = Sample {
            ts: 120,
            dt_ms: 5000,
            cpu_busy: Some(10.0),
            sensors: vec![(0, 40.0)],
            ..Default::default()
        };
        w.write(&s, [(&b"rustc"[..], 50), (&b"sh"[..], 1)].into_iter())
            .unwrap();
        s.ts = 125;
        w.write(&s, [(&b"rustc"[..], 25)].into_iter()).unwrap();
        s.ts = 180;
        w.write(&s, [(&b"rustc"[..], 5)].into_iter()).unwrap();

        let top = query::top_procs(w.conn(), 120, 120, 10).unwrap();
        assert_eq!(top[0].name, "rustc");
        assert_eq!(top[0].ticks, 75);
        let raw = query::sensor_series(w.conn(), 0, 1000, 5, &[2]).unwrap();
        assert_eq!(raw.points.len(), 3);
        let rolled = query::sensor_series(w.conn(), 0, 1000, SENSOR_ROLLUP_SECS, &[2]).unwrap();
        assert_eq!(rolled.points.len(), 1);
        assert_eq!(rolled.points[0].value, 40.0);
        assert_eq!(rolled.stats, raw.stats);

        w.trim(150).unwrap();
        s.ts = 185;
        w.write(&s, [(&b"sh"[..], 3)].into_iter()).unwrap();
        let top = query::top_procs(w.conn(), 0, 1000, 10).unwrap();
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn duplicate_timestamp_writes_nothing() {
        let mut w = Writer::new(db::open_rw_in_memory().unwrap());
        let s = Sample {
            ts: 120,
            dt_ms: 5000,
            ..Default::default()
        };
        w.write(&s, [(&b"rustc"[..], 50)].into_iter()).unwrap();
        w.write(&s, [(&b"rustc"[..], 50)].into_iter()).unwrap();
        assert_eq!(
            query::top_procs(w.conn(), 0, 1000, 10).unwrap()[0].ticks,
            50
        );
    }
}
