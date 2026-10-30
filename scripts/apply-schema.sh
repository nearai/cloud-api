#!/usr/bin/env bash
# Apply the ledger schema to a PostgreSQL database.
#
# The schema is owned by nearai/cloud-platform. This runs the `migrate` subcommand of the
# image pinned in scripts/cloud-platform-schema.env, which is the same migrator that runs
# before every deploy. Applying an up-to-date database is a no-op. cloud-api never migrates.
#
# Connection settings use libpq variables: PGDATABASE (required), PGHOST (default localhost),
# PGPORT (5432), PGUSER (postgres), PGPASSWORD. The migrator sends no TLS for local hosts
# only (loopback, localhost, or a single-label name such as a compose service) and refuses
# any other host, so this script is for local and CI databases.
#
# SCHEMA_DOCKER_NETWORK sets the docker --network value (default: host). Use a compose
# network name when the database is a compose service that is not published to the host.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$script_dir/cloud-platform-schema.env"

: "${PGDATABASE:?PGDATABASE must name the database to migrate}"
export PGHOST="${PGHOST:-localhost}"
export PGPORT="${PGPORT:-5432}"
export PGUSER="${PGUSER:-postgres}"
export PGPASSWORD="${PGPASSWORD:-}"

if ! command -v docker >/dev/null 2>&1; then
  echo "apply-schema: docker is required to run $CLOUD_PLATFORM_IMAGE" >&2
  exit 1
fi

if ! docker image inspect "$CLOUD_PLATFORM_IMAGE" >/dev/null 2>&1; then
  if ! docker pull "$CLOUD_PLATFORM_IMAGE" >/dev/null; then
    echo "apply-schema: cannot pull $CLOUD_PLATFORM_IMAGE." >&2
    echo "apply-schema: log in with a token that can read nearai/cloud-platform packages (docker login ghcr.io)." >&2
    exit 1
  fi
fi

# The secret is streamed on stdin and read through DB_SECRET_FILE=/dev/stdin, so the
# password never reaches a file on disk or the process arguments.
secret_json() {
  python3 -c '
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

secret_json | docker run --rm -i \
  --network "${SCHEMA_DOCKER_NETWORK:-host}" \
  -e DB_SECRET_FILE=/dev/stdin \
  -e CLOUD_PLATFORM_ALLOW_PLAINTEXT_DB=1 \
  "$CLOUD_PLATFORM_IMAGE" migrate
