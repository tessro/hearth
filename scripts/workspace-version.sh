#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$({
  sed -n '/^\[workspace\.package\]$/,/^\[/s/^version = "\([^"]*\)"/\1/p' \
    "$repo_dir/Cargo.toml"
} | head -1)"

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: workspace version is not X.Y.Z: $version" >&2
  exit 1
}
echo "$version"
