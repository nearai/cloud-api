# CLAUDE.md

Guidance for Claude Code and other agents working in this repository. Topic rules live in `.claude/rules/` and load automatically when you work on matching files (see the index below); review criteria live in `REVIEW.md`.

## Privacy comes first

This service runs in a Trusted Execution Environment, and customer trust depends on never exposing customer data. **IMPORTANT: before adding or changing any log, metric label, or `Debug` output, follow `.claude/rules/privacy-logging.md`.** It defines what may be logged (IDs, counts, latencies, error codes) and what never may (credentials and customer content).

Also: API keys and session tokens are SHA-256 hashed before storage, customer data is encrypted at rest and in transit, authentication flows use HTTPS/TLS, and secrets never go into the repository.

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
# Run unit tests (library and binary tests only)
cargo nextest run --lib --bins

# Run all database-backed e2e tests (requires PostgreSQL running)
cargo nextest run --test e2e_all

# Run one e2e test exactly
cargo nextest run --test e2e_all -E 'test(/^module_name::test_name$/)'

# Run the stateless Responses e2e module
cargo nextest run --test e2e_all -E 'test(responses_stateless)'

# Run vLLM integration tests (requires vLLM server)
cargo nextest run --test integration_tests

# Run a specific test by name
cargo nextest run --test e2e_all -E 'test(responses_stateless::completed_stateless_response_exposes_gateway_signature_when_persisted)'
```

### Database Setup for Tests
```bash
# Using Docker (recommended)
docker run --name test-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=platform_api \
  -p 5432:5432 \
  -d postgres:latest

# Then run tests against a database dedicated to this checkout
TEST_DATABASE_NAME=platform_api_e2e_local cargo nextest run --test e2e_all
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

**2. API Key-Based (AI Inference Operations)**
- Used for: Chat completions, Responses, and attestation
- Endpoints: `/v1/chat/completions`, `POST /v1/responses`, `/v1/signature/{chat_id}`, `/v1/attestation/*`
- Format: `Authorization: Bearer sk-live-xxx` or `Authorization: Bearer sk-test-xxx`
- Storage: SHA-256 hashed, workspace-scoped
- Tracking: Last used timestamp, optional expiration

### Inference

- **Chat Completions** (`POST /v1/chat/completions`, `/v1/completions`): drop-in OpenAI-compatible API, SSE streaming with a `[DONE]` terminator, plus non-streaming.
- **Responses** (`POST /v1/responses`): linked to conversation history, with events `response.created`, `response.output_text.delta`, `response.completed`, `response.failed`. Supports client-executed tools (function calls, code_interpreter, computer) and server-executed web_search, file_search, and MCP. On a function call the response pauses as `incomplete`; the client resumes with `previous_response_id` plus `FunctionCallOutput`. Resumption verifies workspace ownership of `previous_response_id` and that each `call_id` maps to exactly one stored function call.

Streaming flow:

```
POST /v1/chat/completions
POST /v1/completions
```
- Drop-in replacement for OpenAI API
- Supports streaming (SSE) and non-streaming
- Standard OpenAI format with `[DONE]` terminator

**B. Responses API (single-turn, stateless)**
```
POST /v1/responses
```
- `store: false` is the only supported mode; an omitted `store` is treated as `false` and `store: true` is rejected
- Clients send all needed prior context in each request. Conversations, `previous_response_id`, background responses, and response-history endpoints are retired or unsupported.
- Raw request/response content, response items, and conversation history are not persisted.
- The default typed adapter makes one Chat Completions inference. A canonical model explicitly allowlisted through `NATIVE_RESPONSES_MODELS`, with an explicit `store: false` request, instead makes one native provider Responses inference; both paths remain stateless and write no response/item history.
- **Existing completed-response attestation is preserved best-effort**: when its signature write succeeds, `GET /v1/signature/resp_*` can retrieve the response ID and signatures over SHA-256 request/response digests. The signature material contains no raw request or response content. A disconnected stream has no completed `resp_*` attestation record or legacy disconnect fallback.
- Event types: `response.created`, `response.output_text.delta`, `response.completed`, `response.failed`
- Only custom `type: "function"` tools are supported. They are client-managed: Cloud returns a `function_call` but never executes it; the client sends the original call and its matching `function_call_output` in a later fresh `store: false` request with its own history.
- Server-executed Responses tools—including `web_search`, `web_context_search`, `file_search`, `code_interpreter`, `computer`, and remote `mcp`—plus file input and image-generation/editing output models are rejected. The independent root `POST /mcp` endpoint continues to expose web search and is not part of Responses execution.
- During Stage I, existing Conversation/File data remains available through authenticated, workspace-scoped migration routes: `POST /v1/conversations/batch`, `GET /v1/conversations/{conversation_id}`, `GET /v1/conversations/{conversation_id}/items`, `DELETE /v1/conversations/{conversation_id}`, `GET /v1/files`, `GET /v1/files/{file_id}`, `GET /v1/files/{file_id}/content`, and `DELETE /v1/files/{file_id}`. They return `Cache-Control: no-store`; creation, upload, all other mutations, and unsupported legacy paths return `410 Gone`.

**Streaming Flow**:
```
Client → CompletionService → Provider Pool (round-robin)
    → vLLM Provider → Inference Server → SSE Stream
    → InterceptStream (captures TTFT, ITL, tokens)
    → Usage tracking (atomic with completion)
```

