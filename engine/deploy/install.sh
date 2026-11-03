#!/usr/bin/env bash
# Install bo-server on a Debian/Ubuntu server. Run as root from the directory holding the
# release files (bo-server, bo-device, deploy/):
#
#   sudo ./deploy/install.sh --host sync.example.com [--host 203.0.113.7] [--port 7443]
#
# Safe to re-run: it never overwrites an existing data directory, so it upgrades the binary
# without touching the database, keys or enrolled devices.
set -euo pipefail

port=7443
hosts=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) hosts+=("$2"); shift 2 ;;
    --port) port="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $EUID -eq 0 ]] || { echo "run as root (sudo)" >&2; exit 1; }
here="$(cd "$(dirname "$0")/.." && pwd)"
[[ -x "$here/bo-server" ]] || { echo "bo-server binary not found next to deploy/" >&2; exit 1; }

data=/var/lib/bo-server

# 1. service account with no login shell and no home
id bo-server >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin bo-server

# 2. binaries (stop first so an upgrade replaces a running file cleanly)
systemctl stop bo-server 2>/dev/null || true
install -m 0755 "$here/bo-server" /usr/local/bin/bo-server
install -m 0755 "$here/bo-device" /usr/local/bin/bo-device

# 3. data directory, initialised once
if [[ ! -f "$data/config.json" ]]; then
  [[ ${#hosts[@]} -gt 0 ]] || { echo "first install needs at least one --host (the name or IP devices will use)" >&2; exit 1; }
  install -d -m 0700 -o bo-server -g bo-server "$data"
  args=()
  for h in "${hosts[@]}"; do args+=(--host "$h"); done
  sudo -u bo-server /usr/local/bin/bo-server init "$data" "${args[@]}" --listen "0.0.0.0:$port"
  chown -R bo-server:bo-server "$data"
  chmod -R go-rwx "$data"
  echo
  echo "IMPORTANT: copy $data/tls/ca.key off this server to safe offline storage, then delete it here:"
  echo "    shred -u $data/tls/ca.key"
  echo "Give devices $data/server-identity.json (public)."
else
  echo "existing installation found in $data: keeping database, keys and enrolments"
fi

# 4. service
install -m 0644 "$here/deploy/bo-server.service" /etc/systemd/system/bo-server.service
systemctl daemon-reload
systemctl enable --now bo-server

# 5. firewall (only if ufw is in use; other firewalls must be opened by hand)
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow "$port/tcp" comment "bo-server sync (TLS)"
fi

sleep 1
systemctl --no-pager --lines=5 status bo-server || true
echo
echo "Enrol a device (stops and restarts the service around the change):"
echo "    sudo systemctl stop bo-server"
echo "    sudo -u bo-server bo-server enroll $data --device <id> --public <key>"
echo "    sudo systemctl start bo-server"
