//! Usage reported by AI coding agents: rate-limit windows and tokens per response.
//!
//! Codex writes a quota snapshot and a token record to its session log on every API
//! response. Claude Code pipes the same data to a status line command, which radar ships
//! as a script that drops the JSON into a state directory. Both are tailed here.

mod claude;
mod codex;
mod json;
mod tail;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use radar_core::SensorKind;

use crate::discover::{Reading, SensorSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
        }
    }
}

/// The latest report for one rate-limit window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quota {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

/// What one agent has reported: its windows in the order first seen, and the tokens of
/// the responses observed since the last sample.
#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    pub windows: Vec<(String, Quota)>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    fn set_quota(&mut self, label: &str, quota: Quota) {
        match self.windows.iter_mut().find(|w| w.0 == label) {
            Some(w) => w.1 = quota,
            None => self.windows.push((label.to_string(), quota)),
        }
    }

    fn add_tokens(&mut self, input: u64, output: u64) {
        self.input_tokens += input;
        self.output_tokens += output;
    }
}

/// Where each agent leaves its data; `None` disables that agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentDirs {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

impl AgentDirs {
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let env_dir = |var: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let state_home = env_dir("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        AgentDirs {
            claude: Some(state_home.join("radar/claude")),
            codex: Some(env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"))),
        }
    }
}

pub struct Agents {
    claude: Option<claude::Tracker>,
    codex: Option<codex::Tracker>,
    usage: [Usage; 2],
}

impl Agents {
    pub fn new(dirs: &AgentDirs) -> Self {
        let mut agents = Agents {
            claude: dirs.claude.as_deref().map(claude::Tracker::new),
            codex: dirs.codex.as_deref().map(codex::Tracker::new),
            usage: Default::default(),
        };
        agents.poll();
        agents.end_sample();
        agents
    }

    fn present(&self, agent: Agent) -> bool {
        match agent {
            Agent::Claude => self.claude.as_ref().is_some_and(|t| t.active()),
            Agent::Codex => self.codex.as_ref().is_some_and(|t| t.active()),
        }
    }

    pub fn usage(&self, agent: Agent) -> &Usage {
        &self.usage[agent as usize]
    }

    pub fn describe(&self, agent: Agent) -> Option<String> {
        match agent {
            Agent::Claude => self.claude.as_ref().map(|t| t.describe()),
            Agent::Codex => self.codex.as_ref().map(|t| t.describe()),
        }
    }

    /// Quota sensors for every window seen so far, then token rates, per present agent.
    pub fn sources(&self) -> Vec<SensorSource> {
        let mut out = Vec::new();
        for agent in Agent::ALL.into_iter().filter(|a| self.present(*a)) {
            let source = |kind, label: &str, reading| SensorSource {
                kind,
                chip: agent.as_str().into(),
                label: label.into(),
                reading,
            };
            for (window, (label, _)) in self.usage(agent).windows.iter().enumerate() {
                out.push(source(
                    SensorKind::Quota,
                    label,
                    Reading::Quota { agent, window },
                ));
            }
            for direction in [Direction::Input, Direction::Output] {
                out.push(source(
                    SensorKind::Tokens,
                    direction.as_str(),
                    Reading::Tokens { agent, direction },
                ));
            }
        }
        out
    }

    /// How many sensors `sources` would return; it grows when an agent reports a new window.
    pub fn source_count(&self) -> usize {
        Agent::ALL
            .into_iter()
            .filter(|a| self.present(*a))
            .map(|a| self.usage(a).windows.len() + 2)
            .sum()
    }

    /// Reads whatever the agents have written since the last poll.
    pub fn poll(&mut self) {
        if let Some(t) = &mut self.claude {
            t.poll(&mut self.usage[Agent::Claude as usize]);
        }
        if let Some(t) = &mut self.codex {
            t.poll(&mut self.usage[Agent::Codex as usize]);
        }
    }

    /// Percent of a window still available, or all of it once the window has reset.
    pub fn quota(&self, agent: Agent, window: usize, now: i64) -> Option<f64> {
        let (_, q) = self.usage(agent).windows.get(window)?;
        Some(match q.resets_at {
            Some(reset) if reset <= now => 100.0,
            _ => (100.0 - q.used_percent).max(0.0),
        })
    }

