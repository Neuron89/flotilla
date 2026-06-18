//! HTTP + WebSocket API and the embedded web/mobile client.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::access::Identity;
use crate::manager::Manager;

/// The single-file web/mobile client, baked into the binary so the daemon is
/// self-contained. Served at `/`.
const WEB_INDEX: &str = include_str!("../web/index.html");

pub fn router(manager: Arc<Manager>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/profiles", get(profiles))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/:id", axum::routing::delete(kill_session))
        .route("/sessions/:id/input", post(send_input))
        .route("/sessions/:id/buffer", get(get_buffer))
        .route("/sessions/:id/attach", get(ws_attach))
        .route("/notify-test", post(notify_test))
        .route("/me", get(me))
        .route("/push/vapid", get(push_vapid))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
        .layer(middleware::from_fn_with_state(manager.clone(), auth))
        .with_state(manager.clone());

    Router::new()
        .route("/", get(index))
        .route("/manifest.json", get(manifest))
        .route("/sw.js", get(service_worker))
        .nest("/api", api)
        .layer(CorsLayer::very_permissive())
}

async fn index() -> Html<&'static str> {
    Html(WEB_INDEX)
}

const MANIFEST: &str = r##"{"name":"Flotilla","short_name":"Flotilla","start_url":"/","display":"standalone","background_color":"#0b0f17","theme_color":"#0b0f17","icons":[]}"##;

async fn manifest() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST)
}

/// Service worker (Phase 3). Served at the origin root so its scope is `/`.
/// Shows a notification on `push` and deep-links to the session on click.
const SERVICE_WORKER: &str = r#"
self.addEventListener('push', (event) => {
  let d = {};
  try { d = event.data ? event.data.json() : {}; } catch (e) {}
  const title = d.title || 'Flotilla';
  const opts = {
    body: d.body || '',
    tag: d.tag,
    renotify: true,
    data: { url: d.url || '/' },
  };
  event.waitUntil(self.registration.showNotification(title, opts));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil((async () => {
    const all = await clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const c of all) {
      if ('focus' in c) {
        try { await c.navigate(url); } catch (e) {}
        return c.focus();
      }
    }
    return clients.openWindow(url);
  })());
});
"#;

async fn service_worker() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        SERVICE_WORKER,
    )
}

/// Hand the browser the VAPID public key it needs to subscribe. 404 when web
/// push isn't configured so the client can hide the toggle.
async fn push_vapid(State(mgr): State<Arc<Manager>>) -> Response {
    let wp = &mgr.config.web_push;
    if !wp.enabled || wp.public_key.is_empty() {
        return (StatusCode::NOT_FOUND, "web push not configured").into_response();
    }
    Json(json!({ "publicKey": wp.public_key })).into_response()
}

/// Store a browser PushSubscription (its JSON deserializes straight into
/// `SubscriptionInfo`).
async fn push_subscribe(
    State(mgr): State<Arc<Manager>>,
    Json(sub): Json<web_push::SubscriptionInfo>,
) -> Response {
    if sub.endpoint.is_empty() || sub.keys.p256dh.is_empty() || sub.keys.auth.is_empty() {
        return (StatusCode::BAD_REQUEST, "incomplete subscription").into_response();
    }
    mgr.add_push_sub(sub);
    StatusCode::CREATED.into_response()
}

#[derive(Deserialize)]
struct Unsub {
    endpoint: String,
}

async fn push_unsubscribe(State(mgr): State<Arc<Manager>>, Json(body): Json<Unsub>) -> Response {
    mgr.remove_push_sub(&body.endpoint);
    StatusCode::NO_CONTENT.into_response()
}

