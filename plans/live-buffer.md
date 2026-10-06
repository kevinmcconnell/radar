# Buffer recent samples in a tmpfs database

## Why

`radar-collect` commits a transaction every 5 seconds. Each commit appends 4–6 full 4 KB pages to the WAL: the tail pages of `sys_samples`, `sensor_samples` and `sensor_rollups`, plus the current minute's `proc_minutes` page(s). That adds up to roughly 300–400 MB of disk writes a day for a few MB of actual data. The `proc_minutes` and `sensor_rollups` upserts rewrite the same rows 12 and 60 times over.

WAL settings can't fix this. `synchronous=NORMAL` is already set, and checkpoints are already small because the same tail pages keep being rewritten. The volume comes from the per-commit WAL appends.

## Approach

Write each sample to a second SQLite database on tmpfs (`$XDG_RUNTIME_DIR/radar/live.db`). Every 5 minutes, move completed rows into `radar.db` in one transaction. The viewer reads both databases through temp views, so the UI still sees every sample with no added lag, and the existing queries don't change.

Accepted trade-off: a power loss or kernel crash can lose up to 5 minutes of data, because tmpfs is RAM. A clean stop (SIGTERM/SIGINT, so also shutdown and logout) loses nothing. A killed collector loses nothing either: the live file outlives the process, including rows staged mid-flush, and the next start flushes it first.

## Live database

- Path: `db::live_path(db) -> Option<PathBuf>` in `radar-core`, returning `$XDG_RUNTIME_DIR/radar/live-<hash>.db`, where the hash is of the main database's absolute path, so collectors on different `--db` paths never share a live file. If `XDG_RUNTIME_DIR` is unset or empty, fall back to `/run/user/<uid>` when that directory exists, else `None`.
- The collector records the live path in `meta` under `live_db` (and removes the key when it runs without one). Readers find the file there rather than computing it, so the viewer and `--serve` need no runtime directory of their own and always read the right file for the database they open.
- Holds `sys_samples`, `sensor_samples`, `sensor_rollups` and `proc_minutes`, with the same columns and primary keys as `radar.db` but **no `REFERENCES` clauses**, because foreign keys can't cross databases. Beside each is a `staged_<table>` with the same columns plus `batch INTEGER`, holding rows on their way into `radar.db`.
- Create them with `CREATE TABLE IF NOT EXISTS` from a `radar-core` function (`schema::create_live(conn, "live")`). The file is ephemeral, so it needs no migrations. If a future schema change adds columns, the collector flushes and then drops and recreates the live tables at startup. Not needed now; note it in a comment.
- Journal mode `WAL` on the live db too, so the viewer can read while the collector writes.
- `sensors`, `proc_names` and `meta` stay in `radar.db` only, so IDs are shared.

## Collector changes

### Opening

In `run()`, after `db::open_rw(&args.db)`:

- If `live_path()` is `Some`, create the directory and `ATTACH DATABASE ?1 AS live`. Otherwise `ATTACH ':memory:' AS live` and use flush-every-sample mode (below). This keeps a single code path.
- `PRAGMA live.journal_mode=WAL`, then `schema::create_live`.
- Set `PRAGMA live.synchronous=OFF` (tmpfs). `schema::create_live` also creates a `staged_<table>` beside each sample table, with the same columns plus `batch INTEGER`. All of this lives in `db::attach_live(conn, Option<&Path>)`, which tests reuse.
- **Before the first sample, run a full flush** (see below). This recovers rows left in `live.db` if the collector previously crashed without a reboot.

### Writing

In `writer.rs`, qualify the four sample tables as `live.` in every `INSERT` (`live.sys_samples`, etc.). Unqualified names would resolve to `main` first. The `sys_samples` insert also skips a timestamp that `main` or the staged rows already hold, so a repeated timestamp (a clock rollback) cannot count its rollups and ticks twice. Build the SQL strings once (constants are fine, since the schema name is always `live`), so the hot path still avoids allocations. The `proc_names` lookup and insert stay on `main`.

Nothing else in `write_sample` changes: the per-sample transaction and the upserts into `live.proc_minutes` and `live.sensor_rollups` work as they do now, but on tmpfs.

### Flushing

Add `Writer::flush(&mut self, before: i64) -> rusqlite::Result<()>`. It moves every live row with `ts < before` into `main`, in **two separate transactions**, because WAL mode doesn't make a transaction across attached databases atomic. Rows are staged inside the live file rather than in temp tables, so nothing depends on the process surviving.

1. Transaction 1 (live only, so atomic): read the last merged batch number from `main.meta` (`flushed_batch`, default 0) and discard any staged rows with that batch or lower, which a crash left behind after they were merged. Then for each of the four tables, `INSERT INTO live.staged_<table> SELECT ..., <batch> FROM live.<table> WHERE ts < ?1` and `DELETE FROM live.<table> WHERE ts < ?1`, with batch = merged + 1. Commit.
2. Transaction 2: merge from the staged tables into `main`, record the batch number in `main.meta`, and clear the staged tables, all in one transaction:
   - `sys_samples`, `sensor_samples`: `INSERT OR IGNORE` (one row per timestamp, so they never collide). Sensor rows are filtered to sensors still in `main.sensors`, because `OR IGNORE` does not ignore a foreign-key failure.
   - `sensor_rollups`:
     ```sql
     INSERT INTO main.sensor_rollups SELECT ts, sensor_id, total, peak, samples
     FROM live.staged_sensor_rollups WHERE sensor_id IN (SELECT id FROM main.sensors)
     ON CONFLICT (ts, sensor_id) DO UPDATE SET
       total = total + excluded.total,
       peak = max(peak, excluded.peak),
       samples = samples + excluded.samples;
     ```
   - `proc_minutes`: same pattern, `cpu_ticks = cpu_ticks + excluded.cpu_ticks`, with `WHERE name_id IN (SELECT id FROM main.proc_names)` so a name lost in a crash drops that row instead of failing the whole merge on the foreign key.

   `main` commits before `live` within that transaction. If the process dies between the two, the staged rows survive with a batch number the meta row already covers, and the next flush discards them. If it dies before, the meta row is older than the batch, and the next flush merges them. Either way nothing is lost or counted twice.

