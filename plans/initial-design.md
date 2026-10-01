# radar — personal system activity recorder & viewer

Build a small two-part tool for a single-user Linux desktop (Omarchy / Arch Linux):

1. **`radar-collect`** — a lightweight daemon (systemd *user* service) that samples system stats every 5 seconds into SQLite.
2. **`radar`** — a GTK4 + libadwaita app showing a single view: pick a time range, see filled line charts of CPU, temperatures, memory, network and disk over time, plus the top N processes by CPU time for that range.

The goal is to answer "how busy has my machine been, and why?" — not to be a general monitoring system.

## Constraints and principles

- **The collector must be nearly invisible.** Target: < 0.2% of one core averaged, < 15 MB RSS. No async runtime, no per-sample allocations in hot paths where avoidable, reuse read buffers, cache prepared statements.
- **Data window: 5 days.** Anything older is deleted periodically. Make the window a config value / CLI flag.
- **No root required.** Read only `/proc` and `/sys`.
- **Discover, don't hardcode.** Sensors, network interfaces and disks vary per machine; detect what exists.
- **Charts are drawn manually with Cairo** in a `GtkDrawingArea`. No charting library.
- **Charts use libadwaita colors** and update live when switching light/dark or accent color.

## Tech stack

- Rust, Cargo workspace with three crates:
  - `radar-core` — schema, migrations, shared query functions, types.
  - `radar-collect` — the collector binary.
  - `radar` — the GTK app.
