//! A single live PTY-backed session: owns the child process, a scrollback
//! buffer for late-attaching clients, and a broadcast channel for live output.

use anyhow::Result;
use bytes::Bytes;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

use crate::config::Profile;

/// Max scrollback retained per session for replay on attach (256 KiB).
const SCROLLBACK_CAP: usize = 256 * 1024;
/// Output more recent than this means the agent is actively producing → "working".
const WORKING_WINDOW_SECS: u64 = 2;

/// Serializable status surfaced to clients.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No output recently; agent is sitting idle.
    Idle,
    /// Producing output right now.
    Working,
    /// Tail of the buffer looks like a prompt awaiting human input.
    Waiting,
    /// Process exited.
    Exited,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub command: String,
    pub cwd: Option<String>,
    pub created_at: u64,
    pub state: State,
    pub exit_code: Option<i32>,
}

pub struct Session {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub command: String,
    pub cwd: Option<String>,
    pub created_at: u64,

    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    buffer: Arc<Mutex<Vec<u8>>>,
    last_output: Arc<Mutex<Instant>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    tx: broadcast::Sender<Bytes>,
}

impl Session {
    /// Spawn a new session from a profile. `rows`/`cols` seed the PTY size.
    pub fn spawn(id: String, name: String, profile: &Profile, rows: u16, cols: u16) -> Result<Arc<Session>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&profile.command);
        cmd.args(&profile.args);
        if let Some(cwd) = &profile.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &profile.env {
            cmd.env(k, v);
        }

        let command_line = {
            let mut parts = vec![profile.command.clone()];
            parts.extend(profile.args.iter().cloned());
            parts.join(" ")
        };

        let child = pair.slave.spawn_command(cmd)?;
        // The parent does not need the slave handle once the child holds it.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::with_capacity(8192)));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let exit_code = Arc::new(Mutex::new(None));
        let child = Arc::new(Mutex::new(child));
        let (tx, _rx) = broadcast::channel::<Bytes>(1024);

        // Blocking reader thread: portable-pty is synchronous I/O.
        {
            let buffer = buffer.clone();
            let last_output = last_output.clone();
            let exit_code = exit_code.clone();
            let child = child.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            let data = &chunk[..n];
                            {
                                let mut buf = buffer.lock();
                                buf.extend_from_slice(data);
                                if buf.len() > SCROLLBACK_CAP {
                                    let overflow = buf.len() - SCROLLBACK_CAP;
                                    buf.drain(0..overflow);
                                }
                            }
                            *last_output.lock() = Instant::now();
                            // Ignore send errors (no subscribers).
                            let _ = tx.send(Bytes::copy_from_slice(data));
                        }
                        Err(_) => break,
                    }
                }
                // Reap the child and record the exit code.
                let code = {
                    let mut c = child.lock();
                    c.wait().ok().map(|s| s.exit_code() as i32)
                };
                *exit_code.lock() = Some(code.unwrap_or(-1));
            });
        }

        Ok(Arc::new(Session {
            id,
            name,
            profile: profile.name.clone(),
            command: command_line,
            cwd: profile.cwd.clone(),
            created_at: now_secs(),
            master: Mutex::new(pair.master),
            writer: Arc::new(Mutex::new(writer)),
            child,
            buffer,
            last_output,
            exit_code,
            tx,
        }))
    }

    /// Current scrollback for replay on attach.
    pub fn snapshot(&self) -> Bytes {
        Bytes::copy_from_slice(&self.buffer.lock())
    }

    /// Subscribe to the live output stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    /// A cloneable writer handle for feeding input from async tasks.
    pub fn writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        self.writer.clone()
    }

    /// Resize the PTY (called on terminal resize from a client).
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.lock().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Terminate the session.
    pub fn kill(&self) {
        let _ = self.child.lock().kill();
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock()
    }

    /// Heuristic status: exited > waiting-for-input > working > idle.
    pub fn state(&self) -> State {
        if let Some(_code) = *self.exit_code.lock() {
            return State::Exited;
        }
        if self.looks_like_prompt() {
            return State::Waiting;
        }
        let elapsed = self.last_output.lock().elapsed().as_secs();
        if elapsed < WORKING_WINDOW_SECS {
            State::Working
        } else {
            State::Idle
        }
    }

    /// Inspect the tail of the buffer for common "awaiting input" signatures.
    fn looks_like_prompt(&self) -> bool {
        let buf = self.buffer.lock();
        let start = buf.len().saturating_sub(240);
        let tail = String::from_utf8_lossy(&buf[start..]);
        let tail_trim = tail.trim_end();
        const NEEDLES: [&str; 8] = [
            "(y/n)",
            "[Y/n]",
            "[y/N]",
            "Do you want",
            "Press enter",
            "❯",
            "? ",
            "password:",
        ];
        NEEDLES.iter().any(|n| tail_trim.contains(n))
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            profile: self.profile.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            state: self.state(),
            exit_code: self.exit_code(),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
