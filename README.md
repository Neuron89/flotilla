# ⚓ Flotilla

**One control plane for a fleet of AI-agent terminal sessions, across every machine you own.**

You bounce between a home server, a work server, two laptops, and a desktop —
running Claude Code and Hermes sessions on all of them. Flotilla gives you a
single board (terminal **and** phone) that shows every session on every host,
lets you attach to any one interactively, spawn new ones, and send a prompt from
the couch — while sessions you're not looking at keep running in the background
on their own machine.

It's the cross-machine, cross-platform piece that [Conductor](https://www.conductor.build/)
(Mac-only) and the single-host Rust multiplexers (Zellij, Herdr, RMUX) don't
give you.

```
   flotilla (TUI)         web / phone (PWA)
        \                      /
         \   HTTP + WebSocket /          ← over the Tailscale tailnet
          \  (per host)      /
   ┌───────┴──────┐  ┌───────┴──────┐  ┌──────────────┐  ┌──────────────┐
   │ home-server  │  │ work-server  │  │  laptop(s)   │  │   desktop    │
   │ flotilla-agent  │ flotilla-agent  │ flotilla-agent  │ flotilla-agent │
   │  ├ claude    │  │  ├ claude    │  │              │  │              │
   │  ├ claude    │  │  └ hermes    │  │              │  │              │
   │  └ hermes    │  │              │  │              │  │              │
   └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘
        Obsidian KB (home server) = shared memory every session inherits
```

## Why this architecture

| Decision | Choice | Why |
|---|---|---|
| **Session engine** | A Rust daemon that owns PTYs directly via [`portable-pty`](https://crates.io/crates/portable-pty) | The fleet is **heterogeneous** — Windows desktop + laptop *and* Linux servers. tmux/Zellij don't run natively on Windows; `portable-pty` uses ConPTY on Windows and Unix PTYs elsewhere, so one engine covers every box. |
| **Network bridge** | [Tailscale](deploy/tailscale-setup.md) | Connects all hosts **and your phone** into one flat network; erases the home/work firewall problem. Flotilla does zero networking of its own. |
| **Clients** | ratatui TUI + xterm.js web client served by the daemon | TUI for hands-on driving; the web client is phone-ready with no extra server. |
| **Shared memory** | The existing [Obsidian KB](deploy/obsidian-shared-memory.md) | Spawned sessions run in your user context and inherit your user-scope MCP, so every agent reads/writes the same brain. |

The trade-off of owning PTYs in the daemon (vs. wrapping tmux) is that **live
sessions don't survive a daemon restart** — see [Limitations](#limitations).

## Components

- **`flotilla-agent`** (`crates/flotilla-agent`) — per-host daemon. Spawns/owns
  agent sessions, exposes REST + WebSocket, detects per-session status, and
  serves the web client. One self-contained binary.
- **`flotilla`** (`crates/flotilla-tui`) — terminal control plane. Lists the
  whole fleet, color-coded by status; attach (raw, tmux-style), spawn, kill.
- **Web/mobile client** (`crates/flotilla-agent/web/index.html`) — baked into
  the daemon, served at `/`. Fleet grid, interactive xterm.js attach, and a
  fire-and-forget "send a prompt" box that shines on a phone.

## Build

Needs Rust. On **Windows with the GNU toolchain**, `dlltool` needs a full
assembler that the bundled binutils lacks — install mingw-w64 (no admin needed):

```powershell
# one-time, if `cargo build` fails with "dlltool could not create import library"
scoop install mingw      # provides as.exe / dlltool / gcc
```

Then:

```bash
cargo build --release
# binaries: target/release/flotilla-agent[.exe]  and  target/release/flotilla[.exe]
```

On Linux/macOS the default toolchain builds with no extra steps.

## Run it (local quick start)

```bash
# 1. start an agent on this machine
./target/release/flotilla-agent --bind 0.0.0.0:7878

# 2a. drive it from the terminal
./target/release/flotilla                      # defaults to http://127.0.0.1:7878

# 2b. or open the web client in a browser / phone
#     http://<this-host>:7878
```

Point the TUI at the whole fleet with a hosts file (see
[`config/flotilla-hosts.example.toml`](config/flotilla-hosts.example.toml)):

```bash
./target/release/flotilla --config flotilla-hosts.toml
```

### TUI keys

| Key | Action |
|---|---|
| `↑`/`↓` or `j`/`k` | move selection |
| `Enter` | attach (raw interactive) |
| `Ctrl-]` | detach (session keeps running) |
| `n` | new session (pick a profile) |
| `x` | kill selected |
| `r` | refresh · `q` quit |

## HTTP / WebSocket API

All under `/api`. Bearer token optional (`Authorization: Bearer …` or `?token=`).

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/health` | host label, os/arch, version, session count |
| `GET` | `/api/profiles` | available launch profiles |
| `GET` | `/api/sessions` | list sessions + status |
| `POST` | `/api/sessions` | spawn `{profile, name?, rows?, cols?}` |
| `DELETE` | `/api/sessions/:id` | kill |
| `POST` | `/api/sessions/:id/input` | fire-and-forget input (auto-appends Enter) |
| `GET` | `/api/sessions/:id/buffer` | current scrollback (bytes) |
| `GET` | `/api/sessions/:id/attach` | **WebSocket**: binary = PTY I/O; text JSON `{"type":"resize",…}` |

Status values: `working` (output now), `waiting` (tail looks like a prompt),
`idle`, `exited`.

## Deploy to the fleet

1. **[Set up Tailscale](deploy/tailscale-setup.md)** on all 5 boxes + your phone (MagicDNS + an ACL).
2. **Install the agent as a service** so sessions persist and start on boot:
   - Linux: `sudo ./deploy/install-linux.sh` (systemd, runs as you)
   - Windows: `./deploy/install-windows.ps1` (logon-triggered Scheduled Task)
3. **[Wire shared memory](deploy/obsidian-shared-memory.md)** — point profile `cwd` at your agent workspace.
4. Put every host's tailnet URL in `flotilla-hosts.toml` (and the web client's Hosts dialog).

## Limitations

- **No live-session survival across daemon restart.** The daemon owns the PTYs,
  so if it restarts, its sessions end. Mitigations: run it as a service (rare
  restarts); a metadata snapshot is written to `data_dir/sessions.json` for
  audit. (A future tmux/Zellij backend on Linux hosts could add true
  detach-survives-restart there.)
- **`wss://` not enabled** in the TUI client — plain `ws://` over the tailnet
  (Tailscale already encrypts). Use the web client over HTTPS if you front it
  with a reverse proxy.
- Status detection is heuristic (output recency + prompt-signature matching),
  not a semantic read of the agent.

## Roadmap

- **Phase 1 ✅** — agent + TUI + web client + deploy scripts (this repo).
- **Phase 2** — push notifications to the phone when a session goes `waiting`;
  richer Next.js dashboard; per-session labels/tags.
- **Phase 3** — optional tmux/Zellij backend on Linux hosts for restart-proof
  sessions; session "templates" (spawn a named set across hosts at once).

## License

MIT