- `rusqlite` (using the system SQLite from Arch's `sqlite` package).
- `gtk4` + `libadwaita` (rs bindings; require libadwaita ≥ 1.6 for the accent color API, with a fallback if unavailable).
- Keep dependencies minimal in the collector (no tokio, no clap-derive heavy stacks if avoidable; `lexopt` or plain `std::env::args` is fine).
- Build dependencies on Arch: `rust`, `pkgconf`, `gtk4`, `libadwaita`, `sqlite`.
- Add a `Makefile` or `justfile` with an `install` target that builds in release mode, copies both binaries to `~/.local/bin`, installs the systemd user unit and a `.desktop` file plus icon under `~/.local/share`, and runs `systemctl --user enable --now radar-collect`.

## Database

Location: `$XDG_DATA_HOME/radar/radar.db` (default `~/.local/share/radar/radar.db`), overridable with `--db`.

Pragmas on open (collector): `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`. The viewer opens read-only (`SQLITE_OPEN_READ_ONLY`), which works concurrently with WAL.

Use `PRAGMA user_version` for schema migrations, implemented in `radar-core`.

```sql
-- One row per 5s sample. Delta columns are NULL for the first sample
-- after startup or after a gap (suspend), since no valid delta exists.
CREATE TABLE sys_samples (
  ts          INTEGER PRIMARY KEY,   -- unix seconds (wall clock), aligned to interval
  dt_ms       INTEGER NOT NULL,      -- actual elapsed time since previous sample (monotonic)
  cpu_busy    REAL,                  -- % of total CPU capacity, 0-100
  cpu_iowait  REAL,                  -- % iowait
  load1       REAL NOT NULL,
  mem_used    INTEGER NOT NULL,      -- bytes: MemTotal - MemAvailable
  mem_total   INTEGER NOT NULL,
  swap_used   INTEGER NOT NULL,
  net_rx      INTEGER,               -- bytes received during this interval (physical ifaces)
  net_tx      INTEGER,
  disk_read   INTEGER,               -- bytes during this interval (whole physical disks)
  disk_write  INTEGER
) STRICT;

-- Discovered gauges: temperatures, plus optional fans / gpu busy / battery power.
CREATE TABLE sensors (
  id     INTEGER PRIMARY KEY,
  kind   TEXT NOT NULL,              -- 'temp' | 'fan' | 'gpu_busy' | 'power'
  chip   TEXT NOT NULL,              -- e.g. 'k10temp', 'amdgpu', 'nvme', 'coretemp'
  label  TEXT NOT NULL,              -- e.g. 'Tctl', 'Tccd1', 'edge', 'Composite'
  unit   TEXT NOT NULL,              -- '°C', 'rpm', '%', 'W'
  UNIQUE (kind, chip, label)
) STRICT;

CREATE TABLE sensor_samples (
  ts         INTEGER NOT NULL,
  sensor_id  INTEGER NOT NULL REFERENCES sensors(id),
  value      REAL NOT NULL,
  PRIMARY KEY (ts, sensor_id)
) STRICT, WITHOUT ROWID;

-- 5-minute rollups of sensor_samples, updated with each sample. Long ranges read these.
CREATE TABLE sensor_rollups (
  ts         INTEGER NOT NULL,       -- start of the 5-minute bucket
  sensor_id  INTEGER NOT NULL REFERENCES sensors(id),
  total      REAL NOT NULL,          -- sum of the values
  peak       REAL NOT NULL,
  samples    INTEGER NOT NULL,
  PRIMARY KEY (ts, sensor_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE proc_names (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE
) STRICT;

-- Per-minute CPU usage per process name. The collector upserts into the
-- current minute bucket every 5s sample.
CREATE TABLE proc_minutes (
  ts         INTEGER NOT NULL,       -- minute bucket (unix seconds, floor to 60)
  name_id    INTEGER NOT NULL REFERENCES proc_names(id),
  cpu_ticks  INTEGER NOT NULL,       -- summed utime+stime delta, in clock ticks
  PRIMARY KEY (ts, name_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE meta (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
) STRICT;   -- store clk_tck, ncpus, hostname, collector version
```

Upsert for processes:

```sql
INSERT INTO proc_minutes (ts, name_id, cpu_ticks) VALUES (?1, ?2, ?3)
ON CONFLICT (ts, name_id) DO UPDATE SET cpu_ticks = cpu_ticks + excluded.cpu_ticks;
```

Expected size at 5 days: ~86k `sys_samples` rows, a few hundred thousand `sensor_samples` and `proc_minutes` rows — tens of MB at most. No VACUUM needed; freed pages from trimming are reused, so the file reaches a steady size.

## Collector (`radar-collect`)

### Loop

- Sleep until the next wall-clock multiple of the interval (default 5s, `--interval`), so timestamps are aligned and predictable.
- Measure actual elapsed time with a monotonic clock (`Instant`) and store it as `dt_ms`.
- **Gap handling:** if elapsed time > 3× interval (suspend, stopped service), write the sample with NULL delta columns and reset all baselines. The viewer treats this as a break in the line.
- Each sample is written in a single transaction.
- Every hour (and at startup), trim: `DELETE FROM <table> WHERE ts < now - window` for all time-series tables. Also delete `proc_names` rows no longer referenced (cheap, occasional).
- Handle SIGTERM/SIGINT cleanly (finish the current transaction, exit).

### What to read (all unprivileged)

| Metric | Source | Notes |
|---|---|---|
| CPU busy / iowait | `/proc/stat` first `cpu` line | Delta of jiffies between samples. busy = (total − idle − iowait) / total. |
| Load | `/proc/loadavg` | First field. |
| Memory / swap | `/proc/meminfo` | `MemTotal − MemAvailable`; `SwapTotal − SwapFree`. |
| Network bytes | `/proc/net/dev` | Sum rx/tx bytes over *physical* interfaces only: those where `/sys/class/net/<iface>/device` exists. This excludes `lo`, docker bridges, veth, tun etc. Handle counter resets (if the new value < old, treat delta as NULL). |
| Disk bytes | `/proc/diskstats` | Whole physical disks only: entries in `/sys/block/` that have a `device` link (excludes loop, dm, zram, partitions). Sectors × 512. |
| Temperatures | `/sys/class/hwmon/hwmon*/` | `name` file gives the chip; `temp*_input` (millidegrees) with optional `temp*_label`. If there's no label, use `temp<N>`. **hwmonN numbering is not stable across boots** — identify sensors by (chip, label). |
| Fans (optional) | hwmon `fan*_input` | Only if present and non-zero at discovery. |
| GPU busy (optional) | `/sys/class/drm/card*/device/gpu_busy_percent` | Present on amdgpu. |
| Battery power (optional) | `/sys/class/power_supply/BAT*/power_now` | Microwatts; laptops only. |
| Per-process CPU | `/proc/[pid]/stat` | See below. |

Re-run sensor, interface and disk discovery every 5 minutes, to handle hotplug (USB NICs, docks).

Common AMD sensors to expect: `k10temp` → `Tctl`, `Tccd1..n`; `amdgpu` → `edge`, `junction`, `mem`; `nvme` → `Composite`. Intel: `coretemp` → `Package id 0`, `Core N`. Don't special-case these in the collector — just record whatever exists. The viewer can choose a sensible default subset (see below).

### Per-process accounting

- Iterate `/proc` numeric entries; read `/proc/[pid]/stat` into a reused buffer.
- Parse carefully: `comm` is in parentheses and may contain spaces or `)`. Find the *last* `)` and parse fields after it.
- Key per-process state by `(pid, starttime)` to survive PID reuse. Keep a `HashMap<(pid, starttime), last_ticks>` between samples.
- Delta = `(utime + stime) − last_ticks`.
- For a process not seen before: if its `starttime` is after the previous sample time, count all its ticks (it started during this interval); otherwise just record a baseline (startup case).
- Drop entries for processes that have exited.
- Aggregate deltas by `comm` name for this sample; skip zero deltas; upsert into the current minute bucket.
- Very short-lived processes that start and exit between samples won't be seen; that's acceptable.
- Store `clk_tck` (from `sysconf(_SC_CLK_TCK)`, usually 100) and CPU count in `meta`, so the viewer can convert ticks to CPU-seconds and percentages.

### Performance notes

- Reuse `String`/`Vec<u8>` buffers for all file reads; avoid `fs::read_to_string` allocations per file.
- Parse with byte scanning, not regex.
- Cache prepared statements (`prepare_cached`).
- In the systemd unit, run at low priority (see below).
- Acceptance check: the collector's own name should appear with negligible CPU time in the viewer's top processes.

### CLI

- `--db <path>`, `--interval <secs>` (default 5), `--retention <days>` (default 5)
- `--once` — take two samples one interval apart, print them as human-readable text, write nothing. This is for debugging.
- `--list-sensors` — print discovered sensors, interfaces and disks.

### systemd user unit

`~/.config/systemd/user/radar-collect.service`:

```ini
[Unit]
Description=radar system activity collector

[Service]
ExecStart=%h/.local/bin/radar-collect
Restart=on-failure
Nice=19
IOSchedulingClass=idle
CPUSchedulingPolicy=batch
MemoryMax=64M

[Install]
WantedBy=default.target
```

The `install` target places this unit and enables it.

## Viewer (`radar`)

### Layout (single window)

- `AdwApplicationWindow` with an `AdwHeaderBar`.
- **Range picker** in the header:
  - Linked toggle buttons for presets: 1h, 6h, 24h, 3d, 5d.
  - A "Custom…" button opening a popover with start and end day (`GtkDropDown` of the days that have data) and time (one `HH:MM` spin button).
  - Presets ending at "now" auto-refresh: every 5s for 1h, less often for longer ranges, up to every 60s. Custom ranges are static.
- **Main area**: a scrollable vertical stack of chart cards, each with a title, current/avg/max summary text, and the chart:
  1. **CPU** — busy % (filled), iowait % (thin line). Y axis fixed 0–100.
  2. **Temperatures** — one line per sensor.
     - Default visible: CPU sensors (`k10temp` `Tctl` or `coretemp` `Package id 0`), plus `amdgpu edge` and `nvme Composite` if present.
     - A small menu lets the user toggle sensors on and off. Persist the choice in GSettings or a small config file.
  3. **Memory** — used (filled), with total as a dashed reference line; swap as a secondary line if non-zero.
  4. **Network** — rx and tx in bytes/sec. rx filled above the axis, tx as a line, or both filled with transparency.
  5. **Disk** — read and write in bytes/sec, same style.
  6. Optional GPU busy / fans / battery power cards, shown only if those sensors exist. The fans card has the same sensor menu as Temperatures.
- **Top processes panel** (side pane on wide windows, below the charts on narrow ones; use `AdwBreakpoint`).
  - Top N (default 10) process names by CPU time in the selected range.
  - Each row shows: name, CPU time (e.g. "2h 14m"), % of machine capacity over the range, and a horizontal bar.

### Queries

Choose a bucket size so each chart gets roughly one point per 2 horizontal pixels: `bucket = max(5, range_secs / (chart_width_px / 2))`, rounded to a "nice" value (5s, 10s, 30s, 1m, 5m, 15m, 30m, 1h).

```sql
-- System series
SELECT (ts / :b) * :b AS t,
       avg(cpu_busy), avg(cpu_iowait), avg(mem_used), max(mem_total), avg(swap_used),
       sum(net_rx) * 1000.0 / sum(dt_ms), sum(net_tx) * 1000.0 / sum(dt_ms),
       sum(disk_read) * 1000.0 / sum(dt_ms), sum(disk_write) * 1000.0 / sum(dt_ms)
FROM sys_samples
WHERE ts BETWEEN :from AND :to
GROUP BY t ORDER BY t;

-- Sensors: avg per bucket (or max for temps — make it a toggle later).
-- The per-sensor avg and max for the card summaries are computed from the
-- same rows. All sensors are read, so the sensor menus need no new query.
SELECT (ts / :b) * :b AS t, sensor_id, sum(value), max(value), count(*)
FROM sensor_samples
WHERE ts BETWEEN :from AND :to
GROUP BY t, sensor_id ORDER BY t;

-- When the bucket is a multiple of 5 minutes, the whole rollups in the range
-- come from sensor_rollups (sum(total), max(peak), sum(samples)), which has
-- 60 times fewer rows. The partial rollups at the two ends of the range
-- still come from sensor_samples, so the result is the same as the raw query.

-- Top processes
SELECT n.name, sum(p.cpu_ticks) AS ticks
FROM proc_minutes p JOIN proc_names n ON n.id = p.name_id
WHERE p.ts BETWEEN :from AND :to
GROUP BY p.name_id ORDER BY ticks DESC LIMIT :n;
```

Note: in the rate calculations, ignore rows where the delta is NULL. `sum()` already skips NULLs, but `sum(dt_ms)` must only include rows with non-NULL deltas. Use `sum(CASE WHEN net_rx IS NOT NULL THEN dt_ms END)`.

- **Gap detection in charts:** if consecutive points are more than 3× the bucket size apart (or a row has NULL deltas), break the line and fill rather than interpolate across the gap.
- Run queries off the main thread (`gio::spawn_blocking` or a worker thread with a channel), and hand results back to the UI. Keep the UI responsive during range changes.

### Chart widget

Implement one reusable `TimeSeriesChart` widget (a subclass of `GtkWidget` or a wrapper around `GtkDrawingArea`) that takes:
- an x range (from, to)
- one or more series (`Vec<(t, Option<f64>)>`, plus style: filled or line, color role)
- a y-axis formatter (%, °C, bytes/s with human units, bytes)
- y-range mode: fixed or auto (auto = 0 to nice-rounded max).

Drawing with Cairo:
- Horizontal gridlines at 3–4 nice y values, with labels on the left. Time labels along the bottom, with tick spacing adapted to the range (e.g. every 10m, 1h, 6h, 1 day; show dates when the range spans days).
- **Filled series:** build the path along the data, close it down to the baseline, and fill with a vertical linear gradient from the series color at ~35% alpha (top) to ~5% alpha (bottom). Then stroke the line on top at full color, 1.5–2px, with round joins.
- Line-only series: stroke only.
- Anti-aliasing on; align gridlines to half-pixels for crispness.
- **Hover (phase 2):** a vertical crosshair plus a small tooltip showing time and values at the cursor (`GtkEventControllerMotion`).
- **Drag to zoom (phase 2):** dragging selects a sub-range and sets it as the custom range.

### Theming with libadwaita colors

- Text, axis and gridline colors: use `widget.color()` (GTK ≥ 4.10, the current CSS foreground color). Use it at ~55% alpha for labels and ~12% alpha for gridlines. This automatically follows light and dark mode.
- Primary series color (e.g. CPU busy): `adw::StyleManager::default().accent_color_rgba()` (libadwaita ≥ 1.6). Fall back to the Adwaita blue if unavailable.
- Additional series colors: use the GNOME/Adwaita named palette (blue, green, yellow, orange, red, purple), with separate light and dark variants:
  - in light mode use the `*_3`/`*_4` shades
  - in dark mode use the `*_2`/`*_3` shades.
  - Hardcode these palette values in a small `palette.rs`, since GTK4 no longer offers a non-deprecated way to look up named CSS colors from code.
  - Assign colors per series role, consistently across charts (e.g. rx = blue, tx = green; read = purple, write = orange).
- Chart card background: use standard `.card` styling from libadwaita; don't paint a custom background.
- Connect to `StyleManager` `notify::dark` and `notify::accent-color`, and call `queue_draw()` on all charts when they fire.
- Also follow `notify::high-contrast` by thickening lines slightly.

## Testing

- All `/proc` and `/sys` parsing lives in pure functions that take `&str` / `&[u8]`, with unit tests against fixture files captured from real machines (include an AMD `k10temp` example and a `comm` containing spaces and parentheses).
- Make the sysfs root path configurable (default `/`) so discovery can be tested against a fake directory tree in `tests/fixtures/`.
- Test delta logic: PID reuse, process start mid-interval, counter reset, suspend gap.
- Test query functions in `radar-core` against an in-memory DB seeded with synthetic data, including gaps and NULL deltas.
- Add a `radar-collect --seed-demo <days>` dev command, or a test helper, that generates realistic fake history so the viewer can be developed without waiting for days of data.

## Build order

1. Workspace, `radar-core` with schema, migrations and query functions, plus tests.
2. Collector parsing functions with fixtures and tests.
3. Collector loop: sampling, deltas, gap handling, DB writes, retention trim, `--once`, `--list-sensors`.
4. systemd unit and `install` target. Install it and let it collect while building the viewer.
5. Demo data seeder.
6. Viewer skeleton: window, header, range presets, background query plumbing.
7. `TimeSeriesChart` widget with gridlines, labels, filled and line series, gap breaks, theming.
8. All chart cards wired up, plus the top processes panel.
9. Custom range popover, sensor visibility menu, adaptive layout.
10. Phase 2: hover tooltip, drag-to-zoom.
11. Verify collector overhead with its own data; profile and tighten if it exceeds the targets.

## Out of scope (for now)

- Remote or multi-machine collection, alerts, exporting.
- Per-process memory or disk I/O (`/proc/[pid]/io` needs privileges for other users' processes). Could be added later as extra columns in `proc_minutes`.
- Rollup tables. With a 5-day window, raw data is small enough to aggregate on the fly. Revisit if the window grows to months.
