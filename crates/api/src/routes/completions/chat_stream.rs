use super::*;

/// Returns a safe-to-log category string for a stream-level completion error.
pub(super) fn completion_stream_error_category(
    e: &inference_providers::CompletionError,
) -> &'static str {
    match e {
        inference_providers::CompletionError::CompletionError(_) => "completion_error",
        inference_providers::CompletionError::HttpError { .. } => "http_error",
        inference_providers::CompletionError::InvalidResponse(_) => "invalid_response",
        inference_providers::CompletionError::NoPubKeyProvider(_) => "stale_pubkey",
        inference_providers::CompletionError::Unknown(_) => "unknown",
        inference_providers::CompletionError::ClientMediaError(_) => "client_media_error",
        inference_providers::CompletionError::Timeout { .. } => "timeout",
    }
}

/// Returns an OpenAI-compatible `error.type` for a stream-level completion error.
/// Used in the `data: {"error":{...}}` SSE frame so clients can branch on the type.
///
/// Match is intentionally exhaustive (no `_` arm): a new `CompletionError`
/// variant must force a compile error here so the OpenAI mapping is reviewed
/// explicitly instead of silently defaulting to `server_error`.
pub(super) fn completion_stream_error_openai_type(
    e: &inference_providers::CompletionError,
) -> &'static str {
    match e {
        inference_providers::CompletionError::HttpError { status_code, .. } => match *status_code {
            429 => "rate_limit_exceeded",
            400..=499 => "invalid_request_error",
            _ => "server_error",
        },
        // Client supplied an unfetchable/undecodable image or video: a 400-class
        // bad-input error, surfaced to the client as invalid_request_error to
        // match the non-stream path (map_provider_error -> InvalidParams -> 400).
        inference_providers::CompletionError::ClientMediaError(_) => "invalid_request_error",
        inference_providers::CompletionError::CompletionError(_)
        | inference_providers::CompletionError::InvalidResponse(_)
        | inference_providers::CompletionError::Unknown(_)
        | inference_providers::CompletionError::NoPubKeyProvider(_)
        | inference_providers::CompletionError::Timeout { .. } => "server_error",
    }
}

/// Build an OpenAI-compatible SSE error frame.
///
/// Format: `data: {"error":{"message":"...","type":"..."}}\n\n`. Replaces the
/// historical `data: error: <msg>\n\n` shape that was not valid JSON and broke
/// clients (opencode, vercel/ai-sdk) parsing the `data:` payload as JSON.
pub(super) fn sse_error_frame(e: &inference_providers::CompletionError) -> Bytes {
    let payload = serde_json::json!({
        "error": {
            "message": e.to_string(),
            "type": completion_stream_error_openai_type(e),
        }
    });
    Bytes::from(format!("data: {payload}\n\n"))
}

pub(super) fn chat_stream_options(
    request: &ChatCompletionRequest,
) -> Option<inference_providers::models::StreamOptions> {
    request
        .extra
        .get("stream_options")
        .cloned()
        .and_then(|stream_options| {
            serde_json::from_value::<inference_providers::models::StreamOptions>(stream_options)
                .ok()
        })
}

pub(super) fn chat_stream_include_usage_requested(request: &ChatCompletionRequest) -> bool {
    chat_stream_options(request)
        .and_then(|stream_options| stream_options.include_usage)
        .unwrap_or(false)
}

pub(super) fn chat_stream_continuous_usage_requested(request: &ChatCompletionRequest) -> bool {
    chat_stream_options(request)
        .and_then(|stream_options| stream_options.continuous_usage_stats)
        .unwrap_or(false)
}

pub(super) fn chat_stream_has_non_text_modalities(request: &ChatCompletionRequest) -> bool {
    request
        .extra
        .get("modalities")
        .and_then(|modalities| serde_json::from_value::<Vec<String>>(modalities.clone()).ok())
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|modality| !modality.eq_ignore_ascii_case("text"))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChatStreamUsageMode {
    pub(super) rewrite_public_stream_usage: bool,
    pub(super) gateway_signature_enabled: bool,
    /// Strip `usage` from intermediate chunks without adding a final usage chunk.
    /// Active for the default case (no `include_usage`) and `include_usage: false`.
    /// Per OpenAI spec, intermediate chunks must carry `usage: null`; populated
    /// usage only appears in the one synthetic final chunk emitted when
    /// `include_usage: true` is explicitly requested.
    pub(super) strip_intermediate_usage: bool,
}

