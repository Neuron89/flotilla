<div align="center">

# ⚓ Flotilla

### One control plane for a fleet of AI-agent terminal sessions — across every machine you own.

<br/>

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/Linux%20·%20macOS%20·%20Windows-supported-2ea44f)](#build)
[![Network](https://img.shields.io/badge/Tailscale-tailnet-blueviolet?logo=tailscale&logoColor=white)](deploy/tailscale-setup.md)
[![Clients](https://img.shields.io/badge/Clients-TUI%20%2B%20Web%2FPWA-0aa)](#components)
[![License](https://img.shields.io/badge/License-MIT-yellow)](#license)
[![Status](https://img.shields.io/badge/Phase%201-shipped%20✅-success)](#roadmap)

</div>

---

You bounce between a home server, a work server, two laptops, and a desktop —
running Claude Code and Hermes sessions on all of them. **Flotilla gives you a
single board** — terminal **and** phone — that shows every session on every host,
lets you attach to any one interactively, spawn new ones, and fire off a prompt
from the couch. Sessions you're not looking at keep running in the background on
their own machine.

It's the cross-machine, cross-platform piece that [Conductor](https://www.conductor.build/)
(Mac-only) and the single-host Rust multiplexers (Zellij, Herdr, RMUX) don't give you.

<br/>

```
            ┌──────────────────┐          ┌──────────────────┐
            │   flotilla (TUI) │          │  web / phone PWA │
            └────────┬─────────┘          └─────────┬────────┘
                     │                              │
                     └──────────────┬───────────────┘
                                    │
                        HTTP + WebSocket  (per host)
                          over the Tailscale tailnet
                                    │
        ┌───────────────┬──────────┴───────┬───────────────┐
        ▼               ▼                  ▼               ▼
┌──────────────┐ ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
│ home-server  │ │ work-server  │ │   laptop(s)  │ │   desktop    │
│              │ │              │ │              │ │              │
│ flotilla-    │ │ flotilla-    │ │ flotilla-    │ │ flotilla-    │
│   agent      │ │   agent      │ │   agent      │ │   agent      │
│  ├ claude 🟢 │ │  ├ claude 🟡 │ │  └ claude ⚪ │ │  └ hermes 🟢 │
│  ├ claude 🟡 │ │  └ hermes 🟢 │ │              │ │              │
│  └ hermes ⚪ │ │              │ │              │ │              │
└──────────────┘ └──────────────┘ └──────────────┘ └──────────────┘
        │
        └──▶ Obsidian KB (home server) = shared memory every session inherits

         🟢 working   🟡 waiting on you   ⚪ idle
```

<br/>

## ✨ What you get

|  |  |
|---|---|
| 🖥️ **One fleet view** | Every session on every host in a single list, color-coded by live status. |
| 📱 **Phone-ready** | The daemon serves a zero-install web/PWA client — drive sessions from your couch. |
| 🔌 **Attach anywhere** | Jump into any session raw & interactive (tmux-style), detach, and it keeps running. |
| 🚀 **Spawn on demand** | Launch a new Claude / Hermes / shell session on any box from a profile picker. |
| 🌐 **Heterogeneous** | One engine spans Windows desktop + laptop **and** Linux servers. |
| 🧠 **Shared brain** | Every spawned agent inherits your user-scope MCP, including the Obsidian KB. |

<br/>

## 🧭 Why this architecture

| Decision | Choice | Why |
|---|---|---|
| **Session engine** | A Rust daemon owning PTYs directly via [`portable-pty`](https://crates.io/crates/portable-pty) | The fleet is **heterogeneous** — Windows + Linux. tmux/Zellij don't run natively on Windows; `portable-pty` uses ConPTY on Windows and Unix PTYs elsewhere, so **one engine covers every box**. |
| **Network bridge** | [Tailscale](deploy/tailscale-setup.md) | Connects all hosts **and your phone** into one flat network; erases the home/work firewall problem. Flotilla does zero networking of its own. |
| **Clients** | ratatui TUI + xterm.js web client served by the daemon | TUI for hands-on driving; the web client is phone-ready with **no extra server**. |
| **Shared memory** | The existing [Obsidian KB](deploy/obsidian-shared-memory.md) | Spawned sessions run in your user context and inherit your user-scope MCP, so every agent reads/writes the same brain. |

> [!NOTE]
> The trade-off of owning PTYs in the daemon (vs. wrapping tmux) is that **live
> sessions don't survive a daemon restart** — see [Limitations](#-limitations).

<br/>

## 🧩 Components

| Component | Crate | Role |
|---|---|---|
| **`flotilla-agent`** | `crates/flotilla-agent` | Per-host daemon. Spawns/owns sessions, exposes REST + WebSocket, detects per-session status, serves the web client. One self-contained binary. |
| **`flotilla`** | `crates/flotilla-tui` | Terminal control plane. Lists the whole fleet color-coded by status; attach (raw, tmux-style), spawn, kill. |
| **Web / mobile client** | `crates/flotilla-agent/web/index.html` | Baked into the daemon, served at `/`. Fleet grid, interactive xterm.js attach, and a fire-and-forget "send a prompt" box that shines on a phone. |

<br/>

## 🔨 Build

> Needs **Rust**.

<details>
<summary><b>⚠️ Windows + GNU toolchain — one-time fix</b></summary>

<br/>

On Windows with the GNU toolchain, `dlltool` needs a full assembler that the
bundled binutils lacks. Install mingw-w64 (no admin needed):

```powershell
# only if `cargo build` fails with "dlltool could not create import library"
scoop install mingw      # provides as.exe / dlltool / gcc
```

</details>

```bash
cargo build --release
# binaries:
#   target/release/flotilla-agent[.exe]   ← the per-host daemon
#   target/release/flotilla[.exe]         ← the TUI control plane
```

On **Linux / macOS** the default toolchain builds with no extra steps.

<br/>

## ⚡ Quick start (local)

```bash
# 1️⃣  start an agent on this machine
./target/release/flotilla-agent --bind 0.0.0.0:7878

# 2️⃣  drive it from the terminal …
./target/release/flotilla                 # defaults to http://127.0.0.1:7878

#     … or open the web client in a browser / phone
#     →  http://<this-host>:7878
```

Point the TUI at the **whole fleet** with a hosts file
(see [`config/flotilla-hosts.example.toml`](config/flotilla-hosts.example.toml)):

```bash
./target/release/flotilla --config flotilla-hosts.toml
```

### ⌨️ TUI keys

| Key | Action |
|:---:|---|
| `↑` `↓` / `j` `k` | move selection |
| `Enter` | attach (raw interactive) |
| `Ctrl-]` | detach — session keeps running |
| `n` | new session (pick a profile) |
| `x` | kill selected |
| `r` | refresh |
| `q` | quit |

<br/>

## 🛰️ HTTP / WebSocket API

All routes under `/api`. Bearer token optional — `Authorization: Bearer …` or `?token=`.

| Method | Path | Purpose |
|:---:|---|---|
| `GET` | `/api/health` | host label, os/arch, version, session count |
| `GET` | `/api/profiles` | available launch profiles |
| `GET` | `/api/sessions` | list sessions + status |
| `POST` | `/api/sessions` | spawn `{profile, name?, tags?, rows?, cols?}` |
| `DELETE` | `/api/sessions/:id` | kill |
| `POST` | `/api/sessions/:id/input` | fire-and-forget input (auto-appends Enter) |
| `GET` | `/api/sessions/:id/buffer` | current scrollback (bytes) |
| `GET` | `/api/sessions/:id/attach` | **WebSocket** — binary = PTY I/O; text JSON `{"type":"resize",…}` |
| `POST` | `/api/notify-test` | fire a sample ntfy push to verify notification config |
| `GET` | `/manifest.json` | PWA manifest (installable web client) |

**Status values:**
🟢 `working` (output now) · 🟡 `waiting` (tail looks like a prompt) · ⚪ `idle` · ⚫ `exited`

<br/>

## 🚢 Deploy to the fleet

```
  ①  Tailscale         ②  Install agent       ③  Shared memory      ④  Wire hosts
     all 5 boxes          as a service            point profile         every tailnet
     + your phone         (boot + persist)        cwd at workspace      URL → hosts.toml
```

1. **[Set up Tailscale](deploy/tailscale-setup.md)** on all boxes + your phone (MagicDNS + an ACL).
2. **Install the agent as a service** so sessions persist and start on boot:
   - 🐧 Linux — `sudo ./deploy/install-linux.sh` (systemd, runs as you)
   - 🪟 Windows — `./deploy/install-windows.ps1` (logon-triggered Scheduled Task)
3. **[Wire shared memory](deploy/obsidian-shared-memory.md)** — point profile `cwd` at your agent workspace.
4. Put every host's tailnet URL in `flotilla-hosts.toml` (and the web client's Hosts dialog).

<br/>

## 🔔 Phone notifications (ntfy)

So you can walk away from every machine and still know when an agent needs you,
each host watches its own sessions and pushes to [ntfy](https://ntfy.sh) on a
transition into 🟡 `waiting` (configurable; add `exited` to catch crashes).

1. Self-host ntfy on your tailnet (or use ntfy.sh with an unguessable topic) and
   install the ntfy app on your phone, subscribed to that topic.
2. Fill in the `[notifications]` block in `flotilla.toml`
   (see [`config/flotilla.example.toml`](config/flotilla.example.toml)) — set
   `enabled = true`, your `topic`, and `public_url` (this host's tailnet URL) so
   the push is **tap-to-open**: it deep-links straight into that session's
   attach view in the web client.
3. Verify end-to-end with the 🔔 button next to each host in the web client's
   **Hosts** dialog (or `POST /api/notify-test`).

The alert carries the host label, session name, profile, and any tags — so a
glance tells you *which* box and *which* agent is blocked.

<br/>

## ⚠️ Limitations

- **Restart survival is opt-in (the `tmux` backend).** With the default `pty`
  backend the daemon owns the PTYs, so a restart ends its sessions. Set
  `backend = "tmux"` on a profile (Linux/macOS) and the command runs inside a
  detached tmux session instead — it **survives a daemon restart**: on boot the
  agent re-adopts every `flotilla_*` tmux session and recovers its scrollback.
  A metadata snapshot is also written to `data_dir/sessions.json`.
- **`wss://` not enabled** in the TUI client — plain `ws://` over the tailnet
  (Tailscale already encrypts). Use the web client over HTTPS if you front it
  with a reverse proxy.
- **Heuristic status detection** — output recency + prompt-signature matching,
  not a semantic read of the agent.

<br/>

## 🗺️ Roadmap

| Phase | Status | Scope |
|:---:|:---:|---|
| **1** | ✅ **Shipped** | agent + TUI + web client + deploy scripts |
| **2** | ✅ **Shipped** | ntfy phone push when a session goes 🟡 `waiting` (or exits); per-session tags + fleet filter; deep-link tap-to-attach; installable PWA |
| **3** | 🚧 **In progress** | ✅ opt-in tmux backend (Linux/macOS) for restart-proof sessions; session "templates" (spawn a named set across hosts at once); web push into the PWA |

<br/>

---

<div align="center">

**Built with Rust** · `tokio` · `axum` · `ratatui` · `portable-pty` · `xterm.js`

📄 [MIT License](#license) · Made by [Hayden Nester](mailto:hsnester@gmail.com)

</div>

## License

MIT
