#!/usr/bin/env bash
# Set up a Cloudflare Tunnel that exposes THIS host's flotilla-agent at a public
# hostname, so you can put Cloudflare Access (OAuth/OTP) in front of it.
#
#   sudo ./deploy/install-cloudflared.sh home-server.example.com [local-port]
#
# This is the locally-managed-tunnel path (Option B in
# deploy/cloudflare-access-setup.md). If you already run a dashboard-managed
# tunnel, just add a public hostname there instead — you don't need this script.
#
# After it finishes, create the Access application + policy for the hostname and
# fill in the [access] block of flotilla.toml. See cloudflare-access-setup.md.
set -euo pipefail

HOSTNAME="${1:-}"
PORT="${2:-7878}"
if [ -z "$HOSTNAME" ]; then
  echo "usage: sudo $0 <public-hostname> [local-port]" >&2
  echo "   e.g. sudo $0 home-server.example.com 7878" >&2
  exit 1
fi

TUNNEL_NAME="flotilla-$(hostname -s)"
CF_DIR="/etc/cloudflared"

echo "==> 1/5 Installing cloudflared"
if ! command -v cloudflared >/dev/null 2>&1; then
  if command -v pacman >/dev/null 2>&1; then
    pacman -S --noconfirm cloudflared
  elif command -v apt-get >/dev/null 2>&1; then
    # Cloudflare apt repo
    mkdir -p --mode=0755 /usr/share/keyrings
    curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg | tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null
    echo "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main" \
      > /etc/apt/sources.list.d/cloudflared.list
    apt-get update && apt-get install -y cloudflared
  else
    echo "Install cloudflared manually for this distro, then re-run." >&2
    exit 1
  fi
else
  echo "    cloudflared already installed ($(cloudflared --version 2>/dev/null | head -1))"
fi

echo "==> 2/5 Login (opens a browser to authorize a Cloudflare zone)"
# Idempotent: cert.pem present means we're already logged in.
if [ ! -f "$CF_DIR/cert.pem" ] && [ ! -f "$HOME/.cloudflared/cert.pem" ]; then
  cloudflared tunnel login
fi

echo "==> 3/5 Creating tunnel '$TUNNEL_NAME' (if absent)"
if ! cloudflared tunnel list 2>/dev/null | awk '{print $2}' | grep -qx "$TUNNEL_NAME"; then
  cloudflared tunnel create "$TUNNEL_NAME"
else
  echo "    tunnel '$TUNNEL_NAME' already exists"
fi
TUNNEL_ID="$(cloudflared tunnel list 2>/dev/null | awk -v n="$TUNNEL_NAME" '$2==n{print $1}')"
CRED_FILE="$(ls -1 "$HOME/.cloudflared/$TUNNEL_ID.json" /root/.cloudflared/"$TUNNEL_ID".json 2>/dev/null | head -1 || true)"

echo "==> 4/5 Writing $CF_DIR/config.yml  ($HOSTNAME -> http://localhost:$PORT)"
mkdir -p "$CF_DIR"
[ -n "$CRED_FILE" ] && install -m 0600 "$CRED_FILE" "$CF_DIR/$TUNNEL_ID.json"
cat > "$CF_DIR/config.yml" <<YAML
tunnel: $TUNNEL_ID
credentials-file: $CF_DIR/$TUNNEL_ID.json

ingress:
  - hostname: $HOSTNAME
    service: http://localhost:$PORT
  - service: http_status:404
YAML

echo "    routing DNS $HOSTNAME -> tunnel"
cloudflared tunnel route dns "$TUNNEL_NAME" "$HOSTNAME" || \
  echo "    (route may already exist — fine)"

echo "==> 5/5 Installing cloudflared as a service"
cloudflared service install || systemctl enable --now cloudflared || true
systemctl restart cloudflared 2>/dev/null || true

cat <<DONE

==> Tunnel up:  https://$HOSTNAME  ->  http://localhost:$PORT

Next (one-time, in the Cloudflare Zero Trust dashboard):
  1. Access -> Applications -> Add (Self-hosted), domain = $HOSTNAME
  2. Add an identity provider (Google/GitHub) or enable One-time PIN
  3. Add an Allow policy (your email / your domain)
  4. Copy the Application Audience (AUD) tag + your team domain into
     flotilla.toml [access], set enabled = true, and restart flotilla-agent.

Full walkthrough: deploy/cloudflare-access-setup.md
DONE
