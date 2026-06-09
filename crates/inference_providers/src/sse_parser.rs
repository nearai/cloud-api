use crate::{ChatCompletionChunk, CompletionChunk, CompletionError, StreamChunk};
use bytes::Bytes;
use futures_util::Stream;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::warn;

/// Represents a single SSE event with both raw bytes and parsed content
#[derive(Debug, Clone, serde::Serialize)]
pub struct SSEEvent {
    /// The raw bytes of this SSE event (including "data: " prefix and newline)
    #[serde(skip)]
    pub raw_bytes: Bytes,
    /// The parsed StreamChunk. `None` for control lines (blank separator
    /// lines, `: comments`, the `data: [DONE]` terminator, non-data SSE
    /// fields like `event:`/`id:`, and the end-of-stream tail flush) — these
    /// carry only `raw_bytes` so the upstream byte stream can be reassembled
    /// exactly for TEE signature verification (issue #701). Control events
    /// are only emitted by passthrough-capable parsers (see
    /// [`SSEEventParser::passthrough_raw`]).
    pub chunk: Option<StreamChunk>,
    /// True when `raw_bytes` are the upstream's OpenAI-format SSE wire bytes
    /// and may be forwarded to clients verbatim. False for providers whose
    /// raw bytes are in a native non-OpenAI format (Gemini, Anthropic) that
    /// must be re-serialized before reaching clients.
    #[serde(skip)]
    pub raw_passthrough: bool,
}

impl SSEEvent {
    /// True if this is the upstream `data: [DONE]` terminator control line.
    /// Used by the route layer to avoid appending a duplicate gateway-minted
    /// `[DONE]` after forwarding the upstream one verbatim.
    pub fn is_done_marker(&self) -> bool {
        if self.chunk.is_some() {
            return false;
        }
        let line = String::from_utf8_lossy(&self.raw_bytes);
        line.trim()
            .strip_prefix("data:")
            .is_some_and(|d| d.trim() == "[DONE]")
    }
}

/// Trait for provider-specific SSE event parsing
///
/// Each provider (OpenAI/vLLM, Anthropic, Gemini) implements this trait
/// to handle their specific event format while sharing common buffer management.
pub trait SSEEventParser: Send + Unpin {
    /// Provider-specific state (message_id, token counts, etc.)
    type State: Send + Unpin;

    /// Parse a single SSE data line into a StreamChunk
    ///
    /// Returns:
    /// - `Ok(Some(chunk))` - Successfully parsed an event
    /// - `Ok(None)` - Line should be skipped (e.g., [DONE] marker, ping events)
    /// - `Err(e)` - Parse error
    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError>;

    /// Whether this parser handles raw JSON lines (not SSE format)
    ///
    /// If true, lines without "data: " prefix are also parsed.
    /// Used by Gemini which can return raw JSON lines.
    fn handles_raw_json() -> bool {
        false
    }

    /// Whether the upstream wire format is OpenAI-format SSE that clients can
    /// consume directly. When true, the parser is lossless: control lines
    /// (blank separators, comments, `[DONE]`, non-data fields) and any
    /// trailing unterminated bytes are emitted as chunk-less [`SSEEvent`]s so
    /// concatenating every event's `raw_bytes` reproduces the upstream byte
    /// stream exactly — required for byte-exact TEE signature verification
    /// through the gateway (issue #701). When false (Gemini, Anthropic
    /// native formats), control lines are silently skipped as before.
    fn passthrough_raw() -> bool {
        false
    }
}

/// Generic buffered SSE parser with proper multi-event handling
///
/// This parser correctly handles the case where multiple SSE events arrive
/// in a single network packet by using a VecDeque to queue pending results.
///
/// # Type Parameters
/// - `S`: The underlying byte stream (typically `impl Stream<Item = Result<Bytes, reqwest::Error>>`)
/// - `P`: The provider-specific event parser implementing `SSEEventParser`
pub struct BufferedSSEParser<S, P: SSEEventParser> {
    inner: S,
    bytes_buffer: Vec<u8>,
    /// Pending results from previous process_buffer() calls.
    /// Multiple SSE events can arrive in a single network packet.
    pending_results: VecDeque<Result<SSEEvent, CompletionError>>,
    /// Set to true after the underlying byte stream returns an error or ends.
    /// Prevents infinite error loops when the stream is broken.
    finished: bool,
    state: P::State,
    _marker: PhantomData<P>,
}

