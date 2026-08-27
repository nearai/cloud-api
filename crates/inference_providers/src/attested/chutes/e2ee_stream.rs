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
//! cryptographically guaranteed.
//!
//! **Terminator confirmed (2026-06-11):** a live round-trip against GLM-5.1-TEE
//! (see the `live_chutes_streaming_done_probe` test in `super`) showed Chutes
//! emits the terminator *inside* the encrypted channel — the stream ends on an
//! authenticated inner `[DONE]`. This narrowly establishes that an EOF *without*
//! an inner `[DONE]` is detected (surfaced as a truncation error). It does **not**
//! make streaming tamper-evident: because content frames carry no sequence
//! numbers, an on-path gateway can still **drop** encrypted content frames and
//! forward the (authenticated) inner `[DONE]`, or **reorder** / **replay** frames,
//! and the decoder cannot detect any of it. So drop, reorder, replay, and
//! truncation-*with*-a-forwarded-inner-`[DONE]` all remain **undetectable** until
//! Chutes adds sequence numbers or a transcript MAC — these stay on the tracked
//! Chutes asks. The only guarantee added here is "no inner `[DONE]` ⇒ error".

use async_stream::try_stream;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::e2ee::{ResponseSession, StreamKey};
use crate::{CompletionError, SSEEvent, StreamChunk, StreamingResult};

