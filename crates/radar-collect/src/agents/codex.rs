//! Codex appends one line per event to `sessions/YYYY/MM/DD/rollout-*.jsonl`. Every API
//! response adds a `token_usage_record` and a `token_count` event; the latter carries the
//! account's rate-limit windows. Only the newest few day directories are watched.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use super::tail::Tail;
use super::{Quota, Usage, json, modified};

const DAYS_WATCHED: usize = 7;
const RECENT_RESPONSES: usize = 64;

pub struct Tracker {
    sessions: PathBuf,
    files: HashMap<PathBuf, FileState>,
    recent: VecDeque<Box<[u8]>>,
    buf: Vec<u8>,
    primed: bool,
}

struct FileState {
    tail: Tail,
    seen: bool,
}

impl Tracker {
    pub fn new(root: &Path) -> Self {
        Tracker {
            sessions: root.join("sessions"),
            files: HashMap::new(),
            recent: VecDeque::with_capacity(RECENT_RESPONSES),
            buf: Vec::new(),
            primed: false,
        }
    }

    pub fn active(&self) -> bool {
        self.sessions.is_dir()
    }

    pub fn describe(&self) -> String {
        format!("{}/*/*/*/rollout-*.jsonl", self.sessions.display())
    }

    pub fn poll(&mut self, usage: &mut Usage) {
        for f in self.files.values_mut() {
            f.seen = false;
        }
        let mut changed = Vec::new();
        for day in recent_day_dirs(&self.sessions) {
            let Ok(rd) = fs::read_dir(&day) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let Some((mtime, len)) = modified(&path) else {
                    continue;
                };
                let primed = self.primed;
                let state = self.files.entry(path.clone()).or_insert_with(|| FileState {
                    tail: if primed {
                        Tail::from_start()
                    } else {
                        Tail::from_end(len)
                    },
                    seen: false,
                });
                state.seen = true;
                if state.tail.pending(len) {
                    changed.push((mtime, path, len));
                }
            }
        }
        self.files.retain(|_, f| f.seen);
        changed.sort();
        for (_, path, len) in changed {
            let Some(state) = self.files.get_mut(&path) else {
                continue;
            };
            let (recent, count_tokens) = (&mut self.recent, self.primed);
            state.tail.read(&path, len, &mut self.buf, |line| {
                process_line(line, usage, recent, count_tokens)
            });
        }
        self.primed = true;
    }
}

