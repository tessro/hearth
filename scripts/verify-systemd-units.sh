#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage_dir="${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}"
unit_dir="$stage_dir/lib/systemd/system"

grep -Fx 'ExecStart=/usr/bin/hearthd --guest-kernel /usr/lib/hearth/kernel/vmlinux' \
  "$unit_dir/hearth.service" >/dev/null
grep -Fx 'ExecStart=/usr/bin/hearth-agentd --token-file %d/http-token --ref-key-file %d/ref-key' \
  "$unit_dir/hearth-agentd.service" >/dev/null

verify_dir="$(mktemp -d)"
trap 'rm -rf "$verify_dir"' EXIT
true_bin="$(type -P true)"
test -x "$true_bin"
for unit in hearth.service hearth-agentd.service; do
  sed "s|^ExecStart=.*$|ExecStart=$true_bin|" "$unit_dir/$unit" > "$verify_dir/$unit"
done

# The staged paths become /usr paths only when a package is installed. Replace
# ExecStart in these copies so systemd checks the unit files, not the build host.
systemd-analyze verify "$verify_dir/hearth.service" "$verify_dir/hearth-agentd.service"
