//! HTTP + WebSocket API and the embedded web/mobile client.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

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
        .layer(middleware::from_fn_with_state(manager.clone(), auth))
        .with_state(manager.clone());

    let root = Router::new()
        .route("/", get(index))
        .route("/manifest.json", get(manifest))
        .route(
            "/vendor/xterm.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("../web/vendor/xterm.css")) }),
        )
        .route(
            "/vendor/xterm.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/vendor/xterm.js"),
                )
            }),
        )
        .route(
            "/vendor/xterm-addon-fit.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/vendor/xterm-addon-fit.js"),
                )
            }),
        )
        .nest("/api", api);

    // CORS: no blanket-permissive layer. The same-origin single-host client and
    // PWA need no CORS headers at all. Only when the operator lists peer origins
    // (multi-host aggregation page) do we attach an allowlist layer.
    match build_cors(&manager.config.allowed_origins) {
        Some(cors) => root.layer(cors),
        None => root,
    }
}

/// Build a CORS layer restricted to exactly the configured origins, or `None`
/// when none are configured (same-origin works without any CORS layer). Invalid
/// entries are logged and skipped rather than panicking.
fn build_cors(allowed: &[String]) -> Option<CorsLayer> {
    if allowed.is_empty() {
        return None;
    }
    let mut origins: Vec<HeaderValue> = Vec::new();
    for o in allowed {
        match o.parse::<HeaderValue>() {
            Ok(hv) => origins.push(hv),
            Err(e) => tracing::warn!("ignoring invalid allowed_origin {o:?}: {e}"),
        }
    }
    if origins.is_empty() {
        return None;
    }
    // Reflect exactly the listed origins (never AllowOrigin::any) with the
    // methods/headers the API uses.
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::DELETE])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

async fn index() -> Html<&'static str> {
    Html(WEB_INDEX)
}

const MANIFEST: &str = r##"{"name":"Flotilla","short_name":"Flotilla","start_url":"/","display":"standalone","background_color":"#0b0f17","theme_color":"#0b0f17","icons":[]}"##;

async fn manifest() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST)
}

/// Fire a sample push so the user can confirm their ntfy config end-to-end.
async fn notify_test(State(mgr): State<Arc<Manager>>) -> Response {
    match crate::notify::test_push(&mgr).await {
        Ok(()) => (StatusCode::OK, "sent").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Bearer-token gate. Accepts the token via `Authorization: Bearer <t>` header
/// or `?token=<t>` query (so browser WebSockets, which can't set headers, work).
/// The check is ENFORCED whenever a token is configured; it is bypassed only
/// when no token is set (the loopback/insecure case gated at startup).
async fn auth(State(mgr): State<Arc<Manager>>, req: axum::extract::Request, next: Next) -> Response {
    let expected = &mgr.config.token;
    if expected.is_empty() {
        return next.run(req).await;
    }
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let from_query = req.uri().query().and_then(|q| {
        url_query(q).get("token").cloned()
    });
    // Constant-time compare to avoid leaking the token via timing. A missing
    // token is rejected. Note: we never log `provided` or the raw query string.
    let ok = match from_header.or(from_query) {
        Some(provided) => {
            constant_time_eq::constant_time_eq(provided.as_bytes(), expected.as_bytes())
        }
        None => false,
    };
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
    }
}

fn url_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| {
            let mut it = kv.splitn(2, '=');
            Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
        })
        .collect()
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
    // PTY writes are blocking OS I/O; a stuck child could otherwise wedge a tokio
    // worker. Run the write on the blocking pool.
    let writer = session.writer();
    let res = tokio::task::spawn_blocking(move || {
        let mut w = writer.lock();
        w.write_all(&bytes).and_then(|_| w.flush())
    })
    .await;
    match res {
        Ok(Ok(())) => StatusCode::ACCEPTED.into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "write failed").into_response(),
    }
}

async fn ws_attach(
    ws: WebSocketUpgrade,
    State(mgr): State<Arc<Manager>>,
    Path(id): Path<String>,
    Query(_q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    // Cross-Site WebSocket Hijacking / DNS-rebinding defense: browsers always
    // send Origin on a WS upgrade, and (unlike headers) it cannot be forged by
    // page JS. Non-browser clients (the TUI) send no Origin and are allowed.
    if !origin_allowed(&headers, &mgr.config.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let Some(session) = mgr.get(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    ws.on_upgrade(move |socket| handle_attach(socket, session))
}

/// Allow the upgrade when: no Origin header (native clients), OR the Origin's
/// host:port matches the request's own Host (same-origin), OR the Origin is in
/// the configured `allowed_origins` list.
fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    let origin = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(o) => o,
        None => return true, // no Origin → not a browser cross-site request
    };
    if allowed.iter().any(|a| a == origin) {
        return true;
    }
    // Same-origin: compare the Origin's authority (host[:port]) to the Host header.
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if let Some(origin_authority) = origin.split("://").nth(1) {
            if origin_authority == host {
                return true;
            }
        }
    }
    false
}

/// Bidirectional bridge between a WebSocket client (xterm.js or the TUI) and the
/// PTY. Protocol: server→client binary = PTY output. client→server binary =
/// input bytes; client→server text (JSON `{"type":"resize","cols":..,"rows":..}`)
/// = control.
async fn handle_attach(socket: WebSocket, session: Arc<crate::pty::Session>) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe BEFORE snapshotting so any bytes produced in the gap are captured
    // by the live stream. Worst case a few bytes replay twice (harmless in a
    // terminal); the alternative ordering silently drops them.
    let mut rx = session.subscribe();

    // Replay scrollback so a fresh client sees existing context.
    let snapshot = session.snapshot();
    if !snapshot.is_empty() && sender.send(Message::Binary(snapshot.to_vec())).await.is_err() {
        return;
    }

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

    // Input pump: client → PTY. All PTY writes go through the blocking pool so a
    // stuck child can't wedge a tokio worker.
    let writer = session.writer();
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Binary(data) => {
                pty_write(&writer, data).await;
            }
            Message::Text(text) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if val.get("type").and_then(|v| v.as_str()) == Some("resize") {
                        // Clamp client-supplied dimensions to a sane range; the raw
                        // `as u16` cast would otherwise wrap absurd values mod 65536.
                        let cols = clamp_dim(val.get("cols").and_then(|v| v.as_u64()), 120);
                        let rows = clamp_dim(val.get("rows").and_then(|v| v.as_u64()), 40);
                        session.resize(rows, cols);
                    }
                } else {
                    // Treat unstructured text as input too.
                    pty_write(&writer, text.into_bytes()).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    out.abort();
}

/// Clamp a client-provided terminal dimension into 1..=1000, falling back to
/// `default` when absent or out of range.
fn clamp_dim(v: Option<u64>, default: u16) -> u16 {
    match v {
        Some(n) if (1..=1000).contains(&n) => n as u16,
        _ => default,
    }
}

/// Perform a blocking PTY write off the async runtime.
async fn pty_write(writer: &Arc<parking_lot::Mutex<Box<dyn Write + Send>>>, data: Vec<u8>) {
    let writer = writer.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let mut w = writer.lock();
        let _ = w.write_all(&data);
        let _ = w.flush();
    })
    .await;
}
