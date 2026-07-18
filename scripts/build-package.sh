#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
format="${1:-}"
target="${2:-}"
version="${HEARTH_VERSION:-}"
stage="${HEARTH_STAGE:-}"
case "$format" in deb|rpm) ;; *) echo "error: format must be deb or rpm" >&2; exit 2;; esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: invalid version" >&2; exit 2; }
test -d "$stage/bin" || { echo "error: invalid stage: $stage" >&2; exit 2; }
case "$stage" in *'|'*|*$'\n'*) echo "error: unsupported stage path" >&2; exit 2;; esac
test -n "$target" || { echo "error: package target is required" >&2; exit 2; }

config="$(mktemp)"
trap 'rm -f "$config"' EXIT
sed -e "s|@HEARTH_VERSION@|$version|g" \
    -e "s|@HEARTH_STAGE@|$stage|g" \
    "$repo_dir/packaging/nfpm.yaml" > "$config"
nfpm package --config "$config" --packager "$format" --target "$target"
