//! A single live session: owns a scrollback buffer for late-attaching clients
//! and a broadcast channel for live output.
//!
//! Two backends sit behind the same interface:
//!
//! * [`Backend::Pty`] — the daemon spawns the agent command directly in a PTY it
//!   owns. Simple and cross-platform, but the session dies if the daemon
//!   restarts (the PTY's owning process went away).
//! * [`Backend::Tmux`] — the agent command runs inside a detached `tmux` session
//!   (`flotilla_<id>`); the daemon owns only a PTY running `tmux attach`. Because
//!   the tmux *server* owns the real process, the session **survives a daemon
//!   restart**: on boot the daemon re-attaches (see [`Session::adopt_tmux`]).
//!   Linux/macOS only.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

use crate::config::Profile;

/// Max scrollback retained per session for replay on attach (256 KiB).
const SCROLLBACK_CAP: usize = 256 * 1024;
/// Output more recent than this means the agent is actively producing → "working".
const WORKING_WINDOW_SECS: u64 = 2;

/// Which engine backs a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Daemon owns the PTY directly. Does not survive a daemon restart.
    #[default]
    Pty,
    /// Command runs in a detached tmux session; daemon owns a `tmux attach` PTY.
    /// Survives a daemon restart.
    Tmux,
}

/// The tmux session name owned by Flotilla for a given session id. Deterministic
/// so adoption after a restart needs no extra persisted state.
pub fn tmux_session_name(id: &str) -> String {
    format!("flotilla_{id}")
}

/// Serializable status surfaced to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Waiting => "waiting",
            State::Exited => "exited",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub command: String,
    pub cwd: Option<String>,
    pub created_at: u64,
    pub tags: Vec<String>,
    pub state: State,
    pub exit_code: Option<i32>,
    /// Which backend owns this session.
    #[serde(default)]
    pub backend: Backend,
}

pub struct Session {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub command: String,
    pub cwd: Option<String>,
    pub created_at: u64,
    pub tags: Vec<String>,
    pub backend: Backend,

    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    buffer: Arc<Mutex<Vec<u8>>>,
    last_output: Arc<Mutex<Instant>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    tx: broadcast::Sender<Bytes>,
}

