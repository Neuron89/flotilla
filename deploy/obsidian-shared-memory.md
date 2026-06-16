# Shared memory: the Obsidian KB as every agent's brain

Flotilla doesn't reinvent shared memory — it makes sure every session it spawns
*inherits* the one you already run: the Obsidian vault on the home server that
your AI instances read and write on every change.

## How the wiring actually works

1. **Run the agent in your user context.** The systemd unit
   (`flotilla-agent@<you>.service`) and the Windows Scheduled Task both run as
   *you*, not as a system account. That means any `claude` session spawned by
   Flotilla inherits your `~/.claude` config — including the **user-scope
   Obsidian MCP server** you've already configured. No per-session setup.

2. **Profiles can pin a workspace.** Point a profile's `cwd` at the agent
   workspace that holds your `CLAUDE.md` / `.mcp.json`, so project-scope context
   loads automatically too:

   ```toml
   [[profiles]]
   name = "claude"
   command = "claude"
   cwd = "/home/hnester/agent-workspace"   # CLAUDE.md links to the vault
   ```

3. **The vault is reachable fleet-wide.** The Obsidian Local REST API
   (port 27124 on the home server) is just another service on the tailnet, so a
   Claude or Hermes session on the *work* server can read/write the same vault
   as one at home. Add it to that host's agent workspace config as the KB
   endpoint.

## Net effect

Spawn a `claude` on any of the five boxes from your phone, and it already knows
your AI system — because it's reading the same Obsidian brain every other
instance writes to. Flotilla is the hands; the vault is the shared memory.

## Optional: announce the session to the vault

If you want each spawned session recorded in the KB, set an env var in the
profile (`FLOTILLA_SESSION = "..."`) and have your `CLAUDE.md` / session hook
write a note. Flotilla exposes the env hook; the KB write stays in your existing
Obsidian tooling.
