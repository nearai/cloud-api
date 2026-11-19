# Messages API and gateway signatures

`POST /v1/messages` forwards Anthropic-backed models to their native Messages
API. For other models it uses Chat Completions and returns Anthropic-format JSON
or SSE. Choose a chat model with client tool support to use Claude Code:

```sh
export ANTHROPIC_BASE_URL="$CLOUD_API_BASE_URL"
export ANTHROPIC_API_KEY="$NEARAI_API_KEY"
export ANTHROPIC_MODEL="$CHAT_MODEL_ID"
claude
```

Set the base URL to the gateway origin, without `/v1`. Deployments enable the
route with `ENABLE_ANTHROPIC_MESSAGES`. Set Claude Code’s default Haiku, Sonnet,
and Opus model overrides to the same chat model if those tasks should also use it.

Chat translation supports system instructions, text and images, client tool
calls and results, and reasoning returned by the model. Anthropic-signed
thinking is relayed on the native path. On the chat path, visible reasoning
has an empty signature and can be replayed as plain reasoning. Setting
`thinking.display` to `"omitted"` hides reasoning from the client. Use visible
thinking for a model that requires its prior reasoning along with tool results.
Models that list `reasoning_effort` among their supported sampling parameters
receive a low, medium, or high effort hint for a thinking budget. Other chat
models use their own reasoning defaults.

Token counting at `/v1/messages/count_tokens` is available for native Anthropic
models. Chat models return a 404 for this operation. Models that require the
OpenAI Responses API return their Chat Completions error on `/v1/messages`.

## Gateway signatures

Successful `POST /v1/messages` responses support `GET /v1/signature/{message_id}`.
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
forwarded verbatim on the native path. On the chat path, hash the translated
JSON returned by the gateway. For SSE, hash the concatenated raw event
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

The receipt attests the request and response bytes observed by the gateway.
The `x-serving-provider` response header identifies the serving provider when
available. `/v1/messages/count_tokens` is unsigned.
