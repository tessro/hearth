#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dev_root="${HEARTH_DEV_ROOT:-}"
runtime_root="$dev_root/run"
systemctl_cmd="${SYSTEMCTL:-systemctl}"
journalctl_cmd="${JOURNALCTL:-journalctl}"
devenv_cmd="${DEVENV:-devenv}"

as_root() {
  if [ -n "$dev_root" ] || [ "${HEARTH_DEV_NO_SUDO:-0}" = 1 ]; then
    "$@"
  else
    sudo "$@"
  fi
}

systemctl_root() {
  as_root "$systemctl_cmd" "$@"
}

reset_dev() {
  for path in \
    "$runtime_root/systemd/system/hearth.service.d/90-hearth-dev.conf" \
    "$runtime_root/systemd/system/hearth-agentd.service.d/90-hearth-dev.conf"; do
    if [ -e "$path" ]; then
      as_root rm -f "$path"
    fi
  done
  for dir in \
    "$runtime_root/systemd/system/hearth.service.d" \
    "$runtime_root/systemd/system/hearth-agentd.service.d"; do
    if [ -d "$dir" ]; then
      as_root rmdir "$dir" 2>/dev/null || true
    fi
  done
  if [ -d "$runtime_root/hearth-dev" ]; then
    as_root rm -rf "$runtime_root/hearth-dev"
  fi
  systemctl_root daemon-reload
  echo "Removed Hearth runtime overrides. Installed units will run on the next restart."
}

if [ "${1:-}" = "--reset" ]; then
  reset_dev
  exit 0
fi

agent_was_active=0
if systemctl_root is-active --quiet hearth-agentd.service; then
  agent_was_active=1
fi

# Finish every build before changing the running services.
if [ "${HEARTH_DEV_SKIP_BUILD:-0}" != 1 ]; then
  "$devenv_cmd" shell -- make build
  if [ "${HEARTH_DEV_AGENT_PLANE:-0}" = 1 ]; then
    "$devenv_cmd" shell -- make agent-plane-artifacts
  fi
fi

sha="$(git -C "$repo_dir" rev-parse --short=12 HEAD)"
source_bin_dir="${HEARTH_DEV_BIN_DIR:-$repo_dir/target/release}"
deploy_dir="$runtime_root/hearth-dev/$sha"
unit_dir="$runtime_root/systemd/system"
for binary in hearthd hearthctl hearth-agentd; do
  test -x "$source_bin_dir/$binary" || {
    echo "error: missing built binary $source_bin_dir/$binary" >&2
    exit 1
  }
done

as_root install -d -m 0755 "$deploy_dir" \
  "$unit_dir/hearth.service.d" "$unit_dir/hearth-agentd.service.d"
for binary in hearthd hearthctl hearth-agentd; do
  as_root install -m 0755 "$source_bin_dir/$binary" "$deploy_dir/$binary"
done

hearth_dropin="$(mktemp)"
agent_dropin="$(mktemp)"
trap 'rm -f "$hearth_dropin" "$agent_dropin"' EXIT
cat > "$hearth_dropin" <<EOF
[Service]
ExecStart=
ExecStart=$deploy_dir/hearthd
Environment=PATH=$deploy_dir:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
EOF
cat > "$agent_dropin" <<EOF
[Service]
ExecStart=
ExecStart=$deploy_dir/hearth-agentd --token-file %d/http-token --ref-key-file %d/ref-key
Environment=PATH=$deploy_dir:/usr/local/bin:/usr/bin:/bin
EOF
as_root install -m 0644 "$hearth_dropin" "$unit_dir/hearth.service.d/90-hearth-dev.conf"
as_root install -m 0644 "$agent_dropin" "$unit_dir/hearth-agentd.service.d/90-hearth-dev.conf"
systemctl_root daemon-reload

restart_failed() {
  unit="$1"
  echo "error: failed to restart $unit" >&2
  systemctl_root --no-pager --full status "$unit" >&2 || true
  as_root "$journalctl_cmd" -u "$unit" -n 50 --no-pager >&2 || true
  exit 1
}

systemctl_root restart hearth.service || restart_failed hearth.service
if [ "$agent_was_active" = 1 ]; then
  systemctl_root restart hearth-agentd.service || restart_failed hearth-agentd.service
fi

if [ "${HEARTH_DEV_SKIP_PING:-0}" != 1 ]; then
  "$deploy_dir/hearthctl" ping
fi
pid="$(systemctl_root show --property MainPID --value hearth.service)"
version="$($deploy_dir/hearthd --version)"
echo "$version (pid $pid, source $sha)"
if [ "${HEARTH_DEV_AGENT_PLANE:-0}" = 1 ]; then
  payload="$repo_dir/target/x86_64-unknown-linux-musl/release/hearth-guestd"
  echo "Guest payload built. Upgrade VMs only with this separate command:"
  echo "$deploy_dir/hearthctl upgrade --from $payload"
fi
