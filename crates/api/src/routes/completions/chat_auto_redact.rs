use super::*;

/// Decide whether to enable auto-redact based on the request, mutate the
/// service request to scrub the body field, and (if enabled) run the PII
/// detector + rewrite messages in place. Returns the placeholder map for
/// downstream un-redact; an empty map is also returned when auto-redact is
/// off.
/// The tuple is `(requested, map, classify_input_tokens)`. `classify_input_tokens`
/// is the number of tokens the privacy-filter billed for the classify pass,
/// for the caller to charge (nearai/cloud-api#602); it is 0 when auto-redact
/// is off or there was nothing to classify.
pub(super) async fn maybe_redact(
    headers: &header::HeaderMap,
    service_request: &mut ServiceCompletionRequest,
    pool: &services::inference_provider_pool::InferenceProviderPool,
) -> Result<(bool, RedactionMap, i64), AutoRedactError> {
    let header_values: Vec<&str> = headers
        .get_all(auto_redact::AUTO_REDACT_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let body_field = service_request
        .extra
        .get(auto_redact::AUTO_REDACT_BODY_FIELD);
    let enabled = auto_redact::is_enabled(header_values.iter().copied(), body_field);

    // Always strip the body field so providers with strict JSON schemas
    // (e.g. Anthropic) don't 400 on unknown keys.
    auto_redact::strip_body_field(&mut service_request.extra);

    if !enabled {
        return Ok((false, RedactionMap::new(), 0));
    }

    let (map, classify_tokens) = auto_redact::redact_messages(
        &mut service_request.messages,
        auto_redact::DEFAULT_PII_MODEL,
        pool,
    )
    .await?;
    Ok((true, map, classify_tokens))
}

/// Convert an [`AutoRedactError`] into the user-facing error response.
/// Detector-unavailable is 503 (fail-closed per design); internal errors
/// are 500.
pub(super) fn auto_redact_error_response(err: AutoRedactError) -> Response {
    match err {
        AutoRedactError::DetectorUnavailable(msg) => {
            tracing::error!(error = %msg, "auto_redact detector unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ResponseJson(ErrorResponse::new(
                    "PII redaction service is unavailable. Retry, or omit auto_redact to send the prompt as-is.".to_string(),
                    "auto_redact_unavailable".to_string(),
                )),
            )
                .into_response()
        }
        AutoRedactError::Internal(msg) => {
            tracing::error!(error = %msg, "auto_redact internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "PII redaction failed".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
                .into_response()
        }
    }
}

