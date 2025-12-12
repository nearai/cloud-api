#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

src="$repo_root/scripts/git-hooks/pre-commit"
dst="$repo_root/.git/hooks/pre-commit"

if [[ ! -f "$src" ]]; then
  echo "Missing hook source: $src" >&2
  exit 1
fi

mkdir -p "$(dirname "$dst")"

# Copy and ensure it is executable.
install -m 0755 "$src" "$dst"

echo "Installed pre-commit hook to: $dst"
