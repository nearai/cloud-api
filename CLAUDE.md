# CLAUDE.md

Guidance for Claude Code and other agents working in this repository. Topic rules live in `.claude/rules/` and load automatically when you work on matching files (see the index below); review criteria live in `REVIEW.md`.

## Privacy comes first

This service runs in a Trusted Execution Environment, and customer trust depends on never exposing customer data. **IMPORTANT: before adding or changing any log, metric label, or `Debug` output, follow `.claude/rules/privacy-logging.md`.** It defines what may be logged (IDs, counts, latencies, error codes) and what never may (credentials and customer content).

Also: API keys and session tokens are SHA-256 hashed before storage, customer data is encrypted at rest and in transit, authentication flows use HTTPS/TLS, and secrets never go into the repository.

## Database freeze

**IMPORTANT: cloud-api has no migrations. Schema changes land in `nearai/cloud-platform` first, and this repo only bumps the pin in `scripts/cloud-platform-schema.env`. Do not change SQL, queries, or behavior of the code that moved to `nearai/cloud-platform` without the matching cloud-platform PR or an owner exception.** cloud-platform owns the schema; a change here alone silently diverges the two services until phase 2. The full rules and the frozen file list are in `.claude/rules/database.md`.

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
cargo nextest run --test e2e_all -E 'test(responses_stateless)'   # stateless Responses coverage
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
├── database/             # repositories implementing those traits
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
- **Responses** (`POST /v1/responses`): stateless, with events `response.created`, `response.output_text.delta`, `response.completed`, and `response.failed`. Omitted or null `store` is normalized to `false`; `store: true`, `conversation`, `previous_response_id`, and `background: true` are rejected. Clients provide all prior context in each request.
  - After alias resolution, canonical `openai/` models use one provider-native Responses request. The upstream provider executes native `web_search`; Cloud does not run a tool loop for this transport. Other models use the typed adapter, which can execute Cloud-managed `web_search` / `web_context_search` and feed results back through a bounded request-scoped inference loop. Neither path writes raw response/item history.
  - Client-managed `function` and `custom` tools are supported, including tools grouped in a `namespace` and Codex's leading developer `additional_tools` declarations. Clients execute returned calls and replay each call with its matching output and prior context in a new request. Preserve `call_id`, names, raw arguments or custom input, and provider metadata such as `thought_signature`.
  - Typed replay accepts assistant `output_text` and plaintext `reasoning` items, using reasoning content or its summary as prior assistant context. Encrypted-only reasoning requires a native Responses backend. The typed adapter supports text custom tools and the Codex `exec` Lark source grammar; other grammars require a native backend.
  - Native requests forward complete replayed `web_search_call` items and `include` unchanged. Provider support determines the returned fields; Cloud preserves source/citation fields instead of stripping them to make providers match. Typed replay accepts completed search markers alongside assistant text and ignores the markers without repeating the search.
  - Generated tool calls finish with `response.output_item.done`; a successful model turn ends with `response.completed`, including when tools await client execution. Reasoning uses `response.reasoning_text.delta`; failures expose `response.error` in `response.failed` and preserve partial usage. Typed hosted search retains search lifecycle events, source citations, and cumulative multi-round usage. Its `web_search.count` (0–20) overrides the model count; zero skips the search provider. Native hosted tools are limited to `web_search`; other builtin/remote tools, file input, and image-generation/editing output models are rejected. The independent root `POST /mcp` web-search endpoint is retained.
  - Completed-response gateway attestation remains best-effort: when persistence succeeds, `GET /v1/signature/resp_*` returns signatures over request/response digests, not raw content. Interrupted streams create no partial `resp_*` attestation record.
  - Named response-history routes (`/v1/responses/{id}`, its `/cancel` and `/input_items` paths) return authenticated `410 Gone`.

Streaming flow:

```
Client → CompletionService → Provider Pool → provider → inference server → SSE
    → InterceptStream (TTFT, ITL, token counts) → usage tracking (atomic with completion)
```

The engine is streaming-first: tokens reach users in real time, usage is tracked during the stream rather than after, and non-streaming clients get the collected stream.

### Conversations and Files

Conversations and Files remain authenticated, workspace-scoped resource APIs. Conversation creation, retrieval, updates, pinning, archiving, cloning, item creation, and deletion are available. Files support upload, listing, metadata/content retrieval, and deletion.

These resources have their own storage lifecycle. Stateless Responses accepts caller-provided context instead of linking to stored Conversations or Files.

### Model discovery

A discovery server is polled (every 5 minutes by default). `GET /models` returns models and their endpoints, and the provider pool updates dynamically, so there are no hardcoded models: capacity scales as servers are added or removed and failed providers drop out. New requests are balanced round-robin; conversations use sticky routing.

### Database

PostgreSQL 16 with deadpool pooling and Patroni (automatic leader discovery, read-replica balancing). The schema is owned by `nearai/cloud-platform`: cloud-api never migrates at startup. `scripts/apply-schema.sh` applies the pinned schema to local and test databases. See `.claude/rules/database.md`.

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
