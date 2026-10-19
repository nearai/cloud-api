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
client disconnects during finalization. Settlement and signature storage each
have a five-second deadline. A settlement failure returns a server error as
before. A signature failure or timeout preserves the successful response and
never replays inference or bills again. A successful write is visible when the
HTTP response completes; clients must fail verification if the receipt cannot
be retrieved or validated. A timed-out database write may still finish later.

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
on Chat Completions. Receipt storage contains hashes, IDs and signatures, not
privacy inputs, detected spans or redacted text.
