#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage_dir="${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}"
target="${HEARTH_RELEASE_TARGET:-x86_64-unknown-linux-musl}"
flavor="${HEARTH_STAGE_FLAVOR:-native}"
case "$flavor" in
  native) host_bin_dir="${HEARTH_HOST_BIN_DIR:-$repo_dir/target/release}" ;;
  portable) host_bin_dir="$repo_dir/target/$target/release" ;;
  *) echo "error: HEARTH_STAGE_FLAVOR must be native or portable" >&2; exit 2 ;;
esac
guest_bin_dir="${HEARTH_GUEST_BIN_DIR:-$repo_dir/target/$target/release}"
kernel_dir="${HEARTH_KERNEL_BUILD_DIR:-$repo_dir/target/release-kernel/current}"
epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_dir" log -1 --format=%ct)}"

case "$stage_dir" in
  ""|/) echo "error: unsafe stage directory: $stage_dir" >&2; exit 1 ;;
esac

for binary in hearthd hearthctl hearth-agentd; do
  test -x "$host_bin_dir/$binary" || {
    echo "error: missing $host_bin_dir/$binary; run the release binary target" >&2
    exit 1
  }
  if [ "$flavor" = portable ] && readelf -lW "$host_bin_dir/$binary" | grep -q ' INTERP '; then
    echo "error: $binary has a dynamic interpreter" >&2
    exit 1
  fi
done
test -x "$guest_bin_dir/hearth-guestd" || { echo "error: static hearth-guestd is missing" >&2; exit 1; }
if readelf -lW "$guest_bin_dir/hearth-guestd" | grep -q ' INTERP '; then
  echo "error: hearth-guestd has a dynamic interpreter" >&2
  exit 1
fi
test -f "$kernel_dir/vmlinux" || {
  echo "error: missing guest kernel; run make release-kernel" >&2
  exit 1
}
test -f "$kernel_dir/contract" || { echo "error: guest kernel contract is missing" >&2; exit 1; }

mkdir -p "$stage_dir"
find "$stage_dir" -mindepth 1 -delete
install -d \
  "$stage_dir/bin" \
  "$stage_dir/lib/hearth/guest" \
  "$stage_dir/lib/hearth/kernel" \
  "$stage_dir/lib/systemd/system" \
  "$stage_dir/lib/sysusers.d" \
  "$stage_dir/lib/tmpfiles.d" \
  "$stage_dir/share/doc/hearth" \
  "$stage_dir/share/licenses/hearth" \
  "$stage_dir/etc/hearth"
install -m 0755 "$host_bin_dir/hearthd" "$host_bin_dir/hearthctl" "$host_bin_dir/hearth-agentd" "$stage_dir/bin/"
install -m 0755 "$guest_bin_dir/hearth-guestd" "$stage_dir/lib/hearth/guest/hearth-guestd"
install -m 0644 "$kernel_dir/vmlinux" "$kernel_dir/contract" "$stage_dir/lib/hearth/kernel/"
install -m 0644 "$repo_dir/packaging/systemd/"*.service "$stage_dir/lib/systemd/system/"
install -m 0644 "$repo_dir/packaging/sysusers.d/hearth.conf" "$stage_dir/lib/sysusers.d/hearth.conf"
install -m 0644 "$repo_dir/packaging/tmpfiles.d/hearth.conf" "$stage_dir/lib/tmpfiles.d/hearth.conf"
install -m 0644 "$repo_dir/systemd/hearth-agentd-verb-policy.toml" "$stage_dir/etc/hearth/verb-policy.toml"
install -m 0644 "$repo_dir/README.md" "$repo_dir/docs/operations.md" "$repo_dir/docs/agent-plane.md" "$stage_dir/share/doc/hearth/"
install -m 0644 "$repo_dir/LICENSE" "$stage_dir/share/licenses/hearth/LICENSE"

find "$stage_dir" -exec touch -h -d "@$epoch" {} +
echo "$stage_dir"
