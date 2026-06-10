//! Streaming adapter: turn Chutes' end-to-end-encrypted SSE response into the
//! cloud-api [`StreamingResult`] of decrypted OpenAI [`SSEEvent`]s.
//!
//! Chutes streams a *one-encapsulation, many-frames* SSE (see
//! [`super::e2ee`]): a first `data: {"e2e_init": base64(mlkem_ct)}` event keys the
//! stream, then each `data: {"e2e": base64(nonce‖ct‖tag)}` event decrypts (with
//! the single stream key) to one raw OpenAI SSE line. `usage`-only events are
//! billing-side and dropped; `{"e2e_error": ...}` ends the stream with an error;
//! `data: [DONE]` terminates. Chunks are **not** gzipped.
//!
//! Decryption errors / a chunk before `e2e_init` are fatal (the trust chain is
//! the AEAD channel) — they end the stream with an error rather than forwarding
//! anything unauthenticated. EOF without a terminal `data: [DONE]` is also fatal
//! (a truncated stream must not look like a successful completion).
//!
//! **Security note (inherent to Chutes' published protocol):** content frames are
//! each AEAD-sealed under one stream key with random per-frame nonces and **no
//! sequence numbers**, so an on-path gateway can drop, reorder, or replay
//! individual frames without breaking any single frame's AEAD tag. We therefore
//! only accept an **authenticated inner** `[DONE]` (decrypted from an `e2e` frame)
//! as a clean terminus; a *plaintext outer* `[DONE]` is forgeable (the gateway
//! could inject it after dropping frames) and is ignored, so a truncated stream
//! surfaces an error instead of a fake success. Frame *ordering* is still not
//! cryptographically guaranteed. **Open (verify on staging):** confirm Chutes
//! emits the terminator *inside* the encrypted channel; if it only sends the
//! outer plaintext `[DONE]`, the terminator is forgeable — add it to the tracked
//! Chutes asks (alongside missing frame sequence numbers + unsigned measurements)
//! and reconsider exposing streaming as attested.

use async_stream::try_stream;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::e2ee::{ResponseSession, StreamKey};
use crate::{CompletionError, SSEEvent, StreamChunk, StreamingResult};

fn b64(field: &str, s: &str) -> Result<Vec<u8>, CompletionError> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| CompletionError::CompletionError(format!("Chutes stream {field} base64: {e}")))
}

/// The `data: [DONE]` terminator event (no parsed chunk).
fn done_event() -> SSEEvent {
    SSEEvent {
        raw_bytes: Bytes::from_static(b"data: [DONE]\n\n"),
        chunk: None,
        raw_passthrough: true,
    }
}

/// Parse one *decrypted* plaintext frame (a raw OpenAI SSE line, e.g.
/// `data: {chunk}` or bare `{chunk}`) into an [`SSEEvent`]. Returns `None` for an
/// empty frame. Pure — unit-tested without any crypto.
fn inner_event(plaintext: &[u8]) -> Result<Option<SSEEvent>, CompletionError> {
    let s = String::from_utf8_lossy(plaintext);
    let s = s.trim();
    // Tolerate either a full `data: ...` SSE line or a bare JSON payload.
    let content = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
    if content.is_empty() {
        return Ok(None);
    }
    if content == "[DONE]" {
        return Ok(Some(done_event()));
    }
    let chunk: crate::ChatCompletionChunk = serde_json::from_str(content)
        .map_err(|e| CompletionError::CompletionError(format!("Chutes stream chunk parse: {e}")))?;
    Ok(Some(SSEEvent {
        // Hand clients a clean, well-framed OpenAI SSE line.
        raw_bytes: Bytes::from(format!("data: {content}\n\n")),
        chunk: Some(StreamChunk::Chat(chunk)),
        raw_passthrough: true,
    }))
}