    /// Tokens per minute over the interval that ended with this sample.
    pub fn tokens(&self, agent: Agent, direction: Direction, dt_ms: i64) -> Option<f64> {
        if dt_ms <= 0 {
            return None;
        }
        let u = self.usage(agent);
        let n = match direction {
            Direction::Input => u.input_tokens,
            Direction::Output => u.output_tokens,
        };
        Some(n as f64 * 60_000.0 / dt_ms as f64)
    }

    pub fn end_sample(&mut self) {
        for u in &mut self.usage {
            u.input_tokens = 0;
            u.output_tokens = 0;
        }
    }
}

fn modified(path: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    pub fn fixture(agent: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agents")
            .join(agent)
    }

    /// A writable copy of a fixture tree, removed on drop.
    pub struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn copy_of(agent: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "radar-agents-{}-{}-{}",
                agent,
                std::process::id(),
                std::thread::current()
                    .name()
                    .unwrap_or("t")
                    .replace("::", "-")
            ));
            let _ = fs::remove_dir_all(&dir);
            copy_tree(&fixture(agent), &dir);
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            let target = to.join(e.file_name());
            if e.path().is_dir() {
                copy_tree(&e.path(), &target);
            } else {
                fs::copy(e.path(), target).unwrap();
            }
        }
    }

    fn labels(sources: &[SensorSource]) -> Vec<(SensorKind, String, String)> {
        sources
            .iter()
            .map(|s| (s.kind, s.chip.clone(), s.label.clone()))
            .collect()
    }

    #[test]
    fn fixture_agents_report_quota_and_no_startup_tokens() {
        let agents = Agents::new(&AgentDirs {
            claude: Some(fixture("claude")),
            codex: Some(fixture("codex")),
        });
        let q = |agent, w| agents.quota(agent, w, 0).unwrap();
        assert_eq!(q(Agent::Codex, 0), 90.0);
        assert_eq!(q(Agent::Codex, 1), 72.0);
        assert_eq!(q(Agent::Claude, 0), 76.5);
        assert_eq!(q(Agent::Claude, 1), 58.8);
        assert_eq!(
            agents.quota(Agent::Codex, 0, 1_790_887_470),
            Some(100.0),
            "a window is whole again once it has reset"
        );
        assert_eq!(
            agents.tokens(Agent::Codex, Direction::Input, 5000),
            Some(0.0)
        );
        assert_eq!(agents.tokens(Agent::Codex, Direction::Input, 0), None);

        let s = |k, c: &str, l: &str| (k, c.to_string(), l.to_string());
        assert_eq!(
            labels(&agents.sources()),
            vec![
                s(SensorKind::Quota, "claude", "5h"),
                s(SensorKind::Quota, "claude", "7d"),
                s(SensorKind::Tokens, "claude", "input"),
                s(SensorKind::Tokens, "claude", "output"),
                s(SensorKind::Quota, "codex", "5h"),
                s(SensorKind::Quota, "codex", "7d"),
                s(SensorKind::Tokens, "codex", "input"),
                s(SensorKind::Tokens, "codex", "output"),
            ]
        );
    }

    #[test]
    fn a_new_window_changes_the_source_count() {
        let scratch = Scratch::copy_of("codex");
        let mut agents = Agents::new(&AgentDirs {
            claude: None,
            codex: Some(scratch.0.clone()),
        });
        assert_eq!(agents.source_count(), 4);
        assert_eq!(agents.sources().len(), 4);
        let day = scratch.0.join("sessions/2026/10/01");
        fs::write(
            day.join("rollout-extra.jsonl"),
            r#"{"timestamp":"t","ordinal":0,"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":1440,"resets_at":9}}}}
"#,
        )
        .unwrap();
        agents.poll();
        assert_eq!(agents.source_count(), 5);
        assert_eq!(agents.sources()[2].label, "1d");
    }

    #[test]
    fn missing_dirs_give_no_sources() {
        let agents = Agents::new(&AgentDirs {
            claude: Some("/nonexistent/claude".into()),
            codex: None,
        });
        assert!(agents.sources().is_empty());
        assert!(Agents::new(&AgentDirs::default()).sources().is_empty());
    }
}
