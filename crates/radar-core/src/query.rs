use rusqlite::{Connection, named_params};

use std::collections::HashMap;

use crate::schema::SENSOR_ROLLUP_SECS;
use crate::types::{Meta, ProcUsage, Sensor, SensorKind, SensorPoint, Stats, SysPoint, SysStats};

const NICE_BUCKETS: &[i64] = &[5, 10, 30, 60, 300, 900, 1800, 3600];

pub fn nice_bucket(range_secs: i64, width_px: i32) -> i64 {
    let points = (width_px / 2).max(1) as i64;
    let raw = (range_secs / points).max(5);
    NICE_BUCKETS
        .iter()
        .copied()
        .find(|&b| b >= raw)
        .unwrap_or_else(|| (raw + 3599) / 3600 * 3600)
}

pub fn sys_series(
    conn: &Connection,
    from: i64,
    to: i64,
    bucket: i64,
) -> rusqlite::Result<Vec<SysPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT (ts / :b) * :b AS t,
                avg(cpu_busy), avg(cpu_iowait), avg(load1),
                avg(mem_used), max(mem_total), avg(swap_used),
                sum(net_rx) * 1000.0 / sum(CASE WHEN net_rx IS NOT NULL THEN dt_ms END),
                sum(net_tx) * 1000.0 / sum(CASE WHEN net_tx IS NOT NULL THEN dt_ms END),
                sum(disk_read) * 1000.0 / sum(CASE WHEN disk_read IS NOT NULL THEN dt_ms END),
                sum(disk_write) * 1000.0 / sum(CASE WHEN disk_write IS NOT NULL THEN dt_ms END),
                max(cpu_busy IS NULL)
         FROM sys_samples
         WHERE ts BETWEEN :from AND :to
         GROUP BY t ORDER BY t",
    )?;
    let rows = stmt.query_map(
        named_params! { ":b": bucket, ":from": from, ":to": to },
        |r| {
            Ok(SysPoint {
                t: r.get(0)?,
                cpu_busy: r.get(1)?,
                cpu_iowait: r.get(2)?,
                load1: r.get(3)?,
                mem_used: r.get(4)?,
                mem_total: r.get(5)?,
                swap_used: r.get(6)?,
                net_rx: r.get(7)?,
                net_tx: r.get(8)?,
                disk_read: r.get(9)?,
                disk_write: r.get(10)?,
                gap: r.get(11)?,
            })
        },
    )?;
    rows.collect()
}

fn stats(avg: Option<f64>, max: Option<f64>) -> Option<Stats> {
    Some(Stats {
        avg: avg?,
        max: max?,
    })
}

