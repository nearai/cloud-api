#!/usr/bin/env bash
# Apply the ledger schema to one PostgreSQL database by running the `migrate` subcommand of
# the image pinned in scripts/cloud-platform-schema.env. That is the migrator that runs before
# every deploy. Applying an up-to-date database is a no-op. cloud-api never migrates.
#
# Usage: scripts/apply-schema.sh
#
# Connection: explicit libpq variables only, all required:
#   PGHOST, PGPORT, PGDATABASE, PGUSER   and   PGPASSWORD (may be empty, must be set)
# The script reads no .env file and no other configuration. Callers resolve their own settings
# and pass these in: the seed binary, the test helpers, and CI do this.
#
# The migrator sends no TLS and accepts plaintext only to local hosts: loopback, localhost, or a
# single-label name such as a compose service.
#
# SCHEMA_DOCKER_NETWORK overrides the docker --network value (default: host). On Docker Desktop,
# localhost is routed through a single-label alias to the host gateway instead.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

missing=()
for name in PGHOST PGPORT PGDATABASE PGUSER; do
  [[ -n "${!name:-}" ]] || missing+=("$name")
done
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "apply-schema: required variables not set: ${missing[*]}" >&2
  echo "apply-schema: usage: PGHOST=... PGPORT=... PGDATABASE=... PGUSER=... PGPASSWORD=... scripts/apply-schema.sh" >&2
  exit 1
fi
if [[ -z "${PGPASSWORD+x}" ]]; then
  echo "apply-schema: PGPASSWORD must be set (it may be empty)" >&2
  exit 1
fi

# shellcheck disable=SC1091
source "$script_dir/cloud-platform-schema.env"

if ! command -v docker >/dev/null 2>&1; then
  echo "apply-schema: docker is required to run $CLOUD_PLATFORM_IMAGE" >&2
  exit 1
fi

if ! docker info >/dev/null 2>&1; then
  echo "apply-schema: the docker daemon is not reachable. Start Docker Desktop or colima first." >&2
  exit 1
fi

if ! docker image inspect "$CLOUD_PLATFORM_IMAGE" >/dev/null 2>&1; then
  if ! pull_output="$(docker pull "$CLOUD_PLATFORM_IMAGE" 2>&1)"; then
    echo "apply-schema: docker could not pull $CLOUD_PLATFORM_IMAGE:" >&2
    printf '%s\n' "$pull_output" | tail -n 3 >&2
    echo "apply-schema: the package is private. Log in with a token that can read nearai/cloud-platform packages (docker login ghcr.io)." >&2
    exit 1
  fi
fi

docker_os="$(docker info --format '{{.OperatingSystem}}' 2>/dev/null || true)"
net_args=(--network "${SCHEMA_DOCKER_NETWORK:-host}")
if [[ -z "${SCHEMA_DOCKER_NETWORK:-}" && "$docker_os" == "Docker Desktop" \
      && "$PGHOST" =~ ^(localhost|127\.0\.0\.1|::1)$ ]]; then
  # Docker Desktop host networking does not reach the Mac's localhost. The migrator accepts
  # plaintext only for single-label names, so map `dbhost` to the host gateway and connect to it.
  net_args=(--add-host "dbhost:host-gateway")
  export PGHOST=dbhost
fi

# The secret is streamed on stdin and read through DB_SECRET_FILE=/dev/stdin, so the
# password never reaches a file on disk or the process arguments.
secret_json() {
  python3 -I -c '
import json, os
print(json.dumps({
    "host": os.environ["PGHOST"],
    "port": int(os.environ["PGPORT"]),
    "dbname": os.environ["PGDATABASE"],
    "username": os.environ["PGUSER"],
    "password": os.environ["PGPASSWORD"],
}))
'
}

set +e
secret_json | docker run --rm -i "${net_args[@]}" \
  -e DB_SECRET_FILE=/dev/stdin \
  -e CLOUD_PLATFORM_ALLOW_PLAINTEXT_DB=1 \
  "$CLOUD_PLATFORM_IMAGE" migrate
status=$?
set -e

if [[ $status -ne 0 ]]; then
  echo "apply-schema: migrate exited with $status (database ${PGHOST}:${PGPORT}/${PGDATABASE})." >&2
  echo "apply-schema: if the container cannot reach that database, set SCHEMA_DOCKER_NETWORK to a docker network that can, or set PGHOST to the address the container should use." >&2
  exit "$status"
fi
