//! Everything the viewer shows for one time range, read from a database in one
//! go. `serve` answers queries for snapshots as JSON lines, which is how the
//! viewer reads another machine's database over ssh.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{Meta, ProcUsage, Sensor, SensorPoint, Stats, SysPoint, SysStats, db, query, schema};

const START_HINT: &str =
    "Start the collector with\n<tt>systemctl --user enable --now radar-collect</tt>";
const RESTART_HINT: &str =
    "Restart the collector with\n<tt>systemctl --user restart radar-collect</tt>";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
    pub top_n: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
    pub sys: Vec<SysPoint>,
    pub sensors: Vec<Sensor>,
    pub sensor_points: Vec<SensorPoint>,
    pub sys_stats: SysStats,
    pub sensor_stats: HashMap<i64, Stats>,
    pub procs: Vec<ProcUsage>,
    pub meta: Meta,
    pub oldest: Option<i64>,
}

/// A snapshot, or the problem for the viewer to show instead.
pub type Reply = Result<Snapshot, Problem>;

/// Why there is nothing to show: a short title, and details in Pango markup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub title: String,
    pub details: String,
}

impl Problem {
    pub fn new(title: impl Into<String>, details: impl Into<String>) -> Self {
        Problem {
            title: title.into(),
            details: details.into(),
        }
    }

    fn unreadable(e: impl std::fmt::Display) -> Self {
        Problem::new("Cannot Read Data", format!("{e}\n\n{START_HINT}"))
    }
}

/// Reads snapshots from the database at `path`, opening it on first use and
/// again after any error, so a collector started later is picked up. The
/// collector's live database, found through `meta`, is read alongside it
/// once that file exists.
pub struct Reader {
    path: PathBuf,
    live: Option<PathBuf>,
    live_from_meta: bool,
    conn: Option<Connection>,
    live_attached: bool,
}

impl Reader {
    pub fn new(path: PathBuf) -> Self {
        Reader {
            path,
            live: None,
            live_from_meta: true,
            conn: None,
            live_attached: false,
        }
    }

    /// Reads the live database at `live` rather than the one `meta` names.
    pub fn with_live(path: PathBuf, live: Option<PathBuf>) -> Self {
        Reader {
            live,
            live_from_meta: false,
            ..Reader::new(path)
        }
    }

    pub fn snapshot(&mut self, q: &Query) -> Reply {
        let reply = self.read(q);
        if reply.is_err() {
            self.conn = None;
        }
        reply
    }

    fn read(&mut self, q: &Query) -> Reply {
        if self.conn.is_none() {
            self.conn = Some(self.open()?);
            self.live_attached = false;
        }
        let conn = self.conn.as_ref().unwrap();
        if !self.live_attached {
            if self.live_from_meta {
                self.live = db::get_meta(conn, db::LIVE_DB_META)
                    .map_err(Problem::unreadable)?
                    .map(PathBuf::from);
            }
            if let Some(live) = self.live.as_deref()
                && live.exists()
            {
                db::attach_live_views(conn, live).map_err(Problem::unreadable)?;
                self.live_attached = true;
            }
        }
        read_snapshot(conn, q).map_err(Problem::unreadable)
    }

    fn open(&self) -> Result<Connection, Problem> {
        if !self.path.exists() {
            return Err(Problem::new(
                "No Data Yet",
                format!("No database at {}\n\n{START_HINT}", self.path.display()),
            ));
        }
        let conn = db::open_ro(&self.path).map_err(Problem::unreadable)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(Problem::unreadable)?;
        if version == schema::VERSION {
            Ok(conn)
        } else {
            Err(Problem::new(
                "Collector Needs a Restart",
                format!(
                    "The database has schema version {version}, but this viewer needs version {}.\n\n{RESTART_HINT}",
                    schema::VERSION
                ),
            ))
        }
    }
}

/// One read transaction, so every part of the snapshot sees the same moment
/// in both databases.
fn read_snapshot(conn: &Connection, q: &Query) -> rusqlite::Result<Snapshot> {
    let tx = conn.unchecked_transaction()?;
    let snapshot = read_parts(&tx, q)?;
    tx.commit()?;
    Ok(snapshot)
}

fn read_parts(conn: &Connection, q: &Query) -> rusqlite::Result<Snapshot> {
    let sensor_data = query::sensor_series(conn, q.from, q.to, q.bucket)?;
    Ok(Snapshot {
        from: q.from,
        to: q.to,
        bucket: q.bucket,
        sys: query::sys_series(conn, q.from, q.to, q.bucket)?,
        sensors: query::sensors(conn)?,
        sensor_points: sensor_data.points,
        sys_stats: query::sys_stats(conn, q.from, q.to)?,
        sensor_stats: sensor_data.stats,
        procs: query::top_procs(conn, q.from, q.to, q.top_n)?,
        meta: query::meta(conn)?,
        oldest: query::oldest_sample(conn)?,
    })
}