/// Average and peak over raw samples, rather than over chart buckets.
pub fn sys_stats(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<SysStats> {
    conn.prepare_cached(
        "SELECT avg(cpu_busy), max(cpu_busy), avg(mem_used), max(mem_used),
                sum(net_rx) * 1000.0 / sum(CASE WHEN net_rx IS NOT NULL THEN dt_ms END),
                max(net_rx * 1000.0 / nullif(dt_ms, 0)),
                sum(disk_read) * 1000.0 / sum(CASE WHEN disk_read IS NOT NULL THEN dt_ms END),
                max(disk_read * 1000.0 / nullif(dt_ms, 0))
         FROM sys_samples
         WHERE ts BETWEEN ?1 AND ?2",
    )?
    .query_row([from, to], |r| {
        Ok(SysStats {
            cpu_busy: stats(r.get(0)?, r.get(1)?),
            mem_used: stats(r.get(2)?, r.get(3)?),
            net_rx: stats(r.get(4)?, r.get(5)?),
            disk_read: stats(r.get(6)?, r.get(7)?),
        })
    })
}

/// The span covered by the minute buckets that overlap `from..=to`, clipped to `to`.
pub fn proc_span(from: i64, to: i64) -> (i64, i64) {
    let lo = from.div_euclid(60) * 60;
    let hi = (to.div_euclid(60) + 1) * 60;
    (lo, hi.min(to.max(lo + 1)))
}

pub struct SensorData {
    pub points: Vec<SensorPoint>,
    pub stats: HashMap<i64, Stats>,
}

/// Reads whole rollups where `bucket` allows it, and raw samples for the rest of the range.
pub fn sensor_series(
    conn: &Connection,
    from: i64,
    to: i64,
    bucket: i64,
    sensor_ids: &[i64],
) -> rusqlite::Result<SensorData> {
    let ids = sensor_ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let ids = format!("[{ids}]");
    let rolled_from =
        (from + SENSOR_ROLLUP_SECS - 1).div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS;
    let rolled_to = (to + 1).div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS;

    if bucket % SENSOR_ROLLUP_SECS == 0 && rolled_from < rolled_to {
        let mut stmt = conn.prepare_cached(
            "SELECT (ts / :b) * :b AS t, sensor_id, sum(total), max(peak), sum(samples)
             FROM (
               SELECT ts, sensor_id, total, peak, samples FROM sensor_rollups
               WHERE ts >= :rolled_from AND ts < :rolled_to
               UNION ALL
               SELECT ts, sensor_id, value, value, 1 FROM sensor_samples
               WHERE ts >= :from AND ts < :rolled_from
               UNION ALL
               SELECT ts, sensor_id, value, value, 1 FROM sensor_samples
               WHERE ts >= :rolled_to AND ts <= :to
             )
             WHERE sensor_id IN (SELECT value FROM json_each(:ids))
             GROUP BY t, sensor_id ORDER BY t",
        )?;
        sensor_data(stmt.query(named_params! {
            ":b": bucket,
            ":from": from,
            ":to": to,
            ":rolled_from": rolled_from,
            ":rolled_to": rolled_to,
            ":ids": ids,
        })?)
    } else {
        let mut stmt = conn.prepare_cached(
            "SELECT (ts / :b) * :b AS t, sensor_id, sum(value), max(value), count(*)
             FROM sensor_samples
             WHERE ts BETWEEN :from AND :to
               AND sensor_id IN (SELECT value FROM json_each(:ids))
             GROUP BY t, sensor_id ORDER BY t",
        )?;
        sensor_data(
            stmt.query(named_params! { ":b": bucket, ":from": from, ":to": to, ":ids": ids })?,
        )
    }
}

fn sensor_data(mut rows: rusqlite::Rows) -> rusqlite::Result<SensorData> {
    let mut points = Vec::new();
    let mut totals: HashMap<i64, (f64, f64, i64)> = HashMap::new();
    while let Some(r) = rows.next()? {
        let (t, sensor_id): (i64, i64) = (r.get(0)?, r.get(1)?);
        let (total, peak, samples): (f64, f64, i64) = (r.get(2)?, r.get(3)?, r.get(4)?);
        points.push(SensorPoint {
            t,
            sensor_id,
            value: total / samples as f64,
        });
        let sensor = totals.entry(sensor_id).or_insert((0.0, peak, 0));
        sensor.0 += total;
        sensor.1 = sensor.1.max(peak);
        sensor.2 += samples;
    }
    let stats = totals
        .into_iter()
        .map(|(id, (total, peak, samples))| {
            let avg = total / samples as f64;
            (id, Stats { avg, max: peak })
        })
        .collect();
    Ok(SensorData { points, stats })
}

pub fn top_procs(
    conn: &Connection,
    from: i64,
    to: i64,
    n: i64,
) -> rusqlite::Result<Vec<ProcUsage>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.name, sum(p.cpu_ticks) AS ticks
         FROM proc_minutes p JOIN proc_names n ON n.id = p.name_id
         WHERE p.ts > :from - 60 AND p.ts <= :to
         GROUP BY p.name_id ORDER BY ticks DESC LIMIT :n",
    )?;
    let rows = stmt.query_map(named_params! { ":from": from, ":to": to, ":n": n }, |r| {
        Ok(ProcUsage {
            name: r.get(0)?,
            ticks: r.get(1)?,
        })
    })?;
    rows.collect()
}

pub fn sensors(conn: &Connection) -> rusqlite::Result<Vec<Sensor>> {
    let mut stmt =
        conn.prepare_cached("SELECT id, kind, chip, label, unit FROM sensors ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(1)?;
        Ok((r.get(0)?, kind, r.get(2)?, r.get(3)?, r.get(4)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, chip, label, unit) = row?;
        if let Some(kind) = SensorKind::parse(&kind) {
            out.push(Sensor {
                id,
                kind,
                chip,
                label,
                unit,
            });
        }
    }
    Ok(out)
}

pub fn meta(conn: &Connection) -> rusqlite::Result<Meta> {
    let mut meta = Meta::default();
    let mut stmt = conn.prepare_cached("SELECT key, value FROM meta")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (key, value) = row?;
        match key.as_str() {
            "clk_tck" => meta.clk_tck = value.parse().unwrap_or(meta.clk_tck),
            "ncpus" => meta.ncpus = value.parse().unwrap_or(meta.ncpus),
            "hostname" => meta.hostname = value,
            _ => {}
        }
    }
    Ok(meta)
}