/// Upper bound on a single SSE line (one `data:` event) before a newline — caps
/// unbounded buffer growth from a stalled/hostile gateway.
const MAX_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;

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
fn inner_event(
    plaintext: &[u8],
    synthetic_stream_id: &str,
) -> Result<Option<SSEEvent>, CompletionError> {
    let s = String::from_utf8_lossy(plaintext);
    let s = s.trim();
    // An SSE comment / keepalive line (e.g. `: ping`) — vLLM/SGLang backends emit
    // these, so a decrypted frame can legitimately be one. Skip it: feeding `:
    // ping` to the JSON parser below would be a fatal error that kills an
    // otherwise-healthy stream. (The outer loop skips these too, line ~177.)
    if s.is_empty() || s.starts_with(':') {
        return Ok(None);
    }
    // Tolerate either a full `data: ...` SSE line or a bare JSON payload.
    let content = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
    if content.is_empty() {
        return Ok(None);
    }
    if content == "[DONE]" {
        return Ok(Some(done_event()));
    }
    let mut chunk_json: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| CompletionError::CompletionError(format!("Chutes stream chunk parse: {e}")))?;
    let needs_synthetic_id = if let Some(object) = chunk_json.as_object_mut() {
        let needs_synthetic_id = match object.get("id") {
            None => true,
            Some(serde_json::Value::String(id)) => id.is_empty(),
            Some(_) => false,
        };
        if needs_synthetic_id {
            object.insert(
                "id".to_string(),
                serde_json::Value::String(synthetic_stream_id.to_string()),
            );
        }
        needs_synthetic_id
    } else {
        false
    };
    let normalized_content;
    let raw_content = if needs_synthetic_id {
        normalized_content = serde_json::to_string(&chunk_json).map_err(|e| {
            CompletionError::CompletionError(format!("Chutes stream chunk parse: {e}"))
        })?;
        normalized_content.as_str()
    } else {
        content
    };
    let chunk: crate::ChatCompletionChunk = serde_json::from_value(chunk_json)
        .map_err(|e| CompletionError::CompletionError(format!("Chutes stream chunk parse: {e}")))?;
    Ok(Some(SSEEvent {
        // Hand clients a clean, well-framed OpenAI SSE line.
        raw_bytes: Bytes::from(format!("data: {raw_content}\n\n")),
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
        // stream ends as truncation. Chutes is confirmed to emit the inner
        // terminator (see the module note), so ignoring the outer one is safe.
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
        // Reject a second e2e_init: ML-KEM decapsulation never fails (implicit
        // rejection), so silently re-keying would let an on-path gateway inject a
        // bogus e2e_init that makes all subsequent genuine frames fail to decrypt.
        // One key per stream — fail clearly instead.
        if stream_key.is_some() {
            return Err(CompletionError::CompletionError(
                "Chutes stream: unexpected second e2e_init (possible on-path tampering)"
                    .to_string(),
            ));
        }
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
        inner_event(&plaintext, session.synthetic_stream_id())
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

            // Bound the line buffer: a hostile/buggy gateway streaming bytes with
            // no newline must not grow it without limit. One SSE event here is a
            // single `data:` line; cap generously.
            if buf.len() > MAX_SSE_LINE_BYTES {
                Err(CompletionError::CompletionError(format!(
                    "Chutes SSE line exceeds {MAX_SSE_LINE_BYTES} bytes without a newline"
                )))?;
            }

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

        // Reached only on EOF *without* an authenticated inner `[DONE]` (or with a
        // dangling partial line) — a truncated/interrupted encrypted stream. We
        // yield an error here; the route layer does append its own gateway-minted
        // `[DONE]` after the stream (completions.rs), but that's emitted *after*
        // this error frame, so the client still sees the failure first and the
        // truncated content is not presented as a clean success.
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

    const SYNTHETIC_STREAM_ID: &str = "chutes-gateway-test";

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
        let ev = inner_event(line, SYNTHETIC_STREAM_ID).unwrap().unwrap();
        assert!(matches!(ev.chunk, Some(StreamChunk::Chat(_))));
        assert!(ev.raw_passthrough);
        assert!(ev.raw_bytes.starts_with(b"data: "));
    }

    #[test]
    fn inner_event_preserves_raw_bytes_when_id_is_present() {
        // Given: a valid provider frame whose key order and whitespace differ
        // from serde_json's normalized representation.
        let frame = concat!(
            "data: ",
            r#"{"model": "m", "id": "provider-id", "object": "chat.completion.chunk", "created": 0, "choices": []}"#,
            "\n\n"
        );

        // When: the frame crosses the Chutes parser without needing an id.
        let event = inner_event(frame.as_bytes(), SYNTHETIC_STREAM_ID)
            .unwrap()
            .unwrap();

        // Then: passthrough bytes remain exactly as the provider emitted them.
        assert_eq!(event.raw_bytes.as_ref(), frame.as_bytes());
    }

    #[test]
    fn inner_event_parses_bare_json_chunk() {
        let line = b"{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[]}";
        assert!(inner_event(line, SYNTHETIC_STREAM_ID).unwrap().is_some());
    }

    #[test]
    fn inner_event_accepts_missing_id_and_normalizes_raw_bytes() {
        // Given: a valid Chutes frame whose provider-specific shape omits `id`.
        let frame = concat!(
            "data: ",
            r#"{"object":"chat.completion.chunk","created":0,"model":"m","choices":[]}"#,
            "\n\n"
        );

        // When: the decrypted frame crosses the Chutes-only parser boundary.
        let event = inner_event(frame.as_bytes(), SYNTHETIC_STREAM_ID)
            .unwrap()
            .unwrap();

        // Then: the typed and reconstructed raw representations are semantically
        // equivalent and carry the same injected stream id.
        let Some(StreamChunk::Chat(chunk)) = event.chunk else {
            panic!("expected a parsed chat chunk");
        };
        assert_eq!(chunk.id, SYNTHETIC_STREAM_ID);

        let raw_line = std::str::from_utf8(&event.raw_bytes).unwrap();
        let raw: serde_json::Value = serde_json::from_str(
            raw_line
                .strip_prefix("data: ")
                .expect("normalized event is a framed SSE data line")
                .trim_end(),
        )
        .unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(
            frame
                .strip_prefix("data: ")
                .expect("fixture is a framed SSE data line")
                .trim_end(),
        )
        .unwrap();
        expected["id"] = serde_json::Value::String(SYNTHETIC_STREAM_ID.to_string());
        assert_eq!(raw, expected);
    }

    #[test]
    fn inner_event_replaces_empty_id_in_raw_and_typed_chunk() {
        // Given: a valid Chutes frame whose provider id is an empty string.
        let frame =
            br#"{"id":"","object":"chat.completion.chunk","created":0,"model":"m","choices":[]}"#;

        // When: the decrypted frame crosses the Chutes-only parser boundary.
        let event = inner_event(frame, SYNTHETIC_STREAM_ID).unwrap().unwrap();

        // Then: both emitted representations carry the same synthetic id.
        let raw: serde_json::Value = serde_json::from_slice(
            event
                .raw_bytes
                .strip_prefix(b"data: ")
                .expect("normalized event is a framed SSE data line"),
        )
        .unwrap();
        let Some(StreamChunk::Chat(chunk)) = event.chunk else {
            panic!("expected a parsed chat chunk");
        };
        assert_eq!(raw["id"], SYNTHETIC_STREAM_ID);
        assert_eq!(chunk.id, SYNTHETIC_STREAM_ID);
    }

    #[test]
    fn missing_id_raw_and_typed_round_trip_sanitizes_provider_fields() {
        // Given: an id-less provider frame carrying fields that the Chutes
        // client-facing allowlist must remove.
        let frame = br#"{"object":"chat.completion.chunk","created":0,"model":"provider/model","prompt_token_ids":[1,2],"prompt_sha256":"secret","choices":[]}"#;
        let session = fresh_session();
        let event = inner_event(frame, session.synthetic_stream_id())
            .unwrap()
            .unwrap();

        // When: the downstream Chutes rewrite sanitizes and canonicalizes it.
        let rewritten =
            super::super::rewrite_sse_event_model(event, Some("canonical/model"), false);

        // Then: both client-facing representations carry the same sanitized,
        // canonical shape. Re-serialized typed chunks are used on route paths
        // that cannot forward raw bytes, so provider internals must be absent.
        let Some(StreamChunk::Chat(chunk)) = &rewritten.chunk else {
            panic!("expected a rewritten chat chunk");
        };
        let typed = serde_json::to_value(chunk).unwrap();
        assert!(
            typed.get("prompt_token_ids").is_none(),
            "typed chunk must not re-expose provider prompt_token_ids"
        );
        assert!(
            typed.get("prompt_sha256").is_none(),
            "typed chunk must not re-expose provider prompt_sha256"
        );
        assert_eq!(typed["model"], "canonical/model");

        let raw_line = std::str::from_utf8(&rewritten.raw_bytes).unwrap();
        let raw: serde_json::Value = serde_json::from_str(
            raw_line
                .strip_prefix("data: ")
                .expect("rewritten event is a framed SSE data line")
                .trim_end(),
        )
        .unwrap();
        assert_eq!(raw.get("id"), typed.get("id"));
        assert!(
            raw["id"].as_str().is_some_and(|id| !id.is_empty()),
            "missing provider id must be replaced before emitting the event"
        );
    }

    #[test]
    fn missing_id_streams_use_stable_unique_synthetic_ids() {
        // Given: two id-less frames from one completion and another id-less
        // frame from a different completion.
        let first_session = fresh_session();
        let second_session = fresh_session();
        let parse_id = |frame: &[u8], session: &ResponseSession| {
            let event = inner_event(frame, session.synthetic_stream_id())
                .unwrap()
                .unwrap();
            let Some(StreamChunk::Chat(chunk)) = event.chunk else {
                panic!("expected a parsed chat chunk");
            };
            chunk.id
        };

        // When: every frame crosses the Chutes parser boundary.
        let first_id = parse_id(
            br#"{"object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"a"}}]}"#,
            &first_session,
        );
        let next_id = parse_id(
            br#"{"object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"b"}}]}"#,
            &first_session,
        );
        let other_id = parse_id(
            br#"{"object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"c"}}]}"#,
            &second_session,
        );

        // Then: identity is visibly gateway-synthetic, stable within one stream,
        // and unique across completions.
        assert!(
            first_id.starts_with("chutes-gateway-"),
            "missing provider id must be visibly gateway-synthetic"
        );
        assert_eq!(first_id, next_id, "one stream must keep one chat id");
        assert_ne!(
            first_id, other_id,
            "different completions must not collide in the signature key"
        );
    }

    #[test]
    fn inner_event_preserves_unknown_top_level_fields() {
        // Given: a valid provider frame with a field outside the shared schema.
        let frame = br#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"prompt_text":null}"#;

        // When: the frame is parsed at the Chutes boundary.
        let event = inner_event(frame, SYNTHETIC_STREAM_ID).unwrap().unwrap();

        // Then: the shared chunk's flatten map preserves the provider field.
        let Some(StreamChunk::Chat(chunk)) = event.chunk else {
            panic!("expected a parsed chat chunk");
        };
        assert_eq!(
            chunk.extra.get("prompt_text"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn inner_event_rejects_missing_choices() {
        // Given: JSON that is not a valid completion chunk because `choices` is absent.
        let frame = br#"{"id":"x","object":"chat.completion.chunk","created":0,"model":"m"}"#;

        // When: it crosses the Chutes parser boundary.
        let error = inner_event(frame, SYNTHETIC_STREAM_ID).unwrap_err();

        // Then: tolerance for a missing id does not weaken required completion data.
        assert!(format!("{error}").contains("missing field `choices`"));
    }

    #[test]
    fn inner_event_done_and_empty() {
        assert!(inner_event(b"data: [DONE]", SYNTHETIC_STREAM_ID)
            .unwrap()
            .unwrap()
            .is_done_marker());
        assert!(inner_event(b"   ", SYNTHETIC_STREAM_ID).unwrap().is_none());
    }

    #[test]
    fn inner_event_skips_keepalive_comment() {
        // A decrypted SSE comment / keepalive line (vLLM/SGLang emit these) must
        // be skipped, not fed to the JSON parser — otherwise a healthy stream dies
        // with a parse error mid-flight.
        assert!(inner_event(b": ping", SYNTHETIC_STREAM_ID)
            .unwrap()
            .is_none());
        assert!(inner_event(b":keepalive", SYNTHETIC_STREAM_ID)
            .unwrap()
            .is_none());
        // Still a fatal error for genuinely non-JSON data content.
        assert!(inner_event(b"data: not json", SYNTHETIC_STREAM_ID).is_err());
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