pub(super) fn chat_stream_usage_mode(
    request: &ChatCompletionRequest,
    model_attestation_supported: Option<bool>,
    e2ee_active: bool,
) -> ChatStreamUsageMode {
    let rewrite_public_stream_usage = request.stream == Some(true)
        && chat_stream_include_usage_requested(request)
        && !chat_stream_continuous_usage_requested(request)
        && model_attestation_supported.is_some()
        && !e2ee_active
        && !chat_stream_has_non_text_modalities(request);

    // For default streaming (no include_usage or include_usage:false), strip populated
    // usage from all chunks so intermediate ones carry `usage: null` per OpenAI spec,
    // and drop the upstream's trailing usage-only chunk entirely (OpenAI emits no
    // final usage chunk when include_usage wasn't requested; forwarding it breaks
    // strict SDK parsers — the same defect class as the Chutes husk fix, #870).
    //
    // Applied for BOTH non-attested and attested models. Self-hosted upstreams
    // always emit per-chunk usage because the nearai provider forces
    // `stream_options: {include_usage: true, continuous_usage_stats: true}` for
    // billing capture, so without this gate every chunk of an attested stream
    // carries a populated `usage` object the client never asked for, plus a
    // trailing `choices: []` usage-only chunk. Stripping re-serializes the chunks,
    // so byte-exact provider-signature verification no longer applies; for attested
    // models the gateway signature covers the rewritten client-facing bytes instead
    // (`gateway_signature_enabled` below), exactly like the
    // `rewrite_public_stream_usage` path does for `include_usage: true`. Internal
    // usage capture is unaffected: InterceptStream records usage from the parsed
    // chunks in the service layer, upstream of this route-level rewrite.
    //
    // Skipped when: model metadata is unavailable (`None` — fall back to byte-exact
    // passthrough rather than guessing), the client requested continuous stats
    // (explicit per-chunk usage), E2EE is active (chunks are opaque), non-text
    // modalities are in use, `rewrite_public_stream_usage` already handles usage
    // shaping, or the client explicitly requested `include_usage:true`.
    let strip_intermediate_usage = request.stream == Some(true)
        && model_attestation_supported.is_some()
        && !rewrite_public_stream_usage
        && !chat_stream_include_usage_requested(request)
        && !chat_stream_continuous_usage_requested(request)
        && !e2ee_active
        && !chat_stream_has_non_text_modalities(request);

    ChatStreamUsageMode {
        rewrite_public_stream_usage,
        // Both rewrite modes re-serialize the client-facing bytes, so for attested
        // models the provider's byte-exact signature can no longer match what the
        // client received. Skip the provider signature fetch and sign the rewritten
        // stream at the gateway instead.
        gateway_signature_enabled: (rewrite_public_stream_usage || strip_intermediate_usage)
            && model_attestation_supported.unwrap_or(false),
        strip_intermediate_usage,
    }
}

pub(super) fn auto_redact_requires_gateway_signature(
    auto_redact_enabled: bool,
    model_attestation_supported: Option<bool>,
) -> bool {
    auto_redact_enabled && model_attestation_supported.unwrap_or(false)
}

pub(super) fn synthesized_done_requires_gateway_signature(
    model_attestation_supported: Option<bool>,
    e2ee_active: bool,
) -> bool {
    // E2EE response bytes are opaque to the gateway. Keep their signature
    // behavior independent of whether an upstream happens to omit `[DONE]`.
    model_attestation_supported == Some(true) && !e2ee_active
}

