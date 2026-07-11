//! flotilla-agent — per-host daemon that owns AI-agent PTY sessions and exposes
//! them over HTTP/WebSocket for the Flotilla fleet control plane.

mod api;
mod config;
mod manager;
mod notify;
mod pty;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "flotilla-agent", version, about)]
struct Args {
    /// Path to a flotilla.toml config file.
    #[arg(short, long, env = "FLOTILLA_CONFIG")]
    config: Option<PathBuf>,

    /// Override the bind address (host:port).
    #[arg(short, long, env = "FLOTILLA_BIND")]
    bind: Option<String>,

    /// Override the bearer token.
    #[arg(long, env = "FLOTILLA_TOKEN")]
    token: Option<String>,

    /// Allow starting on a NON-loopback bind address with NO token. This exposes
    /// unauthenticated shell access to the network — only use behind a trusted
    /// gate (e.g. Tailscale ACLs). Ignored when a token is set (auth is always
    /// enforced then).
    #[arg(long, env = "FLOTILLA_INSECURE_NO_AUTH", default_value_t = false)]
    insecure_no_auth: bool,
}

/// True only if every socket address `bind` resolves to is a loopback address
/// (127.0.0.0/8 or ::1). Unspecified addresses like 0.0.0.0 / :: are NOT
/// loopback, so they correctly require auth.
fn is_loopback_bind(bind: &str) -> bool {
    use std::net::ToSocketAddrs;
    match bind.to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback())
        }
        // If we cannot resolve it, be conservative and treat it as non-loopback
        // so the fail-closed check applies.
        Err(_) => false,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,flotilla_agent=info".into()),
        )
        .init();

    let args = Args::parse();
    let mut cfg = config::Config::load(args.config.as_deref())?;
    if let Some(b) = args.bind {
        cfg.bind = b;
    }
    if let Some(t) = args.token {
        cfg.token = t;
    }

    let bind = cfg.bind.clone();
    let label = cfg.label();
    let token_on = !cfg.token.is_empty();

    // Fail-closed auth gate. Because this daemon pipes bytes into real shells,
    // exposing it without a token is only acceptable on loopback.
    let loopback = is_loopback_bind(&bind);
    if !token_on {
        if loopback {
            tracing::warn!(
                "no token configured — daemon is UNAUTHENTICATED but bound to loopback ({bind}) only"
            );
        } else if args.insecure_no_auth {
            tracing::warn!(
                "⚠ INSECURE: starting on non-loopback bind {bind} with NO token because --insecure-no-auth was set. Anyone who can reach this address has full shell access."
            );
        } else {
            anyhow::bail!(
                "refusing to start: bind {bind} is not loopback and no token is set. \
                 Set a token (config `token`, --token, or FLOTILLA_TOKEN), bind to 127.0.0.1, \
                 or pass --insecure-no-auth to explicitly accept unauthenticated network exposure."
            );
        }
    }

    let manager = manager::Manager::new(cfg);
    notify::spawn_watcher(Arc::clone(&manager));
    let app = api::router(Arc::clone(&manager));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        "flotilla-agent '{label}' listening on http://{bind}  (auth: {})",
        if token_on { "token" } else { "off" }
    );
    tracing::info!("profiles: {}", manager.config.profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));

    axum::serve(listener, app).await?;
    Ok(())
}
