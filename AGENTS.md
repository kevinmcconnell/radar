# Working on Radar

Radar records what a single-user Linux desktop has been doing and charts it.
Read the README for what it does and how to build it. This file is for
anyone changing the code.

## Architecture

Three crates in `crates/`, plus `omarchy-theme` for following the desktop
theme:

- **`radar-core`** is the library both binaries share: the SQLite schema and
  migrations (`schema.rs`), opening and trimming databases (`db.rs`), the
  queries behind every chart (`query.rs`), and `snapshot.rs`, which reads
  everything the viewer shows for one time range in one go.
- **`radar-collect`** is the collector. Every five seconds `sampler.rs` reads
  `/proc` and `/sys` (and the local logs of AI coding agents, in `agents/`)
  and `writer.rs` stores the sample. `discover.rs` finds sensors, interfaces
  and disks; `parse.rs` holds the pure parsers for the files read. With
  `--serve` it answers the viewer's queries on stdin and stdout instead, which
  is how the viewer reads another machine over ssh.
- **`radar`** is the GTK 4 / libadwaita viewer. `worker.rs` runs queries on
  a thread, locally through `snapshot::Reader` or remotely through
  `ssh ... radar-collect --serve`. `window.rs` builds the UI from the
  snapshot it gets back.

### Storage

The main database is `~/.local/share/radar/radar.db`, in WAL mode. Raw
samples (`sys_samples`, `sensor_samples`) sit beside two aggregates the
collector keeps as it writes: `sensor_rollups` per five-minute bucket, and
`proc_minutes` of CPU ticks per process name per minute. `sensors` and
`proc_names` give the IDs those rows refer to. Rows older than the retention
window are trimmed hourly.

To keep the disk quiet, samples are not written to `radar.db` first. The
collector attaches a second database as `live`, a file on tmpfs under
`$XDG_RUNTIME_DIR/radar/`, and writes every sample there. At each
five-minute boundary `Writer::flush` moves the finished buckets and minutes
into `radar.db` in bulk: first into `staged_` tables in the live file, tagged
with a batch number, then into main, recording that batch number in `meta`
in the same transaction. A clean stop flushes everything; a killed collector
finishes or discards whatever batch it left behind when it next starts, so
only a power loss or kernel crash loses samples. The flush adds moved rows
into any aggregate row already in main, so a bucket split by a restart still
adds up.

The viewer never needs to know. `snapshot::Reader` attaches the live file
(found through the `live_db` row in `meta`) and shadows the four sample
tables with temp views over main, live and the not yet merged staged rows,
so the queries in `query.rs` stay unqualified. The collector's own inserts, by contrast, must name
`live.` explicitly, and `db::trim` must consider rows still in `live`.

Keep these rules when touching storage:

- Every query must aggregate with `sum`, `max` or `GROUP BY`, never assume one
  row per key. A bucket or minute can be split between main and live.
- Foreign keys cannot cross databases, so `sensors` and `proc_names` live in
  main only, and the flush drops rows whose parent is gone rather than failing.
- Schema changes to the sample tables need both a migration in `schema.rs`
  and the matching change to the live table definitions there.
- The plan this came from, with the reasoning and rejected alternatives, is
  in `plans/live-buffer.md`.

## Development

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` must all pass; CI runs exactly those.
- Tests live beside the code in `#[cfg(test)]` modules. Database tests use
  `db::open_rw_in_memory()` plus `db::attach_live(&conn, None)`, or a file
  under `std::env::temp_dir()` named with the process ID.
- Collector parsing is tested against the fixture tree in
  `crates/radar-collect/tests/fixtures`; pass `--root` to point the
  collector at one.
- `make install` builds, installs to `~/.local`, and restarts the user
  service. `radar-collect --seed-demo <days>` fills a database with synthetic
  history for working on the viewer.
- Measure disk writes with `write_bytes` from `/proc/$(pidof radar-collect)/io`.
  Databases under `/tmp` are on tmpfs and show nothing there. Buffering cut
  the collector from about 540 MB to about 40 MB of disk writes a day.
