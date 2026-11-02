# Native Messages gateway signatures

When native Anthropic Messages is enabled, successful `POST /v1/messages`
responses support the existing `GET /v1/signature/{message_id}` endpoint.
Use the JSON response's `id`, or `message_start.message.id` for SSE. The
`Inference-Id` response header is the existing UUIDv5 billing lookup key derived
from that full message ID; it is not the signature lookup key. No additional
generation or signature ID is introduced.

Both `?signing_algo=ecdsa` and `?signing_algo=ed25519` are supported. The receipt
has `signature_kind: gateway` and signs:

```text
SHA256(original client request body):SHA256(returned response body)
```

Retain the original request bytes, before parsing/re-serializing or resolving
model aliases. Response hashes cover the application body bytes (after HTTP
content decoding), not compressed wire bytes or HTTP chunk framing. JSON is
returned without re-serialization. For SSE, hash the concatenated raw event
bytes, including event names, whitespace, comments, pings, unknown events and the
complete `message_stop` event. Do not hash a reconstructed Message object.

The gateway holds the terminal SSE event until signature storage finishes or its
five-second deadline expires. `message_stop` is the end of the response; bytes
after it are not forwarded. A successful receipt write is visible before the
client receives this event. Signature finalization does not wait for upstream
transport EOF and continues if the client disconnects while storage is pending.

Errors, streams without a complete terminal event, and cancellations before that
event do not produce complete-response receipts. A missing/invalid message ID,
malformed or repeated start event, or an SSE event exceeding 256 KiB also prevents
receipt creation. Existing response and usage handling remain available; no
replacement response ID is invented. The bounded initial ID peek can omit the
`Inference-Id` header if the ID arrives late; billing and signature lookup still
use the eventual message ID when available. Once the first `message_start` is
observed, a missing or invalid ID ends the peek immediately: the response keeps
streaming without a receipt or `Inference-Id`.

As with Chat Completions, signature storage failure does not replay inference or
replace an otherwise successful response. Verification must fail if a receipt
cannot be retrieved or validated; HTTP 200 alone does not guarantee a receipt.
Usage accounting remains independent and runs once on completion, error, or
disconnect. Signing stores hashes, IDs and signatures, not message content.

These gateway signatures attest the request/response bytes observed by Cloud
API. They do not establish Anthropic model execution inside a TEE. This feature
does not add E2EE or an inference-sdk Anthropic adapter, and
`/v1/messages/count_tokens` remains unsigned.