/// The newest day directories, oldest first, found by name under `sessions/YYYY/MM/DD`.
fn recent_day_dirs(sessions: &Path) -> Vec<PathBuf> {
    let mut days = Vec::new();
    for year in subdirs(sessions) {
        for month in subdirs(&year) {
            days.extend(subdirs(&month));
        }
    }
    let key = |p: &Path| {
        let mut parts: Vec<String> = p
            .iter()
            .rev()
            .take(3)
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        parts.reverse();
        parts
    };
    days.sort_by_key(|p| std::cmp::Reverse(key(p)));
    days.truncate(DAYS_WATCHED);
    days.reverse();
    days
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn process_line(
    line: &[u8],
    usage: &mut Usage,
    recent: &mut VecDeque<Box<[u8]>>,
    count_tokens: bool,
) {
    match json::string(line, "type") {
        Some(b"token_usage_record") if count_tokens => {
            let Some(payload) = json::value(line, "payload") else {
                return;
            };
            let Some(id) = json::string(payload, "response_id") else {
                return;
            };
            if recent.iter().any(|r| **r == *id) {
                return;
            }
            if recent.len() == RECENT_RESPONSES {
                recent.pop_front();
            }
            recent.push_back(id.into());
            let Some(tokens) = json::value(payload, "usage") else {
                return;
            };
            let count = |key| json::integer(tokens, key).unwrap_or(0).max(0) as u64;
            usage.add_tokens(count("input_tokens"), count("output_tokens"));
        }
        Some(b"event_msg") => {
            let Some(payload) = json::value(line, "payload") else {
                return;
            };
            if json::string(payload, "type") != Some(b"token_count") {
                return;
            }
            let Some(limits) = json::value(payload, "rate_limits") else {
                return;
            };
            for key in ["primary", "secondary"] {
                let Some(window) = json::value(limits, key) else {
                    continue;
                };
                let Some(used_percent) = json::number(window, "used_percent") else {
                    continue;
                };
                let label = window_label(json::integer(window, "window_minutes"));
                let resets_at = json::integer(window, "resets_at");
                usage.set_quota(
                    &label,
                    Quota {
                        used_percent,
                        resets_at,
                    },
                );
            }
        }
        _ => {}
    }
}

fn window_label(minutes: Option<i64>) -> String {
    match minutes {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => "window".into(),
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

    fn day_file(root: &Path) -> PathBuf {
        let day = recent_day_dirs(&root.join("sessions")).pop().unwrap();
        fs::read_dir(day).unwrap().flatten().next().unwrap().path()
    }

    #[test]
    fn labels_windows_by_duration() {
        assert_eq!(window_label(Some(300)), "5h");
        assert_eq!(window_label(Some(10080)), "7d");
        assert_eq!(window_label(Some(90)), "90m");
        assert_eq!(window_label(None), "window");
    }

    #[test]
    fn startup_reads_quota_but_not_old_tokens() {
        let mut t = Tracker::new(&fixture("codex"));
        assert!(t.active());
        let mut u = Usage::default();
        t.poll(&mut u);
        assert_eq!(windows(&u), vec![("5h", 10.0), ("7d", 28.0)]);
        assert_eq!(u.windows[0].1.resets_at, Some(1790887470));
        assert_eq!((u.input_tokens, u.output_tokens), (0, 0));
    }

    #[test]
    fn appended_records_count_once_and_null_limits_keep_quota() {
        let scratch = Scratch::copy_of("codex");
        let mut t = Tracker::new(&scratch.0);
        let mut u = Usage::default();
        t.poll(&mut u);

        let path = day_file(&scratch.0);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let record = |id: &str, input, output| {
            format!(
                r#"{{"timestamp":"t","ordinal":1,"type":"token_usage_record","payload":{{"thread_id":"x","response_id":"{id}","usage":{{"input_tokens":{input},"cached_input_tokens":0,"output_tokens":{output},"total_tokens":0}}}}}}"#
            )
        };
        writeln!(f, "{}", record("resp_b", 1000, 50)).unwrap();
        writeln!(f, "{}", record("resp_b", 1000, 50)).unwrap();
        writeln!(f, "{}", record("resp_a", 7, 7)).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"t","ordinal":2,"type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":null}}}}"#
        )
        .unwrap();
        writeln!(f, "{}", record("resp_c", 10, 1)).unwrap();
        write!(
            f,
            r#"{{"timestamp":"t","ordinal":3,"type":"token_usage_record""#
        )
        .unwrap();
        t.poll(&mut u);
        assert_eq!((u.input_tokens, u.output_tokens), (1017, 58));
        assert_eq!(windows(&u), vec![("5h", 10.0), ("7d", 28.0)]);

        writeln!(
            f,
            r#",,"payload":{{"response_id":"resp_d","usage":{{"input_tokens":5,"output_tokens":5}}}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"t","ordinal":4,"type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"primary":{{"used_percent":11.0,"window_minutes":300,"resets_at":1}},"secondary":{{"used_percent":29.5,"window_minutes":10080,"resets_at":2}}}}}}}}"#
        )
        .unwrap();
        t.poll(&mut u);
        assert_eq!(
            (u.input_tokens, u.output_tokens),
            (1022, 63),
            "a line finished later is read once complete"
        );
        assert_eq!(windows(&u), vec![("5h", 11.0), ("7d", 29.5)]);
    }

    #[test]
    fn new_session_files_count_from_the_start() {
        let scratch = Scratch::copy_of("codex");
        let mut t = Tracker::new(&scratch.0);
        let mut u = Usage::default();
        t.poll(&mut u);
        let day = recent_day_dirs(&scratch.0.join("sessions")).pop().unwrap();
        fs::write(
            day.join("rollout-new.jsonl"),
            r#"{"timestamp":"t","ordinal":0,"type":"token_usage_record","payload":{"response_id":"resp_z","usage":{"input_tokens":3,"output_tokens":4}}}
"#,
        )
        .unwrap();
        t.poll(&mut u);
        assert_eq!((u.input_tokens, u.output_tokens), (3, 4));
    }

    #[test]
    fn only_the_newest_days_are_watched() {
        let scratch = Scratch::copy_of("codex");
        let sessions = scratch.0.join("sessions");
        for day in 1..=9 {
            fs::create_dir_all(sessions.join(format!("2025/01/{day:02}"))).unwrap();
        }
        let days: Vec<String> = recent_day_dirs(&sessions)
            .iter()
            .map(|p| {
                p.strip_prefix(&sessions)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(days.len(), DAYS_WATCHED);
        assert_eq!(days.first().unwrap(), "2025/01/04");
        assert_eq!(days.last().unwrap(), "2026/10/01");
    }
}
