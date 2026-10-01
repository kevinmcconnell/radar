use std::path::{Path, PathBuf};
use std::sync::mpsc;

use std::collections::HashMap;

use radar_core::{Meta, ProcUsage, Sensor, SensorPoint, Stats, SysPoint, SysStats, db, query};
use rusqlite::Connection;

pub struct Request {
    pub generation: u64,
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
    pub top_n: i64,
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
            return Err(format!("No database at {}", path.display()));
        }
        *conn = Some(db::open_ro(path).map_err(|e| e.to_string())?);
    }
    let c = conn.as_ref().unwrap();
    let q = || -> rusqlite::Result<Snapshot> {
        Ok(Snapshot {
            generation: req.generation,
            from: req.from,
            to: req.to,
            bucket: req.bucket,
            sys: query::sys_series(c, req.from, req.to, req.bucket)?,
            sensors: query::sensors(c)?,
            sensor_points: query::sensor_series(c, req.from, req.to, req.bucket)?,
            sys_stats: query::sys_stats(c, req.from, req.to)?,
            sensor_stats: query::sensor_stats(c, req.from, req.to)?,
            procs: query::top_procs(c, req.from, req.to, req.top_n)?,
            meta: query::meta(c)?,
        })
    };
    q().map_err(|e| e.to_string())
}
