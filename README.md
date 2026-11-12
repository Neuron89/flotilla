<div align="center">

# Flotilla

### One control plane for a fleet of AI-agent terminal sessions, across every machine you own.

[![Rust](https://img.shields.io/badge/Rust-stable%20(1.78%2B)-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-2ea44f)](#build)
[![Network](https://img.shields.io/badge/network-Tailscale-blueviolet?logo=tailscale&logoColor=white)](deploy/tailscale-setup.md)
[![Clients](https://img.shields.io/badge/clients-TUI%20%2B%20web-0aa)](#components)
[![License](https://img.shields.io/badge/license-MIT-yellow)](LICENSE)
[![Status](https://img.shields.io/badge/status-v0.1%20early-orange)](#status-and-roadmap)

</div>

---

Flotilla is for people who run long-lived coding-agent sessions (Claude Code,
Hermes, or plain shells) on several machines at once: a home server, a work
server, a laptop, a desktop. A small daemon on each host owns the sessions; a
terminal UI and a browser client show every session on every host in one list,
let you attach to any of them interactively, spawn new ones, and send a prompt
from a phone. Sessions keep running on their host when no client is attached.

Terminal multiplexers such as tmux and Zellij solve this for a single Unix
host. Flotilla covers the cross-machine, mixed Windows and Linux case.

```
            +------------------+          +------------------+
            |  flotilla (TUI)  |          |  web client      |
            +--------+---------+          +---------+--------+
                     |                              |
                     +--------------+---------------+
                                    |
                        HTTP + WebSocket (per host)
                          over the Tailscale tailnet
                                    |
        +---------------+-----------+------+---------------+
        v               v                  v               v
+--------------+ +--------------+ +--------------+ +--------------+
| home-server  | | work-server  | |    laptop    | |   desktop    |
|              | |              | |              | |              |
| flotilla-    | | flotilla-    | | flotilla-    | | flotilla-    |
|   agent      | |   agent      | |   agent      | |   agent      |
|  - claude    | |  - claude    | |  - claude    | |  - hermes    |
|  - claude    | |  - hermes    | |              | |              |
|  - shell     | |              | |              | |              |
+--------------+ +--------------+ +--------------+ +--------------+
```

## Features

- **Fleet view**: every session on every configured host in one list, with a
  live status badge (`working`, `waiting`, `idle`, `exited`).
- **Interactive attach**: raw terminal attach from the TUI (detach with
  `Ctrl-]`) or from the browser through xterm.js. On attach, the last 256 KiB
  of scrollback is replayed so a new client sees context.
- **Spawn and kill** sessions on any host from named launch profiles
  (command, arguments, working directory, environment).
- **Send a prompt without attaching**: a one-line input box in the web client
  posts text to a session, which is convenient on a phone.
- **Push notifications**: each agent can publish to an [ntfy](https://ntfy.sh)
  topic when a session starts waiting for input or exits; the notification
  deep-links to that session in the web client.
- **Tags and filtering**: sessions can carry tags; the web client filters by
  name or tag, and the TUI displays them.
- **Cross-platform PTYs**: one engine for Windows (ConPTY) and Unix PTYs via
  [`portable-pty`](https://crates.io/crates/portable-pty).
- **Optional bearer token** on every API route, in addition to the tailnet
  boundary.

## Design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Session engine | A Rust daemon that owns PTYs directly via `portable-pty` | The fleet is mixed Windows and Linux. tmux and Zellij do not run natively on Windows; `portable-pty` uses ConPTY there and Unix PTYs elsewhere. |
| Network | [Tailscale](deploy/tailscale-setup.md) | Puts every host and the phone on one private network. Flotilla does no NAT traversal or identity federation of its own. |
| Clients | ratatui TUI, plus an xterm.js web client embedded in the daemon binary | The TUI is for hands-on work at a keyboard; the web client needs no separate server and works on a phone. |
| Agent context | Sessions run as the logged-in user | Spawned agents inherit that user's shell, `PATH` and agent configuration (for example user-scope MCP servers). See [shared memory notes](deploy/obsidian-shared-memory.md). |

The trade-off of owning PTYs in the daemon rather than wrapping tmux is that
live sessions do not survive a daemon restart. See [Limitations](#limitations).

## Components

| Component | Location | Role |
|---|---|---|
| `flotilla-agent` | `crates/flotilla-agent` | Per-host daemon. Spawns and owns sessions, exposes the REST + WebSocket API, derives per-session status, sends notifications, serves the web client. |
| `flotilla` | `crates/flotilla-tui` | Terminal control plane. Lists the fleet, attaches, spawns, kills. |
| Web client | `crates/flotilla-agent/web/index.html` | Single HTML file compiled into the agent and served at `/`. Fleet list, xterm.js attach, new-session dialog, prompt box, host manager. |

Built with `tokio`, `axum`, `ratatui`, `crossterm`, `portable-pty`,
`tokio-tungstenite`, `reqwest` and xterm.js.

## Build

Requires a recent stable Rust toolchain (the lockfile format needs Cargo 1.78
or newer).

```bash
cargo build --release
# target/release/flotilla-agent[.exe]   per-host daemon
# target/release/flotilla[.exe]         TUI control plane
```

Linux builds with the default toolchain. Linux and Windows are the target
platforms; macOS is supported by the underlying crates but has not been tested.

<details>
<summary>Windows with the GNU toolchain</summary>

If `cargo build` fails with "dlltool could not create import library", the
bundled binutils is missing a full assembler. Install mingw-w64 (no admin
rights needed):

```powershell
scoop install mingw
```

</details>

There is no automated test suite yet.

## Quick start

```bash
# 1. start an agent on this machine
./target/release/flotilla-agent --bind 0.0.0.0:7878

# 2. drive it from the terminal (defaults to http://127.0.0.1:7878)
./target/release/flotilla

#    or open the web client:  http://<this-host>:7878
```

With no config file the agent offers three default profiles: `shell`
(`bash -l`, or `powershell.exe` on Windows), `claude` and `hermes`. The last
two expect those commands to be on `PATH`.

To point the TUI at several hosts, give it a hosts file
(see [`config/flotilla-hosts.example.toml`](config/flotilla-hosts.example.toml)):

```bash
./target/release/flotilla --config flotilla-hosts.toml
```

The web client keeps its own host list in the browser (the **Hosts** dialog)
and connects to each agent directly.

### TUI keys

| Key | Action |
|:---:|---|
| `Up` `Down` / `j` `k` | Move selection |
| `Enter` | Attach (raw, interactive) |
| `Ctrl-]` | Detach; the session keeps running |
| `n` | New session on the selected session's host (profile picker) |
| `x` | Kill the selected session |
| `r` | Refresh now (the list also refreshes every 3 s) |
| `q` / `Esc` | Quit |

## Configuration

### Agent

`flotilla-agent --config /path/to/flotilla.toml`. A commented template is in
[`config/flotilla.example.toml`](config/flotilla.example.toml).

| Key | Default | Purpose |
|---|---|---|
| `bind` | `0.0.0.0:7878` | Listen address |
| `token` | empty (auth off) | Shared bearer token required on every `/api` request |
| `host_label` | machine hostname | Name shown in the clients |
| `data_dir` | platform data dir + `/flotilla` | Where `sessions.json` (metadata snapshot) is written |
| `[[profiles]]` | `shell`, `claude`, `hermes` | Launch profiles: `name`, `command`, `args`, `cwd`, `env`, `description` |
| `[notifications]` | disabled | ntfy settings: `enabled`, `ntfy_url`, `topic`, `token`, `priority`, `notify_on`, `public_url`, `poll_ms` |

Command-line flags and environment variables override the file:

| Flag | Environment variable | Purpose |
|---|---|---|
| `--config` | `FLOTILLA_CONFIG` | Path to `flotilla.toml` |
| `--bind` | `FLOTILLA_BIND` | Listen address |
| `--token` | `FLOTILLA_TOKEN` | Bearer token |
| | `RUST_LOG` | Log filter (default `info`) |

### TUI

| Flag | Environment variable | Purpose |
|---|---|---|
| `--config` | `FLOTILLA_HOSTS` | Hosts file: a list of `[[hosts]]` with `label`, `url`, `token` |

## HTTP and WebSocket API

All routes are under `/api`. When a token is configured, send it as
`Authorization: Bearer <token>` or as a `?token=` query parameter (browsers
cannot set headers on WebSocket connections).

| Method | Path | Purpose |
|:---:|---|---|
| `GET` | `/api/health` | Host label, hostname, OS/arch, version, session count |
| `GET` | `/api/profiles` | Available launch profiles |
| `GET` | `/api/sessions` | List sessions with status |
| `POST` | `/api/sessions` | Spawn: `{profile, name?, tags?, rows?, cols?}` |
| `DELETE` | `/api/sessions/:id` | Kill and remove |
| `POST` | `/api/sessions/:id/input` | Write the request body to the session; a carriage return is appended if missing |
| `GET` | `/api/sessions/:id/buffer` | Current scrollback bytes |
| `GET` | `/api/sessions/:id/attach` | WebSocket. Binary frames are PTY I/O; a text frame `{"type":"resize","cols":N,"rows":N}` resizes the PTY |
| `POST` | `/api/notify-test` | Send a sample ntfy push to verify the notification config |

Outside `/api`: `GET /` serves the web client and `GET /manifest.json` a web
app manifest.

### Session status

Status is a heuristic, evaluated in this order:

| Status | Meaning |
|---|---|
| `exited` | The process has ended (the session stays listed until killed) |
| `waiting` | The last 240 bytes of output contain a prompt signature such as `(y/n)`, `Do you want`, `Press enter` or a prompt glyph |
| `working` | Output was produced within the last 2 seconds |
| `idle` | None of the above |

## Deploying to a fleet

1. **[Set up Tailscale](deploy/tailscale-setup.md)** on every host and on the
   phone (MagicDNS plus an ACL restricting the agent port).
2. **Install the agent as a service** so it starts automatically:
   - Linux: `sudo ./deploy/install-linux.sh` builds the agent, installs it to
     `/usr/local/bin`, writes `/etc/flotilla/flotilla.toml`, and enables a
     systemd unit that runs as the invoking user.
   - Windows: `./deploy/install-windows.ps1` builds the agent, installs it
     under `%LOCALAPPDATA%\Flotilla`, and registers a logon-triggered
     Scheduled Task.
3. **Set profile working directories** if agents should start in a particular
   workspace ([notes](deploy/obsidian-shared-memory.md)).
4. Add each host's tailnet URL to `flotilla-hosts.toml` for the TUI and to the
   web client's **Hosts** dialog.

## Phone notifications (ntfy)

Each agent polls its own sessions and publishes to ntfy when a session
transitions into a watched state (`waiting` by default; add `exited` to catch
crashes).

1. Self-host ntfy on the tailnet, or use ntfy.sh with an unguessable topic.
   Subscribe to the topic in the ntfy app.
2. Fill in `[notifications]` in `flotilla.toml`: `enabled = true`, the `topic`,
   and `public_url` (this host's tailnet URL). With `public_url` set, tapping
   the notification opens `/?open=<session-id>` and the web client attaches to
   that session.
3. Verify with the bell button next to a host in the web client's **Hosts**
   dialog, or `POST /api/notify-test`.

The notification carries the host label, session name, profile and tags.

## Security model

Flotilla gives remote terminal access to each host, so the network boundary
matters.

- The agent is designed to be reachable **only over a private tailnet**. Do
  not expose its port to the public internet.
- The agent serves plain HTTP and WebSocket; transport encryption comes from
  Tailscale.
- The bearer token is optional and off by default. Set one for defence in
  depth, and restrict the port with a tailnet ACL
  ([example](deploy/tailscale-setup.md)).
- CORS is fully permissive so that the web client served by one agent can talk
  to the others. This is another reason to set a token.

## Limitations

- **Sessions do not survive a daemon restart.** The daemon owns the PTYs, so
  when it stops, its sessions end. Running it as a service keeps restarts
  rare. `sessions.json` is a metadata snapshot for audit, not a restore
  mechanism.
- **The TUI speaks plain `http://` and `ws://` only.** It is built without
  TLS support. Agents fronted by an HTTPS reverse proxy can be used from the
  web client.
- **Status detection is heuristic**: output recency plus prompt-signature
  matching, not a semantic read of the agent. False `waiting` states are
  possible.
- **The web client loads xterm.js from the jsDelivr CDN**, so the browser
  needs internet access the first time. There is no service worker or offline
  mode; the manifest only supports adding the page to a phone's home screen.
- The TUI cannot yet name or tag a session at spawn time or filter by tag;
  use the web client or the API for that.

## Status and roadmap

Version 0.1.0. Flotilla is an early-stage personal project: the features below
are implemented, but there are no automated tests and the API may change.

| Phase | Status | Scope |
|:---:|---|---|
| 1 | Implemented | Agent, TUI, web client, Linux and Windows install scripts |
| 2 | Implemented | ntfy push on `waiting` / `exited`, session tags and web filter, deep-link attach, web app manifest |
| 3 | Planned | Optional tmux/Zellij backend on Linux for restart-proof sessions; session templates (spawn a named set across hosts); web push in the browser client |

## Project structure

```
crates/
  flotilla-agent/
    src/main.rs        CLI and startup
    src/api.rs         axum routes, auth middleware, WebSocket bridge
    src/manager.rs     session registry and metadata snapshot
    src/pty.rs         PTY session, scrollback, status heuristic
    src/notify.rs      ntfy watcher
    src/config.rs      config file and default profiles
    web/index.html     embedded web client
  flotilla-tui/
    src/main.rs        ratatui UI and raw attach
    src/client.rs      HTTP client, hosts file, key encoding
config/                example agent config and hosts file
deploy/                systemd unit, install scripts, Tailscale and shared-memory notes
```

## License

MIT. See [LICENSE](LICENSE).
