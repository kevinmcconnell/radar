use std::path::{Path, PathBuf};
use std::sync::mpsc;

use std::collections::HashMap;

use radar_core::{
    Meta, ProcUsage, Sensor, SensorPoint, Stats, SysPoint, SysStats, db, query, schema,
};
use rusqlite::Connection;

use crate::config::Config;

const START_HINT: &str =
    "Start the collector with\n<tt>systemctl --user enable --now radar-collect</tt>";
const RESTART_HINT: &str =
    "Restart the collector with\n<tt>systemctl --user restart radar-collect</tt>";

pub struct Request {
    pub generation: u64,
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
    pub top_n: i64,
    pub config: Config,
}

pub struct Snapshot {
    pub generation: u64,
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

pub type Reply = Result<Snapshot, (u64, String)>;

pub fn spawn(db_path: PathBuf) -> (mpsc::Sender<Request>, async_channel::Receiver<Reply>) {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (reply_tx, reply_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("radar-query".into())
        .spawn(move || {
            let mut conn: Option<Connection> = None;
            while let Ok(mut req) = req_rx.recv() {
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let reply = run(&db_path, &mut conn, &req).map_err(|e| {
                    conn = None;
                    (req.generation, e)
                });
                if reply_tx.send_blocking(reply).is_err() {
                    break;
                }
            }
        })
        .expect("spawn query thread");
    (req_tx, reply_rx)
}

fn run(path: &Path, conn: &mut Option<Connection>, req: &Request) -> Result<Snapshot, String> {
    if conn.is_none() {
        if !path.exists() {
            return Err(format!("No database at {}\n\n{START_HINT}", path.display()));
        }
        let opened = db::open_ro(path).map_err(|e| format!("{e}\n\n{START_HINT}"))?;
        let version: i64 = opened
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("{e}\n\n{START_HINT}"))?;
        if version != schema::VERSION {
            return Err(format!(
                "The database has schema version {version}, but this viewer needs version {}.\n\n{RESTART_HINT}",
                schema::VERSION
            ));
        }
        *conn = Some(opened);
    }
    let c = conn.as_ref().unwrap();
    let q = || -> rusqlite::Result<Snapshot> {
        let sensors = query::sensors(c)?;
        let visible: Vec<i64> = sensors
            .iter()
            .filter(|s| req.config.visible(s))
            .map(|s| s.id)
            .collect();
        let sensor_data = query::sensor_series(c, req.from, req.to, req.bucket, &visible)?;
        Ok(Snapshot {
            generation: req.generation,
            from: req.from,
            to: req.to,
            bucket: req.bucket,
            sys: query::sys_series(c, req.from, req.to, req.bucket)?,
            sensors,
            sensor_points: sensor_data.points,
            sys_stats: query::sys_stats(c, req.from, req.to)?,
            sensor_stats: sensor_data.stats,
            procs: query::top_procs(c, req.from, req.to, req.top_n)?,
            meta: query::meta(c)?,
            oldest: query::oldest_sample(c)?,
        })
    };
    q().map_err(|e| format!("{e}\n\n{START_HINT}"))
}
