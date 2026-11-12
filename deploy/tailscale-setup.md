# Tailscale: the bridge under the fleet

Flotilla does **no** networking, auth federation, or NAT traversal of its own.
It binds a plain HTTP/WS port per host and lets Tailscale be the secure overlay
that connects your home server, work server, both laptops, the desktop, **and
your phone** into one flat network — so `http://work-server:7878` resolves and
is reachable from your couch exactly as it does at your desk.

## 1. Install Tailscale on every host

- Linux: `curl -fsSL https://tailscale.com/install.sh | sh`
- Windows: `winget install Tailscale.Tailscale`
- Phone: Tailscale app from the App Store / Play Store.

Sign every device into the **same tailnet**.

## 2. Enable MagicDNS

Admin console → DNS → enable **MagicDNS**. Now each host is reachable by its
short name (e.g. `home-server`, `work-server`) from any other device. Those are
exactly the names you put in `flotilla-hosts.toml`.

Check names with:

```
tailscale status
```

## 3. (Recommended) Tag the fleet and lock the port with an ACL

So only your own devices can reach the agent port, add a tailnet ACL. Example:

```jsonc
{
  "tagOwners": { "tag:flotilla": ["autogroup:admin"] },
  "acls": [
    // Your personal devices may reach the agent port on any flotilla host.
    { "action": "accept", "src": ["autogroup:member"], "dst": ["tag:flotilla:7878"] }
  ]
}
```

Tag each agent host (`tailscale up --advertise-tags=tag:flotilla`). With this in
place you can safely leave the bearer `token` empty — the tailnet is the gate.
Set a token too if you want belt-and-suspenders.

## 4. Phone access

Once the phone is on the tailnet, open `http://home-server:7878` (or any agent)
in mobile Safari/Chrome. The web client lets you add every host, watch live
status, send a prompt to any agent, and attach a full terminal. Add it to your
home screen for an app-like PWA.

## 5. Tailscale SSH (optional, for deploys)

`tailscale up --ssh` on each host lets you `ssh <host>` over the tailnet with no
key management — handy for running the install scripts and pulling updates.

## Why not Cloudflare Tunnel here?

Cloudflare Tunnel suits *public* services that want a public hostname.
Flotilla is the opposite: a private control plane you never
want on the public internet. Tailscale keeps it dark and still reachable from
anywhere — including the work network, without poking holes in either firewall.
```