impl Session {
    /// Spawn a new session from a profile using the profile's backend.
    /// `rows`/`cols` seed the PTY size. `created_at`/`tags` are carried so the
    /// same constructor serves both fresh spawns and restart-time adoption.
    pub fn spawn(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Session>> {
        let backend = profile.effective_backend();
        match backend {
            Backend::Pty => Self::spawn_pty(id, name, tags, profile, rows, cols),
            Backend::Tmux => Self::spawn_tmux(id, name, tags, profile, rows, cols),
        }
    }

    /// Direct-PTY backend: spawn the command in a PTY the daemon owns.
    fn spawn_pty(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Session>> {
        let mut cmd = CommandBuilder::new(&profile.command);
        cmd.args(&profile.args);
        if let Some(cwd) = &profile.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &profile.env {
            cmd.env(k, v);
        }
        Self::wire(id, name, tags, profile, Backend::Pty, now_secs(), rows, cols, cmd, command_line(profile))
    }

    /// tmux backend: create a detached tmux session running the command, then
    /// own a PTY that runs `tmux attach` for live I/O.
    fn spawn_tmux(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Session>> {
        ensure_tmux()?;
        let tmux = tmux_session_name(&id);

        // Create the detached session running the agent command. The command is
        // a single shell string (tmux runs it via /bin/sh -c).
        let shell_cmd = tmux_shell_command(profile);
        let mut create = Command::new("tmux");
        create
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&tmux)
            .arg("-x")
            .arg(cols.to_string())
            .arg("-y")
            .arg(rows.to_string());
        if let Some(cwd) = &profile.cwd {
            create.arg("-c").arg(cwd);
        }
        create.arg(shell_cmd);
        let status = create
            .status()
            .map_err(|e| anyhow!("failed to run tmux new-session: {e}"))?;
        if !status.success() {
            return Err(anyhow!("tmux new-session for '{tmux}' failed"));
        }

        Self::attach_tmux(id, name, tags, profile, &tmux, now_secs(), rows, cols)
    }

    /// Re-adopt an already-running tmux session after a daemon restart. Returns
    /// `Ok(None)` if the tmux session no longer exists (so the caller drops the
    /// stale metadata entry).
    pub fn adopt_tmux(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        created_at: u64,
        rows: u16,
        cols: u16,
    ) -> Result<Option<Arc<Session>>> {
        if ensure_tmux().is_err() {
            return Ok(None);
        }
        let tmux = tmux_session_name(&id);
        if !tmux_has_session(&tmux) {
            return Ok(None);
        }
        let session = Self::attach_tmux(id, name, tags, profile, &tmux, created_at, rows, cols)?;
        // Repopulate scrollback from tmux's own history so a re-adopted session
        // shows prior context immediately.
        if let Some(history) = tmux_capture(&tmux) {
            session.seed_buffer(history.into_bytes());
        }
        Ok(Some(session))
    }

    /// Open a PTY running `tmux attach` against an existing tmux session and
    /// wire up the reader/broadcast machinery.
    #[allow(clippy::too_many_arguments)]
    fn attach_tmux(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        tmux: &str,
        created_at: u64,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Session>> {
        let mut cmd = CommandBuilder::new("tmux");
        // `-d` detaches any other client so this PTY drives the window size,
        // avoiding tmux shrinking to the smallest attached client.
        cmd.args(["attach-session", "-d", "-t", tmux]);
        Self::wire(id, name, tags, profile, Backend::Tmux, created_at, rows, cols, cmd, command_line(profile))
    }

    /// Shared wiring: open a PTY, spawn `cmd` in it, and start the reader thread
    /// that feeds the scrollback buffer and broadcast channel.
    #[allow(clippy::too_many_arguments)]
    fn wire(
        id: String,
        name: String,
        tags: Vec<String>,
        profile: &Profile,
        backend: Backend,
        created_at: u64,
        rows: u16,
        cols: u16,
        cmd: CommandBuilder,
        command_line: String,
    ) -> Result<Arc<Session>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

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
            created_at,
            tags,
            backend,
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

    /// Pre-fill the scrollback buffer (used when adopting a tmux session so its
    /// existing history is visible to the first client).
    fn seed_buffer(&self, mut bytes: Vec<u8>) {
        if bytes.len() > SCROLLBACK_CAP {
            let overflow = bytes.len() - SCROLLBACK_CAP;
            bytes.drain(0..overflow);
        }
        let mut buf = self.buffer.lock();
        // Prepend history ahead of whatever the live attach has already produced.
        bytes.extend_from_slice(&buf);
        *buf = bytes;
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

    /// Terminate the session. For tmux, kill the underlying tmux session so the
    /// agent process actually dies (just closing our attach PTY would detach,
    /// leaving it running — which is the whole point of the backend).
    pub fn kill(&self) {
        if self.backend == Backend::Tmux {
            let tmux = tmux_session_name(&self.id);
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &tmux])
                .status();
        }
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
            tags: self.tags.clone(),
            state: self.state(),
            exit_code: self.exit_code(),
            backend: self.backend,
        }
    }
}

/// The displayed command line for a profile (program + args joined).
fn command_line(profile: &Profile) -> String {
    let mut parts = vec![profile.command.clone()];
    parts.extend(profile.args.iter().cloned());
    parts.join(" ")
}

/// Build the single shell-command string tmux runs via `/bin/sh -c`, injecting
/// env vars with `env` and shell-quoting every piece.
fn tmux_shell_command(profile: &Profile) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !profile.env.is_empty() {
        parts.push("env".to_string());
        // Sort for deterministic output (tests / readable logs).
        let mut envs: Vec<_> = profile.env.iter().collect();
        envs.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in envs {
            parts.push(format!("{k}={}", sh_quote(v)));
        }
    }
    parts.push(sh_quote(&profile.command));
    for a in &profile.args {
        parts.push(sh_quote(a));
    }
    parts.join(" ")
}

/// Minimal POSIX single-quote shell escaping.
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Verify tmux is on PATH, with a clear error otherwise.
fn ensure_tmux() -> Result<()> {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|_| ())
        .map_err(|_| anyhow!("the 'tmux' backend requires tmux on PATH (install it, or use backend = \"pty\")"))
}

/// True if a tmux session with this name exists.
pub fn tmux_has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Capture the current pane contents (with history) for scrollback seeding.
fn tmux_capture(name: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", name, "-S", "-200"])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
