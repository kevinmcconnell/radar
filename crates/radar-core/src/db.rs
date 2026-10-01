use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::schema;

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

pub fn trim(conn: &mut Connection, cutoff: i64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM sys_samples WHERE ts < ?1", [cutoff])?;
    tx.execute("DELETE FROM sensor_samples WHERE ts < ?1", [cutoff])?;
    tx.execute("DELETE FROM proc_minutes WHERE ts < ?1", [cutoff])?;
    tx.execute(
        "DELETE FROM proc_names WHERE id NOT IN (SELECT DISTINCT name_id FROM proc_minutes)",
        [],
    )?;
    tx.commit()
}
