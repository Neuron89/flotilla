//! flotilla-agent — per-host daemon that owns AI-agent PTY sessions and exposes
//! them over HTTP/WebSocket for the Flotilla fleet control plane.

mod api;
mod config;
mod manager;
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
    let manager = manager::Manager::new(cfg);
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
