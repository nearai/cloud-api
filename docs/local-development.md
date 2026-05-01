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
- A POSIX shell with `curl` and `jq`

## 2. Bring up the stack

```bash
# Postgres only — runs the API on the host so you get fast rebuilds.
docker compose up -d postgres

cp env.example .env
# The defaults in env.example match docker-compose.yml. The only change
# you need for a hello-world session is to enable mock auth:
#   AUTH_MOCK=true
#   POSTGRES_PRIMARY_APP_ID=postgres-test    # already the default

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
`rt_` provided the request also sends `User-Agent: Mock User Agent`. The
seeded admin user (`admin@test.com`, ID `11111111-1111-1111-1111-111111111111`)
is in the `near.ai` admin domain, so it can hit `/v1/admin/*` too.

Two ways to mint a token:

1. **Reuse the seeded admin user** — any `rt_<random>` works:

   ```bash
   export SESSION="rt_$(uuidgen | tr 'A-Z' 'a-z')"
   ```

2. **Impersonate a specific user ID** — embed the UUID after `rt_`:

   ```bash
   export SESSION="rt_11111111-1111-1111-1111-111111111111"
   ```

All session-authenticated requests below use these two headers:

```
Authorization: Bearer $SESSION
User-Agent: Mock User Agent
```

> If you skip the User-Agent, the mock validator returns 401. Real
> browsers and the OAuth flow set their own user agents — mock auth is
> deliberately scoped to this single sentinel string.

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

# 3. Create a workspace inside the org
WS_ID=$(curl -s "$BASE/v1/organizations/$ORG_ID/workspaces" "${AUTH[@]}" \
  -H 'Content-Type: application/json' \
  -d '{"name":"default","description":"manual testing"}' \
  | jq -r .id)
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

# 6. Make a chat completion (also requires backends).
curl -s "$BASE/v1/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "zai-org/GLM-5-FP8",
    "messages": [{"role":"user","content":"hello"}],
    "max_tokens": 16
  }' | jq .
```

> Newly-created API keys can take up to 10 seconds to be picked up by the
> in-memory cache. If step 5 returns 401, retry after a short pause.

## 5. Wiring up inference backends

cloud-api itself doesn't run vLLM/SGLang — it routes to backends fronted
by `model-proxy` (production) or any OpenAI-compatible server. Pick one:

### Option A — point at production backends (read-only)

The fastest way to exercise the streaming path locally. Use a real prod
or staging key for `MODEL_DISCOVERY_API_KEY`, but keep your local seed
data isolated.

```env
MODEL_DISCOVERY_SERVER_URL=https://cloud-api.near.ai/v1/models
MODEL_DISCOVERY_API_KEY=sk-…
MODEL_DISCOVERY_REFRESH_INTERVAL=60
```

This points at production model metadata; chat completions still flow
through your local cloud-api → production backend.

### Option B — local OpenAI-compatible server

Run vLLM, SGLang, or any OpenAI-compatible mock locally and add a model
row pointing at it. Simplest path:

```bash
# 1. Start a local OpenAI-compatible server on :8002
#    (vLLM, SGLang, ollama-compat, etc.)

# 2. Insert a model row that targets it
psql "host=localhost user=postgres password=postgres dbname=platform_api" <<SQL
INSERT INTO models (id, model_id, inference_url, …)
VALUES (gen_random_uuid(), 'local/test-model', 'http://host.docker.internal:8002', …);
SQL
```

The exact column set depends on the latest migration — check
`crates/database/src/migrations/sql/` and the `Model` struct in
`crates/services/src/models/`. For most local work, Option A is enough.

### Option C — admin PATCH

If you have admin access and want to populate models the same way prod
does, use `PATCH /v1/admin/models`. This requires either an
`@near.ai`-seeded user (which `AUTH_MOCK=true` gives you) or an admin
JWT. The body shape is `HashMap<model_id, UpdateModelApiRequest>` —
note that `inferenceUrl` is **camelCase**; the snake_case form is
silently ignored.

```bash
curl -s "$BASE/v1/admin/models" "${AUTH[@]}" \
  -X PATCH -H 'Content-Type: application/json' \
  -d '{
    "local/test-model": {
      "modelDisplayName": "Local Test",
      "modelDescription": "Local OpenAI-compatible backend",
      "contextLength": 8192,
      "inferenceUrl": "http://host.docker.internal:8002"
    }
  }' | jq .
```

Provider refresh runs every `MODEL_DISCOVERY_REFRESH_INTERVAL` seconds
(300 by default). Drop it to 30–60 while iterating.

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
| `GET  /v1/attestation/report`               | API key  | TEE attestation (returns 503 locally — no `dstack` socket)  |
| `GET  /v1/signature/{chat_id}`              | API key  | Per-completion signature lookup                             |

The Scalar UI at `http://localhost:3000/docs` lets you fire each of these
interactively and inspect request/response schemas.

## 7. Troubleshooting

**`make dev` fails with "Seed directory not found"**
The seeder reads `crates/database/src/seed/` relative to the current
directory. Run `make dev` from the repo root.

**401 on management endpoints with mock auth**
Confirm both headers are present: `Authorization: Bearer rt_…` *and*
`User-Agent: Mock User Agent`. Either alone is rejected.

**401 on API-key endpoints right after creating the key**
The API key cache refreshes asynchronously. Wait ~10 seconds and retry.

**`/v1/models` returns an empty list**
No backends are wired up — see §5. The discovery loop logs at
`MODEL_DISCOVERY_*` settings; bump `LOG_LEVEL=debug` to see refresh
attempts.

**`/v1/attestation/report` returns 503**
Expected locally. Attestation requires `/var/run/dstack.sock` and a
running dstack guest agent, which only exist inside the TEE.

**Postgres seed fails with `duplicate key value … users_email_key`**
Another user already owns `admin@test.com`. Run:
```sql
DELETE FROM users
WHERE email = 'admin@test.com'
  AND id != '11111111-1111-1111-1111-111111111111';
```
or `docker compose down -v` to wipe the volume.

## 8. Reproduction scripts

The repo ships a few scripts that double as worked examples:

- `repro_connection_retry.sh` — concurrent stress against
  `/v1/chat/completions`
- `repro_e2ee_stale_pubkey.sh` — exercises the E2EE pubkey rotation
  bug-fix path
- `repro_no_usage_stats.sh` — checks that streaming responses populate
  usage data
- `test_signature.sh` — completion → signature lookup roundtrip

All of them accept `API_KEY` and `API_URL` env overrides, so you can
point them at your local server:

```bash
API_KEY=$API_KEY API_URL=http://localhost:3000 ./test_signature.sh
```
