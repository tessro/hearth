#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
level="${1:-}"
case "$level" in
  major|minor|patch) ;;
  *) echo "usage: scripts/bump-version.sh major|minor|patch" >&2; exit 2 ;;
esac

current="$("$repo_dir/scripts/workspace-version.sh")"
IFS=. read -r major minor patch <<< "$current"
case "$level" in
  major) major=$((major + 1)); minor=0; patch=0 ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  patch) patch=$((patch + 1)) ;;
esac
next="$major.$minor.$patch"

sed -i "0,/^version = \"$current\"$/s//version = \"$next\"/" "$repo_dir/Cargo.toml"
test "$("$repo_dir/scripts/workspace-version.sh")" = "$next"

cd "$repo_dir"
"${CARGO:-cargo}" metadata --format-version 1 >/dev/null
echo "$next"