The viewer's views include staged rows that main has not merged yet, so rows stay visible throughout a flush and never show twice.

The merge upserts are what make partial flushes safe. After a SIGTERM flush and restart, a 5-minute rollup bucket or a minute's process rows can be split between `main` and `live`. The viewer's `UNION ALL` views add the parts together correctly, because every query already aggregates with `sum`/`max`/`GROUP BY`. When the rest of the bucket is flushed later, the upsert merges it rather than hitting a primary-key conflict.

### When to flush

In the main loop, after writing a sample:

- **Normal mode:** when `ts` crosses a `SENSOR_ROLLUP_SECS` boundary, call `flush(ts.div_euclid(SENSOR_ROLLUP_SECS) * SENSOR_ROLLUP_SECS)`. That moves only completed rollup buckets and completed minutes.
- **Fallback mode (no `XDG_RUNTIME_DIR`):** `flush(i64::MAX)` after every sample. This behaves like the current code, just via the live table.
- **On exit:** after the `while !sys::stopping()` loop, call `flush(i64::MAX)`. The existing signal handling already ends the loop cleanly on SIGTERM/SIGINT.
- Log flush errors with `eprintln!` like other errors and keep running. Rows that failed to stage stay in live; rows that failed to merge stay staged. Both go out with the next flush, even after a restart.

### Trimming

- Run trim only straight after a flush that succeeded, so it doesn't race rows still sitting in live or staged. Keep the hourly cadence: check `next_trim` right after a flush.
- **Fix the `proc_names` cleanup** in `db::trim`. It currently deletes names not referenced by `main.proc_minutes`, which would delete names still in use by unflushed `live.proc_minutes` rows. Change it to:
  ```sql
  DELETE FROM proc_names WHERE id NOT IN (
    SELECT name_id FROM main.proc_minutes
    UNION SELECT name_id FROM live.proc_minutes)
  ```
  `trim` only runs in the collector, where `live` is always attached.
- Live rows never need trimming; they're always recent.

## Viewer changes

The viewer reads via `snapshot::Reader` (`radar-core/src/snapshot.rs`), which opens `db::open_ro`. Add live attachment there:

- `Reader` looks up the live path in `meta` (`live_db`) on each read until it finds and attaches one.
- At the start of each `read`, if it isn't attached yet and the live file exists, `ATTACH DATABASE ?1 AS live` (it inherits read-only from the main connection), then create the views:
  ```sql
  CREATE TEMP VIEW sys_samples AS
    SELECT ... FROM main.sys_samples
    UNION ALL SELECT ... FROM live.sys_samples
    UNION ALL SELECT ... FROM live.staged_sys_samples
      WHERE batch > (SELECT coalesce(max(CAST(value AS INTEGER)), 0)
                     FROM main.meta WHERE key = 'flushed_batch');
  ```
  The staged branch only shows a batch that main has not merged yet, so a crash after main's commit but before live's cannot show those rows twice.
  Do the same for `sensor_samples`, `sensor_rollups` and `proc_minutes`. SQLite resolves unqualified names in `temp` before `main`, so every query in `query.rs` picks up both databases unchanged.
- If the live file doesn't exist (collector not running, or not started since boot), skip it and read `main` only. Retry the attach on the next read.
- If a read fails, the existing behaviour of dropping and reopening the connection also covers a recreated `live.db` (for example after a reboot).
- Read the whole snapshot inside one read transaction, so every query sees the same moment in both databases. That also shrinks the window during a flush, when rows are staged out of live but not yet in main, to the first query's few microseconds.
- Change `query::oldest_sample` to `SELECT coalesce((SELECT min(ts) FROM main.sys_samples), (SELECT min(ts) FROM sys_samples))`. `min` over a `UNION ALL` view scans every row; main always holds the oldest sample once anything has been flushed, because trim only runs after a flush.

## Leave alone

- `demo.rs` seeds through `Writer` with an in-memory live database, flushing once a day and at the end.
- `once` mode doesn't touch the database.

## Tests

- **Writer round trip:** write samples into a temp-dir main db with a temp-dir live db, flush at a boundary, and check that rows below the boundary moved and the rest stayed.
- **Split bucket:** write half a rollup bucket and half a minute of procs, `flush(i64::MAX)`, write the rest, flush again. Check that `main` holds exactly one row per key, with totals, peak, samples and ticks equal to a single uninterrupted run.
- **Viewer views:** with rows split across `main` and `live`, `sensor_series`, `sys_series` and `top_procs` return the same results as with everything in `main`.
- **Trim:** a name used only in `live.proc_minutes` survives `trim`.
- **Startup recovery:** leftover rows in an existing `live.db` are merged into `main` on start.

## Verify on the real machine

Before and after, run for an hour and compare `write_bytes` from `/proc/$(pidof radar-collect)/io`. Expect more than a 10× reduction. Also check that the viewer shows the newest 5-second sample immediately, and that `systemctl --user restart radar-collect` mid-bucket leaves no gap or doubled values in the charts.
