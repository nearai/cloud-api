# Tinfoil wire fixtures

Recorded 2026-10-07 against `https://inference.tinfoil.sh/v1` with the prompt `Say OK`
(`max_tokens` 16, or 64 for glm-5-3). `id` and `system_fingerprint` were replaced with
`chatcmpl-test` / `fp-test`; all field names and structure are unchanged. No credentials are stored.

| File | Request |
|---|---|
| `chat_nonstream.json` | gpt-oss-120b, non-stream |
| `chat_stream.sse` | gpt-oss-120b, `stream:true`, `stream_options.include_usage:true` |
| `chat_stream_no_usage.sse` | gpt-oss-120b, `stream:true`, no `stream_options` |
| `chat_stream_glm53_low.sse` | glm-5-3, `reasoning_effort:"low"`, stream |
| `chat_stream_glm53_high.sse`, `chat_nonstream_glm53_high.json` | glm-5-3, `reasoning_effort:"high"` |
| `models.json` | `GET /v1/models` |

## Findings

- Reasoning field is `reasoning` (not `reasoning_content`): `choices[0].message.reasoning` (non-stream) and
  `choices[0].delta.reasoning` (stream), observed on gpt-oss-120b. With a small `max_tokens` the model spends the whole
  budget on reasoning, so `content` is `null` and `finish_reason` is `length`.
- glm-5-3 answered "OK" directly at low and high effort: stream deltas carry only `content` (no `reasoning` key); the non-stream
  message has `"reasoning":null`. The field name is the same (`reasoning`), but no reasoning text was observed for this prompt.
  `usage.completion_tokens_details.reasoning_tokens` is present (0).
- Usage with `include_usage:true`: every chunk carries a cumulative `usage` object (vLLM continuous usage stats), and a final
  chunk with `"choices":[]` and the total `usage` arrives before `[DONE]`.
- Usage without `include_usage`: NO final empty-choices chunk, but per-chunk cumulative `usage` is still present on every
  chunk (including the last chunk with `finish_reason`). Usage is therefore available either way; the last chunk with `usage`
  holds the totals.
- Non-OpenAI extras: per-chunk padding field `delta.p` (random-length alphabet string, ignore), `token_ids`, `prompt_token_ids`,
  `prompt_text`, `stop_reason`, `routed_experts`, `kv_transfer_params`, `metrics`; first chunk has `content:""` plus `role`.
  Non-stream `usage.prompt_tokens_details` may be null or contain `cached_tokens`/`created_cache_tokens`.
- `/v1/models` entries include `reasoning`, `reasoning_params`, `tool_calling`, `multimodal`, `type`
  (chat/embedding/audio/tts/safety/document/tool), `endpoints`, `context_window` and `pricing`.
