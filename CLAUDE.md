# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 🔒 PRIVACY & DATA SECURITY - CRITICAL

**Privacy and data security are THE most important aspects of this service for customer/client trust.**

### Logging Rules (ABSOLUTE REQUIREMENTS)

Production runs at **info level and above**. We ABSOLUTELY CANNOT and SHOULD NOT log customer-related data.

#### ✗ NEVER LOG (Forbidden)
- **Security credentials** - API keys, session tokens, passwords, secrets, OAuth tokens, encryption keys (these could compromise security if leaked)
- **Conversation content** - Any message text, completion text, or AI-generated content
- **Conversation titles** - User-provided or AI-generated conversation names
- **Conversation descriptions** - Any descriptive text about conversations
- **User input** - Messages, prompts, or any user-submitted text
- **AI responses** - Model outputs, completions, or generated text
- **Metadata that reveals customer information** - Custom fields, tags, labels that could expose user activity
- **File contents** - Uploaded file data or processed file content
- **Any PII** - Names, emails (except for auth flow), addresses, phone numbers in user content

#### ✓ OK TO LOG (Permitted for Debugging)
- **IDs only** - `conversation_id`, `org_id`, `user_id`, `workspace_id`, `response_id`
- **System metrics** - Request counts, latency, token counts (numbers only)
- **Error types** - Error codes, HTTP status codes, error categories
- **Performance data** - Time-to-first-token (TTFT), inter-token latency (ITL)
- **System events** - Server startup, shutdown, connection pool status
- **Authentication events** - Login attempts, session creation (not passwords/tokens)

#### Guidelines for Adding Logging
1. **Before adding any log statement**: Ask yourself "Could this reveal anything about a customer's conversation or activity?"
2. **If in doubt, don't log it** - Err on the side of caution
3. **Log IDs, not content** - Use `conversation_id` not conversation title
4. **Review all logging changes carefully** - Every log statement is a potential privacy leak
5. **Use debug/trace levels for detailed data** - These are not enabled in production
6. **Never log request/response bodies** - Even at debug level, unless you're 100% certain they don't contain customer data

#### Examples

**❌ BAD - NEVER DO THIS:**
```rust
tracing::info!("API key: {}", api_key);  // Security risk - exposes credentials!
tracing::info!("Creating conversation: {}", conversation.title);  // Exposes title!
tracing::info!("User message: {}", message.content);  // Exposes content!
tracing::warn!("Invalid input: {}", user_input);  // Exposes user data!
tracing::debug!("Session token: {}", session_token);  // Security risk!
```

**✅ GOOD - Do this instead:**
```rust
tracing::info!("API key validated: api_key_id={}", api_key_id);  // Only log ID
tracing::info!("Creating conversation: conversation_id={}", conversation.id);
tracing::info!("Processing message: conversation_id={}, message_id={}", conv_id, msg_id);
tracing::warn!("Invalid input format: conversation_id={}", conversation_id);
tracing::debug!("Session validated: session_id={}, user_id={}", session_id, user_id);
```

### Security Reminders
- This service runs in a **Trusted Execution Environment (TEE)** - customer trust is paramount
- All customer data must be encrypted at rest and in transit
- API keys and session tokens are SHA-256 hashed before storage
- Never commit secrets or credentials to the repository
- All authentication flows use HTTPS/TLS

---

## Project Overview

NEAR AI Cloud API is a Rust-based multi-tenant AI inference platform running in a Trusted Execution Environment (TEE). It provides:
- **Management Plane** (OAuth): Organization, workspace, and API key administration
- **Data Plane** (API Keys): OpenAI-compatible AI inference with streaming support

## Common Commands

### Development & Building
```bash
# Build the project
cargo build

# Run the API server locally
cargo run --bin api

# Start with Docker Compose (includes PostgreSQL)
docker-compose up -d

# Check code formatting
cargo fmt --all -- --check

# Run linter
cargo clippy

# Format code
cargo fmt
```

### Testing
```bash
# Run unit tests (library and binary tests only)
cargo test --lib --bins

# Run ALL e2e tests (requires PostgreSQL running)
cargo test --test e2e_test

# Run a single e2e test file
cargo test --test e2e_conversations

# Run vLLM integration tests (requires vLLM server)
cargo test --test integration_tests

# Run a specific test by name
cargo test test_create_conversation
```

### Database Setup for Tests
```bash
# Using Docker (recommended)
docker run --name test-postgres \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=platform_api \
  -p 5432:5432 \
  -d postgres:latest

# Then run tests
cargo test --test e2e_test
```

## Architecture

### Workspace Structure (Cargo Workspace)
```
crates/
├── api/                    # HTTP routes & server (Axum)
├── database/               # Database abstraction & repositories
├── config/                 # Environment-based configuration
├── services/               # Domain logic (business logic)
└── inference_providers/    # AI provider abstractions (vLLM)
```

### Hexagonal Architecture Pattern
The codebase follows ports & adapters (hexagonal architecture):
```
Routes (api/src/routes/)
    ↓ HTTP Adapters
Services (services/src/*/)
    ↓ Domain Logic (define traits in ports.rs)
Repositories (database/src/repositories/)
    ↓ Data Adapters (implement service traits)
PostgreSQL Database
```

**Key Principle**: Services depend on abstract traits (ports), not concrete implementations. This makes services framework-agnostic and easy to test.

### Multi-Tenancy Hierarchy
```
Organization (Tenant Root)
├── Members (Roles: owner, admin, member)
├── Workspaces
│   ├── API Keys (workspace-scoped)
│   └── Settings
├── Usage Limits (org-level enforcement)
└── Rate Limits
```

### Authentication Strategy (Mutually Exclusive)

