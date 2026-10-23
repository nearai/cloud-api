# Jev / System One integration

## Design

Jev returns typed decisions from text and, on supported providers, images.
Cloud API exposes `POST /v1/systemone` using
[OpenRouter's System One JSON contract](https://openrouter.ai/openapi.json).
The protocol name is the HTTP path; `decisions` is the output modality.
Input capabilities are declared per model through `input_modalities`.

Two independent axes determine behavior:

| Axis | Hosted Jev | Self-hosted TEE System One |
| --- | --- | --- |
| Input / output | Model-declared inputs / `decisions` | Model-declared inputs / `decisions` |
| Catalog provider type | `external` | `vllm` (existing NEAR fleet adapter) |
| Backend transport | `typesafe` | NEAR attested transport |
| Attestation / verifiable | false / false | true / true |
| Receipt signer | gateway | serving TEE, if it supports per-response signatures |

The normal `GET /v1/models` list includes decision models alongside other models.
Consumers inspect the top-level `output_modalities` (and `input_modalities`).
The legacy `architecture` object and admin metadata use camelCase
`inputModalities` and `outputModalities`. No schema migration or
separate model list is needed.

The provider interface adds a System One capability. The pool filters providers
by that capability and retains its existing retry, fallback, health, and trust
policy. Organization `fallback_enabled` applies. An attested model never falls
back to plaintext. Signature selection uses the **actual serving provider's**
trust tier and signature capability, including fallback, rather than catalog flags.
An attested provider without per-response signatures receives a gateway receipt.

Self-hosted decision requests use request-key routing with bounded spillover
across verified fleet backends. Retryable HTTP errors and connection failures
can try another instance; its response ID pins subsequent signature retrieval
to that instance. Invalid successful responses, invalid TEE receipt IDs and
ambiguous transport/read timeouts stop both instance and provider fallback to
avoid issuing another paid inference.

Endpoint compatibility is checked against an already resolved model, including
aliases. Chat Completions and legacy Completions share the same service check.
Legacy Responses reuses its image-dispatch lookup, and native Responses checks
its already selected model. Neither path forwards decision models; both return
400 directing clients to `/v1/systemone`. Existing image/audio behavior is retained.

## Configure hosted Jev

Set `TYPESAFE_API_KEY` or mount a secret through `TYPESAFE_API_KEY_FILE` (takes
precedence). The existing per-model `providerConfig.api_key` override also works.
Use that override for an OpenRouter credential; do not put secrets in checked-in
model configuration.

Register through the existing admin model API. Relevant fields:

```json
{
  "typesafe/jev-1.13": {
    "modelDisplayName": "Jev 1.13",
    "modelDescription": "Typed decisions through System One",
    "ownedBy": "typesafe",
    "providerType": "external",
    "providerConfig": {
      "backend": "typesafe",
      "base_url": "https://api.typesafe.ai/v1",
      "model_name": "jev-1.13.0"
    },
    "verifiable": false,
    "attestationSupported": false,
    "inputModalities": ["text"],
    "outputModalities": ["decisions"],
    "isActive": false
  }
}
```

This is an **inactive configuration example**, not a deployment or pricing
recommendation. Set verified context/output limits and token prices before
activation, following the existing admin pricing gate. Prices are integer
nano-USD per token. TypeSafe currently reports free output tokens; configure a
zero output price explicitly (and the required `allowFree` acknowledgement)
instead of treating every decision model's output as free. Confirm the current
upstream model name using TypeSafe's authenticated `GET /v1/models`.

For OpenRouter's System One compatibility endpoint, use
`base_url: https://openrouter.ai/api/v1`, its model ID in `model_name`, and an
OpenRouter API key. The adapter appends `/systemone`; it does not translate to
OpenRouter's separate alpha decisions API.
For image-capable OpenRouter models such as `typesafe/jev-1.14`, set
`inputModalities: ["text", "image"]` and use that OpenRouter model ID in
`providerConfig.model_name`.

## Call and verify

```http
POST /v1/systemone
Authorization: Bearer <cloud-api-key>
Content-Type: application/json

{
  "model": "typesafe/jev-1.13",
  "state": {"message": "I was charged twice. Please help."},
  "questions": {
    "billing": {"type": "noul", "instructions": "Is this about billing?"},
    "route": {
      "type": "choice",
      "instructions": "Which team should handle this message?",
      "criteria": {"billing": "Billing issues", "other": null}
    },
    "urgency": {
      "type": "score",
      "instructions": "How urgent is this message?",
      "criteria": ["Can wait", "Needs attention today"]
    }
  }
}
```

`state` and question instructions accept text, objects, or arrays. Every question
requires `instructions`; missing or null instructions return 400 before inference.
`noul` criteria are optional, choice criteria have 1–255 entries,
and score criteria have 1–10 levels. A `box` question requires `instructions`
and has no `criteria`. Score's lower bound follows the official
OpenAPI (the prose guide recommends at least two). Unknown request fields,
including `stream`, are rejected. The gateway validates answer types, keys,
numeric ranges, and nonnegative token counts before recording usage.

### Images and Box through OpenRouter

Use OpenRouter's JSON `image_url` parts with base64 PNG or JPEG data URLs:

```json
{
  "model": "typesafe/jev-1.14",
  "state": [
    "A receipt attached to a refund request.",
    {"type": "image_url", "image_url": {"url": "data:image/png;base64,<image-data>"}}
  ],
  "questions": {
    "receipt": {"type": "box", "instructions": "Locate the entire receipt."}
  }
}
```

Register the model with the OpenRouter configuration above before calling it.
Jev also accepts image parts nested in `state`, `instructions`, and `criteria`;
these JSON values pass through unchanged. See
[OpenRouter's image formats and limits](https://openrouter.ai/docs/guides/community/multimodal-decisions).
Cloud API's request-body limit is 25 MiB, including base64. The endpoint accepts
JSON; TypeSafe-native `assets` and multipart uploads are outside this contract.

A Box answer contains `type: "box"`, a `box` object with numeric `top`, `bottom`,
`left`, and `right` edges, and optional `confidence`, matching OpenRouter's schema.
Box requires an upstream model that supports it, such as Jev 1.14. To check
whether a target exists, include a Noul question alongside the Box.

### Receipts and billing

The successful response body is returned **byte for byte** from the provider,
including unknown response extensions and its reported model name. No ID or
alias warning is injected into the body:

- `X-Generation-Id`: use with `GET /v1/signature/{id}?signing_algo=ecdsa` or
  `ed25519`. The gateway uses the upstream JSON `id` first, then the upstream
  `X-Generation-Id`. If neither is supplied, a gateway-signed response gets a
  new unique ID. Request-tracing headers such as `x-typesafe-request-id` are
  not used. IDs are opaque strings, limited to 1–255 bytes and valid HTTP
  header values. URL-encode the ID as a single path segment for signature lookup.
- `Inference-Id`: UUID derived from that generation ID, used by `/v1/billing/costs`.
- `X-Serving-Provider`: actual serving tier, using the shared `near` / `chutes` /
  `non-attested` header values (`chutes` is the existing attested-third-party label).
- `X-Model-Alias-Resolved`: present when a catalog alias resolves. Set
  `X-No-Aliasing: true` to reject alias resolution before inference.

A gateway receipt signs `SHA256(original request bytes):SHA256(response bytes)`
with both ECDSA and Ed25519 and reports `signature_kind: gateway`. It attests
to what the gateway exchanged; it does not make external Jev inference TEE
verified. A provider receipt is retained with `signature_kind: provider_tee`.
Signature finalization and usage recording are awaited with existing bounded
persistence helpers. Once inference succeeds, finalization survives client
disconnects. As on existing completion paths, persistence failures are logged
without discarding an already completed inference; a receipt can be unavailable
if signing/storage fails.

Authentication, credit/spending checks, API-key rate limits, and organization
concurrency limits apply. Usage is recorded as `decisions`, with the configured
input/output token rates and actual provider attribution. Provider errors retain
their HTTP status (provider authentication failures become 502); upstream error
bodies are not echoed because validation errors can contain client state.
Upstream 429 responses use `error.type: upstream_rate_limit_exceeded`, distinct
from the gateway's `rate_limit_exceeded`; concurrency errors include the model
and configured limit. HTTP 429 remains retryable by client SDKs according to
their own retry policies.

## Self-hosted TEE contract

Use the existing NEAR fleet registration with `providerType: vllm`,
`inferenceUrl`, attestation enabled, and `outputModalities: ["decisions"]`.
The underlying TEE server/model proxy must implement:

1. `POST /v1/systemone` with the same typed request/response protocol.
2. Respect `X-Request-Hash` as the original client body hash, as for chat.
3. Add a globally unique response `id` consisting of 1–255 ASCII letters,
   digits, hyphens, or underscores.
4. Expose the existing `/v1/signature/{id}` contract for both algorithms,
   signing the original request hash and the exact response bytes.

The NEAR adapter uses the verified backend client and retains the backend
rotation index for signature retrieval, then releases the pin after finalization.
Missing/invalid TEE IDs fail closed rather than fabricating a provider receipt.
Cloud API does not add System One support to the model server itself.

## Scope and validation

`/v1/responses`, `/v1/chat/completions`, and `/v1/completions` reject decision models with a pointer to
`/v1/systemone`. There is no streaming, E2EE, or key-pinned routing contract for
this endpoint yet; encryption/routing-key headers are explicitly rejected.
Unknown request fields are rejected until their behavior is explicitly supported;
unknown fields on otherwise valid upstream responses are preserved verbatim.
The existing image receipt issue is tracked separately in
[#1125](https://github.com/nearai/cloud-api/issues/1125).

Tests cover wire compatibility, Box answers with optional confidence, JSON image
parts, model overrides, raw bytes, sanitized upstream
errors, both receipt algorithms, billing/catalog discovery, alias and modality
validation, TEE ID storage boundaries, invalid usage, trust-preserving fallback,
and receipt/billing finalization after client disconnect. Fleet
tests cover distribution, concurrent spillover, instance failover, signature
affinity, and stopping retries after invalid responses or ambiguous timeouts.
No live TypeSafe or OpenRouter call is needed:

```sh
cargo test -p inference_providers systemone --locked
cargo test -p api --lib systemone --locked
DEV=true BRAVE_SEARCH_PRO_API_KEY=unused-test-fixture \
  TEST_DATABASE_NAME=cloud_api_jev_e2e cargo nextest run -p api --test e2e_all --locked -E 'test(systemone)'
```

Configure the test database through the existing `DATABASE_*` environment
variables. `DEV=true` permits ephemeral signing keys in debug test builds;
the unused Brave placeholder satisfies test-server construction without invoking
search. These tests do not assert real hardware attestation.
