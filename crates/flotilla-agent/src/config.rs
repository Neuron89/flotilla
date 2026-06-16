//! Daemon configuration: bind address, optional bearer token, data dir, and
//! the session "profiles" that describe how to launch each kind of agent.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
    };
    let claude = Profile {
        name: "claude".into(),
        command: "claude".into(),
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        description: Some("Claude Code session (inherits user-scope MCP incl. Obsidian KB)".into()),
    };
    let hermes = Profile {
        name: "hermes".into(),
        command: if cfg!(windows) { "hermes.exe".into() } else { "hermes".into() },
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        description: Some("Hermes agent session".into()),
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