**1. Session-Based OAuth (Management Operations)**
- Used for: Organizations, workspaces, users, API key management
- Endpoints: `/v1/organizations/*`, `/v1/workspaces/*`, `/v1/users/*`
- Providers: GitHub OAuth, Google OAuth
- Storage: Hashed session token in database, returned as HTTP-only cookie

**2. API Key-Based (AI Inference Operations)**
- Used for: Chat completions, conversations, responses, attestation
- Endpoints: `/v1/chat/completions`, `/v1/responses/*`, `/v1/conversations/*`
- Format: `Authorization: Bearer sk-live-xxx` or `Authorization: Bearer sk-test-xxx`
- Storage: SHA-256 hashed, workspace-scoped
- Tracking: Last used timestamp, optional expiration

**Critical**: These auth methods are mutually exclusive by design.

### AI Inference: Two APIs, Same Streaming Engine

**A. Chat Completions API (OpenAI-compatible)**
```
POST /v1/chat/completions
POST /v1/completions
```
- Drop-in replacement for OpenAI API
- Supports streaming (SSE) and non-streaming
- Standard OpenAI format with `[DONE]` terminator

**B. Response API (Platform-specific)**
```
POST /v1/responses
```
- Links to conversation history for context
- Rich metadata and event types
- Event types: `response.created`, `response.output_text.delta`, `response.completed`, `response.failed`

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
- Load balancing: round-robin for new requests, sticky routing for conversations

### Database Layer (Patroni High-Availability)
- PostgreSQL 16 with deadpool connection pooling
- **Patroni integration**: Automatic leader discovery, read replica load balancing
- **20+ Repositories**: User, Organization, Workspace, APIKey, Session, Conversation, Response, Model, Usage, Attestation
- **Migrations**: SQL-based using Refinery, run on startup
- Located at: `crates/database/src/migrations/sql/`

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
- `conversations` - Conversation lifecycle
- `responses` - Response streaming with token tracking
- `attestation` - TEE attestation reports, chat signatures
- `models` - Model catalog and pricing
- `usage` - Token tracking, limit enforcement, billing
- `inference_provider_pool` - Model discovery, load balancing
- `mcp` - Model Context Protocol client management
- `files` - File storage (AWS S3)
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
- `conversations.rs` - Conversation management
- `responses.rs` - AI response streaming
- `models.rs` - Model catalog
- `usage.rs` - Usage tracking, billing
- `attestation.rs` - TEE verification, signatures
- `admin.rs` - Admin endpoints
- `files.rs` - File upload/download
- `health.rs` - Health checks
- `api.rs` - API versioning

### Configuration
- Environment variables loaded from `.env` or environment
- Example config: `env.example`
- Config structs: `crates/config/src/types.rs`
- YAML templates: `config/` directory

### Architecture Documentation
Comprehensive C4 diagrams and flows: `docs/architecture/c4-diagrams.md`

## Development Patterns

### Adding a New Service
1. Create directory: `crates/services/src/{service_name}/`
2. Create `ports.rs` with trait definitions
3. Create `mod.rs` or `service.rs` with implementation
4. Define repository trait if data access needed
5. Implement repository in `crates/database/src/repositories/`
6. Update `crates/services/src/lib.rs` to export new service

### Testing E2E Flows
- E2E tests in `crates/api/tests/`
- Common test utilities in `crates/api/tests/common/mod.rs`
- Test helpers: `setup_test_server()`, `create_org()`, `get_api_key_for_org()`
- Each test runs against a real PostgreSQL database
- Tests create isolated data (unique UUIDs, random names)

### Database Migrations
- SQL files in `crates/database/src/migrations/sql/`
- Migrations run automatically on server startup
- Use `refinery` for migration management
- Name format: `V{number}__{description}.sql`

### Error Handling
- API errors use `api::models::ErrorResponse`
- Standard error types: `unauthorized`, `forbidden`, `not_found`, `conflict`, `validation_error`
- Service errors propagate up to routes, converted to HTTP responses

### Logging & Observability
- Structured logging with `tracing` crate
- Production log level: **info and above**
- Module-specific levels: `LOG_MODULE_API=info`, `LOG_MODULE_SERVICES=debug`
- OpenTelemetry metrics support (OTLP exporter)
- Datadog integration available
- **CRITICAL**: See "Privacy & Data Security" section above for what you can and cannot log

## Key Architectural Decisions

### Why Two Auth Methods?
- **Session Auth (OAuth)**: Interactive users managing resources
- **API Key Auth**: Programmatic access to AI inference
- Separation of concerns: management plane vs. data plane
- Clear security boundaries

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
- Conversation consistency via sticky routing

## API Documentation

When server is running:
- **Scalar UI**: http://localhost:3000/docs
- **OpenAPI Spec**: http://localhost:3000/api-docs/openapi.json

Generated from Rust code using `utoipa` annotations.

## Troubleshooting

### Tests Failing with Database Connection Errors
1. Ensure PostgreSQL is running on port 5432
2. Check database credentials match `env.example` defaults
3. Try: `docker run --name test-postgres -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=platform_api -p 5432:5432 -d postgres:latest`

### vLLM Integration Tests Failing
1. Ensure vLLM server is running at configured URL
2. Set environment variables: `VLLM_BASE_URL`, `VLLM_API_KEY` (optional)
3. Check model availability at vLLM endpoint

### Server Won't Start
1. Check all required environment variables in `env.example`
2. Verify database connectivity
3. Check OAuth credentials (if using real OAuth, not mock)
4. Review logs for specific errors (remember: no customer data in logs!)

### Model Discovery Not Working
1. Verify `MODEL_DISCOVERY_SERVER_URL` is accessible
2. Check `MODEL_DISCOVERY_API_KEY` is correct
3. Review discovery server response format matches expected schema