/// Upper bound on the spawned auto-redact billing task so a stuck billing DB
/// or model lookup can't leak a background task indefinitely.
pub(super) const AUTO_REDACT_BILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Record usage for the privacy-filter classify pass that auto-redact runs
/// before a completion (nearai/cloud-api#602). The classify is a real
/// inference call to the PII model, so it is billed like an explicit
/// `/v1/privacy/classify`: `input_tokens × input_rate` on the privacy-filter
/// model, one record per request (a fresh v4 id, matching the explicit
/// privacy endpoints).
///
/// Best-effort: a model-lookup or record failure is logged and swallowed so
/// it never fails the user's completion (which bills separately via
/// `InterceptStream`). The classify already happened and cost GPU time, so
/// it is billed even if the downstream completion later fails.
pub(super) async fn bill_auto_redact_classify(
    app_state: &AppState,
    api_key: &crate::middleware::auth::AuthenticatedApiKey,
    classify_tokens: i64,
) {
    if classify_tokens <= 0 {
        return; // nothing to bill
    }
    // Cap an anomalous count at the same `MAX_REASONABLE_TOKENS` the explicit
    // `/v1/privacy/{classify,redact}` paths use, emitting the same anomaly
    // metric — so x-auto-redact (which shares the privacy-filter classify
    // call) can't massively overcharge on a buggy/malicious provider response
    // instead of just clamping to i32::MAX. See nearai/cloud-api#602 review.
    const MAX_REASONABLE_TOKENS: i32 = 1_000_000;
    let input_tokens = if classify_tokens > MAX_REASONABLE_TOKENS as i64 {
        tracing::error!(
            token_count = classify_tokens,
            max_expected = MAX_REASONABLE_TOKENS,
            model = auto_redact::DEFAULT_PII_MODEL,
            "auto_redact: provider returned unreasonable classify token count - capping"
        );
        let model_tag = format!("model:{}", auto_redact::DEFAULT_PII_MODEL);
        let reason_tag = format!(
            "reason:{}",
            services::metrics::consts::REASON_TOKEN_OVERFLOW
        );
        app_state.metrics_service.record_count(
            services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
            1,
            &[model_tag.as_str(), reason_tag.as_str()],
        );
        MAX_REASONABLE_TOKENS
    } else {
        classify_tokens as i32 // safe: 0 < classify_tokens <= 1_000_000
    };

    let model = match app_state
        .models_service
        .get_model_by_name(auto_redact::DEFAULT_PII_MODEL)
        .await
    {
        Ok(model) => model,
        Err(e) => {
            tracing::error!(
                error = %e,
                model = auto_redact::DEFAULT_PII_MODEL,
                "auto_redact: could not resolve PII model; classify usage not recorded"
            );
            return;
        }
    };

    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "auto_redact: invalid API key id; classify usage not recorded");
            return;
        }
    };

    let usage_request = services::usage::RecordUsageServiceRequest {
        input_tokens,
        inference_id: Some(Uuid::new_v4()),
        stop_reason: Some(services::usage::StopReason::Completed),
        ..services::usage::RecordUsageServiceRequest::base(
            api_key.organization.id.0,
            api_key.workspace.id.0,
            api_key_id,
            model.id,
            services::usage::ports::InferenceType::PrivacyClassify,
        )
    };

    if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
        tracing::error!(error = %e, "auto_redact: failed to record classify usage");
    }
}

/// Walk a non-streaming chat response's text fields and substitute
/// placeholders with their originals. Mutates in place.
///
/// Covers content + reasoning fields and the JSON-encoded
/// `tool_calls[*].function.arguments` string. The latter is critical for
/// agentic flows where the model emits a tool call whose arguments echo
/// the user's PII — without un-redacting them, the placeholder leaks to
/// the client.
pub(super) fn unredact_chat_response_in_place(
    response: &mut inference_providers::ChatCompletionResponse,
    map: &RedactionMap,
) {
    for choice in &mut response.choices {
        if let Some(content) = &mut choice.message.content {
            *content = map.unredact(content);
        }
        if let Some(reasoning) = &mut choice.message.reasoning_content {
            *reasoning = map.unredact(reasoning);
        }
        if let Some(reasoning) = &mut choice.message.reasoning {
            *reasoning = map.unredact(reasoning);
        }
        if let Some(refusal) = &mut choice.message.refusal {
            // A safety-tuned model may produce a refusal that quotes our
            // placeholders back ("I can't email <email1>"). Without
            // un-redacting, the placeholder leaks to the client.
            *refusal = map.unredact(refusal);
        }
        if let Some(tool_calls) = &mut choice.message.tool_calls {
            for tc in tool_calls {
                if let Some(args) = &mut tc.function.arguments {
                    // arguments is itself a JSON-encoded string. JSON-escape
                    // each replacement so PII containing `"`, `\`, or
                    // control chars doesn't corrupt the surrounding JSON.
                    *args = map.unredact_json_string(args);
                }
            }
        }
    }
}

