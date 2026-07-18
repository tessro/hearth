#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export HEARTH_STAGE_DIR="${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$repo_dir" log -1 --format=%ct)}"
HEARTH_DIST_DIR="$tmp/a" "$repo_dir/scripts/archive-release.sh" >/dev/null
HEARTH_DIST_DIR="$tmp/b" "$repo_dir/scripts/archive-release.sh" >/dev/null
cmp "$tmp/a/"*.tar.gz "$tmp/b/"*.tar.gz
echo "archive output is reproducible"
