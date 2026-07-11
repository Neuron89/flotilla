//! Phone push notifications via ntfy. A background task polls session states
//! and, on a transition into a watched state (e.g. `waiting`), publishes to the
//! configured ntfy topic so your phone buzzes when an agent needs you.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Notifications;
use crate::manager::Manager;
use crate::pty::SessionInfo;

/// Start the watcher if notifications are configured. No-op otherwise.
pub fn spawn_watcher(manager: Arc<Manager>) {
    let cfg = manager.config.notifications.clone();
    if !cfg.enabled {
        tracing::info!("notifications: disabled");
        return;
    }
    if cfg.topic.is_empty() {
        tracing::warn!("notifications: enabled but no topic set — skipping");
        return;
    }
    tokio::spawn(async move { watch(manager, cfg).await });
}

async fn watch(manager: Arc<Manager>, cfg: Notifications) {
    let client = reqwest::Client::new();
    let label = manager.config.label();
    let mut last: HashMap<String, String> = HashMap::new();
    // Seed the first observation without notifying, so sessions already present
    // at startup don't all fire at once.
    let mut first = true;
    let mut interval = tokio::time::interval(Duration::from_millis(cfg.poll_ms.max(500)));
    tracing::info!(
        "notifications: watching {}/{} on {:?}",
        cfg.ntfy_url.trim_end_matches('/'),
        cfg.topic,
        cfg.notify_on
    );

    loop {
        interval.tick().await;
        let infos = manager.list();
        let mut seen = HashSet::new();
        for info in &infos {
            seen.insert(info.id.clone());
            let state = info.state.as_str().to_string();
            let prev = last.insert(info.id.clone(), state.clone());
            if first {
                continue;
            }
            let entered = prev.as_deref() != Some(state.as_str());
            let watched = cfg.notify_on.iter().any(|s| s.eq_ignore_ascii_case(&state));
            if entered && watched {
                if let Err(e) = send(&client, &cfg, &label, info).await {
                    tracing::warn!("notifications: ntfy send failed: {e}");
                } else {
                    tracing::info!("notifications: pushed '{}' → {}", info.name, state);
                }
            }
        }
        last.retain(|k, _| seen.contains(k));
        first = false;
    }
}

/// Send a one-off test push using the configured backend.
pub async fn test_push(manager: &Manager) -> anyhow::Result<()> {
    let cfg = &manager.config.notifications;
    if !cfg.enabled || cfg.topic.is_empty() {
        anyhow::bail!("notifications not configured (need enabled = true and a topic)");
    }
    let info = SessionInfo {
        id: "test".into(),
        name: "test".into(),
        profile: "flotilla".into(),
        command: String::new(),
        cwd: None,
        created_at: 0,
        tags: vec!["test".into()],
        state: crate::pty::State::Waiting,
        exit_code: None,
    };
    send(&reqwest::Client::new(), cfg, &manager.config.label(), &info).await
}

async fn send(
    client: &reqwest::Client,
    cfg: &Notifications,
    label: &str,
    info: &SessionInfo,
) -> anyhow::Result<()> {
    let url = format!("{}/{}", cfg.ntfy_url.trim_end_matches('/'), cfg.topic);

    let (tag, verb) = match info.state.as_str() {
        "waiting" => ("bell", "is waiting on you"),
        "exited" => ("checkered_flag", "exited"),
        other => ("information_source", other),
    };

    // ASCII separator only — HTTP header values are latin-1, not UTF-8.
    let title = format!("{label}: {}", info.name);
    let mut body = format!("{} ({}) {verb}", info.name, info.profile);
    if let Some(code) = info.exit_code {
        body.push_str(&format!(" [exit {code}]"));
    }
    if !info.tags.is_empty() {
        body.push_str(&format!("\n#{}", info.tags.join(" #")));
    }

    let mut req = client
        .post(&url)
        .header("Title", title)
        .header("Priority", cfg.priority.as_str())
        .header("Tags", tag)
        .body(body);

    if !cfg.token.is_empty() {
        req = req.bearer_auth(&cfg.token);
    }
    if !cfg.public_url.is_empty() {
        // Deep link carries ONLY the session id — never the bearer token. ntfy
        // messages transit third-party infra and land in notification history.
        let click = format!("{}/?open={}", cfg.public_url.trim_end_matches('/'), info.id);
        req = req
            .header("Click", click.clone())
            .header("Actions", format!("view, Open session, {click}"));
    }

    req.send().await?.error_for_status()?;
    Ok(())
}
