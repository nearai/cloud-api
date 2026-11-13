# Stream usage estimates

## Streams without a usage summary

Cloud API uses a provider usage summary when one arrives, including after a
terminal choice chunk. If a stream ends without one, a client disconnect with
no provider error records an estimate with stop reason `client_disconnect`;
a completed stream records an estimate with stop reason `incomplete`. A
provider or transport error (`last_error`) records no usage when no summary
arrived.

If more output arrives after the last provider usage snapshot, the record
keeps the reported prompt and cache tokens and uses the greater of the
reported output count and the output estimate below. A usage snapshot that
covers the last observed output remains authoritative, including when the
client disconnects before the terminal marker.

A missing provider response ID does not discard observed usage. Such records
receive an independent inference ID and leave the provider request ID absent.
An empty stream without provider usage creates no estimated record.

The input estimate includes message text, tool definitions, `tool_choice`,
prior `tool_calls`, and a fixed 1,024 tokens per non-text content part by
default (`CONTEXT_ROUTE_MEDIA_PART_TOKENS` can override that fixed amount).
It does not count prior-turn reasoning text, which some adapters drop before
the request is sent. A client-encrypted request served by NEAR AI's own
fleet records no estimate, because byte counts over ciphertext do not reflect
the prompt or the output.
The output estimate includes streamed text, reasoning text, and tool-call
name and argument deltas. Text uses a UTF-8 bytes/4 heuristic, so CJK and
emoji counts are approximate. When no usage was reported, cached tokens are
zero and the estimated input is charged at the full input rate.

Estimated records debit credits and count toward spend limits in the same way
as exact records. A client retry after a cut stream is a separate request
and is billed separately. `USAGE_ESTIMATE_INCOMPLETE_STREAMS` defaults to
`true`; set it to `false` or `0` to disable estimates. Invalid values fail
configuration loading.

The native Anthropic streaming endpoint uses the same switch and output
heuristic for text, thinking, and tool JSON deltas after its last output
usage snapshot. Reported input and cache counts are preserved. A final
output usage snapshot remains authoritative, even if the connection closes
before `message_stop`. Provider errors do not trigger output estimation.