/// Answer each query line from `input` with a reply line on `output`, until
/// `input` ends.
pub fn serve(
    reader: &mut Reader,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let reply = match serde_json::from_str::<Query>(&line?) {
            Ok(q) => reader.snapshot(&q),
            Err(e) => Err(Problem::new(
                "Versions Don't Match",
                format!(
                    "The remote collector cannot read the query: {e}\n\nInstall the same version of Radar on both machines."
                ),
            )),
        };
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("radar-snapshot-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn seeded_db(name: &str) -> PathBuf {
        let path = temp_path(name);
        let conn = db::open_rw(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO sys_samples (ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used, mem_total,
                                      swap_used, net_rx, net_tx, disk_read, disk_write)
             VALUES (1000, 5000, 25.0, 1.0, 1.0, 100, 1000, 0, 500, 500, 500, 500);
             INSERT INTO sensors (id, kind, chip, label, unit) VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_samples VALUES (1000, 1, 55.0);
             INSERT INTO sensor_rollups VALUES (900, 1, 55.0, 55.0, 1);
             INSERT INTO proc_names (id, name) VALUES (1, 'cargo');
             INSERT INTO proc_minutes VALUES (960, 1, 300);
             INSERT INTO meta VALUES ('hostname', 'workstation');",
        )
        .unwrap();
        path
    }

    /// The rest of the samples that `seeded_db` starts, into the tables of
    /// database `schema`. In main they land in the rows already there; in live
    /// they are separate rows that the views add together.
    fn add_later_samples(conn: &Connection, schema: &str) {
        conn.execute_batch(&format!(
            "INSERT INTO {schema}.sys_samples (ts, dt_ms, cpu_busy, cpu_iowait, load1, mem_used,
                                      mem_total, swap_used, net_rx, net_tx, disk_read, disk_write)
             VALUES (1005, 5000, 75.0, 1.0, 1.0, 300, 1000, 0, 500, 500, 500, 500);
             INSERT INTO {schema}.sensor_samples VALUES (1005, 1, 65.0);
             INSERT INTO {schema}.sensor_rollups VALUES (900, 1, 65.0, 65.0, 1)
               ON CONFLICT DO UPDATE SET total = total + excluded.total,
                 peak = max(peak, excluded.peak), samples = samples + excluded.samples;
             INSERT INTO {schema}.proc_minutes VALUES (960, 1, 200), (1020, 1, 100)
               ON CONFLICT DO UPDATE SET cpu_ticks = cpu_ticks + excluded.cpu_ticks;"
        ))
        .unwrap();
    }

    const QUERY: Query = Query {
        from: 900,
        to: 1100,
        bucket: 10,
        top_n: 10,
    };

    #[test]
    fn serve_answers_each_query_with_the_snapshot_reader_returns() {
        let path = seeded_db("serve");
        let expected = Reader::new(path.clone()).snapshot(&QUERY).unwrap();
        assert_eq!(expected.meta.hostname, "workstation");
        assert_eq!(expected.sensors[0].label, "Tctl");
        assert_eq!(expected.procs[0].name, "cargo");
        assert_eq!(expected.sensor_stats.len(), 1);

        let input = format!(
            "{0}\nnot a query\n{0}\n",
            serde_json::to_string(&QUERY).unwrap()
        );
        let mut output = Vec::new();
        serve(
            &mut Reader::new(path.clone()),
            input.as_bytes(),
            &mut output,
        )
        .unwrap();

        let replies: Vec<Reply> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0], Ok(expected.clone()));
        let problem = replies[1].as_ref().unwrap_err();
        assert_eq!(problem.title, "Versions Don't Match");
        assert!(
            problem
                .details
                .starts_with("The remote collector cannot read the query")
        );
        assert_eq!(replies[2], Ok(expected));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn samples_in_the_live_database_are_read_with_the_rest() {
        let all_in_main = seeded_db("all-main");
        add_later_samples(&db::open_rw(&all_in_main).unwrap(), "main");
        let expected = Reader::with_live(all_in_main.clone(), None)
            .snapshot(&QUERY)
            .unwrap();
        assert_eq!(expected.procs[0].ticks, 600);
        assert_eq!(expected.sys_stats.cpu_busy.unwrap().avg, 50.0);

        let main = seeded_db("split-main");
        let live = temp_path("split-live");
        let mut reader = Reader::new(main.clone());
        let before = reader.snapshot(&QUERY).unwrap();
        assert_eq!(before.procs[0].ticks, 300);

        let conn = db::open_rw(&main).unwrap();
        db::attach_live(&conn, Some(&live)).unwrap();
        db::set_meta(&conn, db::LIVE_DB_META, live.to_str().unwrap()).unwrap();
        add_later_samples(&conn, "live");
        let split = reader.snapshot(&QUERY).unwrap();
        assert_eq!(split, expected);
        for bucket in [5, 300] {
            let q = Query { bucket, ..QUERY };
            assert_eq!(
                reader.snapshot(&q).unwrap(),
                Reader::with_live(all_in_main.clone(), None)
                    .snapshot(&q)
                    .unwrap()
            );
        }

        // rows staged on their way into main count once: only the batch main
        // has not merged yet shows
        conn.execute_batch(
            "DELETE FROM live.proc_minutes;
             INSERT INTO live.staged_proc_minutes VALUES (960, 1, 200, 1), (1020, 1, 100, 1);
             INSERT INTO live.staged_proc_minutes VALUES (1020, 1, 500, 2);",
        )
        .unwrap();
        db::set_meta(&conn, schema::FLUSHED_BATCH_META, "1").unwrap();
        assert_eq!(reader.snapshot(&QUERY).unwrap().procs[0].ticks, 800);
        for path in [all_in_main, main, live] {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn missing_database_is_reported_until_it_appears() {
        let path =
            std::env::temp_dir().join(format!("radar-snapshot-missing-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut reader = Reader::new(path.clone());
        let problem = reader.snapshot(&QUERY).unwrap_err();
        assert_eq!(problem.title, "No Data Yet");
        assert!(problem.details.starts_with("No database at"));

        db::open_rw(&path).unwrap();
        assert_eq!(reader.snapshot(&QUERY).unwrap().oldest, None);
        let _ = std::fs::remove_file(&path);
    }
}