impl<S, P> BufferedSSEParser<S, P>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
    P: SSEEventParser,
{
    /// Create a new buffered SSE parser with the given state
    pub fn new(stream: S, state: P::State) -> Self {
        Self {
            inner: stream,
            bytes_buffer: Vec::new(),
            pending_results: VecDeque::new(),
            finished: false,
            state,
            _marker: PhantomData,
        }
    }

    fn process_buffer(&mut self) -> Vec<Result<SSEEvent, CompletionError>> {
        let mut results = Vec::new();

        // Process complete lines in the bytes buffer directly.
        // We search for newlines in the raw bytes instead of maintaining a separate
        // String buffer, because String::from_utf8_lossy can change byte counts
        // (replacing invalid sequences with the 3-byte U+FFFD), which caused the
        // two buffers to desync and panic with "index out of range for slice".
        while let Some(newline_pos) = self.bytes_buffer.iter().position(|&b| b == b'\n') {
            let line_len = newline_pos + 1; // Include the newline character

            // Extract raw bytes first to avoid an intermediate allocation and copy.
            let raw_bytes = Bytes::copy_from_slice(&self.bytes_buffer[..line_len]);
            self.bytes_buffer.drain(..line_len);

            // Convert to string for parsing (excluding the trailing newline)
            let line = String::from_utf8_lossy(&raw_bytes[..newline_pos]);
            let line = line.trim();

            let passthrough = P::passthrough_raw();

            // Empty lines and comments carry no parseable payload. For
            // passthrough parsers, emit them as control events so the raw
            // byte stream stays reconstructable; otherwise skip as before.
            if line.is_empty() || line.starts_with(':') {
                if passthrough {
                    results.push(Ok(SSEEvent {
                        raw_bytes,
                        chunk: None,
                        raw_passthrough: true,
                    }));
                }
                continue;
            }

            // Look for data: lines or handle raw JSON if supported
            let data = if let Some(d) = line.strip_prefix("data: ") {
                Some(d)
            } else if P::handles_raw_json() {
                Some(line)
            } else {
                None
            };

            if let Some(data) = data {
                match P::parse_event(&mut self.state, data) {
                    Ok(Some(chunk)) => {
                        results.push(Ok(SSEEvent {
                            raw_bytes,
                            chunk: Some(chunk),
                            raw_passthrough: passthrough,
                        }));
                    }
                    Ok(None) => {
                        // [DONE] marker: control event in passthrough mode,
                        // skipped otherwise.
                        if passthrough {
                            results.push(Ok(SSEEvent {
                                raw_bytes,
                                chunk: None,
                                raw_passthrough: true,
                            }));
                        }
                    }
                    Err(e) => results.push(Err(e)),
                }
            } else if passthrough {
                // Non-data SSE field line (event:, id:, retry:). We don't
                // parse these, but they're part of the signed byte stream.
                results.push(Ok(SSEEvent {
                    raw_bytes,
                    chunk: None,
                    raw_passthrough: true,
                }));
            }
        }

        results
    }
}

