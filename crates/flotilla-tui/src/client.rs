//! Host configuration, the HTTP client for talking to agents, and key→bytes
//! encoding used during interactive attach.

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Host {
    pub label: String,
    pub url: String,
    #[serde(default)]
    pub token: String,
}

/// One member of a template: a session to spawn on a named host.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplateItem {
    /// Host `label` this session is spawned on (matches a `[[hosts]]` entry).
    pub host: String,
    /// Profile to launch on that host.
    pub profile: String,
    /// Optional explicit session name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional tags applied to the spawned session.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A named set of sessions spawned across hosts in one action (Phase 3).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub items: Vec<TemplateItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HostsFile {
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub templates: Vec<Template>,
}

impl HostsFile {
    /// Load hosts + templates. Falls back to a single local agent if absent.
    pub fn load(path: Option<&Path>) -> Result<HostsFile> {
        if let Some(p) = path {
            if p.exists() {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading {}", p.display()))?;
                let hf: HostsFile = toml::from_str(&text)?;
                if !hf.hosts.is_empty() {
                    return Ok(hf);
                }
            }
        }
        Ok(HostsFile {
            hosts: vec![Host {
                label: "local".into(),
                url: "http://127.0.0.1:7878".into(),
                token: String::new(),
            }],
            templates: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub profile: String,
    #[allow(dead_code)]
    pub command: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub state: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub description: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Client {
        Client {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("http client"),
        }
    }

    fn req(&self, host: &Host, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, format!("{}{}", host.url, path));
        if !host.token.is_empty() {
            rb = rb.bearer_auth(&host.token);
        }
        rb
    }

    pub async fn health(&self, host: &Host) -> Result<Health> {
        Ok(self
            .req(host, reqwest::Method::GET, "/api/health")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn sessions(&self, host: &Host) -> Result<Vec<SessionInfo>> {
        Ok(self
            .req(host, reqwest::Method::GET, "/api/sessions")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn profiles(&self, host: &Host) -> Result<Vec<ProfileInfo>> {
        Ok(self
            .req(host, reqwest::Method::GET, "/api/profiles")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn create(&self, host: &Host, profile: &str) -> Result<SessionInfo> {
        self.create_full(host, profile, None, &[]).await
    }

    /// Spawn with an optional explicit name and tags (used by templates).
    pub async fn create_full(
        &self,
        host: &Host,
        profile: &str,
        name: Option<&str>,
        tags: &[String],
    ) -> Result<SessionInfo> {
        let mut body = serde_json::json!({ "profile": profile, "tags": tags });
        if let Some(n) = name {
            body["name"] = serde_json::Value::String(n.to_string());
        }
        Ok(self
            .req(host, reqwest::Method::POST, "/api/sessions")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn kill(&self, host: &Host, id: &str) -> Result<()> {
        self.req(host, reqwest::Method::DELETE, &format!("/api/sessions/{id}"))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Translate a crossterm key event into the bytes a PTY expects. Covers the
/// common cases needed to drive an agent: text, control combos, arrows, etc.
pub fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-A..Ctrl-Z and a few symbols map to 0x01..0x1f.
                let b = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                if c.is_ascii_alphabetic() {
                    out.push(b);
                } else {
                    match c {
                        ' ' => out.push(0),
                        '[' => out.push(0x1b),
                        '\\' => out.push(0x1c),
                        ']' => out.push(0x1d),
                        _ => out.extend_from_slice(c.to_string().as_bytes()),
                    }
                }
            } else {
                if alt {
                    out.push(0x1b);
                }
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        _ => {}
    }
    out
}
