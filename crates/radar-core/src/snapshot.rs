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

/// A snapshot, or a message for the viewer to show instead. The message is
/// Pango markup.
pub type Reply = Result<Snapshot, String>;

/// Reads snapshots from the database at `path`, opening it on first use and
/// again after any error, so a collector started later is picked up.
pub struct Reader {
    path: PathBuf,
    conn: Option<Connection>,
}

impl Reader {
    pub fn new(path: PathBuf) -> Self {
        Reader { path, conn: None }
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
        }
        let conn = self.conn.as_ref().unwrap();
        read_snapshot(conn, q).map_err(|e| format!("{e}\n\n{START_HINT}"))
    }

    fn open(&self) -> Result<Connection, String> {
        if !self.path.exists() {
            return Err(format!(
                "No database at {}\n\n{START_HINT}",
                self.path.display()
            ));
        }
        let conn = db::open_ro(&self.path).map_err(|e| format!("{e}\n\n{START_HINT}"))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("{e}\n\n{START_HINT}"))?;
        if version == schema::VERSION {
            Ok(conn)
        } else {
            Err(format!(
                "The database has schema version {version}, but this viewer needs version {}.\n\n{RESTART_HINT}",
                schema::VERSION
            ))
        }
    }
}

fn read_snapshot(conn: &Connection, q: &Query) -> rusqlite::Result<Snapshot> {
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
            Err(e) => Err(format!(
                "The remote collector cannot read the query: {e}\n\nInstall the same version of Radar on both machines."
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

    fn seeded_db(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("radar-snapshot-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
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
        assert!(
            replies[1]
                .as_ref()
                .unwrap_err()
                .starts_with("The remote collector cannot read the query")
        );
        assert_eq!(replies[2], Ok(expected));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_database_is_reported_until_it_appears() {
        let path =
            std::env::temp_dir().join(format!("radar-snapshot-missing-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut reader = Reader::new(path.clone());
        assert!(
            reader
                .snapshot(&QUERY)
                .unwrap_err()
                .starts_with("No database at")
        );

        db::open_rw(&path).unwrap();
        assert_eq!(reader.snapshot(&QUERY).unwrap().oldest, None);
        let _ = std::fs::remove_file(&path);
    }
}
