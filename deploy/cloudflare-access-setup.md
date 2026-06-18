# Securing Flotilla with a Cloudflare Tunnel + Access (Phase 4)

This puts a real **login (OAuth / one-time-PIN)** in front of each agent and gives
Flotilla **per-user identity** — without any auth code or open ports. Cloudflare
authenticates the user at the edge and forwards a signed JWT; the agent validates
it (`[access]` in `flotilla.toml`).

```
  you ──HTTPS──▶ Cloudflare edge ──(Access login: Google/GitHub/OTP)──▶ Tunnel ──▶ flotilla-agent
                      │                                                              │
                      └── injects Cf-Access-Jwt-Assertion (signed JWT) ─────────────┘  validates JWT
```

Two bonuses this unlocks:
- **HTTPS everywhere**, which is what makes **Web Push** (Phase 3) work in the browser.
- The agent stays bound to `127.0.0.1` / the tailnet — nothing is exposed directly.

You need a domain on Cloudflare and Zero Trust enabled (free tier is fine).

---

## 1. Expose the agent through a Tunnel

Pick one. **Option A** is easiest if you already run a cloudflared tunnel
(e.g. the dashboard-managed one on your PVE host).

### Option A — dashboard-managed tunnel (recommended)

1. Zero Trust dashboard → **Networks → Tunnels →** your tunnel → **Public Hostname → Add**.
2. Subdomain `home-server` (etc.), your domain, **Service** `HTTP` → `localhost:7878`
   (or the tailnet IP:port if the tunnel runs on another box).
3. Save. Repeat per agent (`work-server.example.com`, …).

### Option B — a dedicated local tunnel per agent

Run the helper on the host (installs cloudflared, logs in, creates + routes a
named tunnel, installs the service):

```bash
sudo ./deploy/install-cloudflared.sh home-server.example.com   # the public hostname for THIS agent
```

---

## 2. Create the Access application

1. Zero Trust → **Access → Applications → Add an application → Self-hosted**.
2. **Application domain** = the hostname from step 1 (e.g. `home-server.example.com`).
3. **Identity providers**: enable what you want — Google/GitHub OAuth, or the
   built-in **One-time PIN** (email a code; zero setup).
4. **Policies → Add a policy**: Action **Allow**, rule e.g. *Emails* → `you@example.com`
   (or *Emails ending in* `@yourdomain.com`). This is the real gate.
5. Save.

> Tip: add **one** Access application covering all agents with a wildcard
> (`*.example.com`) if you'd rather not configure each host separately — the AUD
> tag is then shared across them.

## 3. Copy the two values Flotilla needs

- **AUD tag** — Access → your application → **Overview** → *Application Audience (AUD) Tag*
  (a long hex string).
- **Team domain** — Zero Trust → **Settings → Custom Pages / General** → your team
  domain, i.e. `https://YOURTEAM.cloudflareaccess.com`.

## 4. Configure the agent

In each host's `flotilla.toml`:

```toml
[access]
enabled = true
team_domain = "https://YOURTEAM.cloudflareaccess.com"
aud = "the-long-hex-aud-tag"
# allowed_emails = ["you@example.com"]   # optional extra check on top of the policy
```

Restart the agent. The log should read `auth: cloudflare-access` (or
`cloudflare-access + token` if you also kept a bearer `token`). On a bad
`team_domain`/`aud` you'll see a JWKS warning at startup.

## 5. Use it

- Browse to `https://home-server.example.com` → Cloudflare login → the Flotilla
  PWA. The header shows **👤 your-email**. Tap 🔔 to enable Web Push (now that
  you're on HTTPS).
- Point the web client's **Hosts** at the `https://…` URLs. The
  `CF_Authorization` cookie authenticates both the REST calls and the WebSocket
  attach automatically — no token in any URL.

## How auth resolves (per request)

1. **Cloudflare Access JWT** present and valid (`aud` + `iss` + `exp`, signature
   against the team JWKS, plus the optional `allowed_emails`) → allow, identity =
   the JWT's `email`.
2. Else the shared **bearer token** (`Authorization: Bearer`, the
   `flotilla.bearer` WebSocket subprotocol, or the deprecated `?token=`) → allow.
3. Else `401`.

So you can run **Access on the public hostname** and the **TUI with a token over
the tailnet** against the same agent simultaneously.

## Notes & gotchas

- **WebSocket**: Access protects the `/attach` upgrade too; the browser sends the
  `CF_Authorization` cookie automatically. Tailnet/TUI clients send the token in
  the `Sec-WebSocket-Protocol` header (not the URL).
- **Service tokens** (for non-interactive clients) work as well: the agent also
  accepts `Cf-Access-Jwt-Assertion` set by a Cloudflare service token. The TUI
  doesn't do this yet — use the tailnet + bearer token for headless access.
- **Clock skew**: JWT `exp`/`iat` are time-checked; keep host clocks in sync (NTP).
- **Key rotation**: the agent caches the JWKS for an hour and refetches on an
  unknown key id, so Cloudflare key rotation is handled automatically.