pub(super) fn defer_upstream_terminal(
    gateway_signature_enabled: bool,
    public_response_rewritten: bool,
    rewrite_public_stream_usage: bool,
    strip_intermediate_usage: bool,
) -> bool {
    !gateway_signature_enabled
        && !public_response_rewritten
        && !rewrite_public_stream_usage
        && !strip_intermediate_usage
}

/// Append bytes held until provider-signature finalization without exceeding
/// the small terminal suffix bound. Returns false without mutating `held` if
/// the new bytes would exceed the cap.
pub(super) fn try_append_held_provider_terminal_bytes(held: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let Some(total_len) = held.len().checked_add(bytes.len()) else {
        return false;
    };
    if total_len > MAX_HELD_PROVIDER_TERMINAL_BYTES {
        return false;
    }

    held.extend_from_slice(bytes);
    true
}

#[cfg(test)]
pub(super) fn prepare_chat_stream_chunk_for_client(
    chunk: &mut inference_providers::models::ChatCompletionChunk,
    include_usage: bool,
) -> bool {
    let mut final_usage = None;
    prepare_chat_stream_chunk_for_client_with_state(chunk, include_usage, &mut final_usage)
}

pub(super) fn prepare_chat_stream_chunk_for_client_with_state(
    chunk: &mut inference_providers::models::ChatCompletionChunk,
    include_usage: bool,
    final_usage: &mut Option<inference_providers::models::TokenUsage>,
) -> bool {
    let is_usage_only_chunk = chunk.choices.is_empty() && chunk.usage.is_some();
    if !include_usage && is_usage_only_chunk {
        return false;
    }

    if include_usage {
        if let Some(usage) = chunk.usage.take() {
            *final_usage = Some(usage);
        }
    }

    if !include_usage {
        chunk.usage = None;
        return true;
    }

    if is_usage_only_chunk {
        return false;
    }

    true
}

pub(super) fn prepare_stream_chunk_for_client(
    chunk: &mut inference_providers::StreamChunk,
    include_usage: bool,
    final_usage: &mut Option<inference_providers::models::TokenUsage>,
) -> bool {
    match chunk {
        inference_providers::StreamChunk::Chat(chat) => {
            prepare_chat_stream_chunk_for_client_with_state(chat, include_usage, final_usage)
        }
        inference_providers::StreamChunk::Text(_) => true,
    }
}

pub(super) fn rewritten_control_event_bytes(
    event: &inference_providers::SSEEvent,
) -> Option<Bytes> {
    if event.is_done_marker() {
        return None;
    }

    let line = String::from_utf8_lossy(&event.raw_bytes);
    if line.trim_start().starts_with(':') {
        let mut bytes = event.raw_bytes.to_vec();
        if bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        } else {
            bytes.extend_from_slice(b"\n\n");
        }
        Some(Bytes::from(bytes))
    } else {
        None
    }
}

pub(super) fn build_final_usage_chunk_bytes(
    mut usage: inference_providers::TokenUsage,
    template: &ChunkTemplate,
) -> Result<Option<Bytes>, serde_json::Error> {
    let Some((id, model, created, system_fingerprint)) = template else {
        return Ok(None);
    };
    // OpenAI-compatible clients read the reasoning count only from
    // `completion_tokens_details`; SGLang reports it top-level. This chunk is
    // synthesized on the usage-rewrite path, which the gateway signs.
    usage.ensure_standard_reasoning_details();

    let final_usage_chunk =
        inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: Vec::new(),
            usage: Some(usage),
            service_tier: None,
            prompt_token_ids: None,
            modality: None,
            extra: std::collections::HashMap::new(),
        });

    serde_json::to_string(&final_usage_chunk)
        .map(|json_data| Some(Bytes::from(format!("data: {json_data}\n\n"))))
}

// Helper function to extract inference ID from a parsed stream chunk
pub(super) fn extract_inference_id_from_chunk(
    chunk: &inference_providers::StreamChunk,
) -> Option<Uuid> {
    let id = match chunk {
        inference_providers::StreamChunk::Chat(c) => &c.id,
        inference_providers::StreamChunk::Text(c) => &c.id,
    };
    (!id.is_empty()).then(|| hash_inference_id_to_uuid(id))
}
