#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage_dir="${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}"
version="${HEARTH_VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/Cargo.toml" | head -1)}"
flavor="${HEARTH_STAGE_FLAVOR:-native}"

for binary in \
  bin/hearthd \
  bin/hearthctl \
  bin/hearth-agentd \
  lib/hearth/guest/hearth-guestd; do
  path="$stage_dir/$binary"
  test -x "$path" || { echo "error: missing executable $binary" >&2; exit 1; }
  if [ "$binary" = lib/hearth/guest/hearth-guestd ] || [ "$flavor" = portable ]; then
    readelf -lW "$path" | grep -q ' INTERP ' && {
      echo "error: $binary has a dynamic interpreter" >&2
      exit 1
    }
  fi
  "$path" --version | grep -F " $version" >/dev/null || {
    echo "error: $binary does not report $version" >&2
    exit 1
  }
done

test -x "$stage_dir/lib/hearth/guest/hearth-guestd"
test "$(cat "$stage_dir/lib/hearth/kernel/contract")" = "$(sed -n 's/^KERNEL_CONTRACT=//p' "$repo_dir/guest/kernel-version.env")"
test -f "$stage_dir/etc/hearth/verb-policy.toml"
if find "$stage_dir" \( -iname '*authorized_key*' -o -iname '*token*' -o -iname '*ref-key*' -o -iname '*.qcow2' \) | grep -q .; then
  echo "error: release stage contains a key, token, or VM disk" >&2
  exit 1
fi
if find "$stage_dir" -type f -print0 | xargs -0 grep -Il 'cloud-hypervisor' | grep -E '/(bin|guest)/' >/dev/null; then
  echo "error: release stage appears to contain Cloud Hypervisor" >&2
  exit 1
fi
echo "release stage verified: $stage_dir"