impl<S, P> Stream for BufferedSSEParser<S, P>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
    P: SSEEventParser,
{
    type Item = Result<SSEEvent, CompletionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // If the underlying stream has errored or ended, don't poll it again.
        // This prevents infinite error loops when the byte stream is broken
        // (e.g., due to read timeouts under load).
        if this.finished {
            return Poll::Ready(None);
        }

        loop {
            // First, return any pending results from previous process_buffer() calls
            if let Some(result) = this.pending_results.pop_front() {
                return Poll::Ready(Some(result));
            }

            // Try to get more results from the current buffer
            let buffered_results = this.process_buffer();
            if !buffered_results.is_empty() {
                this.pending_results.extend(buffered_results);
                continue;
            }

            // Poll for more data from the underlying stream
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    // Add new data to buffer and loop back to process it
                    this.bytes_buffer.extend_from_slice(&bytes);
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    // Mark stream as finished so we don't poll the broken stream again
                    this.finished = true;
                    return Poll::Ready(Some(Err(CompletionError::CompletionError(e.to_string()))));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    // Stream ended - flush or report any remaining incomplete data
                    if !this.bytes_buffer.is_empty() {
                        if P::passthrough_raw() {
                            // A trailing line without a final newline is part
                            // of the signed upstream byte stream — emit it as
                            // a control event so byte-exact reassembly holds
                            // (mirrors inference-proxy's transformer flush).
                            let leftover = Bytes::from(std::mem::take(&mut this.bytes_buffer));
                            return Poll::Ready(Some(Ok(SSEEvent {
                                raw_bytes: leftover,
                                chunk: None,
                                raw_passthrough: true,
                            })));
                        }
                        if this.bytes_buffer.iter().any(|&b| !b.is_ascii_whitespace()) {
                            warn!("Incomplete SSE data in buffer at stream end");
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ============================================================================
// OpenAI/vLLM Event Parser
// ============================================================================

/// State for OpenAI/vLLM SSE parsing
#[derive(Default)]
pub struct OpenAIParserState {
    pub(crate) is_chat: bool,
    /// Tags `HttpError` events surfaced from in-stream `{"error":{...}}`
    /// frames so `map_provider_error` can distinguish our own vLLM/SGLang
    /// (`false`) from a third-party OpenAI-compatible upstream (`true`).
    /// The (status, external) tuple drives different user-facing messages
    /// — e.g. a 404 from a third-party provider is a `ProviderError 502`,
    /// while a 404 from our own vLLM is `InvalidModel`.
    pub(crate) is_external: bool,
}

impl OpenAIParserState {
    pub fn new(is_chat: bool) -> Self {
        Self {
            is_chat,
            is_external: false,
        }
    }

    pub fn new_with_external(is_chat: bool, is_external: bool) -> Self {
        Self {
            is_chat,
            is_external,
        }
    }
}

/// OpenAI/vLLM event parser
///
/// Handles the standard OpenAI SSE format used by vLLM and OpenAI-compatible APIs.
pub struct OpenAIEventParser;

impl SSEEventParser for OpenAIEventParser {
    type State = OpenAIParserState;

    /// vLLM/SGLang (and OpenAI-compatible third parties) emit the same SSE
    /// wire format clients consume, so raw bytes may be forwarded verbatim.
    /// This is what makes gateway streams byte-exact verifiable against the
    /// inference TEE's response-hash signature (issue #701).
    fn passthrough_raw() -> bool {
        true
    }

    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        // Handle end-of-stream marker
        if data == "[DONE]" {
            return Ok(None);
        }

        // Parse JSON data
        match serde_json::from_str::<serde_json::Value>(data) {
            Ok(json) => {
                // SGLang / vLLM emit an in-stream error frame when an abort
                // fires AFTER the HTTP 200 headers were sent (queue-full,
                // priority-disabled, waiting timeout, etc.):
                //   data: {"error":{"message":"...","type":"...","code":503}}
                // Surface it as a typed `HttpError` so nearai::Provider's
                // rotation-SNI fallback can classify by upstream status
                // (5xx → try a different backend) instead of treating it as
                // a generic `InvalidResponse`, which would terminate the
                // stream silently with a `server_error` SSE frame.
                if let Some(err_obj) = json.get("error").and_then(|v| v.as_object()) {
                    let status_code = err_obj
                        .get("code")
                        .and_then(|v| v.as_u64())
                        .and_then(|n| u16::try_from(n).ok())
                        .filter(|&n| (100..=599).contains(&n))
                        .unwrap_or(502);
                    let message = err_obj
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Upstream stream emitted an error event")
                        .to_string();
                    return Err(CompletionError::HttpError {
                        status_code,
                        message,
                        is_external: state.is_external,
                    });
                }
                let chunk = if state.is_chat {
                    match serde_json::from_value::<ChatCompletionChunk>(json) {
                        Ok(chunk) => StreamChunk::Chat(chunk),
                        Err(_) => {
                            // Don't log error details - may contain customer data
                            warn!("Failed to parse event");
                            return Err(CompletionError::InvalidResponse(
                                "Failed to parse event".to_string(),
                            ));
                        }
                    }
                } else {
                    match serde_json::from_value::<CompletionChunk>(json) {
                        Ok(chunk) => StreamChunk::Text(chunk),
                        Err(_) => {
                            // Don't log error details - may contain customer data
                            warn!("Failed to parse event");
                            return Err(CompletionError::InvalidResponse(
                                "Failed to parse event".to_string(),
                            ));
                        }
                    }
                };
                Ok(Some(chunk))
            }
            Err(_) => {
                // Don't log error details - may contain customer data
                warn!("Failed to parse event");
                Err(CompletionError::InvalidResponse(
                    "Failed to parse event".to_string(),
                ))
            }
        }
    }
}

/// SSE (Server-Sent Events) stream parser for OpenAI/vLLM format
///
/// Type alias for backward compatibility.
pub type SSEParser<S> = BufferedSSEParser<S, OpenAIEventParser>;

/// Create a new SSE parser for OpenAI/vLLM format (our own vLLM/SGLang).
/// In-stream error frames are tagged `is_external: false`.
pub fn new_sse_parser<S>(stream: S, is_chat: bool) -> SSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, OpenAIParserState::new(is_chat))
}

/// Create a new SSE parser for a third-party OpenAI-compatible upstream
/// (the `external::openai_compatible` provider). In-stream error frames
/// surface as `HttpError { is_external: true }` so `map_provider_error`
/// applies the external-provider taxonomy (e.g. 404 → `ProviderError 502`
/// rather than `InvalidModel`).
pub fn new_external_sse_parser<S>(stream: S, is_chat: bool) -> SSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, OpenAIParserState::new_with_external(is_chat, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn test_sse_parser_multiple_events_in_single_packet() {
        // Simulate multiple SSE events arriving in a single network packet
        // This tests that the parser doesn't lose events when process_buffer() returns multiple results
        let multi_event_packet = concat!(
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" World\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n",
        );

        // Create a mock stream that returns all events in one packet
        let bytes = bytes::Bytes::from(multi_event_packet);
        let mock_stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // 3 data events + 3 blank-line control events (lossless passthrough)
        assert_eq!(events.len(), 6, "Expected 6 events, got {}", events.len());

        // Verify each event is Ok
        for (i, event) in events.iter().enumerate() {
            assert!(event.is_ok(), "Event {} should be Ok", i);
        }

        // Verify the content of each parsed event
        let contents: Vec<String> = events
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                if let Some(StreamChunk::Chat(chunk)) = e.chunk {
                    chunk
                        .choices
                        .first()
                        .and_then(|c| c.delta.as_ref().and_then(|d| d.content.clone()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(contents, vec!["Hello", " World", "!"]);
    }

    #[tokio::test]
    async fn test_sse_parser_events_split_across_packets() {
        // Test events split across multiple network packets
        let packet1 = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let packet2 = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" World\"},\"finish_reason\":\"stop\"}]}\n\n";

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(packet1)),
            Ok(bytes::Bytes::from(packet2)),
        ]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // 2 data events + 2 blank-line control events
        assert_eq!(events.len(), 4, "Expected 4 events, got {}", events.len());
        let parsed = events
            .iter()
            .filter(|e| e.as_ref().is_ok_and(|ev| ev.chunk.is_some()))
            .count();
        assert_eq!(parsed, 2, "Expected 2 parsed events");

        for event in &events {
            assert!(event.is_ok());
        }
    }

    #[tokio::test]
    async fn test_sse_parser_handles_done_marker() {
        let packet = concat!(
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );

        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // 1 data event + blank control + [DONE] control + blank control
        assert_eq!(events.len(), 4, "Expected 4 events, got {}", events.len());
        let events: Vec<SSEEvent> = events.into_iter().map(|e| e.unwrap()).collect();
        assert!(
            events[0].chunk.is_some(),
            "First event should be parsed data"
        );
        assert!(
            events[2].is_done_marker(),
            "[DONE] should surface as a control event marked as done"
        );
        assert_eq!(
            events.iter().filter(|e| e.is_done_marker()).count(),
            1,
            "Exactly one [DONE] marker expected"
        );
    }

    #[tokio::test]
    async fn test_sse_parser_skips_comments_and_empty_lines() {
        let packet = concat!(
            ": this is a comment\n",
            "\n",
            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            ": another comment\n",
        );

        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // Comments and blank lines surface as chunk-less control events so
        // the byte stream stays reconstructable; exactly 1 parsed event.
        assert_eq!(events.len(), 5, "Expected 5 events, got {}", events.len());
        let parsed: Vec<_> = events
            .iter()
            .filter(|e| e.as_ref().is_ok_and(|ev| ev.chunk.is_some()))
            .collect();
        assert_eq!(parsed.len(), 1, "Expected 1 parsed event");
    }

    #[tokio::test]
    async fn test_sse_parser_partial_line_buffering() {
        // Test that partial lines are correctly buffered across packets
        let packet1 = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",";
        let packet2 = "\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(packet1)),
            Ok(bytes::Bytes::from(packet2)),
        ]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // 1 data event + 1 blank-line control event
        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());
        assert!(events[0].is_ok());
        assert!(events[0].as_ref().unwrap().chunk.is_some());
    }

    #[tokio::test]
    async fn test_sse_parser_terminates_after_stream_error() {
        // Test that the parser stops polling the underlying stream after an error.
        // We use a custom Stream impl that panics if polled after returning an error,
        // proving the `finished` flag prevents infinite error loops.
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::sync::Arc;
        use std::task::Poll;

        struct ErrorThenPanicStream {
            state: Arc<AtomicU8>, // 0=send_ok, 1=send_none, 2+=panic
        }

        impl Stream for ErrorThenPanicStream {
            type Item = Result<bytes::Bytes, reqwest::Error>;

            fn poll_next(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                let s = self.state.fetch_add(1, Ordering::SeqCst);
                match s {
                    0 => {
                        // First poll: return a valid SSE chunk
                        Poll::Ready(Some(Ok(bytes::Bytes::from(
                            "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n"
                        ))))
                    }
                    1 => {
                        // Second poll: stream ends (simulating a broken connection)
                        // We return None here since we can't easily construct a reqwest::Error.
                        // The `finished` flag is also set on stream end (Poll::Ready(None)).
                        Poll::Ready(None)
                    }
                    _ => {
                        // Third+ poll: should never happen if `finished` flag works
                        panic!("Stream was polled after ending! The `finished` flag is broken.");
                    }
                }
            }
        }

        impl Unpin for ErrorThenPanicStream {}

        let stream = ErrorThenPanicStream {
            state: Arc::new(AtomicU8::new(0)),
        };

        let parser =
            BufferedSSEParser::<_, OpenAIEventParser>::new(stream, OpenAIParserState::new(true));
        let events: Vec<_> = parser.collect().await;

        // Should have exactly 2 events (the good chunk + its blank-line
        // control event). The stream ended after that, and the parser must
        // NOT poll again. If the `finished` flag is broken, the
        // ErrorThenPanicStream will panic.
        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());
        assert!(events[0].is_ok(), "Event should be Ok");
        assert!(events[0].as_ref().unwrap().chunk.is_some());
        assert!(events[1].as_ref().unwrap().chunk.is_none());
    }

    #[tokio::test]
    async fn test_sse_parser_multibyte_utf8_no_panic() {
        // Regression test: the old dual-buffer approach (String + Vec<u8>) panicked
        // when SSE data contained multi-byte UTF-8 characters, because
        // String::from_utf8_lossy could change byte counts relative to the raw
        // bytes buffer, causing an "index out of range for slice" panic.
        //
        // This test uses Chinese characters (3 bytes each in UTF-8) to trigger
        // the length discrepancy that caused the crash in production.
        let packet = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好世界\"},\"finish_reason\":null}]}\n\n";

        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // 1 data event + 1 blank-line control event
        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());
        assert!(events[0].is_ok());
        assert!(events[0].as_ref().unwrap().chunk.is_some());
    }

    #[tokio::test]
    async fn test_sse_parser_split_multibyte_utf8_no_panic() {
        // Regression test: when a multi-byte UTF-8 character is split across two
        // network packets, String::from_utf8_lossy would replace the incomplete
        // sequence with U+FFFD (3 bytes), making the string buffer longer than the
        // bytes buffer and eventually causing a panic.
        //
        // é (U+00E9) is 2 bytes in UTF-8: 0xC3 0xA9
        // We split it so packet 1 ends with 0xC3 and packet 2 starts with 0xA9.
        let json_before = b"data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"caf";
        let split_byte = b"\xC3"; // First byte of é
        let rest_byte = b"\xA9"; // Second byte of é
        let json_after = b"\"},\"finish_reason\":null}]}\n\n";

        let mut packet1 = Vec::new();
        packet1.extend_from_slice(json_before);
        packet1.extend_from_slice(split_byte);

        let mut packet2 = Vec::new();
        packet2.extend_from_slice(rest_byte);
        packet2.extend_from_slice(json_after);

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(packet1)),
            Ok(bytes::Bytes::from(packet2)),
        ]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        // Should get the data event + blank-line control event without
        // panicking. The content will contain the correctly reassembled é
        // since bytes are buffered until newline.
        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());
        assert!(events[0].is_ok(), "The event should be parsed successfully");
        assert!(events[0].as_ref().unwrap().chunk.is_some());
    }

    #[tokio::test]
    async fn test_sse_parser_propagates_error_chunk_as_http_error() {
        // SGLang `--max-queued-requests` abort emits an in-stream error frame
        // *after* HTTP 200 headers. Previously the parser couldn't classify
        // this and surfaced `InvalidResponse("Failed to parse event")`, which
        // hid the upstream status from nearai::Provider's rotation fallback.
        // The fix promotes any `{"error":{"code":N,...}}` chunk to a typed
        // `HttpError { status_code: N }` so the rotation path can recognize
        // it as 5xx and walk to a different backend.
        let packet = "data: {\"error\":{\"object\":\"error\",\"message\":\"The request queue is full.\",\"type\":\"SERVICE_UNAVAILABLE\",\"code\":503}}\n\ndata: [DONE]\n\n";
        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);
        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;
        // The error event, plus control events for the blank separators and
        // the [DONE] terminator (lossless passthrough).
        assert_eq!(
            events.len(),
            4,
            "Expected error event + 3 control events, got {}",
            events.len()
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Err(CompletionError::HttpError { .. })))
                .count(),
            1,
            "Exactly one error event expected"
        );
        match &events[0] {
            Err(CompletionError::HttpError {
                status_code,
                message,
                is_external,
            }) => {
                assert_eq!(*status_code, 503);
                assert!(
                    message.contains("queue is full"),
                    "Expected upstream message to round-trip, got: {message}"
                );
                assert!(
                    !*is_external,
                    "Internal vLLM/SGLang errors must not leak as external"
                );
            }
            other => panic!("Expected HttpError 503, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_sse_parser_error_chunk_without_numeric_code_falls_back_to_502() {
        // Defensive: not every upstream emits `code`. A `{"error":{...}}` with
        // no `code` field still indicates an upstream problem — surface it as
        // 502 so rotation fallback considers it retryable (5xx).
        let packet = "data: {\"error\":{\"message\":\"something went wrong\",\"type\":\"server_error\"}}\n\n";
        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);
        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;
        match &events[0] {
            Err(CompletionError::HttpError { status_code, .. }) => {
                assert_eq!(*status_code, 502, "Missing code should default to 502");
            }
            other => panic!("Expected HttpError 502, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_external_sse_parser_tags_error_chunks_as_external() {
        // External (third-party OpenAI-compatible) providers route through
        // `new_external_sse_parser` so their in-stream error chunks surface
        // with `is_external: true`. This matters for `map_provider_error`:
        // a 404 from a third-party provider should map to `ProviderError
        // 502` (their model is unavailable), not `InvalidModel` (which is
        // the meaning of 404 from our own vLLM infrastructure).
        let packet = "data: {\"error\":{\"message\":\"model not found\",\"type\":\"not_found\",\"code\":404}}\n\n";
        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);
        let parser = new_external_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;
        match &events[0] {
            Err(CompletionError::HttpError {
                status_code,
                is_external,
                ..
            }) => {
                assert_eq!(*status_code, 404);
                assert!(
                    *is_external,
                    "External provider error chunks must be tagged is_external: true"
                );
            }
            other => panic!("Expected HttpError 404 with is_external: true, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_sse_parser_ignores_non_object_error_field() {
        // A chunk like `{"id":..., "error": null}` would historically have
        // failed `ChatCompletionChunk` deserialization and surfaced as
        // `InvalidResponse`. With the new branch we MUST only intercept on
        // `error` being an *object* — otherwise a legitimate streaming
        // response with a null/missing-error semantic would be misclassified
        // as 502.
        let packet = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}],\"error\":null}\n\n";
        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);
        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;
        // 1 data event + 1 blank-line control event
        assert_eq!(events.len(), 2);
        assert!(
            events[0].is_ok(),
            "null `error` field must not trigger the error path: got {:?}",
            events[0]
        );
        assert!(events[0].as_ref().unwrap().chunk.is_some());
    }

    #[tokio::test]
    async fn test_sse_parser_passthrough_byte_exact_reassembly() {
        // Core property behind issue #701: for an OpenAI-format upstream,
        // concatenating raw_bytes of every Ok event must reproduce the
        // upstream byte stream EXACTLY — including comments, blank lines,
        // CRLF line endings, non-data SSE fields, the [DONE] terminator and
        // a trailing unterminated line. This is what makes
        // sha256(client-received bytes) match the response hash signed by
        // the inference TEE (which hashes the bytes it sends).
        let part1: &[u8] = b": keepalive comment\n\ndata: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel";
        let part2: &[u8] = b"lo\"},\"finish_reason\":null}]}\r\n\r\nevent: ping\ndata: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n";
        let part3: &[u8] = b"data: [DONE]\n\ntrailing-unterminated";

        let mock_stream = futures_util::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(part1)),
            Ok(bytes::Bytes::from(part2)),
            Ok(bytes::Bytes::from(part3)),
        ]);

        let parser = new_sse_parser(mock_stream, true);
        let events: Vec<_> = parser.collect().await;

        let mut reassembled: Vec<u8> = Vec::new();
        let mut parsed_count = 0;
        let mut done_count = 0;
        for event in events {
            let event = event.expect("No errors expected in this stream");
            assert!(
                event.raw_passthrough,
                "OpenAI-format parser events must be passthrough-capable"
            );
            if event.chunk.is_some() {
                parsed_count += 1;
            }
            if event.is_done_marker() {
                done_count += 1;
            }
            reassembled.extend_from_slice(&event.raw_bytes);
        }

        let mut original: Vec<u8> = Vec::new();
        original.extend_from_slice(part1);
        original.extend_from_slice(part2);
        original.extend_from_slice(part3);

        assert_eq!(parsed_count, 2, "Expected 2 parsed data chunks");
        assert_eq!(done_count, 1, "Expected exactly one [DONE] control event");
        assert_eq!(
            reassembled, original,
            "Concatenated raw_bytes must reproduce the upstream byte stream exactly"
        );
    }

    #[tokio::test]
    async fn test_sse_parser_non_passthrough_parser_skips_control_lines() {
        // A parser that does NOT opt into passthrough (Gemini/Anthropic
        // native formats) must keep the historical behavior: control lines
        // are silently dropped, no tail flush, and events are not marked
        // passthrough.
        struct NonPassthroughParser;
        impl SSEEventParser for NonPassthroughParser {
            type State = OpenAIParserState;
            fn parse_event(
                state: &mut Self::State,
                data: &str,
            ) -> Result<Option<StreamChunk>, CompletionError> {
                OpenAIEventParser::parse_event(state, data)
            }
        }

        let packet = ": comment\n\ndata: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\ntrailing";
        let mock_stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(packet))]);

        let parser = BufferedSSEParser::<_, NonPassthroughParser>::new(
            mock_stream,
            OpenAIParserState::new(true),
        );
        let events: Vec<_> = parser.collect().await;

        assert_eq!(
            events.len(),
            1,
            "Non-passthrough parser must emit only parsed events"
        );
        let event = events[0].as_ref().unwrap();
        assert!(event.chunk.is_some());
        assert!(
            !event.raw_passthrough,
            "Non-passthrough parser events must not be marked passthrough"
        );
    }

    #[tokio::test]
    async fn test_sse_event_is_done_marker() {
        let done = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(
                b"data: [DONE]
",
            ),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(done.is_done_marker());

        // No space after the colon is still a valid SSE data line
        let done_no_space = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(
                b"data:[DONE]
",
            ),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(done_no_space.is_done_marker());

        let comment = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(
                b": keepalive
",
            ),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(!comment.is_done_marker());

        let blank = SSEEvent {
            raw_bytes: bytes::Bytes::from_static(
                b"
",
            ),
            chunk: None,
            raw_passthrough: true,
        };
        assert!(!blank.is_done_marker());
    }
}