/// Per-choice, per-field streaming un-redact state. For `n > 1`
/// completions the provider may interleave chunks for different choice
/// indices; each choice needs its own sliding tail buffer or split
/// placeholders get cross-contaminated. The text fields (`content`,
/// `reasoning_content`, `reasoning`) are kept independent for the same
/// reason — a model may emit them concurrently.
///
/// `tool_call_arguments` is keyed by `(choice_index, tool_call_index)`
/// because a single response can have multiple parallel tool calls, each
/// with its own arguments JSON stream.
#[derive(Default)]
pub(super) struct StreamUnredactStates {
    pub(super) content: std::collections::HashMap<i64, StreamUnredact>,
    reasoning_content: std::collections::HashMap<i64, StreamUnredact>,
    reasoning: std::collections::HashMap<i64, StreamUnredact>,
    pub(super) tool_call_arguments: std::collections::HashMap<(i64, i64), StreamUnredact>,
}

pub(super) fn unredact_field(
    states: &mut std::collections::HashMap<i64, StreamUnredact>,
    map: &Arc<RedactionMap>,
    idx: i64,
    text: &mut String,
    finalize: bool,
) {
    let s = states
        .entry(idx)
        .or_insert_with(|| StreamUnredact::new(map.clone()));
    // Finalize drains the tail (no further chunks coming for this
    // choice/field); regular path holds up to max_dummy_len bytes.
    *text = if finalize {
        s.drain(text)
    } else {
        s.process(text)
    };
}

/// Drain held tails for `idx` into the finish chunk's delta. Removes the
/// state entries from `states` so [`build_flush_chunks`] emits nothing
/// for this choice. Used only on chunks that carry `finish_reason` — see
/// [`unredact_chunk_in_place`] for the rationale.
///
/// For each text field: if the chunk already carries the field, the
/// previous inline `drain` call has already emptied that state's tail and
/// `flush` returns `""`, so we just remove the now-empty entry. If the
/// chunk did NOT carry the field, we remove the state and attach its
/// flushed tail to a freshly populated field on the chunk's delta.
pub(super) fn finalize_choice_in_place(
    choice: &mut inference_providers::models::ChatChoice,
    idx: i64,
    states: &mut StreamUnredactStates,
) {
    fn merge_into(field: &mut Option<String>, tail: String) {
        if tail.is_empty() {
            return;
        }
        match field {
            // The inline drain path already produced the substituted
            // value, so the flushed tail is empty when we get here and
            // this branch shouldn't normally fire. Guard anyway.
            Some(existing) => existing.push_str(&tail),
            None => *field = Some(tail),
        }
    }

    let delta = choice
        .delta
        .get_or_insert(inference_providers::models::ChatDelta {
            role: None,
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            extra: Default::default(),
        });

    if let Some(s) = states.content.remove(&idx) {
        merge_into(&mut delta.content, s.flush());
    }
    if let Some(s) = states.reasoning_content.remove(&idx) {
        merge_into(&mut delta.reasoning_content, s.flush());
    }
    if let Some(s) = states.reasoning.remove(&idx) {
        merge_into(&mut delta.reasoning, s.flush());
    }

    // Tool-call arguments are keyed by `(choice_idx, tc_idx)`. Take every
    // entry for this choice; sort by tc_idx so the synthesized order is
    // stable; merge each non-empty tail into `delta.tool_calls`. If the
    // chunk already carries a matching tc_idx (inline-drained path), the
    // flushed tail is empty and the loop is a no-op for that entry.
    let mut tc_keys: Vec<(i64, i64)> = states
        .tool_call_arguments
        .keys()
        .filter(|(c, _)| *c == idx)
        .copied()
        .collect();
    tc_keys.sort_unstable();
    for key in tc_keys {
        let Some(s) = states.tool_call_arguments.remove(&key) else {
            continue;
        };
        let tail = s.flush();
        if tail.is_empty() {
            continue;
        }
        let (_, tc_idx) = key;
        let tool_calls = delta.tool_calls.get_or_insert_with(Vec::new);
        // Reuse the existing ToolCallDelta entry for this tc_idx if the
        // chunk already had one, so we don't emit two deltas for the
        // same logical tool call.
        let entry = tool_calls.iter_mut().find(|tc| tc.index == Some(tc_idx));
        match entry {
            Some(tc) => {
                let func =
                    tc.function
                        .get_or_insert(inference_providers::models::FunctionCallDelta {
                            name: None,
                            arguments: None,
                        });
                merge_into(&mut func.arguments, tail);
            }
            None => {
                tool_calls.push(inference_providers::models::ToolCallDelta {
                    id: None,
                    type_: None,
                    index: Some(tc_idx),
                    function: Some(inference_providers::models::FunctionCallDelta {
                        name: None,
                        arguments: Some(tail),
                    }),
                    thought_signature: None,
                });
            }
        }
    }
}

