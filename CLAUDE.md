# CLAUDE.md

Guidance for Claude Code and other agents working in this repository. Topic rules live in `.claude/rules/` and load automatically when you work on matching files (see the index below); review criteria live in `REVIEW.md`.

## Privacy comes first

This service runs in a Trusted Execution Environment, and customer trust depends on never exposing customer data. **IMPORTANT: before adding or changing any log, metric label, or `Debug` output, follow `.claude/rules/privacy-logging.md`.** It defines what may be logged (IDs, counts, latencies, error codes) and what never may (credentials and customer content).

Also: API keys and session tokens are SHA-256 hashed before storage, customer data is encrypted at rest and in transit, authentication flows use HTTPS/TLS, and secrets never go into the repository.

## Database freeze

**IMPORTANT: do not add or edit migrations, and do not change SQL, queries, or behavior of the code that moved to `nearai/cloud-platform`, without the matching cloud-platform PR or an owner exception.** cloud-platform now owns the schema; a change here alone silently diverges the two services until phase 2. The full rules and the frozen file list are in `.claude/rules/database.md`.

## Project overview

NEAR AI Cloud API is a Rust multi-tenant AI inference platform running in a TEE:
- **Management plane** (session/OAuth auth): organizations, workspaces, users, API key administration.
- **Data plane** (API key auth): OpenAI-compatible inference with streaming.

## Commands

```bash
cargo build
cargo run --bin api                     # run the API server locally
docker-compose up -d                    # local stack including PostgreSQL
cargo fmt                               # format
cargo fmt --all -- --check              # CI format check
cargo clippy --all-targets --all-features -- -D warnings   # CI lint
bash scripts/check_dep_guards.sh        # CI dependency guards
cargo nextest run --lib --bins          # unit tests
cargo nextest run test_create_conversation                  # tests matching a name
cargo nextest run --test e2e_all        # database-backed e2e (needs PostgreSQL)
cargo nextest run --test e2e_all -E 'test(/^module_name::test_name$/)'   # one e2e test
TEST_DATABASE_NAME=platform_api_e2e_local cargo nextest run --test e2e_all   # per-checkout e2e DB
cargo nextest run --test integration_tests   # vLLM integration (needs a vLLM server)
```

Local PostgreSQL for tests:

```bash
docker run --name test-postgres -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=platform_api -p 5432:5432 -d postgres:latest
```

## Architecture

Cargo workspace:

```
crates/
├── api/                  # HTTP routes and server (Axum)
├── services/             # domain logic; each domain declares its traits in ports.rs
├── database/             # repositories implementing those traits; SQL migrations
├── config/               # environment-based configuration
└── inference_providers/  # inference backend abstraction (vLLM and attested providers)
```

The layering rules and the existing seams to extend are in `.claude/rules/architecture.md`. Diagrams and flows: `docs/architecture/c4-diagrams.md`.

### Multi-tenancy

```
Organization (tenant root)
├── Members (owner, admin, member)
├── Workspaces
│   ├── API keys (workspace-scoped)
│   └── Settings
├── Usage limits (org-level enforcement)
└── Rate limits
```

### Authentication (two mutually exclusive methods)

- **Session-based OAuth** (GitHub, Google) for management: `/v1/organizations/*`, `/v1/workspaces/*`, `/v1/users/*`. The hashed session token is stored in the database and returned as an HTTP-only cookie.
- **API keys** for inference: `/v1/chat/completions`, `/v1/responses/*`, `/v1/conversations/*`, attestation. `Authorization: Bearer sk-live-...` or `sk-test-...`; SHA-256 hashed, workspace-scoped, with last-used tracking and optional expiry.

The separation is deliberate: interactive users manage resources, programs run inference, and each plane has a clear security boundary. Never let one route accept both.

### Inference

- **Chat Completions** (`POST /v1/chat/completions`, `/v1/completions`): drop-in OpenAI-compatible API, SSE streaming with a `[DONE]` terminator, plus non-streaming.
- **Responses** (`POST /v1/responses`): linked to conversation history, with events `response.created`, `response.output_text.delta`, `response.completed`, `response.failed`. Supports client-executed tools (function calls, code_interpreter, computer) and server-executed web_search, file_search, and MCP. On a function call the response pauses as `incomplete`; the client resumes with `previous_response_id` plus `FunctionCallOutput`. Resumption verifies workspace ownership of `previous_response_id` and that each `call_id` maps to exactly one stored function call.

