//! Cloudflare Access JWT validation (Phase 4).
//!
//! When the agent is fronted by a Cloudflare Tunnel with an Access policy, every
//! request that reaches it has already passed the Access login (OAuth/OTP at the
//! edge) and carries a signed JWT in the `Cf-Access-Jwt-Assertion` header (and a
//! `CF_Authorization` cookie). This module validates that JWT against the team's
//! published JWKS and extracts the authenticated user's identity, giving us
//! **per-user auth** without any login code in the agent itself.
//!
//! Validation checks signature (RS256), `aud` (the Access application's AUD tag),
//! `iss` (the team domain) and expiry, then optionally enforces an email
//! allowlist as defense-in-depth on top of the Cloudflare-side policy.

use std::time::Duration;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::config::AccessConfig;

/// The authenticated caller, attached to the request for handlers/logging.
#[derive(Debug, Clone)]
pub struct Identity {
    pub email: String,
    /// How the caller authenticated: "access" (Cloudflare) or "token" (bearer).
    pub via: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct AccessClaims {
    #[serde(default)]
    email: String,
}

struct CacheState {
    keys: Vec<Jwk>,
    /// Unix-seconds of the last successful fetch (0 = never).
    fetched_at: u64,
}

/// Validates Cloudflare Access JWTs, caching the team JWKS.
pub struct AccessVerifier {
    /// e.g. `https://yourteam.cloudflareaccess.com` (no trailing slash).
    team_domain: String,
    /// The Access application's AUD tag.
    aud: String,
    /// Optional allowlist of emails permitted through (empty = allow any user
    /// the Cloudflare policy already let in).
    allowed_emails: Vec<String>,
    http: reqwest::Client,
    cache: RwLock<CacheState>,
}

/// How long a cached JWKS is considered fresh before a refresh is attempted.
const JWKS_TTL_SECS: u64 = 3600;

impl AccessVerifier {
    /// Build from config. Returns `None` unless Access is enabled and the
    /// required fields are present.
    pub fn from_config(cfg: &AccessConfig) -> Option<AccessVerifier> {
        if !cfg.enabled {
            return None;
        }
        if cfg.team_domain.is_empty() || cfg.aud.is_empty() {
            tracing::warn!("access: enabled but team_domain/aud missing — Access auth disabled");
            return None;
        }
        Some(AccessVerifier {
            team_domain: cfg.team_domain.trim_end_matches('/').to_string(),
            aud: cfg.aud.clone(),
            allowed_emails: cfg
                .allowed_emails
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            cache: RwLock::new(CacheState {
                keys: Vec::new(),
                fetched_at: 0,
            }),
        })
    }

    fn certs_url(&self) -> String {
        format!("{}/cdn-cgi/access/certs", self.team_domain)
    }

    /// Validate a raw JWT and return the caller's identity, or an error string.
    pub async fn verify(&self, token: &str) -> Result<Identity, String> {
        let header = decode_header(token).map_err(|e| format!("bad jwt header: {e}"))?;
        let kid = header.kid.ok_or("jwt missing kid")?;

        // Use cached keys; refresh if stale or the kid isn't known yet (key
        // rotation). Find the matching JWK's RSA components.
        let (n, e) = match self.lookup(&kid) {
            Some(c) => c,
            None => {
                self.refresh().await?;
                self.lookup(&kid).ok_or("no JWKS key matches token kid")?
            }
        };

        let key =
            DecodingKey::from_rsa_components(&n, &e).map_err(|e| format!("bad jwks key: {e}"))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.aud]);
        validation.set_issuer(&[&self.team_domain]);
        // exp is validated by default.

        let data = decode::<AccessClaims>(token, &key, &validation)
            .map_err(|e| format!("jwt validation failed: {e}"))?;
        let email = data.claims.email.to_ascii_lowercase();

        if !self.allowed_emails.is_empty() && !self.allowed_emails.contains(&email) {
            return Err(format!("'{email}' not in access allowlist"));
        }
        Ok(Identity {
            email: if email.is_empty() { "unknown".into() } else { email },
            via: "access",
        })
    }

    fn lookup(&self, kid: &str) -> Option<(String, String)> {
        let c = self.cache.read();
        if c.fetched_at == 0 {
            return None;
        }
        c.keys
            .iter()
            .find(|k| k.kid == kid)
            .map(|k| (k.n.clone(), k.e.clone()))
    }

    /// Returns true if the cache is older than the TTL.
    fn stale(&self) -> bool {
        let c = self.cache.read();
        now_secs().saturating_sub(c.fetched_at) > JWKS_TTL_SECS
    }

    /// Fetch and cache the team JWKS.
    async fn refresh(&self) -> Result<(), String> {
        let url = self.certs_url();
        let jwks: Jwks = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("JWKS fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("JWKS fetch status: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JWKS parse failed: {e}"))?;
        let mut c = self.cache.write();
        c.keys = jwks.keys;
        c.fetched_at = now_secs();
        tracing::debug!("access: refreshed JWKS ({} keys)", c.keys.len());
        Ok(())
    }

    /// Refresh the JWKS at startup (best-effort) and warn loudly on failure so a
    /// misconfigured team_domain is obvious before the first request.
    pub async fn warm(&self) {
        if let Err(e) = self.refresh().await {
            tracing::warn!("access: initial JWKS fetch failed ({e}); will retry on demand");
        } else {
            tracing::info!(
                "access: validating Cloudflare Access JWTs (team {}, aud …{})",
                self.team_domain,
                self.aud.chars().rev().take(6).collect::<String>().chars().rev().collect::<String>()
            );
        }
    }

    /// Proactively refresh if the cache has gone stale (called opportunistically
    /// from the middleware so long-lived daemons pick up key rotation).
    pub async fn refresh_if_stale(&self) {
        if self.stale() {
            let _ = self.refresh().await;
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