/// Dispatch one *outer* Chutes SSE `data:` payload, possibly setting the stream
/// key or yielding a decrypted [`SSEEvent`]. Returns `Ok(Some(event))` to emit,
/// `Ok(None)` to skip (key set / usage / unknown), or `Err` (fatal).
fn handle_outer_payload(
    payload: &str,
    session: &ResponseSession,
    stream_key: &mut Option<StreamKey>,
) -> Result<Option<SSEEvent>, CompletionError> {
    if payload == "[DONE]" {
        // A *plaintext* outer `[DONE]` comes from the untrusted gateway and is
        // forgeable — it could be injected after dropping the remaining encrypted
        // frames to fake a successful-but-truncated stream. Ignore it: only the
        // **authenticated inner** `[DONE]` (decrypted from an `e2e` frame, handled
        // in `inner_event`) is a valid terminus. If no inner `[DONE]` arrives, the
        // stream ends as truncation. (If staging shows Chutes terminates only with
        // the outer `[DONE]`, that's a forgeable terminator — see the module note.)
        return Ok(None);
    }
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return Ok(None), // non-JSON control line — skip
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Ok(None),
    };

    if let Some(init) = obj.get("e2e_init").and_then(|x| x.as_str()) {
        let ct = b64("e2e_init", init)?;
        *stream_key =
            Some(session.stream_key(&ct).map_err(|e| {
                CompletionError::CompletionError(format!("Chutes stream key: {e}"))
            })?);
        Ok(None)
    } else if let Some(e2e) = obj.get("e2e").and_then(|x| x.as_str()) {
        let key = stream_key.as_ref().ok_or_else(|| {
            CompletionError::CompletionError("Chutes stream: e2e chunk before e2e_init".to_string())
        })?;
        let frame = b64("e2e", e2e)?;
        let plaintext = key.decrypt_chunk(&frame).map_err(|e| {
            CompletionError::CompletionError(format!("Chutes stream chunk decrypt: {e}"))
        })?;
        inner_event(&plaintext)
    } else if let Some(err) = obj.get("e2e_error").and_then(|x| x.as_str()) {
        Err(CompletionError::CompletionError(format!(
            "Chutes stream error: {err}"
        )))
    } else {
        // usage-only billing event or anything else — skip.
        Ok(None)
    }
}

