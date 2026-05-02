# Local Development & Manual API Testing

This guide walks through running the cloud-api locally and exercising the
HTTP surface end-to-end with `curl`. It complements the quick-start in the
top-level [`README.md`](../README.md) by covering the mock-auth flow, the
full org → workspace → API key path, and how to point local cloud-api at
real inference backends.

> **Privacy note**: cloud-api runs in a TEE in production. The rules in
> [`CLAUDE.md`](../CLAUDE.md#-privacy--data-security---critical) apply
> locally too — don't paste customer prompts/completions into bug reports.

## 1. Prerequisites

- Rust (toolchain pinned in `rust-toolchain.toml`)
- Docker & Docker Compose
- `psql` is helpful but not required
- A Bash-compatible shell with `curl` and `jq` (the §4 walkthrough uses
  Bash array syntax)

## 2. Bring up the stack

```bash
# Postgres only — runs the API on the host so you get fast rebuilds.
docker compose up -d postgres

cp env.example .env
# env.example ships with AUTH_MOCK=false. Flip it for the walkthrough
# below — the rest of the defaults already match docker-compose.yml:
#   AUTH_MOCK=true                          ← change this
#   POSTGRES_PRIMARY_APP_ID=postgres-test   ← already the default

make dev
```

`make dev` runs migrations, seeds the mock admin user, and starts the API
on `http://localhost:3000`. Useful endpoints once it's up:

| Path                          | Purpose                              |
| ----------------------------- | ------------------------------------ |
| `GET  /health`                | Liveness probe                       |
| `GET  /docs`                  | Scalar UI (interactive)              |
| `GET  /api-docs/openapi.json` | Machine-readable OpenAPI spec        |
| `GET  /v1/models`             | Model catalog (requires API key)     |
| `POST /v1/chat/completions`   | OpenAI-compatible inference          |

To pull down everything:

```bash
docker compose down       # keeps the postgres volume
docker compose down -v    # nukes data; useful if seed conflicts
```

### Running everything in Docker

`docker compose up -d` brings up Postgres, the Datadog agent, and the API
in containers. It's slower to iterate on (full image rebuild per change)
but useful for reproducing a containerized environment. Set `DD_API_KEY`
in `.env` if you want the Datadog agent to actually report; otherwise the
agent will start, complain in logs, and the API will still work.

## 3. Auth flows for manual testing

The API has two mutually-exclusive auth methods:

- **Session auth** (cookies / `Authorization: Bearer rt_…`) for the
  management plane: organizations, workspaces, users, API keys.
- **API key auth** (`Authorization: Bearer sk-…`) for the data plane:
  chat completions, responses, conversations, attestation.

### Mock session auth (no real OAuth)

When `AUTH_MOCK=true`, the API accepts any session token starting with
`rt_`. The mock service always returns the seeded user
`admin@test.com`, but it derives the *user ID* from the token: if the
suffix after `rt_` parses as a UUID, that UUID is used as the effective
`user_id`; otherwise it falls back to the seeded admin
(`11111111-1111-1111-1111-111111111111`).

> **Heads-up**: a *random* UUID will pass auth but blow up the moment
> you create an org — `organization_members.user_id` is a foreign key
> on `users(id)`, and only the seeded UUID exists in `users`. Use one
> of the two patterns below.

Two ways to mint a token:

1. **Reuse the seeded admin user** — any `rt_<not-a-uuid>` falls back
   to the seeded admin:

   ```bash
   export SESSION="rt_local-dev-admin"
   ```

   Or use the seeded UUID explicitly:

   ```bash
   export SESSION="rt_11111111-1111-1111-1111-111111111111"
   ```

2. **Impersonate a different user** — first INSERT the user into
   `users`, then embed *that* UUID after `rt_`.

#### Admin endpoints under mock auth

The seeded mock user is `admin@test.com`. Admin access is granted by
email domain via `AUTH_ADMIN_DOMAINS` (default: `near.ai,near.org`),
so `admin@test.com` is **not** an admin out of the box. To exercise
`/v1/admin/*` locally with mock auth, add `test.com`:

```env
AUTH_ADMIN_DOMAINS=near.ai,near.org,test.com
```

#### Headers used below

```
Authorization: Bearer $SESSION
User-Agent: Mock User Agent
```

The `User-Agent: Mock User Agent` header is **only** required for
refresh-token endpoints (`POST /v1/users/me/access_tokens` and the
auth callback). For ordinary management endpoints, mock auth accepts
any `rt_…` Bearer regardless of User-Agent. Setting it everywhere is
harmless and makes the curl examples uniform.

### Real OAuth (optional)

To test the OAuth flow locally, set `AUTH_MOCK=false` and configure
`GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` (or the Google equivalents)
against an OAuth app whose callback URL is
`http://localhost:3000/v1/auth/callback`. Visit `/v1/auth/login` to start
the flow.

## 4. End-to-end walkthrough

This sequence creates an organization, workspace, and API key with
session auth, then makes an inference request with the resulting API key.
It assumes `make dev` is running and `$SESSION` is exported.

```bash
BASE=http://localhost:3000
AUTH=(-H "Authorization: Bearer $SESSION" -H "User-Agent: Mock User Agent")

# 1. Confirm the seeded admin is wired up
curl -s "$BASE/v1/users/me" "${AUTH[@]}" | jq .

# 2. Create an organization
ORG_ID=$(curl -s "$BASE/v1/organizations" "${AUTH[@]}" \
  -H 'Content-Type: application/json' \
  -d '{"name":"local-dev","description":"manual testing"}' \
  | jq -r .id)
echo "org=$ORG_ID"

# 3. Reuse the default workspace — `POST /v1/organizations` already
#    creates one named `default`. Posting another with the same name
#    would conflict on the (organization_id, name) unique index.
WS_ID=$(curl -s "$BASE/v1/organizations/$ORG_ID/workspaces" "${AUTH[@]}" \
  | jq -r '.workspaces[] | select(.name == "default") | .id')
echo "workspace=$WS_ID"

# 4. Mint an API key — capture the plaintext `key` field, it is shown once.
API_KEY=$(curl -s "$BASE/v1/workspaces/$WS_ID/api-keys" "${AUTH[@]}" \
  -H 'Content-Type: application/json' \
  -d '{"name":"local-test"}' \
  | jq -r .key)
echo "api_key=$API_KEY"

# 5. List models visible to that workspace (note: empty until backends
#    are wired up — see §5).
curl -s "$BASE/v1/models" \
  -H "Authorization: Bearer $API_KEY" | jq .

# 6. Make a chat completion (also requires backends — see §5).
curl -s "$BASE/v1/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "local/test-model",
    "messages": [{"role":"user","content":"hello"}],
    "max_tokens": 16
  }' | jq .
```

> Newly-created API keys can take up to 10 seconds to be picked up by the
> in-memory cache. If step 5 returns 401, retry after a short pause.

## 5. Wiring up inference backends

cloud-api itself doesn't run vLLM/SGLang — it loads model rows from the
`models` table and forwards requests to whatever URL is in
`models.inference_url`. The legacy discovery-server path was removed in
PR #513, so `MODEL_DISCOVERY_SERVER_URL` / `MODEL_DISCOVERY_REFRESH_INTERVAL`
in `env.example` are **no-ops** today; only `INFERENCE_API_KEY` (a.k.a.
`MODEL_DISCOVERY_API_KEY`) is read, as the bearer token forwarded to
the inference URL.

### Option A — exercise prod against the real cloud-api

The fastest way to test the streaming path is to skip local cloud-api
entirely and `curl` `https://cloud-api.near.ai/v1/chat/completions`
directly with a prod or staging API key. Use this when you only need to
verify request/response shapes — no local server required.

### Option B — local OpenAI-compatible server

Run vLLM, SGLang, or any OpenAI-compatible mock locally and insert a
matching `models` row.

```bash
# 1. Start a local OpenAI-compatible server on :8002
#    (vLLM, SGLang, ollama-compat, etc.)

# 2. Insert a model row that targets it. The columns below are the
#    bare minimum — check `crates/database/src/migrations/sql/` for the
#    latest schema (it drifts faster than this doc does).
#
#    URL: use http://localhost:8002 when cloud-api runs on the host
#    (the default `make dev` flow). Use http://host.docker.internal:8002
#    when cloud-api runs inside the docker-compose stack.
psql "host=localhost user=postgres password=postgres dbname=platform_api" <<'SQL'
INSERT INTO models (id, model_id, inference_url, is_active)
VALUES (
  gen_random_uuid(),
  'local/test-model',
  'http://localhost:8002',
  true
);
SQL
```

### Option C — admin PATCH

If you want to populate models the same way prod does, use
`PATCH /v1/admin/models`. The body shape is
`HashMap<model_id, UpdateModelApiRequest>` — note that `inferenceUrl`
is **camelCase**; the snake_case form is silently ignored.

Admin endpoints are gated by `AUTH_ADMIN_DOMAINS`. The default value
(`near.ai,near.org`) excludes the seeded mock user `admin@test.com`,
so for local testing add `test.com`:

```env
AUTH_ADMIN_DOMAINS=near.ai,near.org,test.com
```

Then:

```bash
curl -s "$BASE/v1/admin/models" "${AUTH[@]}" \
  -X PATCH -H 'Content-Type: application/json' \
  -d '{
    "local/test-model": {
      "modelDisplayName": "Local Test",
      "modelDescription": "Local OpenAI-compatible backend",
      "contextLength": 8192,
      "inferenceUrl": "http://localhost:8002"
    }
  }' | jq .
```

Provider refresh runs every 300s by default
(`EXTERNAL_PROVIDER_REFRESH_INTERVAL`). Lower it while iterating.

## 6. Useful endpoints to exercise

| Endpoint                                    | Auth     | Notes                                                       |
| ------------------------------------------- | -------- | ----------------------------------------------------------- |
| `GET  /v1/users/me`                         | session  | Sanity check that mock auth works                           |
| `POST /v1/organizations`                    | session  | Body: `{"name": "...", "description": "..."}`               |
| `POST /v1/organizations/{id}/workspaces`    | session  | Same body shape                                             |
| `POST /v1/workspaces/{id}/api-keys`         | session  | Returns plaintext `key` — store it, it isn't shown again    |
| `GET  /v1/models`                           | API key  | Catalog visible to the workspace                            |
| `POST /v1/chat/completions`                 | API key  | OpenAI-compatible. Add `"stream": true` for SSE             |
| `POST /v1/responses`                        | API key  | Platform-specific event-streamed responses                  |
| `POST /v1/conversations`                    | API key  | Conversation lifecycle                                      |
| `GET  /v1/attestation/report`               | API key  | TEE attestation (503 outside a CVM unless `DEV=true` in debug builds) |
| `GET  /v1/signature/{chat_id}`              | API key  | Per-completion signature lookup                             |

The Scalar UI at `http://localhost:3000/docs` lets you fire each of these
interactively and inspect request/response schemas.

## 7. Troubleshooting

**`make dev` fails with "Seed directory not found"**
The seeder reads `crates/database/src/seed/` relative to the current
directory. Run `make dev` from the repo root.

**401 on management endpoints with mock auth**
Confirm `AUTH_MOCK=true` in the local env and that the Bearer token
starts with `rt_`. The `User-Agent: Mock User Agent` header is only
required for refresh-token endpoints (e.g. `POST /v1/users/me/access_tokens`).

**403 on `/v1/admin/*` with mock auth**
The seeded mock user is `admin@test.com`, which is **not** in the
default `AUTH_ADMIN_DOMAINS=near.ai,near.org`. Add `test.com` to the
list (see §5 Option C) and restart the server.

**401 on API-key endpoints right after creating the key**
The API key cache refreshes asynchronously. Wait ~10 seconds and retry.

**`/v1/models` returns an empty list**
No backends are wired up — see §5. Bump `LOG_LEVEL=debug` to see
provider refresh attempts.

**`/v1/attestation/report` returns 503**
Expected when running outside a CVM/TEE — attestation calls into the
dstack guest agent over `/var/run/dstack.sock`, which is only present
in the TEE. For schema-level testing, debug builds (i.e. anything but
`cargo build --release`) honor `DEV=true`, which short-circuits the
dstack call and returns canned attestation data so the endpoint
returns 200.

**Postgres seed fails with `duplicate key value … users_email_key`**
Another user already owns `admin@test.com`. Run:
```sql
DELETE FROM users
WHERE email = 'admin@test.com'
  AND id != '11111111-1111-1111-1111-111111111111';
```
or `docker compose down -v` to wipe the volume.

## 8. Reproduction scripts

`test_signature.sh` at the repo root doubles as a worked example —
completion → signature lookup roundtrip. It accepts positional
`base_url` and `api_key` arguments so you can point it at the local
server:

```bash
./test_signature.sh http://localhost:3000 "$API_KEY"
```
