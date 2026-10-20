---
paths:
  - "crates/**/*.rs"
---

# Privacy and logging

This service runs in a TEE and sells privacy. Production logs at `info` and above, and those logs leave the enclave, so anything logged at those levels is effectively disclosed. Customer trust depends on this rule more than any other.

## Log identifiers, never content

Allowed at any level:
- IDs: `conversation_id`, `org_id`, `user_id`, `workspace_id`, `response_id`, `api_key_id`, `session_id`
- Numbers: request counts, token counts, latency, TTFT, ITL, byte sizes
- Error kinds, error codes, HTTP status codes
- System events: startup, shutdown, pool status, auth outcomes (not the credential)

Never log, at any level including `debug`/`trace`:
- Credentials: API keys, session or OAuth tokens, passwords, encryption keys, signed URLs
- Conversation content: messages, prompts, completions, tool arguments/results, file contents
- Customer-authored labels: conversation titles and descriptions, metadata, custom fields, tags
- PII inside user content: names, emails (outside the auth flow), addresses, phone numbers
- Whole request or response bodies

```rust
// Do this
tracing::info!(api_key_id = %key.id, "API key validated");
tracing::warn!(conversation_id = %conv_id, "invalid input format");

// Not this: each line discloses customer data or a credential
tracing::info!("API key: {}", api_key);
tracing::info!("Creating conversation: {}", conversation.title);
tracing::warn!("Invalid input: {}", user_input);
```

## Hidden leaks to check

- `{:?}` on a request, message, or config struct prints every field. Log the ID instead, or derive a redacted `Debug`.
- Error values often embed the input that failed (`serde_json` errors quote the offending text, upstream provider errors echo the prompt). Log the error kind or status, not `{e}` / `{e:?}`, when the error can carry customer data.
- `#[tracing::instrument]` records every argument by default. Use `skip_all` and add only ID fields.
- Metric labels are exported too: never put customer-controlled strings in a label.

## Levels

Production does not enable `debug` or `trace`, so detailed diagnostics that contain no customer data (internal state, timings, decision branches) belong there rather than at `info`. The never-log list above still applies at every level.

Before adding a log line, ask whether it could reveal anything about a customer's conversation or activity. If you are unsure whether a value can contain customer data, leave it out of the log.
