//! Claude Code runs a status line command after every assistant message and pipes the
//! session's JSON to it. The `radar-claude-statusline` script appends that JSON as one
//! line to `<session_id>.jsonl` in radar's state directory, and this tracker tails those
//! files. `context_window` holds the token counts of the most recent API response, so a
//! line counts as a response when the session's API time has moved on.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::tail::Tail;
use super::{Quota, Usage, json, modified};

const KEEP_BEHIND_NEWEST: Duration = Duration::from_secs(2 * 86400);
const WINDOWS: [(&str, &str); 2] = [("five_hour", "5h"), ("seven_day", "7d")];

pub struct Tracker {
    dir: PathBuf,
    sessions: HashMap<PathBuf, Session>,
    buf: Vec<u8>,
    primed: bool,
}

struct Session {
    tail: Tail,
    mtime: SystemTime,
    api_ms: Option<i64>,
    seen: bool,
}

impl Tracker {
    pub fn new(dir: &Path) -> Self {
        Tracker {
            dir: dir.to_path_buf(),
            sessions: HashMap::new(),
            buf: Vec::new(),
            primed: false,
        }
    }

    pub fn active(&self) -> bool {
        self.dir.is_dir()
    }

    pub fn describe(&self) -> String {
        format!("{}/*.jsonl", self.dir.display())
    }

    pub fn poll(&mut self, usage: &mut Usage) {
        for s in self.sessions.values_mut() {
            s.seen = false;
        }
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut changed = Vec::new();
        let mut newest = None;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some((mtime, len)) = modified(&path) else {
                continue;
            };
            newest = newest.max(Some(mtime));
            let primed = self.primed;
            let state = self
                .sessions
                .entry(path.clone())
                .or_insert_with(|| Session {
                    tail: if primed {
                        Tail::from_start()
                    } else {
                        Tail::from_end(len)
                    },
                    mtime,
                    api_ms: None,
                    seen: false,
                });
            state.seen = true;
            state.mtime = mtime;
            if state.tail.pending(len) {
                changed.push((mtime, path, len));
            }
        }
        self.sessions.retain(|_, s| s.seen);
        changed.sort();
        for (_, path, len) in &changed {
            self.read(path, *len, usage);
        }
        self.prune(newest);
        self.primed = true;
    }

    fn read(&mut self, path: &Path, len: u64, usage: &mut Usage) {
        let Some(state) = self.sessions.get_mut(path) else {
            return;
        };
        let count_tokens = self.primed;
        let api_ms = &mut state.api_ms;
        state.tail.read(path, len, &mut self.buf, |doc| {
            if let Some(limits) = json::value(doc, "rate_limits") {
                for (key, label) in WINDOWS {
                    let Some(window) = json::value(limits, key) else {
                        continue;
                    };
                    let Some(used_percent) = json::number(window, "used_percentage") else {
                        continue;
                    };
                    let resets_at = json::integer(window, "resets_at");
                    usage.set_quota(
                        label,
                        Quota {
                            used_percent,
                            resets_at,
                        },
                    );
                }
            }
            let now_ms =
                json::value(doc, "cost").and_then(|c| json::integer(c, "total_api_duration_ms"));
            if count_tokens
                && now_ms.is_some()
                && now_ms != *api_ms
                && let Some(context) = json::value(doc, "context_window")
            {
                let count = |key| json::integer(context, key).unwrap_or(0).max(0) as u64;
                usage.add_tokens(count("total_input_tokens"), count("total_output_tokens"));
            }
            *api_ms = now_ms;
        });
    }

    /// Files well behind the newest one belong to sessions that have ended.
    fn prune(&mut self, newest: Option<SystemTime>) {
        let Some(newest) = newest else { return };
        self.sessions.retain(|path, s| {
            let stale = newest
                .duration_since(s.mtime)
                .is_ok_and(|d| d > KEEP_BEHIND_NEWEST);
            if stale {
                let _ = fs::remove_file(path);
            }
            !stale
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Scratch, fixture};
    use super::*;
    use std::io::Write;

    fn windows(u: &Usage) -> Vec<(&str, f64)> {
        u.windows
            .iter()
            .map(|(l, q)| (l.as_str(), q.used_percent))
            .collect()
    }

    fn status(api_ms: i64, input: i64, output: i64, limits: &str) -> String {
        format!(
            r#"{{"session_id":"s2","cost":{{"total_cost_usd":0.1,"total_api_duration_ms":{api_ms}}},"context_window":{{"total_input_tokens":{input},"total_output_tokens":{output},"current_usage":null}}{limits}}}"#
        )
    }

    #[test]
    fn startup_reads_the_latest_quota_but_not_tokens() {
        let mut t = Tracker::new(&fixture("claude"));
        assert!(t.active());
        let mut u = Usage::default();
        t.poll(&mut u);
        assert_eq!(windows(&u), vec![("5h", 23.5), ("7d", 41.2)]);
        assert_eq!(u.windows[1].1.resets_at, Some(1738857600));
        assert_eq!((u.input_tokens, u.output_tokens), (0, 0));
    }

    #[test]
    fn counts_each_response_once_even_several_per_poll() {
        let scratch = Scratch::copy_of("claude");
        let mut t = Tracker::new(&scratch.0);
        let mut u = Usage::default();
        t.poll(&mut u);

        let other = scratch.0.join("s2.jsonl");
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&other)
            .unwrap();
        writeln!(f, "{}", status(100, 20_000, 300, "")).unwrap();
        writeln!(f, "{}", status(100, 20_000, 300, "")).unwrap();
        writeln!(f, "{}", status(180, 21_000, 10, "")).unwrap();
        t.poll(&mut u);
        assert_eq!(
            (u.input_tokens, u.output_tokens),
            (41_000, 310),
            "a rerun without an API call adds nothing"
        );
        assert_eq!(windows(&u), vec![("5h", 23.5), ("7d", 41.2)]);

        let limits = r#","rate_limits":{"five_hour":{"used_percentage":30,"resets_at":5}}"#;
        writeln!(f, "{}", status(250, 1_000, 1, limits)).unwrap();
        t.poll(&mut u);
        assert_eq!((u.input_tokens, u.output_tokens), (42_000, 311));
        assert_eq!(windows(&u), vec![("5h", 30.0), ("7d", 41.2)]);
    }

    #[test]
    fn missing_dir_is_inactive_and_harmless() {
        let mut t = Tracker::new(Path::new("/nonexistent/radar/claude"));
        assert!(!t.active());
        let mut u = Usage::default();
        t.poll(&mut u);
        assert_eq!(u, Usage::default());
    }
}
