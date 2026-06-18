//! Owns the live session map and the best-effort metadata snapshot on disk.

use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::pty::{Backend, Session, SessionInfo};

pub struct Manager {
    pub config: Config,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
}

impl Manager {
    pub fn new(config: Config) -> Arc<Manager> {
        if let Err(e) = std::fs::create_dir_all(&config.data_dir) {
            tracing::warn!("could not create data dir {:?}: {e}", config.data_dir);
        }
        let mgr = Arc::new(Manager {
            config,
            sessions: RwLock::new(HashMap::new()),
        });
        mgr.adopt_persisted();
        mgr
    }

    /// On startup, re-adopt tmux-backed sessions that outlived the previous
    /// daemon process. PTY-backed sessions cannot survive a restart, so stale
    /// entries for them are simply dropped. This is the restart-proof payoff of
    /// the tmux backend.
    fn adopt_persisted(&self) {
        let path = self.config.data_dir.join("sessions.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(infos) = serde_json::from_str::<Vec<SessionInfo>>(&text) else {
            tracing::warn!("could not parse {path:?}; skipping session adoption");
            return;
        };
        let mut adopted = 0usize;
        for info in infos {
            if info.backend != Backend::Tmux {
                continue;
            }
            let Some(profile) = self.config.profile(&info.profile) else {
                tracing::warn!(
                    "session '{}' references unknown profile '{}'; not adopting",
                    info.name,
                    info.profile
                );
                continue;
            };
            match Session::adopt_tmux(
                info.id.clone(),
                info.name.clone(),
                info.tags.clone(),
                profile,
                info.created_at,
                40,
                120,
            ) {
                Ok(Some(session)) => {
                    self.sessions.write().insert(info.id.clone(), session);
                    adopted += 1;
                }
                Ok(None) => {} // tmux session gone — drop it
                Err(e) => tracing::warn!("failed to adopt '{}': {e}", info.name),
            }
        }
        if adopted > 0 {
            tracing::info!("adopted {adopted} surviving tmux session(s) after restart");
            self.persist();
        }
    }

    pub fn create(&self, profile_name: &str, name: Option<String>, tags: Vec<String>, rows: u16, cols: u16) -> Result<SessionInfo> {
        let profile = self
            .config
            .profile(profile_name)
            .ok_or_else(|| anyhow!("unknown profile '{profile_name}'"))?
            .clone();

        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let name = name.unwrap_or_else(|| format!("{profile_name}-{id}"));
        let session = Session::spawn(id.clone(), name, tags, &profile, rows.max(1), cols.max(1))?;
        let info = session.info();
        self.sessions.write().insert(id, session);
        self.persist();
        Ok(info)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.read().get(id).cloned()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self.sessions.read().values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub fn kill(&self, id: &str) -> Result<()> {
        let session = self.get(id).ok_or_else(|| anyhow!("no such session"))?;
        session.kill();
        self.sessions.write().remove(id);
        self.persist();
        Ok(())
    }

    /// Best-effort metadata snapshot. PTYs do not survive a daemon restart, so
    /// this is for observability/audit, not live restore.
    fn persist(&self) {
        let infos = self.list();
        let path = self.config.data_dir.join("sessions.json");
        if let Ok(json) = serde_json::to_string_pretty(&infos) {
            let _ = std::fs::write(path, json);
        }
    }
}