/// Fire a sample push so the user can confirm their ntfy config end-to-end.
async fn notify_test(State(mgr): State<Arc<Manager>>) -> Response {
    match crate::notify::test_push(&mgr).await {
        Ok(()) => (StatusCode::OK, "sent").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// WebSocket subprotocol marker the browser/TUI offers alongside the bearer
/// token, so the token travels in `Sec-WebSocket-Protocol` (which browsers can
/// set) instead of the URL query string (which leaks into logs).
pub const WS_BEARER_PROTO: &str = "flotilla.bearer";

/// Auth gate. A request is admitted if it satisfies *either* Cloudflare Access
/// (a valid JWT in `Cf-Access-Jwt-Assertion` or the `CF_Authorization` cookie,
/// giving per-user identity) *or* the shared bearer token (via
/// `Authorization: Bearer`, the `flotilla.bearer` WebSocket subprotocol, or the
/// deprecated `?token=` query). When neither Access nor a token is configured,
/// the daemon is open (local dev) and the caller is recorded as anonymous.
async fn auth(State(mgr): State<Arc<Manager>>, mut req: axum::extract::Request, next: Next) -> Response {
    // 1. Cloudflare Access (preferred when configured).
    if let Some(verifier) = &mgr.access {
        if let Some(jwt) = access_jwt(&req) {
            verifier.refresh_if_stale().await;
            match verifier.verify(&jwt).await {
                Ok(id) => {
                    req.extensions_mut().insert(id);
                    return next.run(req).await;
                }
                Err(e) => tracing::debug!("access: rejected jwt: {e}"),
            }
        }
    }

    // 2. Shared bearer token.
    let expected = &mgr.config.token;
    if !expected.is_empty() {
        if let Some(provided) = bearer_token(&req) {
            if provided == *expected {
                req.extensions_mut().insert(Identity { email: "token".into(), via: "token" });
                return next.run(req).await;
            }
        }
    } else if mgr.access.is_none() {
        // No auth configured at all → open (local dev).
        req.extensions_mut().insert(Identity { email: "anonymous".into(), via: "open" });
        return next.run(req).await;
    }

    (StatusCode::UNAUTHORIZED, "authentication required").into_response()
}

/// Extract a Cloudflare Access JWT from the header or the `CF_Authorization` cookie.
fn access_jwt(req: &axum::extract::Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get("cf-access-jwt-assertion")
        .and_then(|v| v.to_str().ok())
    {
        return Some(v.to_string());
    }
    let cookies = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok())?;
    for c in cookies.split(';') {
        let c = c.trim();
        if let Some(val) = c.strip_prefix("CF_Authorization=") {
            return Some(val.to_string());
        }
    }
    None
}

/// Extract the shared bearer token from the Authorization header, the
/// `flotilla.bearer` WS subprotocol, or the deprecated `?token=` query.
fn bearer_token(req: &axum::extract::Request) -> Option<String> {
    if let Some(t) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        return Some(t.to_string());
    }
    if let Some(t) = ws_subprotocol_token(req) {
        return Some(t);
    }
    // Deprecated: token in the query string (kept for backward compatibility).
    req.uri()
        .query()
        .and_then(|q| url_query(q).get("token").cloned())
}

/// Parse the bearer token offered as a second value alongside `flotilla.bearer`
/// in `Sec-WebSocket-Protocol`.
fn ws_subprotocol_token(req: &axum::extract::Request) -> Option<String> {
    let protos = req
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())?;
    let parts: Vec<&str> = protos.split(',').map(|s| s.trim()).collect();
    if !parts.contains(&WS_BEARER_PROTO) {
        return None;
    }
    parts
        .into_iter()
        .find(|p| *p != WS_BEARER_PROTO && !p.is_empty())
        .map(|s| s.to_string())
}