### Dynamic Model Discovery
- Discovery Server polled every 5 minutes (configurable)
- `GET /models` returns available models and their vLLM endpoints
- Provider Pool updated dynamically (no hardcoded models)
- Load balancing: round-robin for new requests

### Model discovery

Historical Conversation, Response, ResponseItem, and File schemas/repositories remain for now,
but the retired public APIs must not be wired back to them. Data deletion and schema removal are
separate work.

### External Billing / Credits Service

### Database

- `../nearai-cloud-ui/app/api/stripe/webhook/route.ts` handles Stripe checkout webhooks.
- `../nearai-cloud-ui/app/api/credit/sync/route.ts` recalculates payment credits from Stripe + Hot Labs.
- `../nearai-cloud-ui/lib/webhook-utils.ts` calls `PATCH /v1/admin/organizations/{org_id}/limits`.

That service uses `NEAR_AI_CLOUD_ADMIN_ACCESS_TOKEN` and optional `BILLING_SERVICE_USER_AGENT`
to update organization spend limits in Cloud API after purchases/refunds or manual syncs.
In Cloud API, the target endpoint is `crates/api/src/routes/admin.rs` (`update_organization_limits`).

## Critical Files & Locations

### Adding a New API Endpoint
1. Create route handler in `crates/api/src/routes/`
2. Implement service in `crates/services/src/{domain}/`
   - Define trait in `ports.rs`
   - Implement service in `mod.rs` or `service.rs`
3. Add repository if needed in `crates/database/src/repositories/`
4. Map route in `crates/api/src/lib.rs` (see `routes()` function)

### Service Modules (16 total)
Located in `crates/services/src/`:
- `auth` - Session creation, JWT, OAuth2
- `organization` - Org management, member roles, invitations
- `workspace` - Workspace CRUD, settings
- `user` - User profiles, session management
- `completions` - AI completion orchestration
- `conversations` - Legacy state module with temporary Stage I migration routes; only per-conversation deletion remains enabled
- `responses` - Request-scoped, stateless response orchestration with client-managed custom function calls
- `attestation` - TEE attestation reports, chat signatures
- `models` - Model catalog and pricing
- `usage` - Token tracking, limit enforcement, billing
- `inference_provider_pool` - Model discovery, load balancing
- `mcp` - Model Context Protocol client management
- `files` - Legacy state module with temporary Stage I migration routes; only per-file deletion remains enabled
- `metrics` - OpenTelemetry metrics
- `admin` - Admin operations, analytics
- `common` - Shared utilities

### Route Handlers (18 total)
Located in `crates/api/src/routes/`:
- `auth.rs` - OAuth flows, login, logout, sessions
- `organizations.rs` - Organization CRUD
- `organization_members.rs` - Member management, invitations
- `workspaces.rs` - Workspace & API key management
- `users.rs` - User profile, invitations, sessions
- `completions.rs` - Chat & text completions
- `conversations.rs` - Temporary authenticated, no-store Stage I migration routes; only per-conversation deletion remains enabled
- `responses.rs` - Stateless AI response streaming with client-managed custom function calls
- `models.rs` - Model catalog
- `usage.rs` - Usage tracking, billing
- `attestation.rs` - TEE verification, signatures
- `admin.rs` - Admin endpoints
- `files.rs` - Temporary authenticated, no-store Stage I migration routes; only per-file deletion remains enabled
- `health.rs` - Health checks
- `api.rs` - API versioning

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
| `.claude/rules/database.md` | `crates/database/**`, `ports.rs` | typed parsing vs migrations, migration mechanics, queries |
| `.claude/rules/testing.md` | tests, `.config/nextest.toml` | tests that can fail, e2e isolation on a shared database |
| `.claude/rules/ci-workflows.md` | `.github/**`, `scripts/**` | workflow trust boundaries, concurrency, required checks |
| `REVIEW.md` | pull requests | severity, what to report and skip |

## API documentation

### Why Hexagonal Architecture?
- Services are framework-independent (can swap Axum for another framework)
- Easy to mock for testing (inject fake repositories)
- Domain logic isolated from infrastructure concerns
- Multiple implementations possible (SQL, cache, mock)

### Why Streaming-First?
- Real-time token delivery improves UX
- Usage tracking happens during streaming (not after)
- SSE provides simple, reliable transport
- Non-streaming clients can collect stream and return complete response

### Why Dynamic Model Discovery?
- No hardcoded model configuration
- Automatic scaling as inference servers added/removed
- Graceful handling of provider failures

## API Documentation

When server is running:
- **Scalar UI**: http://localhost:3000/docs
- **OpenAPI Spec**: http://localhost:3000/api-docs/openapi.json

Generated from Rust code using `utoipa` annotations.

## Troubleshooting

- **Tests fail to connect to the database**: make sure PostgreSQL is on port 5432 with the `env.example` credentials (see the docker command above).
- **vLLM integration tests fail**: set `VLLM_BASE_URL` (and `VLLM_API_KEY` if needed) and check the model is served.
- **Server won't start**: check required variables in `env.example`, database connectivity, OAuth credentials (unless using mock OAuth), then the startup logs for the specific error.
- **Model discovery not working**: check `MODEL_DISCOVERY_SERVER_URL` is reachable, `MODEL_DISCOVERY_API_KEY` is correct, and the response matches the expected schema.
