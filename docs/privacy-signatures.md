# Privacy response signatures

Successful `POST /v1/privacy/classify` and `POST /v1/privacy/redact` responses
with a valid upstream `id` support `GET /v1/signature/{id}`. URL-encode the ID
when constructing the lookup path. Both `signing_algo=ecdsa` and
`signing_algo=ed25519` are supported.

- `id` is the classifier's response ID. Classify preserves the entire upstream
  response body byte for byte. Redact carries that ID into its final JSON.
- `Inference-Id` is `hash_inference_id_to_uuid(id)`, the existing UUIDv5 billing
  lookup key. The usage record stores the original ID as `provider_request_id`.
- No `X-Generation-Id`, separate signature ID, or second redaction ID is added.

Privacy uses the same ID and lookup contract as other inference endpoints:
providers must issue a unique response ID for each inference, and signature
lookup requires a valid API key but does not enforce receipt ownership by
organization or API key. IDs share the existing receipt and billing namespaces.

The receipt has `signature_kind: gateway` and signs:

```text
SHA256(original client request body):SHA256(final response body)
```

Keep the exact request and response application bytes, including whitespace;
re-serializing parsed JSON can change their hashes. For redact, the request hash
covers the original client request (before normalization to a classifier call),
and the response hash covers the final redacted JSON, including its ID, rather
than the classifier's spans.

After inference succeeds, usage settlement and receipt storage continue if the
client disconnects during finalization. The initial usage write and signature
storage each have a five-second deadline. A usage write failure or timeout
preserves the successful response and retries billing asynchronously with the
same `Inference-Id`, including the random fallback ID. The database's per-org
idempotency key prevents that retry from charging twice if the original write
later commits. A signature failure or timeout also preserves the response;
neither path replays inference. A successful signature write is visible when
the HTTP response completes; clients must fail verification if the receipt
cannot be retrieved or validated. A timed-out database write may finish later,
and an unsuccessful billing retry is logged for investigation.

Legacy responses with a missing, malformed, empty, control-character-containing,
or longer-than-255-byte ID keep their classification/redaction behavior. They
receive no receipt or invented response ID; billing retains a random UUID,
exposed through `Inference-Id`, and no `provider_request_id`.

These signatures authenticate bytes observed and emitted by the gateway. The
Privacy transport currently has no per-response signing contract with the model
provider, even when the classifier is hosted in a TEE. Catalog attestation flags
and chat-signature support do not establish such a contract. A future classify
provider-TEE path must verify a receipt from the actual serving provider over
the appropriate request and response; redact still needs a gateway receipt for
its locally rewritten output. This change does not affect automatic redaction
on Chat Completions. Receipt storage contains hashes, IDs and signatures rather
than raw inputs, detected spans or redacted text. Hashing is not anonymization;
request/response hashes can still reveal whether a candidate payload matches.

## Cloud-platform signature reads

When `CLOUD_PLATFORM_URL` and `CLOUD_PLATFORM_API_KEY_FILE` are both set,
chat-signature reads are served from cloud-platform's
`GET /v1/internal/chat-signatures/{chat_id}` instead of the local repository.
Unset, nothing changes. There is no other switch. When cloud-platform cannot
answer (an error, timeout or open breaker), the read falls back to the local
repository.

This relies on the shared database: cloud-api writes `chat_signatures` locally,
and cloud-platform reads the same table. Do not configure the client if the two
services are ever given separate databases, unless the signature writes also go
to cloud-platform.