/// Apply streaming un-redact to a single parsed chunk, mutating any text
/// deltas (content + reasoning) in place. Stateful: per-choice tail
/// buffers carry across calls via `states`.
pub(super) fn unredact_chunk_in_place(
    chunk: &mut inference_providers::StreamChunk,
    states: &mut StreamUnredactStates,
    map: &Arc<RedactionMap>,
) {
    match chunk {
        inference_providers::StreamChunk::Chat(c) => {
            for choice in &mut c.choices {
                let idx = choice.index;
                // A chunk carrying `finish_reason` is the last chunk
                // we'll see for this choice. Drain the held tail into
                // its fields rather than relying on the end-of-stream
                // flush (which emits a synthetic chunk AFTER the
                // finish_reason, missed by clients that stop reading
                // on finish_reason without waiting for `[DONE]`).
                let finalize = choice.finish_reason.is_some();
                if let Some(delta) = &mut choice.delta {
                    if let Some(content) = &mut delta.content {
                        unredact_field(&mut states.content, map, idx, content, finalize);
                    }
                    if let Some(rc) = &mut delta.reasoning_content {
                        unredact_field(&mut states.reasoning_content, map, idx, rc, finalize);
                    }
                    if let Some(r) = &mut delta.reasoning {
                        unredact_field(&mut states.reasoning, map, idx, r, finalize);
                    }
                    if let Some(tcs) = &mut delta.tool_calls {
                        for (pos, tc) in tcs.iter_mut().enumerate() {
                            // Per-tool-call streaming state keyed by
                            // (choice_index, tool_call_index). Use the
                            // delta's index if present; otherwise fall
                            // back to the *position* in this chunk so two
                            // parallel indexless tool calls don't collide.
                            let tc_idx = tc.index.unwrap_or(pos as i64);
                            if let Some(func) = &mut tc.function {
                                if let Some(args) = &mut func.arguments {
                                    // arguments is JSON-encoded; substitute
                                    // with JSON-escaped originals.
                                    let s = states
                                        .tool_call_arguments
                                        .entry((idx, tc_idx))
                                        .or_insert_with(|| {
                                            StreamUnredact::new_for_json_string(map.clone())
                                        });
                                    *args = if finalize {
                                        s.drain(args)
                                    } else {
                                        s.process(args)
                                    };
                                }
                            }
                        }
                    }
                }
                if finalize {
                    // Catch every per-choice state whose field was *absent*
                    // from this chunk's delta — those are the held tails
                    // the inline path can't see. Without this, a finish
                    // chunk with `delta: {}` leaves the tail in `states`
                    // and build_flush_chunks emits it AFTER the finish
                    // event (invisible to clients that stop on
                    // finish_reason). The inline-drained entries above
                    // are already empty; this loop just removes them.
                    finalize_choice_in_place(choice, idx, states);
                }
            }
        }
        inference_providers::StreamChunk::Text(c) => {
            for choice in &mut c.choices {
                let idx = choice.index;
                let finalize = choice.finish_reason.is_some();
                unredact_field(&mut states.content, map, idx, &mut choice.text, finalize);
            }
        }
    }
}

