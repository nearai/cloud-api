use super::*;

// Convert MessageContent to serde_json::Value, preserving multimodal parts (images, audio, etc.)
pub(super) fn message_content_to_value(content: &Option<MessageContent>) -> serde_json::Value {
    match content {
        None => serde_json::Value::String(String::new()),
        Some(content) => {
            let converted = crate::conversions::convert_content_to_vllm(content.clone());
            serde_json::to_value(&converted).expect("MessageContent should always be serializable")
        }
    }
}

/// Header carrying a client-supplied session identifier, used only to derive
/// a placement affinity key (`services::completions::affinity::derive`).
/// Never logged, never forwarded to a provider.
pub(super) const SESSION_ID_HEADER: &str = "x-session-id";

/// Read the `x-session-id` header, if present and valid UTF-8.
pub(super) fn session_hint_from_headers(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get(SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

// Convert HTTP ChatCompletionRequest to service CompletionRequest
#[allow(clippy::too_many_arguments)]
pub(super) fn convert_chat_request_to_service(
    request: &ChatCompletionRequest,
    user_id: Uuid,
    api_key_id: String,
    organization_id: Uuid,
    workspace_id: Uuid,
    fallback_enabled: bool,
    request_priority: i32,
    body_hash: RequestBodyHash,
    request_id: Uuid,
) -> ServiceCompletionRequest {
    // The body-hash middleware already buffered these exact bytes. Keep one
    // parsed in-memory copy for protocol adapters; never log or persist it.
    let original_request = serde_json::from_slice(&body_hash.body_bytes).ok();
    // `presence_penalty` / `frequency_penalty` are typed fields on
    // `ChatCompletionRequest`, so `#[serde(flatten)] extra` never captures them.
    // The service layer hardcodes the typed `ChatCompletionParams` penalty slots
    // to `None`, so forward them through `extra` (matching seed/logprobs and the
    // text-completion path) — otherwise they are silently dropped before reaching
    // the self-hosted backend (nearai/cloud-api #622).
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
        messages: request
            .messages
            .iter()
            .map(|msg| CompletionMessage {
                role: msg.role.clone(),
                content: message_content_to_value(&msg.content),
                tool_call_id: msg.tool_call_id.clone(),
                reasoning_content: msg.prior_reasoning(),
                tool_calls: msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| services::completions::ports::CompletionToolCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                            thought_signature: tc.thought_signature.clone(),
                        })
                        .collect()
                }),
            })
            .collect(),
        // Service and provider adapters share one effective output limit.
        max_tokens: request.max_completion_tokens.or(request.max_tokens),
        temperature: request.temperature,
        top_p: request.top_p,
        stop: request.stop.clone().map(|s| s.into_vec()),
        stream: request.stream,
        user_id: user_id.into(),
        n: request.n,
        service_tier: request.service_tier,
        api_key_id,
        organization_id,
        workspace_id,
        fallback_enabled,
        metadata: None,
        store: None,
        body_hash: body_hash.hash.clone(),
        response_id: None, // Direct chat completions API calls don't have a response_id
        skip_provider_chat_signature: false,
        original_request,
        extra,
    }
}

