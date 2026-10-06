use std::collections::HashMap;

use radar_core::db;
use std::path::Path;

use radar_core::schema::{FLUSHED_BATCH_META, LIVE_TABLES, SENSOR_ROLLUP_SECS};
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::discover::SensorSource;
use crate::sampler::Sample;

/// Samples go into the `live` database first, and `flush` moves finished
/// rows into the main one in bulk. With a live file on tmpfs, that keeps the
/// per-sample page writes off the disk.
pub struct Writer {
    conn: Connection,
    buffered: bool,
    sensor_ids: Vec<i64>,
    name_ids: HashMap<Box<[u8]>, i64>,
}

impl Writer {
    pub fn new(conn: Connection, live: Option<&Path>) -> rusqlite::Result<Self> {
        db::attach_live(&conn, live)?;
        match live {
            Some(path) => db::set_meta(&conn, db::LIVE_DB_META, &path.to_string_lossy())?,
            None => db::remove_meta(&conn, db::LIVE_DB_META)?,
        }
        Ok(Writer {
            conn,
            buffered: live.is_some(),
            sensor_ids: Vec::new(),
            name_ids: HashMap::new(),
        })
    }

    /// Whether samples wait in a live file until the next flush, rather than
    /// in memory that must be flushed after every sample.
    pub fn buffered(&self) -> bool {
        self.buffered
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

    /// Moves every live row with `ts < before` into the main database.
    ///
    /// Two transactions, because a transaction over two WAL databases is not
    /// atomic. The first moves the rows into the `staged_` tables of the live
    /// file, tagged with a new batch number. The second merges them into main
    /// and records that batch number in `meta`, so a batch found staged
    /// after a crash is merged if its number is newer than the record, and
    /// discarded if it is not. Nothing is lost or counted twice however the
    /// process dies between or during them. The merges add to any existing
    /// rollup or minute row, so a bucket split by a restart adds up
    /// correctly. Rows whose sensor or process name is gone from main are
    /// dropped rather than failing the merge on a foreign key.
    pub fn flush(&mut self, before: i64) -> rusqlite::Result<()> {
        let merged: i64 = db::get_meta(&self.conn, FLUSHED_BATCH_META)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let batch = merged + 1;

        let tx = self.conn.transaction()?;
        for (table, columns) in LIVE_TABLES {
            tx.prepare_cached(&format!(
                "DELETE FROM live.staged_{table} WHERE batch <= ?1"
            ))?
            .execute([merged])?;
            tx.prepare_cached(&format!(
                "INSERT INTO live.staged_{table} ({columns}, batch)
                 SELECT {columns}, ?1 FROM live.{table} WHERE ts < ?2"
            ))?
            .execute([batch, before])?;
            tx.prepare_cached(&format!("DELETE FROM live.{table} WHERE ts < ?1"))?
                .execute([before])?;
        }
        tx.commit()?;

        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "INSERT OR IGNORE INTO main.sys_samples
               SELECT ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used, mem_total, swap_used,
                      net_rx, net_tx, disk_read, disk_write
               FROM live.staged_sys_samples;
             INSERT OR IGNORE INTO main.sensor_samples
               SELECT ts, sensor_id, value FROM live.staged_sensor_samples
               WHERE sensor_id IN (SELECT id FROM main.sensors);
             INSERT INTO main.sensor_rollups
               SELECT ts, sensor_id, total, peak, samples FROM live.staged_sensor_rollups
               WHERE sensor_id IN (SELECT id FROM main.sensors)
               ON CONFLICT (ts, sensor_id) DO UPDATE SET
                 total = total + excluded.total,
                 peak = max(peak, excluded.peak),
                 samples = samples + excluded.samples;
             INSERT INTO main.proc_minutes
               SELECT ts, name_id, cpu_ticks FROM live.staged_proc_minutes
               WHERE name_id IN (SELECT id FROM main.proc_names)
               ON CONFLICT (ts, name_id) DO UPDATE SET cpu_ticks = cpu_ticks + excluded.cpu_ticks;
             DELETE FROM live.staged_sys_samples;
             DELETE FROM live.staged_sensor_samples;
             DELETE FROM live.staged_sensor_rollups;
             DELETE FROM live.staged_proc_minutes;",
        )?;
        db::set_meta(&tx, FLUSHED_BATCH_META, &batch.to_string())?;
        tx.commit()
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
            "INSERT OR IGNORE INTO live.sys_samples
           (ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used, mem_total, swap_used,
            net_rx, net_tx, disk_read, disk_write)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
         WHERE NOT EXISTS (SELECT 1 FROM main.sys_samples WHERE ts = ?1)
           AND NOT EXISTS (SELECT 1 FROM live.staged_sys_samples WHERE ts = ?1)",
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
        "INSERT OR IGNORE INTO live.sensor_samples (ts, sensor_id, value) VALUES (?1, ?2, ?3)",
    )?;
    let rollup = s.ts.div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS;
    let mut rollup_stmt = tx.prepare_cached(
        "INSERT INTO live.sensor_rollups (ts, sensor_id, total, peak, samples) VALUES (?1, ?2, ?3, ?3, 1)
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
        "INSERT INTO live.proc_minutes (ts, name_id, cpu_ticks) VALUES (?1, ?2, ?3)
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
    use crate::discover::Reading;
    use radar_core::{SensorKind, query};
    use std::path::PathBuf;

    fn sensor(chip: &str, label: &str) -> SensorSource {
        SensorSource {
            kind: SensorKind::Temp,
            chip: chip.into(),
            label: label.into(),
            reading: Reading::File {
                path: "/x".into(),
                scale: 1.0,
            },
        }
    }

    fn writer() -> Writer {
        Writer::new(db::open_rw_in_memory().unwrap(), None).unwrap()
    }

    fn sample(ts: i64, value: f64) -> Sample {
        Sample {
            ts,
            dt_ms: 5000,
            cpu_busy: Some(10.0),
            sensors: vec![(0, value)],
            ..Default::default()
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn rollups(conn: &Connection) -> Vec<(i64, f64, f64, i64)> {
        conn.prepare("SELECT ts, total, peak, samples FROM main.sensor_rollups ORDER BY ts")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn temp_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("radar-writer-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn writes_samples_and_accumulates_minute_buckets() {
        let mut w = writer();
        w.register_sensors(&[sensor("k10temp", "Tctl"), sensor("nvme", "Composite")])
            .unwrap();
        w.register_sensors(&[sensor("nvme", "Composite")]).unwrap();
        assert_eq!(w.sensor_ids, vec![2]);

        w.write(
            &sample(120, 40.0),
            [(&b"rustc"[..], 50), (&b"sh"[..], 1)].into_iter(),
        )
        .unwrap();
        w.write(&sample(125, 40.0), [(&b"rustc"[..], 25)].into_iter())
            .unwrap();
        w.write(&sample(180, 40.0), [(&b"rustc"[..], 5)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();

        let top = query::top_procs(w.conn(), 120, 120, 10).unwrap();
        assert_eq!(top[0].name, "rustc");
        assert_eq!(top[0].ticks, 75);
        let raw = query::sensor_series(w.conn(), 0, 1000, 5).unwrap();
        assert_eq!(raw.points.len(), 3);
        let rolled = query::sensor_series(w.conn(), 0, 1000, SENSOR_ROLLUP_SECS).unwrap();
        assert_eq!(rolled.points.len(), 1);
        assert_eq!(rolled.points[0].value, 40.0);
        assert_eq!(rolled.stats, raw.stats);

        w.trim(150).unwrap();
        w.write(&sample(185, 40.0), [(&b"sh"[..], 3)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();
        let top = query::top_procs(w.conn(), 0, 1000, 10).unwrap();
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn duplicate_timestamp_writes_nothing() {
        let mut w = writer();
        let s = Sample {
            ts: 120,
            dt_ms: 5000,
            ..Default::default()
        };
        w.write(&s, [(&b"rustc"[..], 50)].into_iter()).unwrap();
        w.write(&s, [(&b"rustc"[..], 50)].into_iter()).unwrap();
        w.flush(i64::MAX).unwrap();
        assert_eq!(
            query::top_procs(w.conn(), 0, 1000, 10).unwrap()[0].ticks,
            50
        );
    }

    #[test]
    fn a_timestamp_already_in_main_is_not_counted_again() {
        let mut w = writer();
        w.register_sensors(&[sensor("k10temp", "Tctl")]).unwrap();
        w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();
        w.write(&sample(120, 90.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();

        assert_eq!(rollups(w.conn()), [(0, 40.0, 40.0, 1)]);
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            50
        );
    }

    #[test]
    fn a_timestamp_already_staged_is_not_counted_again() {
        let mut w = writer();
        w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.conn()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse BEFORE INSERT ON main.sys_samples
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
        assert!(w.flush(i64::MAX).is_err());
        w.write(&sample(120, 90.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.conn().execute_batch("DROP TRIGGER refuse").unwrap();
        w.flush(i64::MAX).unwrap();
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            50
        );
    }

    #[test]
    fn a_failed_merge_is_retried_by_the_next_flush() {
        let mut w = writer();
        w.register_sensors(&[sensor("k10temp", "Tctl")]).unwrap();
        w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.conn()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse BEFORE INSERT ON main.sys_samples
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
            )
            .unwrap();
        assert!(w.flush(i64::MAX).is_err());
        assert_eq!(count(w.conn(), "live.sys_samples"), 0);
        assert_eq!(count(w.conn(), "live.staged_sys_samples"), 1);
        assert_eq!(count(w.conn(), "main.sys_samples"), 0);

        w.conn().execute_batch("DROP TRIGGER refuse").unwrap();
        w.write(&sample(125, 60.0), [(&b"rustc"[..], 25)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();
        assert_eq!(count(w.conn(), "main.sys_samples"), 2);
        assert_eq!(count(w.conn(), "live.staged_sys_samples"), 0);
        assert_eq!(rollups(w.conn()), [(0, 100.0, 60.0, 2)]);
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            75
        );
    }

    #[test]
    fn a_batch_staged_before_a_crash_is_merged_on_restart() {
        let main = temp_path("staged-main");
        let live = temp_path("staged-live");
        {
            let mut w = Writer::new(db::open_rw(&main).unwrap(), Some(&live)).unwrap();
            w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
                .unwrap();
            w.conn()
                .execute_batch(
                    "CREATE TEMP TRIGGER refuse BEFORE INSERT ON main.sys_samples
                     BEGIN SELECT RAISE(ABORT, 'refused'); END",
                )
                .unwrap();
            assert!(w.flush(i64::MAX).is_err());
        }
        let mut w = Writer::new(db::open_rw(&main).unwrap(), Some(&live)).unwrap();
        assert_eq!(count(w.conn(), "live.staged_sys_samples"), 1);
        w.flush(i64::MAX).unwrap();
        assert_eq!(count(w.conn(), "main.sys_samples"), 1);
        assert_eq!(count(w.conn(), "live.staged_sys_samples"), 0);
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            50
        );
        for path in [main, live] {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_batch_already_merged_before_a_crash_is_not_merged_again() {
        let mut w = writer();
        w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();
        let merged: i64 = db::get_meta(w.conn(), FLUSHED_BATCH_META)
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        w.conn()
            .execute(
                "INSERT INTO live.staged_proc_minutes VALUES (120, 1, 50, ?1)",
                [merged],
            )
            .unwrap();

        w.flush(i64::MAX).unwrap();

        assert_eq!(count(w.conn(), "live.staged_proc_minutes"), 0);
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            50
        );
    }

    #[test]
    fn flush_moves_only_rows_before_the_boundary() {
        let mut w = writer();
        w.register_sensors(&[sensor("k10temp", "Tctl")]).unwrap();
        w.write(&sample(295, 40.0), [(&b"rustc"[..], 5)].into_iter())
            .unwrap();
        w.write(&sample(305, 50.0), [(&b"rustc"[..], 5)].into_iter())
            .unwrap();
        assert_eq!(count(w.conn(), "main.sys_samples"), 0);

        w.flush(300).unwrap();

        for (table, _) in LIVE_TABLES {
            assert_eq!(count(w.conn(), &format!("main.{table}")), 1, "{table}");
            assert_eq!(count(w.conn(), &format!("live.{table}")), 1, "{table}");
            assert_eq!(
                count(w.conn(), &format!("live.staged_{table}")),
                0,
                "{table}"
            );
        }
        assert_eq!(query::oldest_sample(w.conn()).unwrap(), Some(295));
        assert_eq!(rollups(w.conn()), [(0, 40.0, 40.0, 1)]);
    }

    #[test]
    fn a_bucket_split_by_a_flush_adds_up() {
        let mut split = writer();
        let mut whole = writer();
        for w in [&mut split, &mut whole] {
            w.register_sensors(&[sensor("k10temp", "Tctl")]).unwrap();
            w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
                .unwrap();
        }
        split.flush(i64::MAX).unwrap();
        for w in [&mut split, &mut whole] {
            w.write(&sample(125, 60.0), [(&b"rustc"[..], 25)].into_iter())
                .unwrap();
            w.flush(i64::MAX).unwrap();
        }

        assert_eq!(rollups(split.conn()), [(0, 100.0, 60.0, 2)]);
        assert_eq!(rollups(split.conn()), rollups(whole.conn()));
        let top = query::top_procs(split.conn(), 120, 120, 10).unwrap();
        assert_eq!((top[0].name.as_str(), top[0].ticks), ("rustc", 75));
        assert_eq!(top, query::top_procs(whole.conn(), 120, 120, 10).unwrap());
    }

    #[test]
    fn trim_keeps_names_still_used_in_live() {
        let mut w = writer();
        w.write(&sample(120, 40.0), [(&b"old"[..], 5)].into_iter())
            .unwrap();
        w.flush(i64::MAX).unwrap();
        w.write(&sample(4000, 40.0), [(&b"new"[..], 5)].into_iter())
            .unwrap();

        w.trim(3000).unwrap();

        let names: Vec<String> = w
            .conn()
            .prepare("SELECT name FROM proc_names ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(names, ["new"]);
        w.flush(i64::MAX).unwrap();
        assert_eq!(
            query::top_procs(w.conn(), 0, 5000, 10).unwrap()[0].name,
            "new"
        );
    }

    #[test]
    fn flush_drops_minutes_whose_name_is_gone() {
        let mut w = writer();
        w.conn()
            .execute("INSERT INTO live.proc_minutes VALUES (60, 999, 5)", [])
            .unwrap();
        w.flush(i64::MAX).unwrap();
        assert_eq!(count(w.conn(), "main.proc_minutes"), 0);
        assert_eq!(count(w.conn(), "live.staged_proc_minutes"), 0);
    }

    #[test]
    fn rows_left_in_the_live_file_are_recovered_on_start() {
        let main = temp_path("recover-main");
        let live = temp_path("recover-live");
        {
            let mut w = Writer::new(db::open_rw(&main).unwrap(), Some(&live)).unwrap();
            assert!(w.buffered());
            w.register_sensors(&[sensor("k10temp", "Tctl")]).unwrap();
            w.write(&sample(120, 40.0), [(&b"rustc"[..], 50)].into_iter())
                .unwrap();
        }
        let mut w = Writer::new(db::open_rw(&main).unwrap(), Some(&live)).unwrap();
        assert_eq!(
            db::get_meta(w.conn(), db::LIVE_DB_META).unwrap().as_deref(),
            Some(live.to_str().unwrap())
        );
        assert_eq!(count(w.conn(), "main.sys_samples"), 0);
        w.flush(i64::MAX).unwrap();
        assert_eq!(count(w.conn(), "main.sys_samples"), 1);
        assert_eq!(count(w.conn(), "live.sys_samples"), 0);
        assert_eq!(
            query::top_procs(w.conn(), 120, 120, 10).unwrap()[0].ticks,
            50
        );
        for path in [main, live] {
            let _ = std::fs::remove_file(&path);
        }
    }
}