/// Snapshot of metadata from the first chunk we see in a stream, used to
/// build a synthetic flush chunk at end-of-stream with matching id/model.
/// Layout: `(id, model, created, system_fingerprint)`.
pub(super) type ChunkTemplate = Option<(String, String, i64, Option<String>)>;

/// Drain a per-field state map at end-of-stream, emitting a synthetic
/// final SSE chunk for each choice index that still has bytes held in its
/// tail buffer. Without this, an upstream stream that ends mid-placeholder
/// would silently truncate the client's view of the response.
pub(super) fn build_flush_chunks(
    states: &mut StreamUnredactStates,
    template: &ChunkTemplate,
) -> Vec<Bytes> {
    let Some((id, model, created, system_fingerprint)) = template else {
        return Vec::new();
    };

    let mut out: Vec<Bytes> = Vec::new();

    // --- Pass 1: content / reasoning_content / reasoning ---
    let mut pending: std::collections::BTreeMap<i64, (String, String, String)> =
        std::collections::BTreeMap::new();
    for (idx, st) in states.content.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().0 = text;
        }
    }
    for (idx, st) in states.reasoning_content.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().1 = text;
        }
    }
    for (idx, st) in states.reasoning.drain() {
        let text = st.flush();
        if !text.is_empty() {
            pending.entry(idx).or_default().2 = text;
        }
    }
    for (idx, (content, rc, r)) in pending {
        let chunk = inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: vec![inference_providers::models::ChatChoice {
                index: idx,
                delta: Some(inference_providers::models::ChatDelta {
                    role: None,
                    content: if content.is_empty() {
                        None
                    } else {
                        Some(content)
                    },
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: if rc.is_empty() { None } else { Some(rc) },
                    reasoning: if r.is_empty() { None } else { Some(r) },
                    extra: Default::default(),
                }),
                logprobs: None,
                finish_reason: None,
                token_ids: None,
            }],
            usage: None,
            service_tier: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        };
        if let Ok(s) = serde_json::to_string(&chunk) {
            out.push(Bytes::from(format!("data: {s}\n\n")));
        }
    }

    // --- Pass 2: tool_call_arguments ---
    // Each held tail becomes a synthetic tool-call delta with just the
    // arguments fragment. The placeholder may be incomplete (the LLM was
    // cut off mid-`<email1>`), in which case the literal bytes are emitted
    // — visible signal of truncation, but no silent loss of held bytes
    // and no leaking placeholder we never minted.
    let mut tc_drain: Vec<((i64, i64), String)> = states
        .tool_call_arguments
        .drain()
        .filter_map(|(key, st)| {
            let text = st.flush();
            if text.is_empty() {
                None
            } else {
                Some((key, text))
            }
        })
        .collect();
    // Stable ordering: choice index ascending, then tool_call index.
    tc_drain.sort_by_key(|((c, t), _)| (*c, *t));
    for ((choice_idx, tc_idx), args) in tc_drain {
        let chunk = inference_providers::models::ChatCompletionChunk {
            id: id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: *created,
            model: model.clone(),
            system_fingerprint: system_fingerprint.clone(),
            choices: vec![inference_providers::models::ChatChoice {
                index: choice_idx,
                delta: Some(inference_providers::models::ChatDelta {
                    role: None,
                    content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![inference_providers::models::ToolCallDelta {
                        id: None,
                        type_: None,
                        index: Some(tc_idx),
                        function: Some(inference_providers::models::FunctionCallDelta {
                            name: None,
                            arguments: Some(args),
                        }),
                        thought_signature: None,
                    }]),
                    reasoning_content: None,
                    reasoning: None,
                    extra: Default::default(),
                }),
                logprobs: None,
                finish_reason: None,
                token_ids: None,
            }],
            usage: None,
            service_tier: None,
            prompt_token_ids: None,
            modality: None,
            extra: Default::default(),
        };
        if let Ok(s) = serde_json::to_string(&chunk) {
            out.push(Bytes::from(format!("data: {s}\n\n")));
        }
    }

    out
}
