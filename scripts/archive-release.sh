#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage_dir="${HEARTH_STAGE_DIR:-$repo_dir/target/release-stage}"
dist_dir="${HEARTH_DIST_DIR:-$repo_dir/dist}"
version="${HEARTH_VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/Cargo.toml" | head -1)}"
epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_dir" log -1 --format=%ct)}"
archive="$dist_dir/hearth-$version-x86_64-linux.tar.gz"

test -d "$stage_dir/bin" || { echo "error: release stage is missing" >&2; exit 1; }
mkdir -p "$dist_dir"
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  --format=posix --pax-option=delete=atime,delete=ctime \
  --transform="s,^,hearth-$version/," -C "$stage_dir" -cf - . \
  | gzip -n -9 > "$archive"
echo "$archive"
