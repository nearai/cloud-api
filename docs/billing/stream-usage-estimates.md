# Stream usage estimates

## Streams without a usage summary

Cloud API uses a provider usage summary when one arrives, including after a
terminal choice chunk. If a stream ends without one, a client disconnect with
no provider error records an estimate with stop reason `client_disconnect`;
a completed stream records an estimate with stop reason `incomplete`. A
provider or transport error (`last_error`) records no usage when no summary
arrived.

Some providers report prompt usage at stream start with zero completion
tokens and send the completion total only in the final event. If output was
streamed but the last usage seen still has zero completion tokens, the record
keeps the reported prompt and cache tokens and uses the output estimate below.

The input estimate includes message text, tool definitions, `tool_choice`,
prior `tool_calls`, and a fixed 1,024 tokens per non-text content part by
default (`CONTEXT_ROUTE_MEDIA_PART_TOKENS` can override that fixed amount).
The output estimate includes streamed text, reasoning text, and tool-call
name and argument deltas. Text uses a UTF-8 bytes/4 heuristic, so CJK and
emoji counts are approximate. Estimated cached tokens are zero, so the entire
estimated input is charged at the full input rate.

Estimated records debit credits and count toward spend limits in the same way
as exact records. A client retry after a cut stream is a separate request
and is billed separately. `USAGE_ESTIMATE_INCOMPLETE_STREAMS` defaults to
`true`; set it to `false` or `0` to disable estimates. Invalid values fail
configuration loading.
