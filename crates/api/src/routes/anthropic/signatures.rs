//! Gateway receipts for native Messages, without re-encoding JSON or SSE.

use super::{AnthropicRawBody, AnthropicRawError, Bytes};
use futures_util::StreamExt;
use services::attestation::ports::AttestationServiceTrait;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

const SIGNATURE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENT_BYTES: usize = 256 * 1024;

pub(super) fn body_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn valid_message_id(id: &str) -> bool {
    // provider_request_id is VARCHAR(255). Keep IDs opaque; never substitute
    // an HTTP request ID or a newly generated response ID for a missing one.
    !id.is_empty() && id.len() <= 255 && !id.chars().any(char::is_control)
}

pub(super) struct MessageSigningContext {
    service: Arc<dyn AttestationServiceTrait>,
    request_hash: String,
}

impl MessageSigningContext {
    pub(super) fn new(service: Arc<dyn AttestationServiceTrait>, request: &[u8]) -> Self {
        Self {
            service,
            request_hash: body_hash(request),
        }
    }

    pub(super) async fn store(self, id: String, response_hash: String) {
        // Native Messages never pins a provider connection: the gateway signs
        // these bytes. Bound the database write as well as signature creation.
        let result = tokio::time::timeout(
            SIGNATURE_TIMEOUT,
            self.service
                .store_chat_signature(&id, self.request_hash, response_hash),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::error!(message_id = %id, "Native Anthropic gateway signature storage failed or timed out");
        }
    }
}

pub(super) fn signed_stream(
    inner: AnthropicRawBody,
    signing: MessageSigningContext,
) -> AnthropicRawBody {
    Box::pin(futures_util::stream::unfold(
        SignedStream::new(inner, signing),
        |mut state| async move { state.next().await.map(|item| (item, state)) },
    ))
}

struct SignedStream {
    inner: Option<AnthropicRawBody>,
    signing: Option<MessageSigningContext>,
    response_hash: Sha256,
    message_id: Option<String>,
    frame: Vec<u8>,
    pending: Bytes,
    pending_error: Option<AnthropicRawError>,
    line_len: usize,
    last_was_cr: bool,
    eligible: bool,
    passthrough: bool,
    done: bool,
}

impl SignedStream {
    fn new(inner: AnthropicRawBody, signing: MessageSigningContext) -> Self {
        Self {
            inner: Some(inner),
            signing: Some(signing),
            response_hash: Sha256::new(),
            message_id: None,
            frame: Vec::new(),
            pending: Bytes::new(),
            pending_error: None,
            line_len: 0,
            last_was_cr: false,
            eligible: true,
            passthrough: false,
            done: false,
        }
    }

    async fn next(&mut self) -> Option<Result<Bytes, AnthropicRawError>> {
        loop {
            if self.done {
                return None;
            }
            if let Some(error) = self.pending_error.take() {
                self.done = true;
                return Some(Err(error));
            }
            if !self.pending.is_empty() {
                if self.passthrough {
                    return Some(Ok(std::mem::take(&mut self.pending)));
                }
                // Frame SSE by blank lines, accepting LF and CRLF even when
                // either delimiter is split across network chunks. Keep the
                // original bytes; parsing is only for ID/completion tracking.
                for index in 0..self.pending.len() {
                    if self.frame.len() + index + 1 > MAX_EVENT_BYTES {
                        self.eligible = false;
                        self.passthrough = true;
                        self.signing.take();
                        self.frame.extend_from_slice(&self.pending.split_to(index));
                        tracing::warn!("Native Anthropic oversized SSE event cannot be signed");
                        return Some(Ok(Bytes::from(std::mem::take(&mut self.frame))));
                    }
                    let byte = self.pending[index];
                    let boundary = byte == b'\n'
                        && (self.line_len == 0 || (self.line_len == 1 && self.last_was_cr));
                    if byte == b'\n' {
                        self.line_len = 0;
                    } else {
                        self.line_len += 1;
                    }
                    self.last_was_cr = byte == b'\r';
                    if boundary {
                        self.frame
                            .extend_from_slice(&self.pending.split_to(index + 1));
                        let frame = Bytes::from(std::mem::take(&mut self.frame));
                        self.response_hash.update(&frame);
                        if self.observe_event(&frame) {
                            // message_stop is the protocol end. Do not poll
                            // for transport EOF (which may never arrive), or
                            // forward trailing bytes after the signed end.
                            self.done = true;
                            self.pending = Bytes::new();
                            self.inner.take();
                            if self.eligible {
                                if let (Some(signing), Some(id)) =
                                    (self.signing.take(), self.message_id.take())
                                {
                                    let hash = hex::encode(self.response_hash.clone().finalize());
                                    // Detaching on client cancellation lets a
                                    // complete response finish its bounded
                                    // receipt write. Partial streams never
                                    // schedule a receipt.
                                    if tokio::spawn(signing.store(id, hash)).await.is_err() {
                                        tracing::error!("Native Anthropic signature task failed");
                                    }
                                }
                            }
                        }
                        return Some(Ok(frame));
                    }
                }
                self.frame.extend_from_slice(&self.pending);
                self.pending = Bytes::new();
            }
            match self.inner.as_mut()?.next().await {
                Some(Ok(bytes)) => self.pending = bytes,
                Some(Err(error)) => {
                    self.pending_error = Some(error);
                    if !self.frame.is_empty() {
                        return Some(Ok(Bytes::from(std::mem::take(&mut self.frame))));
                    }
                }
                None => {
                    self.done = true;
                    // EOF alone is not successful Messages completion. Flush
                    // an incomplete event verbatim, without issuing a receipt.
                    return (!self.frame.is_empty())
                        .then(|| Ok(Bytes::from(std::mem::take(&mut self.frame))));
                }
            }
        }
    }

    /// Returns true only for a complete message_stop event. Unknown events
    /// and comments are preserved; invalid control data disables signing.
    fn observe_event(&mut self, frame: &[u8]) -> bool {
        let mut data = Vec::new();
        let mut event_name = None;
        for line in frame.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(value) = line.strip_prefix(b"data:") {
                if !data.is_empty() {
                    data.push(b'\n');
                }
                data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
            } else if let Some(value) = line.strip_prefix(b"event:") {
                event_name = Some(value.strip_prefix(b" ").unwrap_or(value));
            }
        }
        if data.is_empty() {
            return false;
        }
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(&data) else {
            self.eligible = false;
            return false;
        };
        let kind = event.get("type").and_then(serde_json::Value::as_str);
        if event_name.is_some_and(|name| Some(name) != kind.map(str::as_bytes)) {
            self.eligible = false;
            return false;
        }
        match kind {
            Some("message_start") => {
                let id = event
                    .pointer("/message/id")
                    .and_then(serde_json::Value::as_str);
                if self.message_id.is_some() || !id.is_some_and(valid_message_id) {
                    self.eligible = false;
                } else {
                    self.message_id = id.map(str::to_string);
                }
            }
            Some("error") => self.eligible = false,
            Some("message_stop") => return true,
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests;