/// Decrypt a Chutes E2EE SSE byte stream into a [`StreamingResult`]. Generic over
/// the byte source so it can be unit-tested with a synthetic stream; the provider
/// passes `response.bytes_stream()` (errors pre-mapped to [`CompletionError`]).
pub fn decrypt_e2ee_sse<S>(byte_stream: S, session: ResponseSession) -> StreamingResult
where
    S: Stream<Item = Result<Bytes, CompletionError>> + Unpin + Send + 'static,
{
    let s = try_stream! {
        let mut byte_stream = byte_stream;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream_key: Option<StreamKey> = None;

        while let Some(next) = byte_stream.next().await {
            let chunk = next?;
            buf.extend_from_slice(&chunk);

            // Process complete '\n'-terminated lines (the gateway reframes SSE
            // line-by-line; each event is a single `data:` line here).
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                // Take the line out (owned) before draining, so we don't hold a
                // borrow across the buffer mutation; one allocation per line.
                let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
                buf.drain(..=pos);
                let line = line.trim();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let Some(payload) = line.strip_prefix("data:") else {
                    continue; // ignore non-data SSE fields (event:, id:, ...)
                };
                let payload = payload.trim();
                if let Some(event) = handle_outer_payload(payload, &session, &mut stream_key)? {
                    if event.is_done_marker() {
                        yield event;
                        return; // clean terminus
                    }
                    yield event;
                }
            }
        }

        // Reached only on EOF *without* a terminal `[DONE]` (or with a dangling
        // partial line) — a truncated/interrupted encrypted stream. Fail closed,
        // so the route layer does not mint its own `[DONE]` and present a
        // truncated completion as successful.
        Err(CompletionError::CompletionError(
            "Chutes E2EE stream ended without a terminal [DONE] (truncated or interrupted)"
                .to_string(),
        ))?;
    };
    Box::pin(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attested::chutes::e2ee::build_request;
    use ml_kem::kem::{Kem, KeyExport};
    use ml_kem::MlKem768;

    fn fresh_session() -> ResponseSession {
        // A valid instance pubkey so build_request succeeds; we only exercise the
        // non-crypto control paths ([DONE], e2e_error) with the returned session.
        let (_dk, ek) = MlKem768::generate_keypair();
        let pk = ek.to_bytes();
        build_request(pk.as_slice(), &serde_json::json!({"model": "m"}))
            .unwrap()
            .session
    }

    fn synthetic(lines: &[&str]) -> impl Stream<Item = Result<Bytes, CompletionError>> + Unpin {
        let body = lines.join("");
        futures_util::stream::iter(vec![Ok(Bytes::from(body))])
    }

    #[test]
    fn inner_event_parses_data_prefixed_chunk() {
        let line = b"data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[]}";
        let ev = inner_event(line).unwrap().unwrap();
        assert!(matches!(ev.chunk, Some(StreamChunk::Chat(_))));
        assert!(ev.raw_passthrough);
        assert!(ev.raw_bytes.starts_with(b"data: "));
    }

    #[test]
    fn inner_event_parses_bare_json_chunk() {
        let line = b"{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[]}";
        assert!(inner_event(line).unwrap().is_some());
    }

    #[test]
    fn inner_event_done_and_empty() {
        assert!(inner_event(b"data: [DONE]")
            .unwrap()
            .unwrap()
            .is_done_marker());
        assert!(inner_event(b"   ").unwrap().is_none());
    }

    #[tokio::test]
    async fn outer_plaintext_done_is_truncation_not_success() {
        // A bare plaintext outer [DONE] from the (untrusted) gateway must NOT be
        // accepted as a clean terminus — only an authenticated inner [DONE] is.
        // With no inner [DONE], this is a truncated stream → error.
        let st = synthetic(&["data: [DONE]\n\n"]);
        let mut out = decrypt_e2ee_sse(st, fresh_session());
        let err = out.next().await.unwrap().unwrap_err();
        assert!(format!("{err}").contains("without a terminal [DONE]"));
    }

    #[tokio::test]
    async fn stream_surfaces_e2e_error() {
        let st = synthetic(&["data: {\"e2e_error\":\"backend exploded\"}\n\n"]);
        let mut out = decrypt_e2ee_sse(st, fresh_session());
        let err = out.next().await.unwrap().unwrap_err();
        assert!(format!("{err}").contains("backend exploded"));
    }

    #[tokio::test]
    async fn stream_skips_usage_only_events() {
        // usage-only events are dropped (not yielded as content); with no inner
        // [DONE] the stream then ends as truncation — so the first (and only)
        // item is the truncation error, never a content event.
        let st = synthetic(&[
            "data: {\"usage\":{\"prompt_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let mut out = decrypt_e2ee_sse(st, fresh_session());
        let err = out.next().await.unwrap().unwrap_err();
        assert!(format!("{err}").contains("without a terminal [DONE]"));
    }

    #[tokio::test]
    async fn stream_without_done_is_error() {
        // A stream that ends without a terminal [DONE] is a truncation — must
        // surface an error, not end cleanly (which would look like success).
        let st = synthetic(&["data: {\"usage\":{\"prompt_tokens\":1}}\n\n"]);
        let mut out = decrypt_e2ee_sse(st, fresh_session());
        let err = out.next().await.unwrap().unwrap_err();
        assert!(format!("{err}").contains("without a terminal [DONE]"));
    }

    #[tokio::test]
    async fn stream_rejects_e2e_chunk_before_init() {
        let st = synthetic(&["data: {\"e2e\":\"QUJD\"}\n\n"]);
        let mut out = decrypt_e2ee_sse(st, fresh_session());
        let err = out.next().await.unwrap().unwrap_err();
        assert!(format!("{err}").contains("before e2e_init"));
    }
}