Streaming flow:

```
Client → CompletionService → Provider Pool → provider → inference server → SSE
    → InterceptStream (TTFT, ITL, token counts) → usage tracking (atomic with completion)
```

The engine is streaming-first: tokens reach users in real time, usage is tracked during the stream rather than after, and non-streaming clients get the collected stream.

### Model discovery

A discovery server is polled (every 5 minutes by default). `GET /models` returns models and their endpoints, and the provider pool updates dynamically, so there are no hardcoded models: capacity scales as servers are added or removed and failed providers drop out. New requests are balanced round-robin; conversations use sticky routing.

### Database

PostgreSQL 16 with deadpool pooling and Patroni (automatic leader discovery, read-replica balancing). Refinery SQL migrations in `crates/database/src/migrations/sql/` run on startup. The schema is frozen here: `.claude/rules/database.md`.

### Configuration

Static config: environment variables or a `.env` file (see `env.example`), structs in `crates/config/src/types.rs`, YAML templates in `config/`. Runtime-changeable config uses the `admin_settings` KV store: `.claude/rules/architecture.md`.

### Observability

Structured logging with `tracing`; production runs at `info` with per-module levels (`LOG_MODULE_API=info`, `LOG_MODULE_SERVICES=debug`). Metrics go out through OpenTelemetry (OTLP exporter), with Datadog integration available.

### External billing service

Cloud API has no direct Stripe integration. Billing lives in the Next.js app in `../nearai-cloud-ui` (Vercel): `app/api/stripe/webhook/route.ts` handles Stripe checkout, `app/api/credit/sync/route.ts` recalculates credits, and `lib/webhook-utils.ts` calls `PATCH /v1/admin/organizations/{org_id}/limits` (`update_organization_limits` in `crates/api/src/routes/admin.rs`) using `NEAR_AI_CLOUD_ADMIN_ACCESS_TOKEN` and optional `BILLING_SERVICE_USER_AGENT`.

## Rules index

Each rule has one home; other files point to it rather than restating it.

| File | Applies to | Covers |
|---|---|---|
| `.claude/rules/privacy-logging.md` | `crates/**/*.rs` | what may and may not be logged |
| `.claude/rules/architecture.md` | `crates/**/*.rs` | layers, extending existing seams, errors, runtime config, file size |
| `.claude/rules/rust-correctness.md` | `crates/**/*.rs` | panics, untrusted input, fail-closed, async, races, rolling deploys |
| `.claude/rules/database.md` | `crates/database/**`, `ports.rs`, moved services and routes | database freeze, typed parsing vs migrations, migration mechanics, queries |
| `.claude/rules/testing.md` | tests, `.config/nextest.toml` | tests that can fail, e2e isolation on a shared database |
| `.claude/rules/ci-workflows.md` | `.github/**`, `scripts/**` | workflow trust boundaries, concurrency, required checks |
| `REVIEW.md` | pull requests | severity, what to report and skip |

## API documentation

With the server running: Scalar UI at http://localhost:3000/docs, OpenAPI spec at http://localhost:3000/api-docs/openapi.json (generated from `utoipa` annotations).

Routes moving to `nearai/cloud-platform` are marked `deprecated` in that spec by the list in `crates/api/src/openapi_deprecation.rs` (documentation only; behavior is unchanged). Add a route to that list when it moves; content and inference routes stay unmarked.

## Troubleshooting

- **Tests fail to connect to the database**: make sure PostgreSQL is on port 5432 with the `env.example` credentials (see the docker command above).
- **vLLM integration tests fail**: set `VLLM_BASE_URL` (and `VLLM_API_KEY` if needed) and check the model is served.
- **Server won't start**: check required variables in `env.example`, database connectivity, OAuth credentials (unless using mock OAuth), then the startup logs for the specific error.
- **Model discovery not working**: check `MODEL_DISCOVERY_SERVER_URL` is reachable, `MODEL_DISCOVERY_API_KEY` is correct, and the response matches the expected schema.
