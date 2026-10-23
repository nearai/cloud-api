#!/usr/bin/env bash
# Dependency guards, run in CI (lint job) and locally.
#
# 1. `sigstore-verify` may be a direct dependency of `tinfoil_provenance` only.
#    Guard 1 reads every dependency kind (normal, dev and build), so a test-only
#    dependency on it elsewhere also fails.
# 2. The CI tooling crates `tinfoil_sync` and `chutes_sync` must never appear in
#    the normal dependency tree of `api` or `services`. Guard 2 reads the normal
#    (runtime) tree only, because dev-dependencies never ship.
set -euo pipefail

cd "$(dirname "$0")/.."

command -v jq >/dev/null 2>&1 || { echo "ERROR: jq is required for the dependency guards" >&2; exit 1; }

ALLOWED_SIGSTORE_DEPENDENTS=("tinfoil_provenance")
TOOLING_CRATES=("tinfoil_sync" "chutes_sync")
GUARDED_CRATES=("api" "services")

fail=0

# --- Guard 1: direct dependents of sigstore-verify -------------------------
metadata=$(cargo metadata --format-version 1 --no-deps)
dependents=$(printf '%s' "$metadata" | jq -r '
  .packages[]
  | select(any(.dependencies[]; .name == "sigstore-verify"))
  | .name' | sort -u)

for pkg in $dependents; do
  allowed=0
  for ok in "${ALLOWED_SIGSTORE_DEPENDENTS[@]}"; do
    [[ "$pkg" == "$ok" ]] && allowed=1
  done
  if [[ $allowed -eq 0 ]]; then
    echo "ERROR: crate '$pkg' depends directly on 'sigstore-verify'; only ${ALLOWED_SIGSTORE_DEPENDENTS[*]} may." >&2
    fail=1
  fi
done

# --- Guard 2: tooling crates must not be reachable from api/services -------
for guarded in "${GUARDED_CRATES[@]}"; do
  for tool in "${TOOLING_CRATES[@]}"; do
    set +e
    out=$(cargo tree -p "$guarded" -e normal -i "$tool" 2>&1)
    status=$?
    set -e
    if [[ $status -ne 0 ]]; then
      # `cargo tree -i X` errors when X is not in the graph: that is a pass.
      if grep -q "did not match any packages" <<<"$out"; then
        continue
      fi
      echo "ERROR: 'cargo tree -p $guarded -i $tool' failed unexpectedly:" >&2
      echo "$out" >&2
      fail=1
    elif [[ $(grep -c . <<<"$out") -gt 1 ]]; then
      echo "ERROR: '$guarded' depends on tooling crate '$tool':" >&2
      echo "$out" >&2
      fail=1
    fi
  done
done

if [[ $fail -ne 0 ]]; then
  echo "Dependency guards FAILED." >&2
  exit 1
fi
echo "Dependency guards passed."
