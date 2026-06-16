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
if [ ! -f /etc/flotilla/flotilla.toml ]; then
  cp "$REPO_ROOT/config/flotilla.example.toml" /etc/flotilla/flotilla.toml
  # Default the host label to this machine's hostname.
  sed -i "s/^host_label = .*/host_label = \"$(hostname)\"/" /etc/flotilla/flotilla.toml
  echo "    wrote /etc/flotilla/flotilla.toml (edit profiles/token as needed)"
else
  echo "    /etc/flotilla/flotilla.toml exists — leaving it"
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
