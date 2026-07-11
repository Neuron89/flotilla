#!/usr/bin/env bash
# Install flotilla-agent as a systemd service on a Linux host (home/work server,
# CachyOS laptop). Run from the repo root:  sudo ./deploy/install-linux.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN_USER="${SUDO_USER:-$USER}"

echo "==> Building release binary"
( cd "$REPO_ROOT" && cargo build --release -p flotilla-agent )

echo "==> Installing binary to /usr/local/bin"
install -m 0755 "$REPO_ROOT/target/release/flotilla-agent" /usr/local/bin/flotilla-agent

echo "==> Installing config to /etc/flotilla"
mkdir -p /etc/flotilla
CONFIG=/etc/flotilla/flotilla.toml
if [ ! -f "$CONFIG" ]; then
  cp "$REPO_ROOT/config/flotilla.example.toml" "$CONFIG"
  # Default the host label to this machine's hostname.
  sed -i "s/^host_label = .*/host_label = \"$(hostname)\"/" "$CONFIG"
  echo "    wrote $CONFIG (edit profiles/token as needed)"
else
  echo "    $CONFIG exists — leaving it"
fi

# The config holds the bearer token + ntfy token — lock it down to the owner.
# The service runs as $RUN_USER (see flotilla-agent@.service's User=%i), so
# that's who needs to be able to read it, not root.
chown "$RUN_USER" "$CONFIG"
chmod 600 "$CONFIG"

# Fail-open guard: an empty (or still-placeholder) token means the daemon
# refuses non-loopback binds. If the token field hasn't been set for real,
# generate a random one now so installs are secure by default.
if grep -qE '^token = "(|REPLACE_ME[A-Za-z0-9_]*)"[[:space:]]*(#.*)?$' "$CONFIG"; then
  GEN_TOKEN="$(openssl rand -hex 24 2>/dev/null || head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32)"
  sed -i "s/^token = \"[^\"]*\"/token = \"$GEN_TOKEN\"/" "$CONFIG"
  echo "    generated a random bearer token (shown once):"
  echo "        $GEN_TOKEN"
  echo "    it's saved in $CONFIG — copy it into flotilla-hosts.toml for the TUI/web client"
fi

echo "==> Installing systemd unit (runs as: $RUN_USER)"
install -m 0644 "$REPO_ROOT/deploy/flotilla-agent.service" /etc/systemd/system/flotilla-agent@.service
systemctl daemon-reload
systemctl enable --now "flotilla-agent@${RUN_USER}.service"

echo "==> Done. Status:"
systemctl --no-pager status "flotilla-agent@${RUN_USER}.service" || true
echo
echo "Tailnet URL for the fleet:  http://$(hostname):7878"
echo "Logs:  journalctl -u flotilla-agent@${RUN_USER} -f"
