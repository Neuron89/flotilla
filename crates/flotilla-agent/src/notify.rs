//! Push notifications. A background task polls session states and, on a
//! transition into a watched state (e.g. `waiting`), fans out to every enabled
//! sink:
//!
//! * **ntfy** (Phase 2) — POST to an ntfy topic so the phone app buzzes.
//! * **Web Push** (Phase 3) — VAPID-encrypted push to every browser/PWA that
//!   subscribed via the API, so an installed PWA gets a native notification
//!   even with no ntfy app.
//!
//! Both sinks share the `[notifications]` trigger settings (`notify_on`,
//! `poll_ms`, `public_url`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use web_push::{
    ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushClient, WebPushMessageBuilder,
};

use crate::config::{Notifications, WebPush};
use crate::manager::Manager;
use crate::pty::SessionInfo;

/// Start the watcher if any sink is configured. No-op otherwise.
pub fn spawn_watcher(manager: Arc<Manager>) {
    let ntfy_on = manager.config.notifications.enabled && !manager.config.notifications.topic.is_empty();
    let push_on = manager.config.web_push.enabled && !manager.config.web_push.private_key.is_empty();
    if !ntfy_on && !push_on {
        tracing::info!("notifications: disabled");
        return;
    }
    tracing::info!(
        "notifications: enabled (ntfy: {}, web-push: {})",
        if ntfy_on { "on" } else { "off" },
        if push_on { "on" } else { "off" }
    );
    tokio::spawn(async move { watch(manager).await });
}

async fn watch(manager: Arc<Manager>) {
    let client = reqwest::Client::new();
    let cfg = manager.config.notifications.clone();
    let label = manager.config.label();
    let mut last: HashMap<String, String> = HashMap::new();
    // Seed the first observation without notifying, so sessions already present
    // at startup don't all fire at once.
    let mut first = true;
    let mut interval = tokio::time::interval(Duration::from_millis(cfg.poll_ms.max(500)));

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
                fan_out(&client, &manager, &cfg, &label, info).await;
            }
        }
        last.retain(|k, _| seen.contains(k));
        first = false;
    }
}

/// Push one event to every enabled sink.
async fn fan_out(
    client: &reqwest::Client,
    manager: &Manager,
    cfg: &Notifications,
    label: &str,
    info: &SessionInfo,
) {
    if cfg.enabled && !cfg.topic.is_empty() {
        if let Err(e) = send_ntfy(client, cfg, label, info).await {
            tracing::warn!("notifications: ntfy send failed: {e}");
        } else {
            tracing::info!("notifications: ntfy pushed '{}' → {}", info.name, info.state.as_str());
        }
    }
    let wp = &manager.config.web_push;
    if wp.enabled && !wp.private_key.is_empty() {
        send_web_push(manager, wp, &cfg.public_url, label, info).await;
    }
}

/// Fire a one-off test event to whichever sinks are configured.
pub async fn test_push(manager: &Manager) -> anyhow::Result<()> {
    let cfg = &manager.config.notifications;
    let wp = &manager.config.web_push;
    let ntfy_on = cfg.enabled && !cfg.topic.is_empty();
    let push_on = wp.enabled && !wp.private_key.is_empty();
    if !ntfy_on && !push_on {
        anyhow::bail!("no push sink configured (enable ntfy with a topic, or web_push with keys)");
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
        backend: crate::pty::Backend::Pty,
    };
    if ntfy_on {
        send_ntfy(&reqwest::Client::new(), cfg, &manager.config.label(), &info).await?;
    }
    if push_on {
        send_web_push(manager, wp, &cfg.public_url, &manager.config.label(), &info).await;
    }
    Ok(())
}

/// Build the human-facing notification text shared by both sinks.
fn render(label: &str, info: &SessionInfo) -> (String, String, &'static str) {
    let (tag, verb) = match info.state.as_str() {
        "waiting" => ("bell", "is waiting on you"),
        "exited" => ("checkered_flag", "exited"),
        other => ("information_source", other),
    };
    let title = format!("{label}: {}", info.name);
    let mut body = format!("{} ({}) {verb}", info.name, info.profile);
    if let Some(code) = info.exit_code {
        body.push_str(&format!(" [exit {code}]"));
    }
    if !info.tags.is_empty() {
        body.push_str(&format!("\n#{}", info.tags.join(" #")));
    }
    (title, body, tag)
}

async fn send_ntfy(
    client: &reqwest::Client,
    cfg: &Notifications,
    label: &str,
    info: &SessionInfo,
) -> anyhow::Result<()> {
    let url = format!("{}/{}", cfg.ntfy_url.trim_end_matches('/'), cfg.topic);
    let (title, body, tag) = render(label, info);

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
        let click = format!("{}/?open={}", cfg.public_url.trim_end_matches('/'), info.id);
        req = req
            .header("Click", click.clone())
            .header("Actions", format!("view, Open session, {click}"));
    }

    req.send().await?.error_for_status()?;
    Ok(())
}

/// Encrypt and deliver a Web Push to every stored subscription. Subscriptions
/// the push service reports as gone (404/410) are pruned.
async fn send_web_push(
    manager: &Manager,
    wp: &WebPush,
    public_url: &str,
    label: &str,
    info: &SessionInfo,
) {
    let subs = manager.push_subs();
    if subs.is_empty() {
        return;
    }
    let (title, body, _tag) = render(label, info);
    let url = if public_url.is_empty() {
        format!("/?open={}", info.id)
    } else {
        format!("{}/?open={}", public_url.trim_end_matches('/'), info.id)
    };
    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "url": url,
        "tag": info.id,
    })
    .to_string();

    let http = web_push::HyperWebPushClient::new();
    for sub in subs {
        match build_message(wp, &sub, payload.as_bytes()) {
            Ok(msg) => match http.send(msg).await {
                Ok(()) => {}
                Err(web_push::WebPushError::EndpointNotFound(_))
                | Err(web_push::WebPushError::EndpointNotValid(_)) => {
                    tracing::info!("web-push: pruning gone subscription");
                    manager.remove_push_sub(&sub.endpoint);
                }
                Err(e) => tracing::warn!("web-push: send failed: {e}"),
            },
            Err(e) => tracing::warn!("web-push: build failed: {e}"),
        }
    }
}

fn build_message(
    wp: &WebPush,
    sub: &SubscriptionInfo,
    payload: &[u8],
) -> Result<web_push::WebPushMessage, web_push::WebPushError> {
    let mut sig = VapidSignatureBuilder::from_base64(&wp.private_key, sub)?;
    sig.add_claim("sub", wp.subject.as_str());
    let signature = sig.build()?;

    let mut builder = WebPushMessageBuilder::new(sub);
    builder.set_payload(ContentEncoding::Aes128Gcm, payload);
    builder.set_vapid_signature(signature);
    builder.build()
}

/// Generate a fresh VAPID keypair (pure Rust, no openssl) and return
/// `(private_key, public_key)` as URL-safe base64 without padding — the format
/// `web-push` and the browser's `applicationServerKey` expect.
pub fn generate_vapid_keys() -> (String, String) {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    let secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let private = b64.encode(secret.to_bytes());
    let public = b64.encode(secret.public_key().to_encoded_point(false).as_bytes());
    (private, public)
}
