use super::*;

// Convert HTTP CompletionRequest to service CompletionRequest
#[allow(clippy::too_many_arguments)]
pub(super) fn convert_text_request_to_service(
    request: &CompletionRequest,
    prompt: String,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    fallback_enabled: bool,
    request_priority: i32,
    body_hash: RequestBodyHash,
    request_id: Uuid,
) -> ServiceCompletionRequest {
    // presence_penalty / frequency_penalty are standard sampling params the chat
    // backend accepts but the service request has no typed slot for, so forward
    // them through `extra` rather than dropping them. echo / logprobs / best_of
    // are rejected upstream (see unsupported_completion_param) — they have no
    // equivalent under the translate-to-chat path — so they never reach here set.
    let mut extra = request.extra.clone();
    if let Some(presence_penalty) = request.presence_penalty {
        extra.insert(
            "presence_penalty".to_string(),
            serde_json::json!(presence_penalty),
        );
    }
    if let Some(frequency_penalty) = request.frequency_penalty {
        extra.insert(
            "frequency_penalty".to_string(),
            serde_json::json!(frequency_penalty),
        );
    }

    ServiceCompletionRequest {
        request_priority,
        session_hint: None,
        request_id,
        model: request.model.clone(),
        messages: vec![CompletionMessage {
            reasoning_content: None,
            role: "user".to_string(),
            content: serde_json::Value::String(prompt),
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone().map(|s| s.into_vec()),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        service_tier: None,
        api_key_id,
        organization_id,
        workspace_id,
        fallback_enabled,
        metadata: None,
        store: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct text completions API calls don't have a response_id
        skip_provider_chat_signature: false,
        original_request: None,
        extra,
    }
}

/// Legacy text-completion parameters that have no equivalent under this
/// endpoint's translate-to-chat implementation. Returns the offending parameter
/// name when the request sets one to a non-default value so the caller can
/// reject with a 400 rather than silently returning OpenAI-incompatible
/// semantics. `presence_penalty` / `frequency_penalty` are intentionally absent
/// — they are forwarded to the provider (see convert_text_request_to_service).
pub(super) fn unsupported_completion_param(request: &CompletionRequest) -> Option<&'static str> {
    if request.echo == Some(true) {
        // echo prepends the prompt to the completion; no chat equivalent.
        return Some("echo");
    }
    if request.logprobs.is_some() {
        // Legacy `logprobs` is an int (top-N tokens); the mapper always returns
        // null and chat logprobs are a different shape.
        return Some("logprobs");
    }
    if request.best_of.is_some_and(|b| b > 1) {
        // Server-side best-of-N selection is not plumbed through.
        return Some("best_of");
    }
    None
}

/// Create text completion
///
/// Generate AI model responses for text prompts. OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/completions",
    tag = "Chat",
    request_body = CompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = CompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    OpenAiJson(request): OpenAiJson<CompletionRequest>,
) -> axum::response::Response {
    requested_model.set(&request.model);
    debug!(
        "Text completions request from api key: {:?}",
        api_key.api_key.id
    );
    debug!(
        "Request model: {}, stream: {:?}, org: {}, workspace: {}",
        request.model, request.stream, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate_request() {
        return (StatusCode::BAD_REQUEST, ResponseJson(error)).into_response();
    }

    let request_id = correlation.request_id;

    // See chat_completions: do NOT span.enter() in async code; .instrument() the
    // inner future so the span wraps every await without leaking across tasks.
    let span = tracing::info_span!(
        "completions",
        request_id = %request_id,
        org_id = %api_key.organization.id.0,
        workspace_id = %api_key.workspace.id.0,
        model = %request.model,
    );

    completions_inner(app_state, api_key, body_hash, headers, request, request_id)
        .instrument(span)
        .await
}

// The legacy text-completions endpoint is implemented by translating the
// `prompt` into a single user chat message (see convert_text_request_to_service)
// and reshaping the chat response back into the `object: "text_completion"`
// format. Consequences worth knowing:
//   - the backend applies its chat template to the prompt, so output is not
//     identical to a raw completion against the base model;
//   - the response is synthesized, so unlike /v1/chat/completions it is not
//     byte-verifiable against attestation (the Inference-Id header still maps
//     to the provider response id).
// Usage tracking/billing is handled inside the completion service, same as chat.
//
// E2E encryption and auto-redact are NOT supported here: both rely on the chat
// path forwarding the provider's bytes (encrypted / un-redacted) straight to the
// client, which is incompatible with reshaping the response into text_completion
// format. Rather than silently bypass these privacy features (sending plaintext /
// un-redacted prompts), we reject such requests with a 400.
#[allow(clippy::too_many_arguments)]
pub(super) async fn completions_inner(
    app_state: AppState,
    api_key: AuthenticatedApiKey,
    body_hash: RequestBodyHash,
    headers: header::HeaderMap,
    request: CompletionRequest,
    request_id: Uuid,
) -> axum::response::Response {
    let request_hash = body_hash.hash.clone();

    // Reject E2E encryption: validate for parity (an invalid version still 400s
    // the same way chat does), then refuse if any encryption header is present.
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(h) => h,
        Err(err) => return err.into_response(),
    };
    if encryption_headers.signing_algo.is_some()
        || encryption_headers.client_pub_key.is_some()
        || encryption_headers.model_pub_key.is_some()
        || encryption_headers.encryption_version.is_some()
        || encryption_headers.encrypt_all_fields.is_some()
    {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "End-to-end encryption is not supported on /v1/completions; use /v1/chat/completions".to_string(),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Reject auto-redact opt-in (header or body field) rather than sending the
    // raw prompt to the provider with redaction silently disabled.
    let auto_redact_headers: Vec<&str> = headers
        .get_all(auto_redact::AUTO_REDACT_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if auto_redact::is_enabled(
        auto_redact_headers.iter().copied(),
        request.extra.get(auto_redact::AUTO_REDACT_BODY_FIELD),
    ) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Auto-redact is not supported on /v1/completions; use /v1/chat/completions"
                    .to_string(),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Reject advertised-but-unmappable legacy params instead of silently
    // dropping them and returning OpenAI-incompatible semantics.
    if let Some(param) = unsupported_completion_param(&request) {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!(
                    "Parameter '{param}' is not supported on /v1/completions; use /v1/chat/completions"
                ),
                "unsupported_parameter".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve the prompt to the single string this endpoint supports. Batch
    // (array) and token-id prompts deserialize fine but have no mapping under
    // the translate-to-chat path, so reject them with a clean 400.
    let prompt = match request.prompt.single_text() {
        Ok(p) => p.to_string(),
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    msg.to_string(),
                    "unsupported_parameter".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Strict alias mode + pre-dispatch alias detection (issue #573) — the
    // service resolves aliases for this endpoint exactly like chat, so it
    // gets the same contract: x-no-aliasing rejects, and aliased responses
    // carry the warning + x-model-alias-resolved header.
    let caller = ModelCaller::Organization(api_key.organization.id.0);
    if let Err(resp) =
        reject_if_aliased(&app_state.models_service, caller, &headers, &request.model).await
    {
        return resp;
    }
    let alias_canonical = app_state
        .models_service
        .resolve_alias_cached(&request.model, caller)
        .await;

    let resolved_model_name = alias_canonical.as_deref().unwrap_or(&request.model);
    let legacy_gateway_signature_enabled =
        match app_state.models_service.get_models_with_pricing().await {
            Ok(models) => models
                .iter()
                .find(|model| model.model_name == resolved_model_name)
                .is_some_and(|model| model.attestation_supported),
            Err(error) => {
                tracing::warn!(
                    model = %request.model,
                    error = %error,
                    "Failed to read cached model metadata for legacy completion signing"
                );
                false
            }
        };

    let mut service_request = convert_text_request_to_service(
        &request,
        prompt,
        api_key.api_key.created_by_user_id.0,
        api_key.api_key.id.0.clone(),
        api_key.organization.id.0,
        api_key.workspace.id.0,
        api_key.organization.fallback_enabled(),
        api_key.organization.request_priority,
        body_hash,
        request_id,
    );
    service_request.session_hint = session_hint_from_headers(&headers);
    // This endpoint always converts the provider's chat-completion payload
    // into the legacy completion format, so a provider signature cannot
    // verify the bytes returned to the client.
    service_request.skip_provider_chat_signature = true;

    if request.stream == Some(true) {
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Peek the first data chunk to surface the Inference-Id
                // header, consuming any leading control events. This route
                // reshapes chat chunks into text-completion format, so
                // control lines are never forwarded (no byte passthrough
                // here by design) and the consumed events can be discarded.
                // Bounded so a keepalive-only upstream can't stall the
                // response: past the cap we proceed without an Inference-Id.
                let mut peekable_stream = Box::pin(stream.peekable());
                let mut control_skipped = 0usize;
                let mut stream_chat_id: Option<String> = None;
                let inference_id = loop {
                    let is_control = match peekable_stream.as_mut().peek().await {
                        Some(Ok(event)) => {
                            if let Some(chunk) = &event.chunk {
                                stream_chat_id = Some(match chunk {
                                    inference_providers::StreamChunk::Chat(c) => c.id.clone(),
                                    inference_providers::StreamChunk::Text(c) => c.id.clone(),
                                })
                                .filter(|id| !id.is_empty());
                                break extract_inference_id_from_chunk(chunk);
                            }
                            true
                        }
                        _ => break None,
                    };
                    if is_control {
                        if control_skipped >= MAX_LEADING_CONTROL_EVENTS {
                            break None;
                        }
                        control_skipped += 1;
                        peekable_stream.next().await;
                    }
                };

                if inference_id.is_none() {
                    tracing::warn!(
                        organization_id = %api_key.organization.id.0,
                        model = %request.model,
                        "Could not extract inference ID from first chunk for text completion (streaming)"
                    );
                }

                let serving = if let Some(ref chat_id) = stream_chat_id {
                    app_state
                        .inference_provider_pool
                        .get_serving_provider_for_chat_id(chat_id)
                        .await
                } else {
                    None
                };

                let organization_id = api_key.organization.id.0;
                let model_for_err = request.model.clone();
                let public_signature_hasher = Arc::new(tokio::sync::Mutex::new(Sha256::new()));
                let public_signature_chat_id =
                    Arc::new(tokio::sync::Mutex::new(stream_chat_id.clone()));
                let stream_error_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
                let attestation_service = app_state.attestation_service.clone();

                // Warning to inject into the first streamed chunk of an
                // alias-served response (issue #573).
                let alias_warning_pending: Arc<std::sync::Mutex<Option<String>>> =
                    Arc::new(std::sync::Mutex::new(alias_canonical.as_ref().map(
                        |canonical| alias_warning_message(&request.model, canonical),
                    )));
                let pending_warning = alias_warning_pending.clone();
                let public_signature_hasher_for_chunks = public_signature_hasher.clone();
                let public_signature_chat_id_for_chunks = public_signature_chat_id.clone();
                let stream_error_count_for_chunks = stream_error_count.clone();

                let byte_stream = peekable_stream
                    .filter_map(move |result| {
                        let model_for_err = model_for_err.clone();
                        let pending_warning = pending_warning.clone();
                        let public_signature_hasher = public_signature_hasher_for_chunks.clone();
                        let public_signature_chat_id = public_signature_chat_id_for_chunks.clone();
                        let stream_error_count = stream_error_count_for_chunks.clone();
                        async move {
                            match result {
                                // Control lines (blank/comment/[DONE]) carry no
                                // parsed payload — skip; the gateway appends its
                                // own [DONE] terminator below. This route reshapes
                                // chat chunks into text-completion format, so it
                                // always re-serializes (no byte passthrough).
                                Ok(event) => {
                                    let chunk = event.chunk?;
                                    let chat_id = match &chunk {
                                        inference_providers::StreamChunk::Chat(chunk) => {
                                            chunk.id.clone()
                                        }
                                        inference_providers::StreamChunk::Text(chunk) => {
                                            chunk.id.clone()
                                        }
                                    };
                                    let mut stored_chat_id = public_signature_chat_id.lock().await;
                                    if stored_chat_id.is_none() {
                                        *stored_chat_id = Some(chat_id);
                                    }
                                    drop(stored_chat_id);

                                    let text_chunk = chat_chunk_to_text_chunk(chunk);
                                    // The first chunk of an alias-served response
                                    // gets a top-level "warning" (issue #573).
                                    let alias_warning =
                                        pending_warning.lock().ok().and_then(|mut g| g.take());
                                    let json_data = match alias_warning {
                                        Some(warning) => {
                                            serde_json::to_value(&text_chunk).map(|mut v| {
                                                if let Some(obj) = v.as_object_mut() {
                                                    obj.insert(
                                                        "warning".to_string(),
                                                        serde_json::Value::String(warning),
                                                    );
                                                }
                                                v.to_string()
                                            })
                                        }
                                        None => serde_json::to_string(&text_chunk),
                                    }
                                    .unwrap_or_else(|e| {
                                        tracing::error!(
                                            %organization_id,
                                            "Failed to serialize text completion chunk: {e}"
                                        );
                                        "{}".to_string()
                                    });
                                    let bytes = Bytes::from(format!("data: {json_data}\n\n"));
                                    if legacy_gateway_signature_enabled {
                                        public_signature_hasher.lock().await.update(&bytes);
                                    }
                                    Some(Ok::<Bytes, Infallible>(bytes))
                                }
                                Err(e) => {
                                    stream_error_count
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::error!(
                                        %organization_id,
                                        model = %model_for_err,
                                        error_type = %completion_stream_error_category(&e),
                                        "Text completion stream error"
                                    );
                                    Some(Ok::<Bytes, Infallible>(sse_error_frame(&e)))
                                }
                            }
                        }
                    })
                    .chain(futures::stream::once({
                        let public_signature_hasher = public_signature_hasher.clone();
                        let public_signature_chat_id = public_signature_chat_id.clone();
                        let stream_error_count = stream_error_count.clone();
                        let request_hash = request_hash.clone();
                        let model_name = request.model.clone();
                        async move {
                            let done = Bytes::from_static(b"data: [DONE]\n\n");
                            let chat_id = public_signature_chat_id.lock().await.clone();
                            let stream_errored = stream_error_count
                                .load(std::sync::atomic::Ordering::Relaxed)
                                > 0;

                            if legacy_gateway_signature_enabled && !stream_errored {
                                let response_hash = {
                                    let mut hasher = public_signature_hasher.lock().await;
                                    hasher.update(&done);
                                    hex::encode(hasher.clone().finalize())
                                };
                                if let Some(chat_id) = chat_id {
                                    match tokio::time::timeout(
                                        Duration::from_secs(STREAM_SIGNATURE_STORE_TIMEOUT_SECS),
                                        attestation_service.store_chat_signature(
                                            &chat_id,
                                            request_hash,
                                            response_hash,
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => tracing::error!(
                                            %organization_id,
                                            model = %model_name,
                                            error = %error,
                                            "Failed to store legacy completion stream signature"
                                        ),
                                        Err(_) => tracing::error!(
                                            %organization_id,
                                            model = %model_name,
                                            "Timed out storing legacy completion stream signature"
                                        ),
                                    }
                                } else {
                                    tracing::warn!(
                                        %organization_id,
                                        model = %model_name,
                                        "Cannot store legacy completion stream signature: no chat_id observed"
                                    );
                                }
                            }

                            Ok::<Bytes, Infallible>(done)
                        }
                    }));

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive");

                let mut exposed_headers: Vec<&str> = Vec::new();
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }
                if let Some(serving) = serving {
                    response_builder =
                        response_builder.header(HEADER_SERVING_PROVIDER, serving.label());
                    exposed_headers.push(HEADER_SERVING_PROVIDER);
                }
                // Announce alias substitution so it is never silent (issue #573)
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }
                if !exposed_headers.is_empty() {
                    response_builder = response_builder
                        .header("Access-Control-Expose-Headers", exposed_headers.join(", "));
                }

                response_builder
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    } else {
        match app_state
            .completion_service
            .create_chat_completion(service_request)
            .await
        {
            Ok(response_with_bytes) => {
                let chat_id = response_with_bytes.response.id.clone();
                let inference_id = hash_inference_id_to_uuid(&chat_id);
                let completion = chat_response_to_text_response(response_with_bytes.response);

                let body_bytes = match serde_json::to_vec(&completion) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize text completion response");
                        app_state
                            .attestation_service
                            .release_chat_signature_pin(&chat_id)
                            .await;
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            ResponseJson(ErrorResponse::new(
                                "Failed to assemble response".to_string(),
                                "internal_server_error".to_string(),
                            )),
                        )
                            .into_response();
                    }
                };

                // Annotate alias-served responses (issue #573). This
                // endpoint already re-serializes (no raw-bytes contract),
                // so the warning injection costs nothing extra.
                let body_bytes = match &alias_canonical {
                    Some(canonical) => inject_warning_field(
                        &body_bytes,
                        &alias_warning_message(&request.model, canonical),
                    )
                    .unwrap_or(body_bytes),
                    None => body_bytes,
                };

                if legacy_gateway_signature_enabled {
                    let response_hash = hex::encode(Sha256::digest(&body_bytes));
                    if let Err(error) = app_state
                        .attestation_service
                        .store_chat_signature_and_unpin(&chat_id, request_hash, response_hash)
                        .await
                    {
                        tracing::error!(
                            chat_id = %chat_id,
                            error = %error,
                            "Failed to store legacy completion signature"
                        );
                    }
                } else {
                    app_state
                        .attestation_service
                        .release_chat_signature_pin(&chat_id)
                        .await;
                }

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(HEADER_INFERENCE_ID, inference_id.to_string());

                let mut exposed_headers: Vec<&str> = vec![HEADER_INFERENCE_ID];
                response_builder = response_builder
                    .header(HEADER_SERVING_PROVIDER, response_with_bytes.serving.label());
                exposed_headers.push(HEADER_SERVING_PROVIDER);
                // Announce alias substitution so it is never silent (issue #573)
                if let Some(canonical) = &alias_canonical {
                    if let Ok(value) = header::HeaderValue::from_str(&format!(
                        "{} -> {}",
                        request.model, canonical
                    )) {
                        response_builder =
                            response_builder.header(HEADER_MODEL_ALIAS_RESOLVED, value);
                        exposed_headers.push(HEADER_MODEL_ALIAS_RESOLVED);
                    }
                }
                response_builder = response_builder
                    .header("Access-Control-Expose-Headers", exposed_headers.join(", "));

                response_builder.body(Body::from(body_bytes)).unwrap()
            }
            Err(domain_error) => {
                let status_code = map_domain_error_to_status(&domain_error);
                (
                    status_code,
                    ResponseJson::<ErrorResponse>(domain_error.into()),
                )
                    .into_response()
            }
        }
    }
}

/// Reshape a non-streaming chat completion response into the legacy
/// `object: "text_completion"` format: each choice's assistant message content
/// becomes the choice `text`.
pub(super) fn chat_response_to_text_response(
    response: inference_providers::ChatCompletionResponse,
) -> CompletionResponse {
    let cached = response.usage.cached_tokens();
    CompletionResponse {
        id: response.id,
        object: "text_completion".to_string(),
        created: response.created,
        model: response.model,
        choices: response
            .choices
            .into_iter()
            .map(|c| CompletionChoice {
                index: c.index,
                text: c.message.content.unwrap_or_default(),
                logprobs: None,
                finish_reason: c.finish_reason,
            })
            .collect(),
        usage: CompletionUsage {
            prompt_tokens: response.usage.prompt_tokens,
            prompt_tokens_details: (cached != 0).then_some(InputTokensDetails {
                cached_tokens: cached as i64,
            }),
            completion_tokens: response.usage.completion_tokens,
            completion_tokens_details: None,
            total_tokens: response.usage.total_tokens,
            reasoning_tokens: None,
        },
    }
}

/// Reshape a streaming chat chunk into the legacy text-completion chunk format:
/// the assistant `delta.content` becomes the choice `text`.
pub(super) fn chat_chunk_to_text_chunk(
    chunk: inference_providers::StreamChunk,
) -> inference_providers::models::CompletionChunk {
    let chat = match chunk {
        inference_providers::StreamChunk::Chat(c) => c,
        // The service emits chat chunks for this request shape; pass any
        // already-text chunk through unchanged.
        inference_providers::StreamChunk::Text(c) => return c,
    };
    inference_providers::models::CompletionChunk {
        id: chat.id,
        object: "text_completion".to_string(),
        created: chat.created,
        model: chat.model,
        system_fingerprint: chat.system_fingerprint,
        choices: chat
            .choices
            .into_iter()
            .map(|c| inference_providers::models::TextChoice {
                index: c.index,
                text: c.delta.and_then(|d| d.content).unwrap_or_default(),
                logprobs: None,
                finish_reason: c.finish_reason,
            })
            .collect(),
        usage: chat.usage,
    }
}