/// Create chat completion
///
/// Generate AI model responses for chat conversations. Supports both streaming and non-streaming modes.
/// OpenAI-compatible endpoint.
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "Chat",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Completion generated successfully", body = ChatCompletionResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn chat_completions(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    OpenAiJson(request): OpenAiJson<ChatCompletionRequest>,
) -> axum::response::Response {
    requested_model.set(&request.model);
    debug!(
        "Chat completions request from api key: {:?}",
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

    // Create a span carrying correlation IDs so every log line within this
    // request carries request_id/org_id/workspace_id in Datadog.
    //
    // NOTE: we do NOT call span.enter() here. In async code, Span::enter()
    // stores the guard in a thread-local; when the future yields at an .await
    // the guard stays entered on that thread, so any other task scheduled on
    // the same thread incorrectly inherits this span as its parent, corrupting
    // log context. Use .instrument(span) on the inner async block instead.
    let span = tracing::info_span!(
        "chat_completions",
        request_id = %request_id,
        org_id = %api_key.organization.id.0,
        workspace_id = %api_key.workspace.id.0,
        model = %request.model,
    );

    chat_completions_inner(app_state, api_key, body_hash, headers, request, request_id)
        .instrument(span)
        .await
}

// Inner async fn so .instrument(span) wraps all awaits in the handler.
// Split from `chat_completions` only to correctly scope the tracing span:
// using span.enter() in an async fn risks the guard outliving an .await yield,
// causing other tasks on the same thread to inherit this span's context.
#[allow(clippy::too_many_arguments)]
pub(super) async fn chat_completions_inner(
    app_state: crate::routes::api::AppState,
    api_key: crate::middleware::auth::AuthenticatedApiKey,
    body_hash: crate::middleware::RequestBodyHash,
    headers: header::HeaderMap,
    request: ChatCompletionRequest,
    request_id: Uuid,
) -> axum::response::Response {
    let request_hash = body_hash.hash.clone();

    // Convert HTTP request to service parameters
    // Note: Names are not passed - high-cardinality data is tracked via database, not metrics
    let mut service_request = convert_chat_request_to_service(
        &request,
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

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Add validated headers to service_request.extra
    insert_encryption_headers(&encryption_headers, &mut service_request.extra);
    let e2ee_active = e2ee_requested(&encryption_headers);
    let include_stream_usage_in_response = chat_stream_include_usage_requested(&request);

    // Strict alias mode: refuse to serve through an alias before any
    // inference happens (issue #573).
    if let Err(resp) = reject_if_aliased(&app_state.models_service, &headers, &request.model).await
    {
        return resp;
    }

    // Pre-dispatch alias detection (issue #573): the canonical name when
    // the requested model is a registered alias, mirroring the resolution
    // the completion service is about to apply. Derived purely from the
    // catalog — never from the response's `model` echo, which carries no
    // signal about alias-ness: external providers may rewrite the upstream
    // model name (`provider_config.model_name`), and that override can
    // even equal the alias string itself. Cache-backed (no DB on the hot
    // path); advisory only — strict mode above stays authoritative.
    let alias_canonical = app_state
        .models_service
        .resolve_alias_cached(&request.model)
        .await;
    let resolved_model_name = alias_canonical.as_deref().unwrap_or(&request.model);
    let (model_attestation_supported, model_input_modalities) =
        match app_state.models_service.get_models_with_pricing().await {
            // Exact catalog name first, then the alias target: a name that is
            // both a model and another model's alias must read its own
            // capabilities.
            Ok(models) => models
                .iter()
                .find(|model| model.model_name == request.model)
                .or_else(|| {
                    models
                        .iter()
                        .find(|model| model.model_name == resolved_model_name)
                })
                .map(|model| {
                    (
                        Some(model.attestation_supported),
                        model.input_modalities.clone(),
                    )
                })
                .unwrap_or((None, None)),
            Err(error) => {
                tracing::warn!(
                    model = %request.model,
                    error = %error,
                    "Failed to read cached model metadata for attestation signing decisions"
                );
                (None, None)
            }
        };

    // Refuse gated parts (video) the catalog does not declare for this model
    // before any dispatch. Engines answer an unsupported modality
    // inconsistently (a valid video makes SGLang's GLM processor raise a 500,
    // which surfaced here as a retried 502); the catalog's `inputModalities`
    // is the contract, so the client gets a deterministic, non-retryable 400.
    // See `GATED_INPUT_MODALITIES` for what is and is not gated.
    let requested_modalities = crate::models::requested_input_modalities(&request.messages);
    if let Some(unsupported) = crate::models::unsupported_input_modality(
        &requested_modalities,
        model_input_modalities.as_deref(),
    ) {
        tracing::info!(
            model = %request.model,
            modality = unsupported,
            "Rejecting request: model does not declare the requested input modality"
        );
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::with_param(
                format!(
                    "Model '{}' does not support {unsupported} input. Supported input modalities: {}.",
                    request.model,
                    model_input_modalities
                        .as_deref()
                        .filter(|m| !m.is_empty())
                        .map(|m| m.join(", "))
                        .unwrap_or_else(|| "none declared".to_string())
                ),
                "invalid_request_error".to_string(),
                "messages".to_string(),
            )),
        )
            .into_response();
    }
    let usage_mode = chat_stream_usage_mode(&request, model_attestation_supported, e2ee_active);
    let rewrite_public_stream_usage = usage_mode.rewrite_public_stream_usage;
    let strip_intermediate_usage = usage_mode.strip_intermediate_usage;

    // Auto-redact (opt-in via x-auto-redact header or auto_redact body field).
    // On success this may rewrite service_request.messages to substitute
    // placeholders for PII; the returned map drives the response un-redact.
    let (auto_redact_requested, redaction_map, auto_redact_classify_tokens) = match maybe_redact(
        &headers,
        &mut service_request,
        &app_state.inference_provider_pool,
    )
    .await
    {
        Ok(out) => out,
        Err(e) => return auto_redact_error_response(e),
    };
    // Bill the privacy-filter classify pass that auto-redact ran (it is a
    // real inference call to the PII model). Charged like an explicit
    // `/v1/privacy/classify`. Spawned (not awaited) and time-bounded so a
    // slow/exhausted billing DB or model lookup can never stall the user's
    // completion before dispatch — the classify already succeeded, and the
    // chat bills separately via InterceptStream. See nearai/cloud-api#602.
    if auto_redact_requested {
        let app_state = app_state.clone();
        let api_key = api_key.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(
                AUTO_REDACT_BILL_TIMEOUT,
                bill_auto_redact_classify(&app_state, &api_key, auto_redact_classify_tokens),
            )
            .await
            .is_err()
            {
                tracing::error!("auto_redact: classify billing timed out; usage not recorded");
            }
        });
    }
    // Treat auto-redact as effectively *enabled* only when the detector
    // actually minted placeholders. A request that opts in but contains no
    // PII has nothing to substitute, so we skip the response re-serialize
    // (preserves raw-bytes signing) and the streaming wrap (preserves the
    // existing debug log path).
    let auto_redact_enabled = auto_redact_requested && !redaction_map.is_empty();
    if auto_redact_enabled {
        // The typed messages now contain placeholders. Do not let the
        // Anthropic wire adapter bypass that mutation with the original body.
        service_request.original_request = None;
    }

    // Non-attested (Incognito) models have no model TEE signature to collect.
    // Sign their public request/response bytes at the Gateway even when neither
    // body was rewritten. Missing catalog metadata is not a non-attested model.
    let non_attested_requires_gateway_signature = model_attestation_supported == Some(false);
    // Auto-redact and alias handling can change the bytes returned to the
    // client. A provider signature covers the upstream request and response,
    // so it cannot verify those public bytes.
    let alias_requires_gateway_signature =
        alias_canonical.is_some() && model_attestation_supported.unwrap_or(false);
    let gateway_signature_enabled = non_attested_requires_gateway_signature
        || usage_mode.gateway_signature_enabled
        || auto_redact_requires_gateway_signature(auto_redact_enabled, model_attestation_supported)
        || alias_requires_gateway_signature;
    let public_response_rewritten = auto_redact_enabled || alias_canonical.is_some();
    // A raw stream may also require a Gateway signature at EOF: if its provider
    // omitted `[DONE]`, the route appends one for the client.
    // Hash attested streams as they are emitted so that tail path can decide
    // whether the final client-visible bytes need a Gateway signature.
    let may_need_synthesized_done_gateway_signature =
        synthesized_done_requires_gateway_signature(model_attestation_supported, e2ee_active);
    let hash_client_visible_stream =
        gateway_signature_enabled || may_need_synthesized_done_gateway_signature;
    // Never publish a provider signature over bytes that this route rewrites.
    // The actual provider can additionally require Gateway signing once the
    // stream starts, even if no public bytes need rewriting.
    service_request.skip_provider_chat_signature =
        gateway_signature_enabled || public_response_rewritten;
    // Defer an upstream terminator whenever this route would otherwise relay it
    // unchanged. The completion service owns the authoritative model lookup, so
    // it decides whether finalization stores a provider signature or is a no-op.
    // That keeps the signature available before clients observe `[DONE]` even
    // when the route-level model metadata cache is unavailable or stale.
    let defer_upstream_terminal = defer_upstream_terminal(
        gateway_signature_enabled,
        public_response_rewritten,
        rewrite_public_stream_usage,
        strip_intermediate_usage,
    );
    let redaction_map = Arc::new(redaction_map);

    // Check if streaming is requested
    if request.stream == Some(true) {
        // Call the streaming completion service
        match app_state
            .completion_service
            .create_chat_completion_stream(service_request)
            .await
        {
            Ok(stream) => {
                // Make stream peekable to extract chat_id for Inference-Id header
                let mut peekable_stream = Box::pin(stream.peekable());

                // Peek for the Inference-Id header. Control events (e.g. an
                // upstream keepalive comment) may precede the first data
                // chunk; consume and stash them so they are still forwarded
                // to the client in order (they're part of the signed byte
                // stream — issue #701). Bounded by MAX_LEADING_CONTROL_EVENTS
                // so a misbehaving upstream that only emits keepalives can't
                // stall response start or grow this buffer unbounded — past
                // the cap we proceed without an Inference-Id and let the rest
                // flow through the byte stream below.
                let mut leading_control: Vec<
                    Result<inference_providers::SSEEvent, inference_providers::CompletionError>,
                > = Vec::new();
                // Raw chat_id string captured alongside the hashed UUID so we can
                // look up the serving-provider tier from the pool's chat_id mapping.
                let mut stream_chat_id: Option<String> = None;
                let inference_id = loop {
                    let is_control = match peekable_stream.as_mut().peek().await {
                        Some(Ok(event)) => {
                            if let Some(chunk) = &event.chunk {
                                // Capture the raw chat_id for the tier lookup below.
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
                        if leading_control.len() >= MAX_LEADING_CONTROL_EVENTS {
                            break None;
                        }
                        if let Some(ev) = peekable_stream.next().await {
                            leading_control.push(ev);
                        }
                    }
                };

                // The model may have fallen back to Chutes, which attests its
                // deployment but does not sign individual responses. Decide
                // from the actual serving provider before emitting any bytes,
                // including the leading control events buffered above.
                let serving_provider = match stream_chat_id.as_deref() {
                    Some(chat_id) => {
                        app_state
                            .inference_provider_pool
                            .get_provider_by_chat_id(chat_id)
                            .await
                    }
                    None => None,
                };
                let provider_requires_gateway_signature = match &serving_provider {
                    Some(provider) => !provider.supports_chat_signatures(),
                    // Missing provider information does not imply missing signature support.
                    None => false,
                };
                let gateway_signature_enabled =
                    gateway_signature_enabled || provider_requires_gateway_signature;
                let hash_client_visible_stream =
                    hash_client_visible_stream || provider_requires_gateway_signature;

                // Warning to inject into the first streamed chunk. Skipped
                // for E2EE (the chunks are opaque; the response header is
                // the only signal there).
                let alias_warning_pending: Arc<std::sync::Mutex<Option<String>>> =
                    Arc::new(std::sync::Mutex::new(
                        alias_canonical
                            .as_ref()
                            .filter(|_| !e2ee_active)
                            .map(|canonical| alias_warning_message(&request.model, canonical)),
                    ));
                // Alias-served responses can't use byte-exact passthrough: the
                // first chunk is rewritten to carry the warning, and the TEE
                // signs under the canonical model name anyway.
                let alias_served = alias_canonical.is_some();

                if inference_id.is_none() {
                    tracing::warn!(
                        organization_id = %api_key.organization.id.0,
                        model = %request.model,
                        "Could not extract inference ID from first chunk for chat completion (streaming)"
                    );
                }

                let stream_error_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

                let error_count_clone = stream_error_count.clone();
                let request_model = request.model.clone();
                let organization_id = api_key.organization.id.0;

                // Set when the upstream's own `data: [DONE]` terminator was
                // observed, so the end-of-stream tail doesn't append a
                // gateway-minted one. The original terminal bytes may be held
                // until completion-service finalization finishes.
                let upstream_done_forwarded = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let upstream_done_for_chain = upstream_done_forwarded.clone();
                let holding_provider_terminal_suffix =
                    Arc::new(std::sync::atomic::AtomicBool::new(false));
                let holding_provider_terminal_suffix_for_chain =
                    holding_provider_terminal_suffix.clone();
                let held_provider_terminal_bytes =
                    Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
                let held_provider_terminal_bytes_for_chain = held_provider_terminal_bytes.clone();

                // Per-stream un-redact state, keyed by choice index so n>1
                // completions don't cross-contaminate sliding tails. When
                // auto-redact is off, the map stays empty and we skip the
                // mutex hop entirely.
                let unredact_states: Arc<tokio::sync::Mutex<StreamUnredactStates>> =
                    Arc::new(tokio::sync::Mutex::new(StreamUnredactStates::default()));
                // Capture the first chunk's metadata so the end-of-stream
                // flush can synthesize a final SSE chunk with matching
                // id/model/created.
                let chunk_template: Arc<tokio::sync::Mutex<ChunkTemplate>> =
                    Arc::new(tokio::sync::Mutex::new(None));
                let chunk_template_for_chain = chunk_template.clone();
                let unredact_states_for_chain = unredact_states.clone();
                let redaction_map_for_chunks = redaction_map.clone();
                let final_stream_usage = Arc::new(tokio::sync::Mutex::new(
                    None::<inference_providers::TokenUsage>,
                ));
                let final_stream_usage_for_chain = final_stream_usage.clone();
                let public_signature_hasher = Arc::new(tokio::sync::Mutex::new(Sha256::new()));
                let public_signature_chat_id =
                    Arc::new(tokio::sync::Mutex::new(stream_chat_id.clone()));
                let public_signature_hasher_for_chain = public_signature_hasher.clone();
                let public_signature_chat_id_for_chain = public_signature_chat_id.clone();
                let attestation_service_for_chain = app_state.attestation_service.clone();

                // Re-attach any stashed leading control events, then convert
                // to a raw bytes stream.
                let event_stream = futures::stream::iter(leading_control).chain(peekable_stream);

                let byte_stream = event_stream
                    .filter_map(move |result| {
                        let error_count_inner = error_count_clone.clone();
                        let model_for_err = request_model.clone();
                        let states = unredact_states.clone();
                        let template = chunk_template.clone();
                        let map = redaction_map_for_chunks.clone();
                        let pending_warning = alias_warning_pending.clone();
                        let upstream_done = upstream_done_forwarded.clone();
                        let include_stream_usage_in_response = include_stream_usage_in_response;
                        let rewrite_public_stream_usage = rewrite_public_stream_usage;
                        let strip_intermediate_usage = strip_intermediate_usage;
                        let gateway_signature_enabled = gateway_signature_enabled;
                        let defer_upstream_terminal = defer_upstream_terminal;
                        let holding_provider_terminal_suffix =
                            holding_provider_terminal_suffix.clone();
                        let held_provider_terminal_bytes = held_provider_terminal_bytes.clone();
                        let hash_client_visible_stream = hash_client_visible_stream;
                        let public_signature_hasher = public_signature_hasher.clone();
                        let public_signature_chat_id = public_signature_chat_id.clone();
                        let final_stream_usage = final_stream_usage.clone();
                        async move {
                            match result {
                                Ok(event) => {
                                    if holding_provider_terminal_suffix
                                        .load(std::sync::atomic::Ordering::Relaxed)
                                    {
                                        let is_separator = event.chunk.is_none()
                                            && !event.raw_bytes.is_empty()
                                            && event
                                                .raw_bytes
                                                .iter()
                                                .all(|byte| matches!(*byte, b'\r' | b'\n'));
                                        let mut held = held_provider_terminal_bytes.lock().await;
                                        if is_separator
                                            && try_append_held_provider_terminal_bytes(
                                                &mut held,
                                                &event.raw_bytes,
                                            )
                                        {
                                            return None;
                                        }

                                        held.clear();
                                        drop(held);
                                        holding_provider_terminal_suffix.store(
                                            false,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        let error = inference_providers::CompletionError::CompletionError(
                                            "Malformed SSE stream after [DONE]".into(),
                                        );
                                        let count = error_count_inner.fetch_add(
                                            1,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        if count == 0 {
                                            tracing::error!(
                                                %organization_id,
                                                model = %model_for_err,
                                                "Completion stream emitted bytes after its terminal marker"
                                            );
                                        }
                                        return Some(Ok::<Bytes, Infallible>(sse_error_frame(&error)));
                                    }

                                    if event.is_done_marker() && defer_upstream_terminal {
                                        upstream_done.store(
                                            true,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        let mut held = held_provider_terminal_bytes.lock().await;
                                        if try_append_held_provider_terminal_bytes(
                                            &mut held,
                                            &event.raw_bytes,
                                        ) {
                                            holding_provider_terminal_suffix.store(
                                                true,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                            return None;
                                        }

                                        held.clear();
                                        drop(held);
                                        let error = inference_providers::CompletionError::CompletionError(
                                            "Malformed SSE stream after [DONE]".into(),
                                        );
                                        let count = error_count_inner.fetch_add(
                                            1,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        if count == 0 {
                                            tracing::error!(
                                                %organization_id,
                                                model = %model_for_err,
                                                "Completion stream terminal marker exceeded the held suffix limit"
                                            );
                                        }
                                        return Some(Ok::<Bytes, Infallible>(sse_error_frame(&error)));
                                    }
                                    // Byte-exact passthrough (issue #701): when no public
                                    // chunk rewriting is active, forward the upstream wire
                                    // bytes untouched. Explicit include_usage shaping needs
                                    // parsed-chunk serialization; encrypted and
                                    // continuous-usage streams preserve passthrough.
                                    //
                                    // Disabled for alias-served responses: those inject
                                    // a top-level `warning` into the first chunk (below),
                                    // and are non-byte-verifiable anyway since the TEE
                                    // signs under the canonical model name, not the alias
                                    // the client requested.
                                    //
                                    // Also disabled when strip_intermediate_usage is active:
                                    // per OpenAI spec intermediate chunks must carry
                                    // `usage: null`, so we re-serialize to null it out.
                                    if event.raw_passthrough
                                        && !auto_redact_enabled
                                        && !alias_served
                                        && !rewrite_public_stream_usage
                                        && !strip_intermediate_usage
                                    {
                                        if hash_client_visible_stream {
                                            if let Some(chunk) = &event.chunk {
                                                let candidate = match chunk {
                                                    inference_providers::StreamChunk::Chat(chunk) => {
                                                        chunk.id.clone()
                                                    }
                                                    inference_providers::StreamChunk::Text(chunk) => {
                                                        chunk.id.clone()
                                                    }
                                                };
                                                let mut chat_id =
                                                    public_signature_chat_id.lock().await;
                                                if chat_id.is_none() && !candidate.is_empty() {
                                                    *chat_id = Some(candidate);
                                                }
                                            }
                                        }
                                        if event.is_done_marker() {
                                            if gateway_signature_enabled {
                                                return None;
                                            }
                                            upstream_done
                                                .store(true, std::sync::atomic::Ordering::Relaxed);
                                        }
                                        if hash_client_visible_stream {
                                            public_signature_hasher
                                                .lock()
                                                .await
                                                .update(&event.raw_bytes);
                                        }
                                        return Some(Ok::<Bytes, Infallible>(event.raw_bytes));
                                    }

                                    // Re-serialization path: auto-redact rewrites chunk
                                    // text, and non-OpenAI upstreams (Gemini native,
                                    // Anthropic) need normalization to OpenAI format.
                                    // Control lines carry no parsed payload; forward
                                    // them raw so keepalives/comments are preserved.
                                    // Hold [DONE] back for rewritten streams
                                    // (redacted, usage-rewritten, or usage-stripped)
                                    // so the tail can emit final usage, store any
                                    // gateway signature, and only then append
                                    // [DONE] — a client that fetches the signature
                                    // the moment it sees [DONE] must not race the
                                    // store.
                                    let Some(mut chunk) = event.chunk else {
                                        if event.is_done_marker() {
                                            if gateway_signature_enabled
                                                || auto_redact_enabled
                                                || rewrite_public_stream_usage
                                                || strip_intermediate_usage
                                            {
                                                return None;
                                            }
                                            upstream_done.store(
                                                true,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                        }
                                        let control_bytes = if rewrite_public_stream_usage {
                                            rewritten_control_event_bytes(&event)
                                        } else {
                                            Some(event.raw_bytes)
                                        };
                                        if let Some(control_bytes) = control_bytes {
                                            if hash_client_visible_stream {
                                                public_signature_hasher
                                                    .lock()
                                                    .await
                                                    .update(&control_bytes);
                                            }
                                            return Some(Ok::<Bytes, Infallible>(control_bytes));
                                        }
                                        return None;
                                    };
                                    if hash_client_visible_stream || public_response_rewritten {
                                        let candidate = match &chunk {
                                            inference_providers::StreamChunk::Chat(chunk) => {
                                                chunk.id.clone()
                                            }
                                            inference_providers::StreamChunk::Text(chunk) => {
                                                chunk.id.clone()
                                            }
                                        };
                                        let mut chat_id = public_signature_chat_id.lock().await;
                                        if chat_id.is_none() && !candidate.is_empty() {
                                            *chat_id = Some(candidate);
                                        }
                                    }
                                    if let inference_providers::StreamChunk::Chat(chat) = &chunk {
                                        {
                                            let mut t = template.lock().await;
                                            if t.is_none() {
                                                *t = Some((
                                                    chat.id.clone(),
                                                    chat.model.clone(),
                                                    chat.created,
                                                    chat.system_fingerprint.clone(),
                                                ));
                                            }
                                        }
                                    }
                                    if rewrite_public_stream_usage {
                                        let mut final_usage = final_stream_usage.lock().await;
                                        if !prepare_stream_chunk_for_client(
                                            &mut chunk,
                                            include_stream_usage_in_response,
                                            &mut final_usage,
                                        ) {
                                            return None;
                                        }
                                    } else if strip_intermediate_usage {
                                        // Default streaming and include_usage:false: null out
                                        // usage on all chunks (usage-only chunks are dropped).
                                        // No final usage chunk is emitted (include_usage:false).
                                        let mut discarded = None;
                                        if !prepare_stream_chunk_for_client(
                                            &mut chunk,
                                            false, // include_usage = false
                                            &mut discarded,
                                        ) {
                                            return None;
                                        }
                                    }

                                    // This chunk is re-serialized, so usage it still
                                    // carries (e.g. continuous usage stats on an alias
                                    // or auto-redact stream) also reports the reasoning
                                    // count where OpenAI-compatible clients read it.
                                    let chunk_usage = match &mut chunk {
                                        inference_providers::StreamChunk::Chat(chat) => {
                                            chat.usage.as_mut()
                                        }
                                        inference_providers::StreamChunk::Text(text) => {
                                            text.usage.as_mut()
                                        }
                                    };
                                    if let Some(usage) = chunk_usage {
                                        usage.ensure_standard_reasoning_details();
                                    }

                                    if auto_redact_enabled {
                                        // Swap minted placeholders in this
                                        // chunk's text deltas back to originals.
                                        let mut s = states.lock().await;
                                        unredact_chunk_in_place(&mut chunk, &mut s, &map);
                                    }

                                    // Serialize the parsed chunk (normalized to OpenAI format)
                                    // instead of forwarding raw provider bytes, which may be
                                    // in a provider-specific format (e.g. Gemini native).
                                    // The first chunk of an alias-served response gets a
                                    // top-level "warning" so the substitution isn't silent.
                                    let alias_warning =
                                        pending_warning.lock().ok().and_then(|mut g| g.take());
                                    let json_data = match alias_warning {
                                        Some(warning) => {
                                            serde_json::to_value(&chunk).map(|mut v| {
                                                if let Some(obj) = v.as_object_mut() {
                                                    obj.insert(
                                                        "warning".to_string(),
                                                        serde_json::Value::String(warning),
                                                    );
                                                }
                                                v.to_string()
                                            })
                                        }
                                        None => serde_json::to_string(&chunk),
                                    }
                                    .unwrap_or_else(|e| {
                                        tracing::error!(
                                            %organization_id,
                                            "Failed to serialize stream chunk: {e}"
                                        );
                                        "{}".to_string()
                                    });
                                    let (
                                        serialized_chunk_len,
                                        choices_count,
                                        finish_reason_count,
                                        usage_present,
                                    ) = match &chunk {
                                        inference_providers::StreamChunk::Chat(chat) => (
                                            json_data.len(),
                                            chat.choices.len(),
                                            chat.choices
                                                .iter()
                                                .filter(|choice| choice.finish_reason.is_some())
                                                .count(),
                                            chat.usage.is_some(),
                                        ),
                                        inference_providers::StreamChunk::Text(text) => (
                                            json_data.len(),
                                            text.choices.len(),
                                            text.choices
                                                .iter()
                                                .filter(|choice| choice.finish_reason.is_some())
                                                .count(),
                                            text.usage.is_some(),
                                        ),
                                    };
                                    // Suppress per-chunk debug logging when
                                    // auto_redact is enabled: the chunk now
                                    // holds the user's original PII (we just
                                    // un-redacted it). Logging it would
                                    // defeat the privacy guarantee at debug.
                                    if !auto_redact_enabled {
                                        tracing::debug!(
                                            %request_id,
                                            serialized_chunk_len,
                                            choices_count,
                                            has_choices = choices_count > 0,
                                            finish_reason_count,
                                            has_finish_reason = finish_reason_count > 0,
                                            usage_present,
                                            "Completion stream event serialized"
                                        );
                                    }
                                    // Format as SSE event with proper newlines
                                    let sse_bytes = Bytes::from(format!("data: {json_data}\n\n"));
                                    if hash_client_visible_stream {
                                        public_signature_hasher
                                            .lock()
                                            .await
                                            .update(&sse_bytes);
                                    }
                                    Some(Ok::<Bytes, Infallible>(sse_bytes))
                                }
                                Err(e) => {
                                    let count = error_count_inner
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if count == 0 {
                                        tracing::error!(
                                            %organization_id,
                                            model = %model_for_err,
                                            error_type = %completion_stream_error_category(&e),
                                            "Completion stream error"
                                        );
                                    }
                                    Some(Ok::<Bytes, Infallible>(sse_error_frame(&e)))
                                }
                            }
                        }
                    })
                    .chain(
                        futures::stream::once({
                            // End-of-stream tail: emit any flush chunks (held
                            // tail bytes that the sliding window didn't get to
                            // resolve) inline with [DONE]. They're framed as
                            // separate SSE events by `\n\n` so the client sees
                            // them as distinct deltas. When the upstream's own
                            // [DONE] was already forwarded verbatim
                            // (passthrough), nothing is appended — the byte
                            // stream must end exactly as the upstream's did.
                            let organization_id = api_key.organization.id.0;
                            let model_name = request.model.clone();
                            let request_hash = request_hash.clone();
                            async move {
                                let mut combined: Vec<u8> = Vec::new();
                                let error_count_final =
                                    stream_error_count.load(std::sync::atomic::Ordering::Relaxed);
                                if auto_redact_enabled {
                                    let mut states = unredact_states_for_chain.lock().await;
                                    let template = chunk_template_for_chain.lock().await.clone();
                                    for bytes in build_flush_chunks(&mut states, &template) {
                                        combined.extend_from_slice(&bytes);
                                    }
                                }

                                if rewrite_public_stream_usage && include_stream_usage_in_response {
                                    let final_usage = final_stream_usage_for_chain.lock().await.clone();
                                    let template = chunk_template_for_chain.lock().await.clone();
                                    if error_count_final > 0 {
                                        if final_usage.is_some() {
                                            tracing::warn!(
                                                %organization_id,
                                                model = %model_name,
                                                total_stream_errors = error_count_final,
                                                "Suppressing final usage chunk because the stream ended with errors"
                                            );
                                        }
                                    } else {
                                        match final_usage {
                                            Some(usage) => match build_final_usage_chunk_bytes(usage, &template) {
                                                Ok(Some(bytes)) => {
                                                    combined.extend_from_slice(&bytes);
                                                }
                                                Ok(None) => {
                                                    tracing::warn!(
                                                        %organization_id,
                                                        model = %model_name,
                                                        "Cannot emit final usage chunk: no chat chunk template observed"
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        %organization_id,
                                                        model = %model_name,
                                                        "Failed to serialize final usage chunk: {e}"
                                                    );
                                                }
                                            },
                                            None => {
                                                tracing::warn!(
                                                    %organization_id,
                                                    model = %model_name,
                                                    "include_usage was requested but upstream did not provide usage; omitting final usage chunk"
                                                );
                                            }
                                        }
                                    }
                                }

                                if error_count_final > 1 {
                                    tracing::error!(
                                        %organization_id,
                                        model = %model_name,
                                        total_stream_errors = error_count_final,
                                        "Completion stream ended with multiple errors"
                                    );
                                }

                                let synthesized_done = !upstream_done_for_chain
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                if synthesized_done {
                                    combined.extend_from_slice(b"data: [DONE]\n\n");
                                } else if error_count_final == 0
                                    && defer_upstream_terminal
                                    && holding_provider_terminal_suffix_for_chain
                                        .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    let held = {
                                        let mut bytes = held_provider_terminal_bytes_for_chain
                                            .lock()
                                            .await;
                                        std::mem::take(&mut *bytes)
                                    };
                                    combined.extend_from_slice(&held);
                                }

                                if gateway_signature_enabled {
                                    // The completion stream releases its
                                    // provider-signature routing pin before this
                                    // tail runs. Gateway signing only covers the
                                    // final public bytes and therefore does not
                                    // need that pinned provider connection.
                                    let chat_id =
                                        public_signature_chat_id_for_chain.lock().await.clone();
                                    if error_count_final == 0 {
                                        let response_hash = {
                                            let mut hasher =
                                                public_signature_hasher_for_chain.lock().await;
                                            hasher.update(&combined);
                                            hex::encode(hasher.clone().finalize())
                                        };

                                        if let Some(chat_id) = chat_id {
                                            match tokio::time::timeout(
                                                Duration::from_secs(
                                                    STREAM_SIGNATURE_STORE_TIMEOUT_SECS,
                                                ),
                                                attestation_service_for_chain.store_chat_signature(
                                                    &chat_id,
                                                    request_hash.clone(),
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
                                                    "Failed to store public stream chat signature"
                                                ),
                                                Err(_) => tracing::error!(
                                                    %organization_id,
                                                    model = %model_name,
                                                    "Timed out storing public stream chat signature"
                                                ),
                                            }
                                        } else {
                                            tracing::warn!(
                                                %organization_id,
                                                model = %model_name,
                                                "Cannot store public stream chat signature: no chat_id observed"
                                            );
                                        }
                                    }
                                } else if synthesized_done
                                    && may_need_synthesized_done_gateway_signature
                                    && error_count_final == 0
                                {
                                    // The service has already released the provider
                                    // routing pin for this EOF-only path. Store a
                                    // Gateway signature over the exact final stream
                                    // without taking ownership of that pin again.
                                    let chat_id =
                                        public_signature_chat_id_for_chain.lock().await.clone();
                                    if let Some(chat_id) = chat_id {
                                        let response_hash = {
                                            let mut hasher =
                                                public_signature_hasher_for_chain.lock().await;
                                            hasher.update(&combined);
                                            hex::encode(hasher.clone().finalize())
                                        };
                                        match tokio::time::timeout(
                                            Duration::from_secs(
                                                STREAM_SIGNATURE_STORE_TIMEOUT_SECS,
                                            ),
                                            attestation_service_for_chain.store_chat_signature(
                                                &chat_id,
                                                request_hash.clone(),
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
                                                "Failed to store synthesized-done stream signature"
                                            ),
                                            Err(_) => tracing::error!(
                                                %organization_id,
                                                model = %model_name,
                                                "Timed out storing synthesized-done stream signature"
                                            ),
                                        }
                                    } else {
                                        tracing::warn!(
                                            %organization_id,
                                            model = %model_name,
                                            "Cannot store synthesized-done stream signature: no chat_id observed"
                                        );
                                    }
                                }
                                if combined.is_empty() {
                                    // Avoid emitting an empty body frame.
                                    None
                                } else {
                                    Some(Ok::<Bytes, Infallible>(Bytes::from(combined)))
                                }
                            }
                        })
                        .filter_map(std::future::ready),
                    );

                let serving = serving_provider
                    .as_ref()
                    .map(|provider| inference_providers::ServingProvider::of(provider.as_ref()));

                // Return raw streaming response with SSE headers
                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive");

                // Collect CORS-exposed header names so the
                // Access-Control-Expose-Headers value is a single
                // comma-joined list (repeated header lines are not
                // consistently merged by browsers).
                let mut exposed_headers: Vec<&str> = Vec::new();

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }

                // Surface which provider tier served this streaming completion.
                if let Some(serving) = serving {
                    response_builder =
                        response_builder.header(HEADER_SERVING_PROVIDER, serving.label());
                    exposed_headers.push(HEADER_SERVING_PROVIDER);
                }

                // Announce alias substitution so it is never silent (issue #573).
                // Guarded HeaderValue construction: a header-invalid byte in a
                // model name must not panic the `.body().unwrap()` below.
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
        // Call the non-streaming completion service
        match app_state
            .completion_service
            .create_chat_completion(service_request)
            .await
        {
            Ok(mut response_with_bytes) => {
                // Extract inference ID from response ID (reuse same hashing as usage tracking)
                let inference_id =
                    Some(hash_inference_id_to_uuid(&response_with_bytes.response.id));

                // When auto-redact is enabled, we substitute placeholders back to
                // originals and re-serialize. The provider's raw_bytes are over the
                // redacted form; we deliberately drop that signed payload because
                // the client opted into munging the response.
                let body_bytes = if auto_redact_enabled {
                    unredact_chat_response_in_place(
                        &mut response_with_bytes.response,
                        &redaction_map,
                    );
                    // This body is re-serialized, so no provider signature covers
                    // it; expose the reasoning count where OpenAI-compatible
                    // clients read it.
                    response_with_bytes
                        .response
                        .usage
                        .ensure_standard_reasoning_details();
                    match serde_json::to_vec(&response_with_bytes.response) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to re-serialize unredacted chat response");
                            app_state
                                .attestation_service
                                .release_chat_signature_pin(&response_with_bytes.response.id)
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
                    }
                } else {
                    // Return the exact bytes from the provider for hash verification.
                    // This ensures clients can hash the response and compare with
                    // attestation endpoints.
                    response_with_bytes.raw_bytes
                };

                // Annotate alias-served responses with a top-level "warning"
                // (issue #573). This re-serializes the body, so — like
                // auto-redact — it deliberately gives up raw-bytes hash
                // verification for these responses; clients that need the
                // raw-bytes guarantee should send the canonical model name
                // (or x-no-aliasing). Since the body is rewritten, the
                // reasoning count is also mirrored into the standard usage
                // field. E2EE bodies are opaque and are left untouched
                // (inject_chat_warning_field returns None for them, and we
                // don't attempt it) — the header below is the signal.
                let body_bytes = match &alias_canonical {
                    Some(canonical) if !e2ee_active => inject_chat_warning_field(
                        &body_bytes,
                        &alias_warning_message(&request.model, canonical),
                    )
                    .unwrap_or(body_bytes),
                    _ => body_bytes,
                };

                // Attestation support does not imply per-response signatures:
                // Chutes needs a Gateway receipt even for unchanged JSON. The
                // pool records the actual serving provider before returning,
                // so this also covers NEAR-to-Chutes fallback.
                if gateway_signature_enabled
                    || app_state
                        .inference_provider_pool
                        .get_provider_by_chat_id(&response_with_bytes.response.id)
                        .await
                        .is_some_and(|provider| !provider.supports_chat_signatures())
                {
                    let response_hash = hex::encode(Sha256::digest(&body_bytes));
                    if let Err(error) = app_state
                        .attestation_service
                        .store_chat_signature_and_unpin(
                            &response_with_bytes.response.id,
                            request_hash.clone(),
                            response_hash,
                        )
                        .await
                    {
                        tracing::error!(
                            chat_id = %response_with_bytes.response.id,
                            error = %error,
                            "Failed to store public chat completion signature"
                        );
                    }
                } else if public_response_rewritten {
                    app_state
                        .attestation_service
                        .release_chat_signature_pin(&response_with_bytes.response.id)
                        .await;
                }

                let mut response_builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json");

                // Collect CORS-exposed header names so the
                // Access-Control-Expose-Headers value is a single
                // comma-joined list (repeated header lines are not
                // consistently merged by browsers).
                let mut exposed_headers: Vec<&str> = Vec::new();

                // Add Inference-Id header if available
                if let Some(uuid) = inference_id {
                    response_builder =
                        response_builder.header(HEADER_INFERENCE_ID, uuid.to_string());
                    exposed_headers.push(HEADER_INFERENCE_ID);
                }

                // Surface which provider tier served this non-streaming completion.
                response_builder = response_builder
                    .header(HEADER_SERVING_PROVIDER, response_with_bytes.serving.label());
                exposed_headers.push(HEADER_SERVING_PROVIDER);

                // Announce alias substitution so it is never silent (issue #573).
                // Guarded HeaderValue construction: a header-invalid byte in a
                // model name must not panic the `.body().unwrap()` below.
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
