//! Lets ssh ask for passwords, host key confirmations, and security key
//! touches in the viewer's window. ssh runs this same binary as its askpass
//! program, which hands the prompt over a socket to the viewer and prints the
//! answer it gets back.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use gtk::glib;

const SOCKET_ENV: &str = "RADAR_ASKPASS_SOCKET";
const ANSWERED: &str = "answered\n";
/// Ends a prompt. ssh prompts span lines but never hold a NUL.
const END: u8 = 0;
const PARENT_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A password or passphrase.
    Secret,
    /// A yes or no, like whether to trust a host key.
    Confirm,
    /// Something to do elsewhere, like touching a security key. ssh takes
    /// the notice down itself and expects no answer.
    Notice,
}

/// A question from ssh. Dropping it unanswered cancels the connection.
pub struct Prompt {
    pub text: String,
    pub kind: Kind,
    reply: mpsc::Sender<String>,
    gone: async_channel::Receiver<()>,
}

impl Prompt {
    fn parse(
        message: &str,
        reply: mpsc::Sender<String>,
        gone: async_channel::Receiver<()>,
    ) -> Self {
        let (hint, text) = message.split_once('\n').unwrap_or(("", message));
        let kind = if hint == "none" {
            Kind::Notice
        } else if hint == "confirm" || text.contains("(yes/no") {
            Kind::Confirm
        } else {
            Kind::Secret
        };
        Prompt {
            text: text.trim().to_string(),
            kind,
            reply,
            gone,
        }
    }

    pub fn answer(self, answer: String) {
        let _ = self.reply.send(answer);
    }

    /// Wait until ssh no longer needs this notice shown.
    pub async fn gone(&self) {
        let _ = self.gone.recv().await;
    }
}

/// The socket ssh's askpass runs reach the viewer on, removed when dropped.
pub struct Listener {
    path: PathBuf,
}

impl Listener {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn listen() -> (Listener, async_channel::Receiver<Prompt>) {
    let path = glib::user_runtime_dir().join(format!("radar-askpass-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let socket = UnixListener::bind(&path).expect("listen for ssh prompts");
    let (prompt_tx, prompt_rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("radar-askpass".into())
        .spawn(move || {
            for stream in socket.incoming().flatten() {
                if relay(stream, &prompt_tx).is_err() {
                    break;
                }
            }
        })
        .expect("spawn askpass thread");
    (Listener { path }, prompt_rx)
}

/// Pass one prompt to the viewer and its answer back, or for a notice, wait
/// until ssh takes it down. Errs once the viewer is gone.
fn relay(
    mut stream: UnixStream,
    prompts: &async_channel::Sender<Prompt>,
) -> Result<(), async_channel::SendError<Prompt>> {
    let mut message = Vec::new();
    if BufReader::new(&stream)
        .read_until(END, &mut message)
        .is_err()
    {
        return Ok(());
    }
    message.pop();
    let (reply_tx, reply_rx) = mpsc::channel();
    let (gone_tx, gone_rx) = async_channel::bounded(1);
    let prompt = Prompt::parse(&String::from_utf8_lossy(&message), reply_tx, gone_rx);
    let kind = prompt.kind;
    prompts.send_blocking(prompt)?;
    if kind == Kind::Notice {
        // ssh kills the askpass when it is done, which closes the stream.
        let _ = stream.read(&mut [0]);
        drop(gone_tx);
    } else if let Ok(answer) = reply_rx.recv() {
        let _ = stream.write_all(format!("{ANSWERED}{answer}").as_bytes());
    }
    Ok(())
}

/// Have ssh ask its questions through the viewer listening on `listener`.
pub fn route_prompts(ssh: &mut Command, listener: &Path) {
    let exe = std::env::current_exe().expect("find the radar binary");
    ssh.env("SSH_ASKPASS", exe)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(SOCKET_ENV, listener);
}

/// When ssh runs this binary as its askpass, answer its prompt and say how
/// it went. `None` when this is an ordinary launch of the viewer.
pub fn answer_for_ssh() -> Option<glib::ExitCode> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let hint = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match ask(&socket, &hint, &prompt) {
        Some(answer) => {
            println!("{answer}");
            Some(glib::ExitCode::SUCCESS)
        }
        None => Some(glib::ExitCode::FAILURE),
    }
}

/// Send the prompt and wait for the answer. A notice gets none, so this
/// waits until ssh kills the askpass, or dies itself: the viewer kills ssh
/// to switch machines, and then nothing else would end the wait.
fn ask(socket: &OsStr, hint: &str, prompt: &str) -> Option<String> {
    let ssh = std::os::unix::process::parent_id();
    let mut stream = UnixStream::connect(socket).ok()?;
    let mut message = format!("{hint}\n{prompt}").into_bytes();
    message.push(END);
    stream.write_all(&message).ok()?;
    stream.set_read_timeout(Some(PARENT_CHECK)).ok()?;

    let mut reply = Vec::new();
    let mut buf = [0; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                    && std::os::unix::process::parent_id() == ssh => {}
            Err(_) => return None,
        }
    }
    String::from_utf8(reply)
        .ok()?
        .strip_prefix(ANSWERED)
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(message: &str) -> Prompt {
        let (reply_tx, _) = mpsc::channel();
        let (_, gone_rx) = async_channel::bounded(1);
        Prompt::parse(message, reply_tx, gone_rx)
    }

    #[test]
    fn prompts_say_what_ssh_wants() {
        let password = parse("\nme@box's password: ");
        assert_eq!(password.text, "me@box's password:");
        assert_eq!(password.kind, Kind::Secret);

        let host_key = parse(
            "\nThe authenticity of host 'box' can't be established.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
        );
        assert!(host_key.text.starts_with("The authenticity of host 'box'"));
        assert_eq!(host_key.kind, Kind::Confirm);

        assert_eq!(parse("confirm\nAllow use of key?").kind, Kind::Confirm);
        assert_eq!(
            parse("none\nConfirm user presence for key ED25519-SK").kind,
            Kind::Notice
        );
    }

    #[test]
    fn answers_cancellations_and_notices_reach_the_other_side() {
        let path = std::env::temp_dir().join(format!("radar-askpass-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let socket = UnixListener::bind(&path).unwrap();
        let (prompt_tx, prompt_rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            for stream in socket.incoming().flatten() {
                relay(stream, &prompt_tx).unwrap();
            }
        });

        let viewer = std::thread::spawn(move || {
            let first = prompt_rx.recv_blocking().unwrap();
            assert_eq!(first.text, "me@box's password:");
            first.answer(String::new());
            drop(prompt_rx.recv_blocking().unwrap());
            prompt_rx.recv_blocking().unwrap()
        });
        assert_eq!(
            ask(path.as_os_str(), "", "me@box's password: "),
            Some(String::new())
        );
        assert_eq!(ask(path.as_os_str(), "", "me@box's password: "), None);

        let mut askpass = UnixStream::connect(&path).unwrap();
        askpass.write_all(b"none\nTouch the key\0").unwrap();
        let notice = viewer.join().unwrap();
        assert_eq!(notice.kind, Kind::Notice);
        assert_eq!(notice.text, "Touch the key");
        assert!(notice.gone.try_recv().is_err_and(|e| e.is_empty()));
        drop(askpass);
        assert!(notice.gone.recv_blocking().is_err());
        let _ = std::fs::remove_file(&path);
    }
}
