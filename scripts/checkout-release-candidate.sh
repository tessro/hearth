#!/usr/bin/env bash
set -euo pipefail

bundle="${1:-}"
sha="${2:-}"
test -f "$bundle" || { echo "error: release candidate bundle is missing: $bundle" >&2; exit 2; }
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "error: invalid release candidate SHA: $sha" >&2; exit 2; }

git fetch --no-tags "$bundle" refs/heads/release-candidate
git checkout --detach "$sha"
test "$(git rev-parse HEAD)" = "$sha"
test -z "$(git status --porcelain)"
