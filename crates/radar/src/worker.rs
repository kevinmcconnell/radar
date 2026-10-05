use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;

use gtk::glib;
use radar_core::snapshot::{self, Query, Reader};

/// Non-interactive ssh sessions often leave `~/.local/bin`, where `make install`
/// puts the collector, off the PATH.
const REMOTE_COMMAND: &str = r#"PATH="$HOME/.local/bin:$PATH" exec radar-collect --serve"#;

pub struct Request {
    pub generation: u64,
    /// The ssh destination to read from, or `None` for this machine.
    pub machine: Option<String>,
    pub query: Query,
}

pub struct Reply {
    pub generation: u64,
    pub snapshot: snapshot::Reply,
}

pub fn spawn(db_path: PathBuf) -> (mpsc::Sender<Request>, async_channel::Receiver<Reply>) {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (reply_tx, reply_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("radar-query".into())
        .spawn(move || {
            let mut local = Reader::new(db_path);
            let mut remote: Option<Remote> = None;
            while let Ok(mut req) = req_rx.recv() {
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let snapshot = match &req.machine {
                    Some(machine) => read_remote(&mut remote, machine, &req.query),
                    None => {
                        remote = None;
                        local.snapshot(&req.query)
                    }
                };
                let reply = Reply {
                    generation: req.generation,
                    snapshot,
                };
                if reply_tx.send_blocking(reply).is_err() {
                    break;
                }
            }
        })
        .expect("spawn query thread");
    (req_tx, reply_rx)
}

fn read_remote(remote: &mut Option<Remote>, machine: &str, q: &Query) -> snapshot::Reply {
    if remote.as_ref().is_none_or(|r| r.machine != machine) {
        *remote = None;
        *remote = Some(Remote::connect(machine)?);
    }
    match remote.as_mut().unwrap().snapshot(q) {
        Ok(reply) => reply,
        Err(e) => {
            *remote = None;
            Err(e)
        }
    }
}

/// `radar-collect --serve` on another machine, reached over ssh.
struct Remote {
    machine: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Remote {
    fn connect(machine: &str) -> Result<Self, String> {
        let mut child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                machine,
                REMOTE_COMMAND,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "Cannot run ssh: {}",
                    glib::markup_escape_text(&e.to_string())
                )
            })?;
        Ok(Remote {
            machine: machine.to_string(),
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: BufReader::new(child.stdout.take().expect("piped stdout")),
            child,
        })
    }

    /// The remote collector's reply, or why the connection broke.
    fn snapshot(&mut self, q: &Query) -> Result<snapshot::Reply, String> {
        let mut line = serde_json::to_string(q).expect("queries serialize");
        line.push('\n');
        let sent = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush());
        if sent.is_err() {
            return Err(self.failure());
        }
        line.clear();
        match self.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => Err(self.failure()),
            Ok(_) => Ok(serde_json::from_str(&line).unwrap_or_else(|e| {
                Err(format!(
                    "Unexpected reply from {}: {e}\n\nInstall the same version of Radar on both machines.",
                    glib::markup_escape_text(&self.machine)
                ))
            })),
        }
    }

    /// Why ssh ended. Its stderr is read to the end before waiting, so a
    /// chatty ssh cannot block on a full pipe.
    fn failure(&mut self) -> String {
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let status = self.child.wait();
        let machine = glib::markup_escape_text(&self.machine);
        if status.is_ok_and(|s| s.code() == Some(127)) {
            format!(
                "Radar is not installed on {machine}.\n\nInstall it there with <tt>make install</tt>"
            )
        } else {
            format!(
                "Cannot read from {machine}.\n\n{}",
                glib::markup_escape_text(stderr.trim())
            )
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
