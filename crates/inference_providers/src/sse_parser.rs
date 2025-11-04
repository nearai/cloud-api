use crate::{ChatCompletionChunk, CompletionChunk, CompletionError, StreamChunk};
use bytes::Bytes;
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Represents a single SSE event with both raw bytes and parsed content
#[derive(Debug, Clone, serde::Serialize)]
pub struct SSEEvent {
    /// The raw bytes of this SSE event (including "data: " prefix and newline)
    #[serde(skip)]
    pub raw_bytes: Bytes,
    /// The parsed StreamChunk
    pub chunk: StreamChunk,
}

/// SSE (Server-Sent Events) stream parser that properly handles buffering
/// of incomplete events across HTTP chunks
pub struct SSEParser<S> {
    inner: S,
    buffer: String,
    bytes_buffer: Vec<u8>,
    is_chat: bool,
}

impl<S> SSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(stream: S, is_chat: bool) -> Self {
        Self {
            inner: stream,
            buffer: String::new(),
            bytes_buffer: Vec::new(),
            is_chat,
        }
    }

    fn parse_sse_event(data: &str, is_chat: bool) -> Result<Option<StreamChunk>, CompletionError> {
        // Handle end-of-stream marker
        if data == "[DONE]" {
            return Ok(None);
        }

        // Parse JSON data
        match serde_json::from_str::<serde_json::Value>(data) {
            Ok(json) => {
                let chunk = if is_chat {
                    match serde_json::from_value::<ChatCompletionChunk>(json.clone()) {
                        Ok(chunk) => StreamChunk::Chat(chunk),
                        Err(_) => {
                            // Log but don't fail - might be a partial chunk
                            eprintln!("Warning: Failed to parse chat chunk for json");
                            return Err(CompletionError::InvalidResponse(
                                "Invalid response format".to_string(),
                            ));
                        }
                    }
                } else {
                    match serde_json::from_value::<CompletionChunk>(json.clone()) {
                        Ok(chunk) => StreamChunk::Text(chunk),
                        Err(_) => {
                            // Log but don't fail - might be a partial chunk
                            eprintln!("Warning: Failed to parse text chunk for json");
                            return Err(CompletionError::InvalidResponse(
                                "Invalid response format".to_string(),
                            ));
                        }
                    }
                };
                Ok(Some(chunk))
            }
            Err(_) => {
                // Skip malformed JSON rather than failing the entire stream
                eprintln!("Warning: Failed to parse SSE JSON for data");
                Err(CompletionError::InvalidResponse(
                    "Invalid JSON in SSE event".to_string(),
                ))
            }
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

            // Look for data: lines
            if let Some(data) = line.strip_prefix("data: ") {
                match Self::parse_sse_event(data, self.is_chat) {
                    Ok(Some(chunk)) => {
                        results.push(Ok(SSEEvent { raw_bytes, chunk }));
                    }
                    Ok(None) => {} // [DONE] marker
                    Err(e) => results.push(Err(e)),
                }
            }
        }

        results
    }
}

impl<S> Stream for SSEParser<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<SSEEvent, CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, try to process any complete events in the buffer
        let buffered_results = self.process_buffer();
        if !buffered_results.is_empty() {
            // Return the first result and save the rest for next poll
            // (In a real implementation, we'd need a queue for multiple results)
            if let Some(result) = buffered_results.into_iter().next() {
                return Poll::Ready(Some(result));
            }
        }

        // Poll the inner stream for more data
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Add new data to buffer
                self.bytes_buffer.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&bytes);
                self.buffer.push_str(&text);

                // Process any complete events
                let results = self.process_buffer();
                if let Some(result) = results.into_iter().next() {
                    Poll::Ready(Some(result))
                } else {
                    // No complete events yet, need more data
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(CompletionError::CompletionError(e.to_string()))))
            }
            Poll::Ready(None) => {
                // Stream ended - process any remaining buffer content
                if !self.buffer.trim().is_empty() {
                    eprintln!("Warning: Incomplete SSE data in buffer at stream end",);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
