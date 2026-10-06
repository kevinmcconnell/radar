use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::schema::{self, FLUSHED_BATCH_META, LIVE_TABLES, SENSOR_ROLLUP_SECS};

pub fn default_db_path() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        });
    data_home.join("radar/radar.db")
}

/// Where the collector keeps the samples it has not yet moved into the main
/// database at `db`: a file on the user's runtime tmpfs, named after the
/// main database so that collectors on different databases never share one.
/// `None` when there is no runtime directory for this user.
pub fn live_path(db: &Path) -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let dir = PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()));
            dir.is_dir().then_some(dir)
        })?;
    Some(live_path_in(&runtime_dir, db))
}

fn live_path_in(runtime_dir: &Path, db: &Path) -> PathBuf {
    let db = std::path::absolute(db).unwrap_or_else(|_| db.to_path_buf());
    let name = fnv1a(db.as_os_str().as_encoded_bytes());
    runtime_dir.join(format!("radar/live-{name:016x}.db"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The key under which the collector records its live database path in
/// `meta`, so that readers find the file wherever the collector put it.
pub const LIVE_DB_META: &str = "live_db";

pub fn open_rw(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut conn = Connection::open(path)?;
    configure_rw(&mut conn)?;
    Ok(conn)
}

pub fn open_rw_in_memory() -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure_rw(&mut conn)?;
    Ok(conn)
}

fn configure_rw(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::migrate(conn)
}

/// Attaches the live database as `live` and creates its tables. Without a
/// path, the live database is in memory.
pub fn attach_live(conn: &Connection, path: Option<&Path>) -> rusqlite::Result<()> {
    match path {
        Some(path) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            conn.execute("ATTACH DATABASE ?1 AS live", [path.to_string_lossy()])?;
            conn.pragma_update(Some("live"), "journal_mode", "WAL")?;
            conn.pragma_update(Some("live"), "synchronous", "OFF")?;
        }
        None => conn.execute_batch("ATTACH DATABASE ':memory:' AS live")?,
    }
    schema::create_live(conn)
}

/// Attaches the live database read-only and shadows each sample table with
/// a temp view over both databases, so unqualified queries see every sample,
/// including rows staged on their way from live into main that main does
/// not hold yet.
pub fn attach_live_views(conn: &Connection, path: &Path) -> rusqlite::Result<()> {
    conn.execute("ATTACH DATABASE ?1 AS live", [path.to_string_lossy()])?;
    for (table, columns) in LIVE_TABLES {
        conn.execute_batch(&format!(
            "CREATE TEMP VIEW {table} AS
               SELECT {columns} FROM main.{table}
               UNION ALL SELECT {columns} FROM live.{table}
               UNION ALL SELECT {columns} FROM live.staged_{table}
                 WHERE batch > (SELECT coalesce(max(CAST(value AS INTEGER)), 0)
                                FROM main.meta WHERE key = '{FLUSHED_BATCH_META}')"
        ))?;
    }
    Ok(())
}

pub fn open_ro(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute((key, value))?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |r| r.get(0))
        .optional()
}

pub fn remove_meta(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(())
}

/// Sensor samples are kept back to the start of their rollup, so the two tables stay in step.
/// Names still used by rows waiting in `live` are kept, so `live` must be attached.
pub fn trim(conn: &mut Connection, cutoff: i64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM sys_samples WHERE ts < ?1", [cutoff])?;
    let rollup_cutoff = cutoff.div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS;
    tx.execute("DELETE FROM sensor_samples WHERE ts < ?1", [rollup_cutoff])?;
    tx.execute("DELETE FROM sensor_rollups WHERE ts < ?1", [rollup_cutoff])?;
    tx.execute("DELETE FROM proc_minutes WHERE ts < ?1", [cutoff])?;
    tx.execute(
        "DELETE FROM proc_names WHERE id NOT IN (
           SELECT name_id FROM main.proc_minutes UNION SELECT name_id FROM live.proc_minutes)",
        [],
    )?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_path_depends_on_the_database_path() {
        let runtime_dir = Path::new("/run/user/1000");
        let a = live_path_in(runtime_dir, Path::new("/data/a.db"));
        let b = live_path_in(runtime_dir, Path::new("/data/b.db"));
        assert!(a.starts_with("/run/user/1000/radar"));
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("live-")
        );
        assert_ne!(a, b);
        assert_eq!(a, live_path_in(runtime_dir, Path::new("/data/a.db")));
    }
}
