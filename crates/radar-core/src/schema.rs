use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE sys_samples (
  ts          INTEGER PRIMARY KEY,
  dt_ms       INTEGER NOT NULL,
  cpu_busy    REAL,
  cpu_iowait  REAL,
  load1       REAL NOT NULL,
  mem_used    INTEGER NOT NULL,
  mem_total   INTEGER NOT NULL,
  swap_used   INTEGER NOT NULL,
  net_rx      INTEGER,
  net_tx      INTEGER,
  disk_read   INTEGER,
  disk_write  INTEGER
) STRICT;

CREATE TABLE sensors (
  id     INTEGER PRIMARY KEY,
  kind   TEXT NOT NULL,
  chip   TEXT NOT NULL,
  label  TEXT NOT NULL,
  unit   TEXT NOT NULL,
  UNIQUE (kind, chip, label)
) STRICT;

CREATE TABLE sensor_samples (
  ts         INTEGER NOT NULL,
  sensor_id  INTEGER NOT NULL REFERENCES sensors(id),
  value      REAL NOT NULL,
  PRIMARY KEY (ts, sensor_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE proc_names (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE proc_minutes (
  ts         INTEGER NOT NULL,
  name_id    INTEGER NOT NULL REFERENCES proc_names(id),
  cpu_ticks  INTEGER NOT NULL,
  PRIMARY KEY (ts, name_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE meta (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
) STRICT;
"#,
    r#"
CREATE TABLE sensor_rollups (
  ts         INTEGER NOT NULL,
  sensor_id  INTEGER NOT NULL REFERENCES sensors(id),
  total      REAL NOT NULL,
  peak       REAL NOT NULL,
  samples    INTEGER NOT NULL,
  PRIMARY KEY (ts, sensor_id)
) STRICT, WITHOUT ROWID;

INSERT INTO sensor_rollups
SELECT (ts / 300) * 300, sensor_id, sum(value), max(value), count(*)
FROM sensor_samples
GROUP BY 1, 2;
"#,
];

pub const SENSOR_ROLLUP_SECS: i64 = 300;

pub const VERSION: i64 = MIGRATIONS.len() as i64;

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_db_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, VERSION);
    }

    #[test]
    fn rollup_migration_backfills_existing_samples() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO sensors VALUES (1, 'temp', 'k10temp', 'Tctl', '°C');
             INSERT INTO sensor_samples VALUES (295, 1, 30.0), (300, 1, 40.0), (305, 1, 60.0);",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let rollups: Vec<(i64, f64, f64, i64)> = conn
            .prepare("SELECT ts, total, peak, samples FROM sensor_rollups ORDER BY ts")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rollups, [(0, 30.0, 30.0, 1), (300, 100.0, 60.0, 2)]);
        assert_eq!(SENSOR_ROLLUP_SECS, 300);
    }
}