fn url_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| {
            let mut it = kv.splitn(2, '=');
            Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// The authenticated caller (Cloudflare Access email, or how the token auth'd).
async fn me(ext: Option<Extension<Identity>>) -> impl IntoResponse {
    match ext {
        Some(Extension(id)) => Json(json!({ "email": id.email, "via": id.via })),
        None => Json(json!({ "email": "anonymous", "via": "open" })),
    }
}

async fn health(State(mgr): State<Arc<Manager>>) -> impl IntoResponse {
    Json(json!({
        "label": mgr.config.label(),
        "hostname": hostname::get().ok().and_then(|h| h.into_string().ok()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": mgr.list().len(),
    }))
}

async fn profiles(State(mgr): State<Arc<Manager>>) -> impl IntoResponse {
    let list: Vec<_> = mgr
        .config
        .profiles
        .iter()
        .map(|p| json!({ "name": p.name, "description": p.description, "command": p.command }))
        .collect();
    Json(list)
}

async fn list_sessions(State(mgr): State<Arc<Manager>>) -> impl IntoResponse {
    Json(mgr.list())
}

#[derive(Deserialize)]
struct CreateBody {
    profile: String,
    name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default = "default_cols")]
    cols: u16,
}
fn default_rows() -> u16 {
    40
}
fn default_cols() -> u16 {
    120
}

async fn create_session(
    State(mgr): State<Arc<Manager>>,
    Json(body): Json<CreateBody>,
) -> Response {
    match mgr.create(&body.profile, body.name, body.tags, body.rows, body.cols) {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn kill_session(State(mgr): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match mgr.kill(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

async fn get_buffer(State(mgr): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match mgr.get(&id) {
        Some(s) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            s.snapshot(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

/// Fire-and-forget input — used by the phone "send a prompt" box. Appends a
/// carriage return so the line is submitted if the caller didn't include one.
async fn send_input(
    State(mgr): State<Arc<Manager>>,
    Path(id): Path<String>,
    body: String,
) -> Response {
    let Some(session) = mgr.get(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let mut bytes = body.into_bytes();
    if !bytes.ends_with(b"\n") && !bytes.ends_with(b"\r") {
        bytes.push(b'\r');
    }
    let writer = session.writer();
    {
        let mut w = writer.lock();
        if w.write_all(&bytes).and_then(|_| w.flush()).is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed").into_response();
        }
    }
    StatusCode::ACCEPTED.into_response()
}

async fn ws_attach(
    ws: WebSocketUpgrade,
    State(mgr): State<Arc<Manager>>,
    Path(id): Path<String>,
    Query(_q): Query<HashMap<String, String>>,
    _headers: HeaderMap,
) -> Response {
    let Some(session) = mgr.get(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    // If the client offered the bearer subprotocol, select it so the handshake
    // completes (browsers abort a WS whose offered protocol isn't echoed back).
    ws.protocols([WS_BEARER_PROTO])
        .on_upgrade(move |socket| handle_attach(socket, session))
}

/// Bidirectional bridge between a WebSocket client (xterm.js or the TUI) and the
/// PTY. Protocol: server→client binary = PTY output. client→server binary =
/// input bytes; client→server text (JSON `{"type":"resize","cols":..,"rows":..}`)
/// = control.
async fn handle_attach(socket: WebSocket, session: Arc<crate::pty::Session>) {
    let (mut sender, mut receiver) = socket.split();

    // Replay scrollback so a fresh client sees existing context.
    let snapshot = session.snapshot();
    if !snapshot.is_empty() {
        if sender.send(Message::Binary(snapshot.to_vec())).await.is_err() {
            return;
        }
    }

    let mut rx = session.subscribe();

    // Output pump: PTY → client.
    let out = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if sender.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        let _ = sender.close().await;
    });

    // Input pump: client → PTY.
    let writer = session.writer();
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Binary(data) => {
                let mut w = writer.lock();
                let _ = w.write_all(&data);
                let _ = w.flush();
            }
            Message::Text(text) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if val.get("type").and_then(|v| v.as_str()) == Some("resize") {
                        let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
                        let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(40) as u16;
                        session.resize(rows, cols);
                    }
                } else {
                    // Treat unstructured text as input too.
                    let mut w = writer.lock();
                    let _ = w.write_all(text.as_bytes());
                    let _ = w.flush();
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    out.abort();
}
