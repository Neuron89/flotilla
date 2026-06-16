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
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

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

    Router::new()
        .route("/", get(index))
        .route("/manifest.json", get(manifest))
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

/// Fire a sample push so the user can confirm their ntfy config end-to-end.
async fn notify_test(State(mgr): State<Arc<Manager>>) -> Response {
    match crate::notify::test_push(&mgr).await {
        Ok(()) => (StatusCode::OK, "sent").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Bearer-token gate. Accepts the token via `Authorization: Bearer <t>` header
/// or `?token=<t>` query (so browser WebSockets, which can't set headers, work).
/// Empty configured token disables the check.
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
    let provided = from_header.or(from_query);
    if provided.as_deref() == Some(expected.as_str()) {
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
    ws.on_upgrade(move |socket| handle_attach(socket, session))
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
