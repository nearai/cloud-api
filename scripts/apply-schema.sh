#!/usr/bin/env bash
# Apply the ledger schema to a PostgreSQL database.
#
# The schema is owned by nearai/cloud-platform. This runs the `migrate` subcommand of the
# image pinned in scripts/cloud-platform-schema.env, which is the same migrator that runs
# before every deploy. Applying an up-to-date database is a no-op. cloud-api never migrates.
#
# Connection settings, highest precedence first (the same order the seed binary uses for
# .env, where exported variables win over the file):
#   1. exported PGHOST, PGPORT, PGUSER, PGPASSWORD, PGDATABASE
#   2. exported DATABASE_HOST, DATABASE_PORT, DATABASE_USERNAME, DATABASE_PASSWORD, DATABASE_NAME
#   3. the same names in the repo's .env, read without overriding anything above
#   4. defaults: localhost, 5432, postgres, empty password. PGDATABASE or DATABASE_NAME is required.
#
# The migrator sends no TLS to local hosts only (loopback, localhost, or a single-label name
# such as a compose service) and refuses any other host.
#
# SCHEMA_DOCKER_NETWORK overrides the docker --network value. The default is host, except on
# Docker Desktop, where localhost is mapped to the host gateway through a single-label alias.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

eval "$(python3 -I - "$repo_root/.env" <<'PY'
import os, shlex, sys

def read_dotenv(path):
    values = {}
    try:
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                line = raw.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                if line.startswith("export "):
                    line = line[len("export "):]
                key, _, value = line.partition("=")
                value = value.strip()
                if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                    value = value[1:-1]
                values[key.strip()] = value
    except FileNotFoundError:
        pass
    return values

env = {**read_dotenv(sys.argv[1]), **os.environ}

def pick(name, legacy, default=""):
    for key in (name, legacy):
        if env.get(key):
            return env[key]
    return default

settings = {
    "PGHOST": pick("PGHOST", "DATABASE_HOST", "localhost"),
    "PGPORT": pick("PGPORT", "DATABASE_PORT", "5432"),
    "PGUSER": pick("PGUSER", "DATABASE_USERNAME", "postgres"),
    "PGPASSWORD": pick("PGPASSWORD", "DATABASE_PASSWORD", ""),
    "PGDATABASE": pick("PGDATABASE", "DATABASE_NAME", ""),
}
for name, value in settings.items():
    print(f"export {name}={shlex.quote(value)}")
PY
)"

if [[ -z "${PGDATABASE:-}" ]]; then
  echo "apply-schema: PGDATABASE (or DATABASE_NAME) must name the database to migrate" >&2
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
