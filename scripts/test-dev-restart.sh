#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/fake-bins"

for binary in hearthd hearthctl hearth-agentd; do
  cp "${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}/bin/$binary" "$tmp/fake-bins/$binary"
done

cat > "$tmp/bin/systemctl" <<'EOF'
#!/bin/sh
echo "$*" >> "$HEARTH_TEST_SYSTEMCTL_LOG"
case "$*" in
  "is-active --quiet hearth-agentd.service") exit "${HEARTH_TEST_AGENT_ACTIVE:-1}" ;;
  "show --property MainPID --value hearth.service") echo 4242 ;;
esac
exit 0
EOF
cat > "$tmp/bin/journalctl" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$tmp/bin/devenv-fail" <<'EOF'
#!/bin/sh
exit 23
EOF
chmod 0755 "$tmp/bin/"*

export HEARTH_DEV_ROOT="$tmp/root"
export HEARTH_DEV_NO_SUDO=1
export HEARTH_DEV_BIN_DIR="$tmp/fake-bins"
export HEARTH_DEV_SKIP_BUILD=1
export HEARTH_DEV_SKIP_PING=1
export SYSTEMCTL="$tmp/bin/systemctl"
export JOURNALCTL="$tmp/bin/journalctl"
export HEARTH_TEST_SYSTEMCTL_LOG="$tmp/systemctl.log"
export HEARTH_TEST_AGENT_ACTIVE=0

"$repo_dir/scripts/dev-restart.sh" >/dev/null
test -f "$tmp/root/run/systemd/system/hearth.service.d/90-hearth-dev.conf"
test -f "$tmp/root/run/systemd/system/hearth-agentd.service.d/90-hearth-dev.conf"
grep -F 'restart hearth.service' "$tmp/systemctl.log" >/dev/null
grep -F 'restart hearth-agentd.service' "$tmp/systemctl.log" >/dev/null

"$repo_dir/scripts/dev-restart.sh" --reset >/dev/null
test ! -e "$tmp/root/run/hearth-dev"
test ! -e "$tmp/root/run/systemd/system/hearth.service.d/90-hearth-dev.conf"

: > "$tmp/systemctl.log"
unset HEARTH_DEV_SKIP_BUILD
export DEVENV="$tmp/bin/devenv-fail"
if "$repo_dir/scripts/dev-restart.sh" >/dev/null 2>&1; then
  echo "error: failed build test succeeded" >&2
  exit 1
fi
if grep -F 'restart ' "$tmp/systemctl.log" >/dev/null; then
  echo "error: a failed build restarted a service" >&2
  exit 1
fi
echo "dev restart helper tests passed"
