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
    /// The parsed StreamChunk
    pub chunk: StreamChunk,
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
    buffer: String,
    bytes_buffer: Vec<u8>,
    /// Pending results from previous process_buffer() calls.
    /// Multiple SSE events can arrive in a single network packet.
    pending_results: VecDeque<Result<SSEEvent, CompletionError>>,
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
            buffer: String::new(),
            bytes_buffer: Vec::new(),
            pending_results: VecDeque::new(),
            state,
            _marker: PhantomData,
        }
    }

    fn process_buffer(&mut self) -> Vec<Result<SSEEvent, CompletionError>> {
        let mut results = Vec::new();

        // Process complete lines in the buffer
        while let Some(newline_pos) = self.buffer.find('\n') {
            let line_len = newline_pos + 1; // Include the newline character

            // Extract the raw bytes for this line
            let raw_bytes = Bytes::copy_from_slice(&self.bytes_buffer[..line_len]);
            self.bytes_buffer.drain(..line_len);

            // Extract the string line
            let line = self.buffer.drain(..=newline_pos).collect::<String>();
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with(':') {
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
                        results.push(Ok(SSEEvent { raw_bytes, chunk }));
                    }
                    Ok(None) => {} // Skip (e.g., [DONE] marker)
                    Err(e) => results.push(Err(e)),
                }
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
        // Get mutable reference to self - safe because all fields are Unpin
        let this = self.get_mut();

        // First, return any pending results from previous process_buffer() calls
        if let Some(result) = this.pending_results.pop_front() {
            return Poll::Ready(Some(result));
        }

        // Try to get more results from the current buffer
        let buffered_results = this.process_buffer();
        if !buffered_results.is_empty() {
            // Store all results in pending queue
            this.pending_results.extend(buffered_results);
            if let Some(result) = this.pending_results.pop_front() {
                return Poll::Ready(Some(result));
            }
        }

        // Poll for more data from the underlying stream
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Add new data to buffer
                this.bytes_buffer.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&bytes);
                this.buffer.push_str(&text);

                // Process any complete events
                let results = this.process_buffer();
                if !results.is_empty() {
                    // Store all results in pending queue
                    this.pending_results.extend(results);
                    if let Some(result) = this.pending_results.pop_front() {
                        return Poll::Ready(Some(result));
                    }
                }
                // No complete events yet, wake and try again
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(CompletionError::CompletionError(e.to_string()))))
            }
            Poll::Ready(None) => {
                // Stream ended - process any remaining buffer content
                if !this.buffer.trim().is_empty() {
                    warn!("Incomplete SSE data in buffer at stream end");
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
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
}

impl OpenAIParserState {
    pub fn new(is_chat: bool) -> Self {
        Self { is_chat }
    }
}

/// OpenAI/vLLM event parser
///
/// Handles the standard OpenAI SSE format used by vLLM and OpenAI-compatible APIs.
pub struct OpenAIEventParser;

impl SSEEventParser for OpenAIEventParser {
    type State = OpenAIParserState;

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
                let chunk = if state.is_chat {
                    match serde_json::from_value::<ChatCompletionChunk>(json) {
                        Ok(chunk) => StreamChunk::Chat(chunk),
                        Err(e) => {
                            // Log error type only - don't log content to protect customer data
                            warn!(error = %e, "Failed to parse chat completion chunk");
                            return Err(CompletionError::InvalidResponse(
                                "Invalid response format".to_string(),
                            ));
                        }
                    }
                } else {
                    match serde_json::from_value::<CompletionChunk>(json) {
                        Ok(chunk) => StreamChunk::Text(chunk),
                        Err(e) => {
                            // Log error type only - don't log content to protect customer data
                            warn!(error = %e, "Failed to parse text completion chunk");
                            return Err(CompletionError::InvalidResponse(
                                "Invalid response format".to_string(),
                            ));
                        }
                    }
                };
                Ok(Some(chunk))
            }
            Err(e) => {
                // Log error type only - don't log content to protect customer data
                warn!(error = %e, "Failed to parse SSE JSON");
                Err(CompletionError::InvalidResponse(
                    "Invalid JSON in SSE event".to_string(),
                ))
            }
        }
    }
}

/// SSE (Server-Sent Events) stream parser for OpenAI/vLLM format
///
/// Type alias for backward compatibility.
pub type SSEParser<S> = BufferedSSEParser<S, OpenAIEventParser>;

/// Create a new SSE parser for OpenAI/vLLM format
pub fn new_sse_parser<S>(stream: S, is_chat: bool) -> SSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    BufferedSSEParser::new(stream, OpenAIParserState::new(is_chat))
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

        // Should have received all 3 events
        assert_eq!(events.len(), 3, "Expected 3 events, got {}", events.len());

        // Verify each event is Ok
        for (i, event) in events.iter().enumerate() {
            assert!(event.is_ok(), "Event {} should be Ok", i);
        }

        // Verify the content of each event
        let contents: Vec<String> = events
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                if let StreamChunk::Chat(chunk) = e.chunk {
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

        assert_eq!(events.len(), 2, "Expected 2 events, got {}", events.len());

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

        // Should only have 1 event (the [DONE] marker is skipped)
        assert_eq!(events.len(), 1, "Expected 1 event, got {}", events.len());
        assert!(events[0].is_ok());
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

        // Should only have 1 event (comments and empty lines are skipped)
        assert_eq!(events.len(), 1, "Expected 1 event, got {}", events.len());
        assert!(events[0].is_ok());
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

        assert_eq!(events.len(), 1, "Expected 1 event, got {}", events.len());
        assert!(events[0].is_ok());
    }
}