pub fn oldest_sample(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MIN(ts) FROM sys_samples", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn insert_sys(conn: &Connection, ts: i64, dt_ms: i64, cpu: Option<f64>, rx: Option<i64>) {
        conn.execute(
            "INSERT INTO sys_samples (ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used, mem_total,
                                      swap_used, net_rx, net_tx, disk_read, disk_write)
             VALUES (?1, ?2, ?3, ?3, 1.0, 100, 1000, 0, ?4, ?4, ?4, ?4)",
            rusqlite::params![ts, dt_ms, cpu, rx],
        )
        .unwrap();
    }

    #[test]
    fn oldest_sample_is_none_without_samples() {
        let conn = db::open_rw_in_memory().unwrap();
        assert_eq!(oldest_sample(&conn).unwrap(), None);

        insert_sys(&conn, 1010, 5000, None, None);
        insert_sys(&conn, 1000, 5000, None, None);
        assert_eq!(oldest_sample(&conn).unwrap(), Some(1000));
    }

    #[test]
    fn nice_bucket_rounds_up() {
        assert_eq!(nice_bucket(3600, 1000), 10);
        assert_eq!(nice_bucket(60, 1000), 5);
        assert_eq!(nice_bucket(5 * 86400, 800), 1800);
        assert_eq!(nice_bucket(5 * 86400, 400), 3600);
        assert_eq!(nice_bucket(30 * 86400, 100), 15 * 3600);
    }

    #[test]
    fn sys_series_buckets_and_ignores_null_deltas_in_rates() {
        let conn = db::open_rw_in_memory().unwrap();
        insert_sys(&conn, 1000, 5000, None, None);
        insert_sys(&conn, 1005, 5000, Some(20.0), Some(5000));
        insert_sys(&conn, 1010, 5000, Some(40.0), Some(15000));
        insert_sys(&conn, 1020, 5000, Some(10.0), Some(1000));

        let pts = sys_series(&conn, 1000, 1030, 10).unwrap();
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].t, 1000);
        assert_eq!(pts[0].cpu_busy, Some(20.0));
        assert_eq!(pts[0].net_rx, Some(1000.0));
        assert_eq!(pts[1].t, 1010);
        assert_eq!(pts[1].net_rx, Some(3000.0));
        assert_eq!(pts[2].mem_total, Some(1000.0));
    }

    #[test]
    fn sys_series_bucket_with_only_null_deltas_has_no_rate() {
        let conn = db::open_rw_in_memory().unwrap();
        insert_sys(&conn, 1000, 3_600_000, None, None);
        let pts = sys_series(&conn, 0, 2000, 5).unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].cpu_busy, None);
        assert_eq!(pts[0].net_rx, None);
        assert_eq!(pts[0].mem_used, Some(100.0));
    }

    #[test]
    fn top_procs_sums_over_range() {
        let mut conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO proc_names (id, name) VALUES (1, 'firefox'), (2, 'cargo'), (3, 'old');
             INSERT INTO proc_minutes VALUES (60, 1, 100), (120, 1, 50), (60, 2, 300), (0, 3, 999);",
        )
        .unwrap();
        let top = top_procs(&conn, 60, 120, 10).unwrap();
        assert_eq!(
            top,
            vec![
                ProcUsage {
                    name: "cargo".into(),
                    ticks: 300
                },
                ProcUsage {
                    name: "firefox".into(),
                    ticks: 150
                },
            ]
        );

        db::trim(&mut conn, 60).unwrap();
        let names: i64 = conn
            .query_row("SELECT count(*) FROM proc_names", [], |r| r.get(0))
            .unwrap();
        assert_eq!(names, 2);
    }

    #[test]
    fn sys_series_marks_gap_buckets_and_stats_use_raw_samples() {
        let conn = db::open_rw_in_memory().unwrap();
        insert_sys(&conn, 1000, 5000, Some(0.0), Some(0));
        insert_sys(&conn, 1005, 5000, Some(100.0), Some(5000));
        insert_sys(&conn, 1010, 3_600_000, None, None);
        insert_sys(&conn, 1015, 5000, Some(0.0), Some(0));

        let pts = sys_series(&conn, 1000, 1020, 10).unwrap();
        assert!(!pts[0].gap);
        assert!(pts[1].gap);
        assert_eq!(pts[0].cpu_busy, Some(50.0));

        let st = sys_stats(&conn, 1000, 1020).unwrap();
        assert_eq!(
            st.cpu_busy,
            Some(Stats {
                avg: 100.0 / 3.0,
                max: 100.0
            })
        );
        assert_eq!(st.net_rx.unwrap().max, 1000.0);
    }

    #[test]
    fn top_procs_includes_overlapping_minutes() {
        let conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO proc_names (id, name) VALUES (1, 'a');
             INSERT INTO proc_minutes VALUES (0, 1, 1), (60, 1, 10), (120, 1, 100), (180, 1, 1000);",
        )
        .unwrap();
        assert_eq!(top_procs(&conn, 75, 135, 10).unwrap()[0].ticks, 110);
        assert_eq!(proc_span(75, 135), (60, 135));
        assert_eq!(proc_span(60, 600), (60, 600));
    }

    #[test]
    fn sensor_series_and_meta() {
        let conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_samples VALUES (100, 1, 40.0), (105, 1, 50.0), (110, 1, 60.0);",
        )
        .unwrap();
        db::set_meta(&conn, "clk_tck", "100").unwrap();
        db::set_meta(&conn, "ncpus", "32").unwrap();

        let data = sensor_series(&conn, 100, 110, 10, &[1]).unwrap();
        assert_eq!(
            data.points,
            vec![
                SensorPoint {
                    t: 100,
                    sensor_id: 1,
                    value: 45.0
                },
                SensorPoint {
                    t: 110,
                    sensor_id: 1,
                    value: 60.0
                },
            ]
        );
        assert_eq!(
            data.stats[&1],
            Stats {
                avg: 50.0,
                max: 60.0
            }
        );
        let s = sensors(&conn).unwrap();
        assert_eq!(s[0].key(), "k10temp/Tctl");
        assert_eq!(meta(&conn).unwrap().ncpus, 32);
    }

    #[test]
    fn sensor_series_reads_only_the_given_sensors() {
        let conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C'),
                                        (2, 'temp', 'k10temp', 'Tccd1', '°C');
             INSERT INTO sensor_samples VALUES (100, 1, 40.0), (100, 2, 70.0);
             INSERT INTO sensor_rollups VALUES (0, 1, 40.0, 40.0, 1), (0, 2, 70.0, 70.0, 1);",
        )
        .unwrap();

        for bucket in [10, SENSOR_ROLLUP_SECS] {
            let data = sensor_series(&conn, 0, 299, bucket, &[2]).unwrap();
            assert_eq!(data.points.len(), 1);
            assert_eq!(data.points[0].sensor_id, 2);
            assert_eq!(data.stats.keys().collect::<Vec<_>>(), [&2]);

            let none = sensor_series(&conn, 0, 299, bucket, &[]).unwrap();
            assert!(none.points.is_empty() && none.stats.is_empty());
        }
    }

    #[test]
    fn sensor_series_combines_rollups_into_larger_buckets() {
        let conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_rollups VALUES (900, 1, 100.0, 60.0, 2),
                                               (1200, 1, 60.0, 30.0, 2),
                                               (1800, 1, 80.0, 80.0, 1);",
        )
        .unwrap();

        let data = sensor_series(&conn, 900, 2099, 900, &[1]).unwrap();
        assert_eq!(
            data.points,
            vec![
                SensorPoint {
                    t: 900,
                    sensor_id: 1,
                    value: 40.0
                },
                SensorPoint {
                    t: 1800,
                    sensor_id: 1,
                    value: 80.0
                },
            ]
        );
        assert_eq!(
            data.stats[&1],
            Stats {
                avg: 48.0,
                max: 80.0
            }
        );
    }

    fn roll_up_sensor_samples(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO sensor_rollups
             SELECT (ts / 300) * 300, sensor_id, sum(value), max(value), count(*)
             FROM sensor_samples GROUP BY 1, 2;",
        )
        .unwrap();
    }

    #[test]
    fn sensor_series_reads_partial_rollups_from_raw_samples() {
        let conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_samples VALUES
               (100, 1, 10.0), (310, 1, 20.0), (580, 1, 90.0), (600, 1, 30.0), (899, 1, 50.0),
               (900, 1, 70.0), (1250, 1, 40.0), (1400, 1, 99.0);",
        )
        .unwrap();
        roll_up_sensor_samples(&conn);

        let data = sensor_series(&conn, 310, 1300, 300, &[1]).unwrap();
        let values: Vec<(i64, f64)> = data.points.iter().map(|p| (p.t, p.value)).collect();
        assert_eq!(
            values,
            [(300, 55.0), (600, 40.0), (900, 70.0), (1200, 40.0)]
        );
        assert_eq!(
            data.stats[&1],
            Stats {
                avg: 50.0,
                max: 90.0
            }
        );

        let within_one_rollup = sensor_series(&conn, 300, 320, 300, &[1]).unwrap();
        assert_eq!(
            within_one_rollup.stats[&1],
            Stats {
                avg: 20.0,
                max: 20.0
            }
        );
    }

    #[test]
    fn trim_keeps_sensor_samples_and_rollups_in_step() {
        let mut conn = db::open_rw_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_samples VALUES (590, 1, 5.0), (900, 1, 10.0), (1000, 1, 50.0);",
        )
        .unwrap();
        roll_up_sensor_samples(&conn);

        db::trim(&mut conn, 1000).unwrap();

        let raw = sensor_series(&conn, 0, 1199, 5, &[1]).unwrap();
        let rolled = sensor_series(&conn, 0, 1199, 300, &[1]).unwrap();
        assert_eq!(
            raw.stats[&1],
            Stats {
                avg: 30.0,
                max: 50.0
            }
        );
        assert_eq!(rolled.stats, raw.stats);
    }
}
