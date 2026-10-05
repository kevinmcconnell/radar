use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};

use gtk::glib;
use radar_core::snapshot::{self, Problem, Query, Reader};

use crate::askpass;

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

/// The ssh process reaching another machine, shared so it can be killed
/// while the worker is stuck waiting on it.
type Ssh = Arc<Mutex<Option<Child>>>;

pub struct Worker {
    requests: mpsc::Sender<Request>,
    ssh: Ssh,
}

impl Worker {
    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    /// Kill the ssh connection, even one still waiting on a password or a
    /// security key touch, so the worker can move on to the next request.
    pub fn hang_up(&self) {
        if let Some(child) = self.ssh.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// Answer requests on a thread of their own. ssh asks its questions through
/// the askpass `prompts` socket.
pub fn spawn(db_path: PathBuf, prompts: PathBuf) -> (Worker, async_channel::Receiver<Reply>) {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (reply_tx, reply_rx) = async_channel::unbounded();
    let ssh = Ssh::default();
    let worker = Worker {
        requests: req_tx,
        ssh: ssh.clone(),
    };
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
                    Some(machine) => read_remote(&mut remote, machine, &prompts, &ssh, &req.query),
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
    (worker, reply_rx)
}

fn read_remote(
    remote: &mut Option<Remote>,
    machine: &str,
    prompts: &Path,
    ssh: &Ssh,
    q: &Query,
) -> snapshot::Reply {
    if remote.as_ref().is_none_or(|r| r.machine != machine) {
        *remote = None;
        *remote = Some(Remote::connect(machine, prompts, ssh.clone())?);
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
    ssh: Ssh,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: ChildStderr,
    answered: bool,
}

impl Remote {
    fn connect(machine: &str, prompts: &Path, ssh_slot: Ssh) -> Result<Self, Problem> {
        let mut ssh = Command::new("ssh");
        ssh.args([
            "-T",
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
        .stderr(Stdio::piped());
        askpass::route_prompts(&mut ssh, prompts);
        let mut child = ssh.spawn().map_err(|e| {
            Problem::new(
                "Cannot Run ssh",
                glib::markup_escape_text(&e.to_string()).as_str(),
            )
        })?;
        let remote = Remote {
            machine: machine.to_string(),
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: BufReader::new(child.stdout.take().expect("piped stdout")),
            stderr: child.stderr.take().expect("piped stderr"),
            ssh: ssh_slot,
            answered: false,
        };
        *remote.ssh.lock().unwrap() = Some(child);
        Ok(remote)
    }

    /// The remote collector's reply, or why the connection broke.
    fn snapshot(&mut self, q: &Query) -> Result<snapshot::Reply, Problem> {
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
            Ok(_) => {
                self.answered = true;
                Ok(serde_json::from_str(&line).unwrap_or_else(|e| {
                    Err(Problem::new(
                        "Versions Don't Match",
                        format!(
                            "Unexpected reply from {}: {e}\n\nInstall the same version of Radar on both machines.",
                            glib::markup_escape_text(&self.machine)
                        ),
                    ))
                }))
            }
        }
    }

    /// Why ssh ended. Its stderr is read to the end before waiting, so a
    /// chatty ssh cannot block on a full pipe.
    fn failure(&mut self) -> Problem {
        let mut stderr = String::new();
        let _ = self.stderr.read_to_string(&mut stderr);
        let status = self
            .ssh
            .lock()
            .unwrap()
            .as_mut()
            .expect("ssh runs while connected")
            .wait();
        if status.is_ok_and(|s| s.code() == Some(127)) {
            Problem::new(
                format!("Radar Not Installed on {}", self.machine),
                "Install it there with <tt>make install</tt>",
            )
        } else if self.answered {
            Problem::new(
                format!("Lost Connection to {}", self.machine),
                glib::markup_escape_text(stderr.trim()).as_str(),
            )
        } else {
            Problem::new(
                format!("Cannot Connect to {}", self.machine),
                glib::markup_escape_text(stderr.trim()).as_str(),
            )
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(mut child) = self.ssh.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
