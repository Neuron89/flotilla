//! Daemon configuration: bind address, optional bearer token, data dir, and
//! the session "profiles" that describe how to launch each kind of agent.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::pty::Backend;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Address to bind the HTTP/WS server to. Default 0.0.0.0:7878 (tailnet-facing).
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Optional bearer token. When non-empty, every request must send
    /// `Authorization: Bearer <token>`. Tailscale already gates the network;
    /// this is defense-in-depth. Empty = disabled (fine for local dev).
    #[serde(default)]
    pub token: String,

    /// Where to persist the session metadata snapshot.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Friendly name for this host shown in the fleet view. Defaults to hostname.
    #[serde(default)]
    pub host_label: Option<String>,

    /// Launch profiles, e.g. "claude", "hermes", "shell".
    #[serde(default = "default_profiles")]
    pub profiles: Vec<Profile>,

    /// Phone push notifications (Phase 2).
    #[serde(default)]
    pub notifications: Notifications,
}

/// ntfy-backed push notifications. When a session transitions into a state in
/// `notify_on` (e.g. `waiting`), the agent POSTs to its ntfy topic so your
/// phone buzzes — even when you've walked away from the machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Notifications {
    /// Master switch.
    #[serde(default)]
    pub enabled: bool,
    /// ntfy server base URL. Self-hosted (e.g. http://ntfy.tailnet) or https://ntfy.sh.
    #[serde(default = "default_ntfy_url")]
    pub ntfy_url: String,
    /// ntfy topic to publish to (pick something unguessable on ntfy.sh).
    #[serde(default)]
    pub topic: String,
    /// Optional ntfy access token (Bearer) for protected topics.
    #[serde(default)]
    pub token: String,
    /// ntfy priority: min|low|default|high|urgent.
    #[serde(default = "default_priority")]
    pub priority: String,
    /// Which state transitions trigger a push: any of "waiting", "exited".
    #[serde(default = "default_notify_on")]
    pub notify_on: Vec<String>,
    /// This agent's reachable web URL (e.g. http://home-server:7878), used to
    /// build the notification's tap-to-open deep link. Empty = no click action.
    #[serde(default)]
    pub public_url: String,
    /// How often to poll session states for transitions, in milliseconds.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

fn default_ntfy_url() -> String {
    "https://ntfy.sh".to_string()
}
fn default_priority() -> String {
    "high".to_string()
}
fn default_notify_on() -> Vec<String> {
    vec!["waiting".to_string()]
}
fn default_poll_ms() -> u64 {
    1500
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            enabled: false,
            ntfy_url: default_ntfy_url(),
            topic: String::new(),
            token: String::new(),
            priority: default_priority(),
            notify_on: default_notify_on(),
            public_url: String::new(),
            poll_ms: default_poll_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    /// Profile key used by the API (e.g. "claude").
    pub name: String,
    /// Program to execute.
    pub command: String,
    /// Arguments passed to the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory. For Claude/Hermes this is where the shared
    /// Obsidian-backed agent workspace lives so MCP/KB context is in scope.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables to inject into the session.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Human description shown in the UIs.
    #[serde(default)]
    pub description: Option<String>,
    /// Session backend: "pty" (default, daemon owns the PTY) or "tmux"
    /// (restart-proof, Linux/macOS). Ignored on Windows, which always uses pty.
    #[serde(default)]
    pub backend: Backend,
}

impl Profile {
    /// The backend actually used, accounting for platform support. tmux is not
    /// available on Windows, so a tmux profile transparently falls back to pty
    /// there rather than failing to spawn.
    pub fn effective_backend(&self) -> Backend {
        if cfg!(windows) {
            Backend::Pty
        } else {
            self.backend
        }
    }
}

fn default_bind() -> String {
    "0.0.0.0:7878".to_string()
}

fn default_data_dir() -> PathBuf {
    dirs_data().join("flotilla")
}

fn dirs_data() -> PathBuf {
    // Cross-platform best-effort data dir without pulling in the `dirs` crate.
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("share");
    }
    PathBuf::from(".")
}

/// Default profiles shipped when no config file exists. The shell profile uses
/// the platform's native shell so the daemon is useful out of the box.
fn default_profiles() -> Vec<Profile> {
    let shell = Profile {
        name: "shell".into(),
        command: if cfg!(windows) { "powershell.exe".into() } else { "bash".into() },
        args: if cfg!(windows) { vec!["-NoLogo".into()] } else { vec!["-l".into()] },
        cwd: None,
        env: HashMap::new(),
        description: Some("Native interactive shell".into()),
        backend: Backend::Pty,
    };
    let claude = Profile {
        name: "claude".into(),
        command: "claude".into(),
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        description: Some("Claude Code session (inherits user-scope MCP incl. Obsidian KB)".into()),
        backend: Backend::Pty,
    };
    let hermes = Profile {
        name: "hermes".into(),
        command: if cfg!(windows) { "hermes.exe".into() } else { "hermes".into() },
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        description: Some("Hermes agent session".into()),
        backend: Backend::Pty,
    };
    vec![shell, claude, hermes]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: default_bind(),
            token: String::new(),
            data_dir: default_data_dir(),
            host_label: None,
            profiles: default_profiles(),
            notifications: Notifications::default(),
        }
    }
}

impl Config {
    /// Load from a TOML file. Returns the default config if the path is None or
    /// the file does not exist.
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
        let Some(path) = path else {
            return Ok(Config::default());
        };
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn label(&self) -> String {
        self.host_label.clone().unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown-host".to_string())
        })
    }
}
