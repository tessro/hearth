#!/usr/bin/env bash
set -euo pipefail

tag="${1:-${GITHUB_REF_NAME:-}}"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: release tag must match vX.Y.Z: $tag" >&2
  exit 1
}
test "$tag" = "v$version" || {
  echo "error: tag $tag does not match Cargo version $version" >&2
  exit 1
}
echo "$version"
