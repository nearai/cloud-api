use super::*;

#[test]
fn chat_token_limits_are_normalized_before_dispatch() {
    use serde_json::json;

    for (limits, expected) in [
        (json!({"max_completion_tokens": 128}), Some(128)),
        (json!({"max_tokens": 64}), Some(64)),
        (
            json!({"max_completion_tokens": 128, "max_tokens": 64}),
            Some(128),
        ),
        (
            json!({"max_completion_tokens": null, "max_tokens": 64}),
            Some(64),
        ),
        (json!({"max_completion_tokens": null}), None),
        (json!({}), None),
    ] {
        let mut body = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hi"}],
        });
        body.as_object_mut()
            .unwrap()
            .extend(limits.as_object().unwrap().clone());
        let request: ChatCompletionRequest = serde_json::from_value(body.clone()).unwrap();
        request.validate_request().unwrap();
        let service = convert_chat_request_to_service(
            &request,
            Uuid::nil(),
            Uuid::nil().to_string(),
            Uuid::nil(),
            Uuid::nil(),
            false,
            0,
            RequestBodyHash {
                hash: "original-request-hash".to_string(),
                body_bytes: serde_json::to_vec(&body).unwrap().into(),
            },
            Uuid::nil(),
        );
        assert_eq!(service.max_tokens, expected, "{body}");
        assert!(!service.extra.contains_key("max_completion_tokens"));
        // Preserve the original bytes/hash contract for signatures and wire adapters.
        assert_eq!(service.body_hash, "original-request-hash");
        assert_eq!(service.original_request, Some(body.clone()));

        let provider: inference_providers::ChatCompletionParams = request.into();
        assert_eq!(provider.max_tokens, expected, "{body}");
        assert_eq!(provider.max_completion_tokens, None);
        assert!(!provider.extra.contains_key("max_completion_tokens"));
    }
}

#[test]
fn session_hint_is_read_from_x_session_id_header() {
    let mut headers = header::HeaderMap::new();
    assert_eq!(session_hint_from_headers(&headers), None);

    headers.insert("X-Session-Id", header::HeaderValue::from_static("abc"));
    assert_eq!(session_hint_from_headers(&headers).as_deref(), Some("abc"));

    let mut bad = header::HeaderMap::new();
    bad.insert(
        SESSION_ID_HEADER,
        header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    );
    assert_eq!(session_hint_from_headers(&bad), None);
}

#[test]
fn model_public_key_alone_does_not_enable_e2ee() {
    let mut headers = crate::routes::common::EncryptionHeaders {
        signing_algo: None,
        client_pub_key: None,
        model_pub_key: None,
        encryption_version: None,
        encrypt_all_fields: None,
    };
    assert!(!e2ee_requested(&headers));
    headers.model_pub_key = Some("model routing key".to_string());
    assert!(!e2ee_requested(&headers));

    headers.client_pub_key = Some("client response key".to_string());
    assert!(e2ee_requested(&headers));
}

#[test]
fn encryption_parameters_still_enable_e2ee() {
    for field in ["signing_algo", "encryption_version", "encrypt_all_fields"] {
        let mut headers = crate::routes::common::EncryptionHeaders {
            signing_algo: None,
            client_pub_key: None,
            model_pub_key: Some("model routing key".to_string()),
            encryption_version: None,
            encrypt_all_fields: None,
        };
        match field {
            "signing_algo" => headers.signing_algo = Some("ed25519".to_string()),
            "encryption_version" => headers.encryption_version = Some("2".to_string()),
            "encrypt_all_fields" => headers.encrypt_all_fields = Some("true".to_string()),
            _ => unreachable!(),
        }
        assert!(e2ee_requested(&headers), "{field}");
    }
}

fn make_model_with_pricing(
    input_modalities: Option<Vec<String>>,
    output_modalities: Option<Vec<String>>,
) -> services::models::ModelWithPricing {
    services::models::ModelWithPricing {
        upcoming_pricing: None,
        id: uuid::Uuid::new_v4(),
        model_name: "test/model".to_string(),
        model_display_name: "Test Model".to_string(),
        model_description: "A test model".to_string(),
        model_icon: None,
        input_cost_per_token: 0,
        output_cost_per_token: 0,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 4096,
        verifiable: false,
        aliases: vec![],
        owned_by: "test".to_string(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: false,
        input_modalities,
        output_modalities,
        inference_url: None,
        hugging_face_id: None,
        quantization: None,
        max_output_length: Some(1024),
        supported_sampling_parameters: vec![],
        supported_features: vec![],
        datacenters: None,
        is_ready: None,
        deprecation_date: None,
        successor_model_name: None,
        openrouter_slug: None,
        is_listed: true,
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn model_with_pricing_to_info_emits_effective_output_limits_in_public_json() {
    let mut model = make_model_with_pricing(None, None);
    model.max_output_length = Some(2_048);

    let info = model_with_pricing_to_info(model);
    let json = serde_json::to_value(&info).unwrap();

    assert_eq!(json["max_output_length"], 2_048);
    assert_eq!(json["top_provider"]["max_completion_tokens"], 2_048);
}

#[test]
fn model_with_pricing_to_info_omits_output_limit_keys_when_none() {
    let mut model = make_model_with_pricing(None, None);
    model.max_output_length = None;

    let info = model_with_pricing_to_info(model);
    let json = serde_json::to_value(&info).unwrap();

    assert!(json.get("max_output_length").is_none());
    assert!(
            json["top_provider"].get("max_completion_tokens").is_none(),
            "serialized top_provider must omit max_completion_tokens when no effective output limit exists"
        );
}

#[test]
fn model_without_architecture_defaults_to_text_modalities() {
    // OpenRouter requires input_modalities / output_modalities. Models whose
    // architecture column was never backfilled (NULL modalities) must still
    // emit the text/text defaults so the required fields are never absent.
    let info = model_with_pricing_to_info(make_model_with_pricing(None, None));

    assert_eq!(info.input_modalities, Some(vec!["text".to_string()]));
    assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));

    let architecture = info
        .architecture
        .expect("architecture must be populated even without DB modalities");
    assert_eq!(architecture.input_modalities, vec!["text".to_string()]);
    assert_eq!(architecture.output_modalities, vec!["text".to_string()]);
}

#[test]
fn model_with_architecture_preserves_real_modalities() {
    // When the DB has real modalities they must pass through untouched
    // (both the flat fields and the nested architecture shape).
    let info = model_with_pricing_to_info(make_model_with_pricing(
        Some(vec!["text".to_string(), "image".to_string()]),
        Some(vec!["text".to_string()]),
    ));

    assert_eq!(
        info.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
    assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));

    let architecture = info.architecture.expect("architecture must be populated");
    assert_eq!(
        architecture.input_modalities,
        vec!["text".to_string(), "image".to_string()]
    );
    assert_eq!(architecture.output_modalities, vec!["text".to_string()]);
}

#[test]
fn model_without_cache_read_pricing_omits_input_cache_read() {
    let info = model_with_pricing_to_info(make_model_with_pricing(None, None));
    let json = serde_json::to_value(&info).unwrap();

    assert!(
        json["pricing"].get("input_cache_read").is_none(),
        "None means cache pricing is disabled and input_cache_read must be omitted"
    );
}

#[test]
fn model_with_cache_read_pricing_emits_positive_input_cache_read() {
    let mut model = make_model_with_pricing(None, None);
    model.cache_read_cost_per_token = Some(50_000);

    let info = model_with_pricing_to_info(model);
    let json = serde_json::to_value(&info).unwrap();

    assert_eq!(json["pricing"]["input_cache_read"], "0.00005");
}

#[test]
fn model_with_free_cache_read_pricing_emits_zero_input_cache_read() {
    // Some(0) is a genuinely free cache-read price — unlike None
    // (disabled), it must render as the string "0", not be omitted.
    let mut model = make_model_with_pricing(None, None);
    model.cache_read_cost_per_token = Some(0);

    let info = model_with_pricing_to_info(model);
    let json = serde_json::to_value(&info).unwrap();

    assert_eq!(json["pricing"]["input_cache_read"], "0");
}

#[test]
fn profiled_model_exposes_text_pricing_and_keeps_standard_short_projection() {
    let mut model = make_model_with_pricing(None, None);
    model.input_cost_per_token = 5_000;
    model.cache_read_cost_per_token = Some(500);
    model.output_cost_per_token = 30_000;
    model.text_pricing = Some(
        services::usage::TextPricingProfile::from_json(serde_json::json!({
            "version": 1,
            "currency": "USD",
            "unit": "million_tokens",
            "tiers": {
                "default": {"short": {
                    "uncachedInput": "5.00",
                    "cachedInput": "0.50",
                    "cacheWrite": "6.25",
                    "output": "30.00"
                }}
            }
        }))
        .unwrap(),
    );

    let json = serde_json::to_value(model_with_pricing_to_info(model)).unwrap();
    assert_eq!(json["textPricing"]["version"], 1);
    assert_eq!(json["pricing"]["input"], 5.0);
    assert_eq!(json["pricing"]["output"], 30.0);
    assert_eq!(json["pricing"]["prompt"], "0.000005");
    assert_eq!(json["pricing"]["input_cache_read"], "0.0000005");
}

#[test]
fn model_without_openrouter_slug_omits_nested_object() {
    // No override set → the public ModelInfo must not carry the nested
    // `openrouter` object at all (serde skips it when None).
    let info = model_with_pricing_to_info(make_model_with_pricing(None, None));
    assert!(
        info.openrouter.is_none(),
        "openrouter object must be omitted when no slug override is set"
    );
    // And it must not appear in the serialized JSON either.
    let json = serde_json::to_value(&info).unwrap();
    assert!(
        json.get("openrouter").is_none(),
        "serialized JSON must omit the openrouter key when unset"
    );
}

#[test]
fn model_with_openrouter_slug_emits_nested_object() {
    // Override set → the public ModelInfo must carry
    // `openrouter: { slug: <value> }`.
    let mut model = make_model_with_pricing(None, None);
    model.openrouter_slug = Some("z-ai/glm-5.1".to_string());
    let info = model_with_pricing_to_info(model);
    let openrouter = info
        .openrouter
        .as_ref()
        .expect("openrouter object must be present when slug override is set");
    assert_eq!(openrouter.slug, "z-ai/glm-5.1");

    let json = serde_json::to_value(&info).unwrap();
    assert_eq!(
        json["openrouter"]["slug"],
        serde_json::json!("z-ai/glm-5.1"),
        "serialized JSON must nest the slug under openrouter.slug"
    );
}

fn make_chat_chunk(id: &str) -> inference_providers::StreamChunk {
    inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: 1234567890,
        model: "test-model".to_string(),
        system_fingerprint: None,
        choices: vec![],
        usage: None,
        service_tier: None,
        prompt_token_ids: None,
        modality: None,
        extra: Default::default(),
    })
}

fn chat_request_with_include_usage(include_usage: Option<bool>) -> ChatCompletionRequest {
    let mut extra = std::collections::HashMap::new();
    if let Some(include_usage) = include_usage {
        extra.insert(
            "stream_options".to_string(),
            serde_json::json!({ "include_usage": include_usage }),
        );
    }
    ChatCompletionRequest {
        model: "test-model".to_string(),
        messages: vec![],
        max_completion_tokens: None,
        max_tokens: None,
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(true),
        stop: None,
        presence_penalty: None,
        frequency_penalty: None,
        service_tier: None,
        extra,
    }
}

fn chat_stream_chunk_with_usage(
    choices: Vec<inference_providers::models::ChatChoice>,
) -> inference_providers::models::ChatCompletionChunk {
    inference_providers::models::ChatCompletionChunk {
        id: "chatcmpl-test".to_string(),
        object: "chat.completion.chunk".to_string(),
        created: 1234567890,
        model: "test-model".to_string(),
        system_fingerprint: None,
        choices,
        usage: Some(inference_providers::models::TokenUsage::new(10, 5)),
        service_tier: None,
        prompt_token_ids: None,
        modality: None,
        extra: Default::default(),
    }
}

fn chat_stream_content_choice() -> inference_providers::models::ChatChoice {
    inference_providers::models::ChatChoice {
        index: 0,
        delta: Some(inference_providers::models::ChatDelta {
            role: None,
            content: Some("hello".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            extra: Default::default(),
        }),
        logprobs: None,
        finish_reason: None,
        token_ids: None,
    }
}

fn chat_stream_finish_choice() -> inference_providers::models::ChatChoice {
    inference_providers::models::ChatChoice {
        finish_reason: Some(inference_providers::models::FinishReason::Stop),
        ..chat_stream_content_choice()
    }
}

#[test]
fn chat_stream_include_usage_defaults_to_false() {
    let request = chat_request_with_include_usage(None);
    assert!(!chat_stream_include_usage_requested(&request));

    let request = chat_request_with_include_usage(Some(false));
    assert!(!chat_stream_include_usage_requested(&request));

    let request = chat_request_with_include_usage(Some(true));
    assert!(chat_stream_include_usage_requested(&request));
}

#[test]
fn chat_stream_continuous_usage_is_detected() {
    let mut request = chat_request_with_include_usage(Some(true));
    request.extra.insert(
        "stream_options".to_string(),
        serde_json::json!({
            "include_usage": true,
            "continuous_usage_stats": true
        }),
    );

    assert!(chat_stream_continuous_usage_requested(&request));
}

#[test]
fn auto_redact_requires_gateway_signature_for_attested_models() {
    assert!(auto_redact_requires_gateway_signature(true, Some(true)));
    assert!(!auto_redact_requires_gateway_signature(false, Some(true)));
    assert!(!auto_redact_requires_gateway_signature(true, Some(false)));
    assert!(!auto_redact_requires_gateway_signature(true, None));
}

#[test]
fn synthesized_done_does_not_change_e2ee_signature_kind() {
    assert!(synthesized_done_requires_gateway_signature(
        Some(true),
        false
    ));
    assert!(!synthesized_done_requires_gateway_signature(
        Some(true),
        true
    ));
    assert!(!synthesized_done_requires_gateway_signature(
        Some(false),
        false
    ));
    assert!(!synthesized_done_requires_gateway_signature(None, false));
}

#[test]
fn raw_streams_defer_their_terminal_without_model_metadata() {
    assert!(defer_upstream_terminal(false, false, false, false));
    assert!(!defer_upstream_terminal(true, false, false, false));
    assert!(!defer_upstream_terminal(false, true, false, false));
    assert!(!defer_upstream_terminal(false, false, true, false));
    assert!(!defer_upstream_terminal(false, false, false, true));
}

#[test]
fn oversized_padded_done_marker_is_not_held() {
    let padding = " ".repeat(MAX_HELD_PROVIDER_TERMINAL_BYTES);
    let event = inference_providers::SSEEvent {
        raw_bytes: Bytes::from(format!("data: [DONE]{padding}\n")),
        chunk: None,
        raw_passthrough: true,
    };
    assert!(
        event.is_done_marker(),
        "whitespace-padded marker is still DONE"
    );
    assert!(event.raw_bytes.len() > MAX_HELD_PROVIDER_TERMINAL_BYTES);

    let mut held = Vec::new();
    assert!(
        !try_append_held_provider_terminal_bytes(&mut held, &event.raw_bytes),
        "the initial terminal frame must obey the held suffix limit"
    );
    assert!(held.is_empty(), "rejected bytes must not be retained");
}

#[test]
fn include_usage_rewrites_and_signs_attested_streams() {
    let request = chat_request_with_include_usage(Some(true));
    let mode = chat_stream_usage_mode(&request, Some(true), false);

    assert!(mode.rewrite_public_stream_usage);
    assert!(mode.gateway_signature_enabled);
    // rewrite already handles usage shaping; strip is not additionally needed
    assert!(!mode.strip_intermediate_usage);
}

#[test]
fn include_usage_rewrites_non_attested_streams() {
    let request = chat_request_with_include_usage(Some(true));
    let mode = chat_stream_usage_mode(&request, Some(false), false);

    assert!(mode.rewrite_public_stream_usage);
    // Incognito signing is selected independently of usage shaping by the route.
    assert!(!mode.gateway_signature_enabled);
    assert!(!mode.strip_intermediate_usage);
}

#[test]
fn default_attested_stream_strips_usage_and_signs_at_gateway() {
    // Default streaming for ATTESTED models: the self-hosted upstream always
    // emits per-chunk usage plus a trailing usage-only chunk (the nearai
    // provider forces include_usage + continuous_usage_stats for billing), so
    // the route must strip usage and drop the trailing chunk per OpenAI spec.
    // Re-serializing breaks byte-exact provider-signature verification, so the
    // gateway signature covers the rewritten client-facing bytes instead —
    // same mechanism as the include_usage:true rewrite path.
    let request = chat_request_with_include_usage(None);
    let mode = chat_stream_usage_mode(&request, Some(true), false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(
        mode.strip_intermediate_usage,
        "attested default streams must strip usage per OpenAI spec"
    );
    assert!(
        mode.gateway_signature_enabled,
        "stripped attested streams must be gateway-signed so the stored \
             signature matches the bytes the client received"
    );
}

#[test]
fn include_usage_false_attested_stream_strips_usage_and_signs_at_gateway() {
    // Explicit include_usage:false for ATTESTED model: same as default —
    // strip usage, drop the trailing usage-only chunk, gateway-sign the
    // rewritten bytes.
    let request = chat_request_with_include_usage(Some(false));
    let mode = chat_stream_usage_mode(&request, Some(true), false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(mode.strip_intermediate_usage);
    assert!(mode.gateway_signature_enabled);
}

#[test]
fn default_non_attested_stream_strips_intermediate_usage() {
    // Non-attested (external) models: no TEE signature to preserve.
    // Strip usage so intermediate chunks carry `usage: null` per OpenAI spec.
    let request = chat_request_with_include_usage(None);
    let mode = chat_stream_usage_mode(&request, Some(false), false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(mode.strip_intermediate_usage);
}

#[test]
fn include_usage_false_non_attested_stream_strips_intermediate_usage() {
    let request = chat_request_with_include_usage(Some(false));
    let mode = chat_stream_usage_mode(&request, Some(false), false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(mode.strip_intermediate_usage);
}

#[test]
fn continuous_usage_stats_preserves_passthrough() {
    let mut request = chat_request_with_include_usage(None);
    request.extra.insert(
        "stream_options".to_string(),
        serde_json::json!({ "continuous_usage_stats": true }),
    );
    // continuous_usage_stats is an explicit request for per-chunk usage;
    // strip_intermediate_usage must be false so those stats flow through.
    let mode = chat_stream_usage_mode(&request, None, false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(!mode.strip_intermediate_usage);
}

#[test]
fn include_usage_true_with_metadata_lookup_failure_preserves_passthrough() {
    // Regression test: when the client explicitly requests include_usage:true but
    // model metadata is unavailable (model_attestation_supported == None, e.g. the
    // model is absent from the catalog or get_models_with_pricing() errored), both
    // rewrite_public_stream_usage and strip_intermediate_usage must be false so
    // the stream falls back to byte-exact passthrough. Before the fix, this path
    // could activate strip_intermediate_usage, silently dropping all usage from the
    // response — strictly worse than the prior passthrough behaviour.
    let request = chat_request_with_include_usage(Some(true));
    let mode = chat_stream_usage_mode(&request, None, false);

    assert!(
        !mode.rewrite_public_stream_usage,
        "rewrite requires model metadata; must be false when metadata unavailable"
    );
    assert!(
        !mode.strip_intermediate_usage,
        "explicit include_usage:true must never strip usage, even on metadata lookup failure"
    );
}

#[test]
fn default_stream_with_metadata_lookup_failure_preserves_passthrough() {
    // Default streaming with model metadata unavailable
    // (model_attestation_supported == None): fall back to byte-exact
    // passthrough. Without metadata we cannot know whether the stream needs
    // a gateway signature, so rewriting would either break provider-signature
    // verification (attested) or be applied blindly; passthrough is the safe
    // fallback, matching the include_usage:true metadata-failure behaviour.
    let request = chat_request_with_include_usage(None);
    let mode = chat_stream_usage_mode(&request, None, false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(
        !mode.strip_intermediate_usage,
        "metadata lookup failure must preserve raw passthrough"
    );
}

#[test]
fn include_usage_true_non_attested_does_not_strip() {
    // When include_usage:true and model_attestation_supported == Some(false),
    // rewrite_public_stream_usage handles usage shaping; strip must not also fire.
    let request = chat_request_with_include_usage(Some(true));
    let mode = chat_stream_usage_mode(&request, Some(false), false);

    assert!(
        mode.rewrite_public_stream_usage,
        "non-attested + include_usage:true should use the rewrite path"
    );
    assert!(
        !mode.strip_intermediate_usage,
        "strip must not fire when include_usage:true — client asked for usage"
    );
}

#[test]
fn continuous_usage_stats_opts_out_of_rewrite() {
    let mut request = chat_request_with_include_usage(Some(true));
    request.extra.insert(
        "stream_options".to_string(),
        serde_json::json!({
            "include_usage": true,
            "continuous_usage_stats": true
        }),
    );

    let passthrough = chat_stream_usage_mode(&request, Some(true), false);
    assert!(!passthrough.rewrite_public_stream_usage);
    assert!(!passthrough.gateway_signature_enabled);
    assert!(!passthrough.strip_intermediate_usage);

    let e2ee_passthrough = chat_stream_usage_mode(&request, Some(true), true);
    assert!(!e2ee_passthrough.rewrite_public_stream_usage);
    assert!(!e2ee_passthrough.strip_intermediate_usage);

    request.extra.insert(
        "modalities".to_string(),
        serde_json::json!(["text", "audio"]),
    );
    let audio_passthrough = chat_stream_usage_mode(&request, Some(true), false);
    assert!(!audio_passthrough.rewrite_public_stream_usage);
    assert!(!audio_passthrough.strip_intermediate_usage);
}

#[test]
fn e2ee_stream_preserves_passthrough() {
    // E2EE chunks are opaque; cannot modify them.
    let request = chat_request_with_include_usage(None);
    let mode = chat_stream_usage_mode(&request, Some(true), true);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(!mode.strip_intermediate_usage);
}

#[test]
fn non_text_modality_stream_preserves_passthrough() {
    let mut request = chat_request_with_include_usage(None);
    request.extra.insert(
        "modalities".to_string(),
        serde_json::json!(["text", "audio"]),
    );
    let mode = chat_stream_usage_mode(&request, Some(true), false);

    assert!(!mode.rewrite_public_stream_usage);
    assert!(!mode.gateway_signature_enabled);
    assert!(!mode.strip_intermediate_usage);
}

#[test]
fn e2ee_non_attested_stream_does_not_strip_intermediate_usage() {
    // Regression guard: E2EE chunks are opaque ciphertext — stripping usage
    // requires parsing and re-serializing, which would corrupt or discard
    // encrypted payload bytes. The `!e2ee_active` guard in
    // `chat_stream_usage_mode` must hold even when
    // `model_attestation_supported == Some(false)`, i.e. the non-attested
    // path that would otherwise activate strip_intermediate_usage.
    let request = chat_request_with_include_usage(None);
    let mode = chat_stream_usage_mode(&request, Some(false), true /* e2ee_active */);

    assert!(
        !mode.strip_intermediate_usage,
        "E2EE streams must never activate strip_intermediate_usage: \
             chunks are opaque ciphertext and cannot be re-serialized"
    );
}

#[test]
fn non_text_modality_non_attested_stream_does_not_strip_intermediate_usage() {
    // Regression guard: non-text modality streams (e.g. audio) carry binary
    // or provider-specific payloads that must not be re-serialized. The
    // `!chat_stream_has_non_text_modalities` guard in `chat_stream_usage_mode`
    // must hold even when `model_attestation_supported == Some(false)`.
    let mut request = chat_request_with_include_usage(None);
    request.extra.insert(
        "modalities".to_string(),
        serde_json::json!(["text", "audio"]),
    );
    let mode = chat_stream_usage_mode(&request, Some(false), false);

    assert!(
        !mode.strip_intermediate_usage,
        "non-text modality streams must never activate strip_intermediate_usage: \
             payloads may be binary/provider-specific and cannot be re-serialized"
    );
}

#[test]
fn chat_stream_non_text_modalities_preserve_raw_passthrough() {
    let mut request = chat_request_with_include_usage(None);
    assert!(!chat_stream_has_non_text_modalities(&request));

    request.extra.insert(
        "modalities".to_string(),
        serde_json::json!(["text", "audio"]),
    );
    assert!(chat_stream_has_non_text_modalities(&request));

    request
        .extra
        .insert("modalities".to_string(), serde_json::json!(["TEXT"]));
    assert!(!chat_stream_has_non_text_modalities(&request));
}

#[test]
fn default_chat_stream_suppresses_provider_usage_but_serializes_null() {
    let mut chunk = chat_stream_chunk_with_usage(vec![chat_stream_content_choice()]);

    assert!(prepare_chat_stream_chunk_for_client(&mut chunk, false));
    assert!(chunk.usage.is_none());

    let serialized = serde_json::to_value(&chunk).expect("chunk should serialize");
    assert!(
        serialized
            .get("usage")
            .is_some_and(serde_json::Value::is_null),
        "ordinary chunks should carry usage:null instead of provider usage"
    );
}

#[test]
fn default_chat_stream_drops_provider_usage_only_chunk() {
    let mut chunk = chat_stream_chunk_with_usage(vec![]);

    assert!(!prepare_chat_stream_chunk_for_client(&mut chunk, false));
}

#[test]
fn include_usage_records_usage_for_synthetic_final_chunk() {
    let mut final_usage = None;
    let mut content_chunk = chat_stream_chunk_with_usage(vec![chat_stream_content_choice()]);
    assert!(prepare_chat_stream_chunk_for_client_with_state(
        &mut content_chunk,
        true,
        &mut final_usage
    ));
    assert!(content_chunk.usage.is_none());
    assert_eq!(
        final_usage.as_ref().map(|usage| usage.total_tokens),
        Some(15)
    );

    let content_json = serde_json::to_value(&content_chunk).expect("chunk should serialize");
    assert!(
        content_json
            .get("usage")
            .is_some_and(serde_json::Value::is_null),
        "intermediate chunks should carry usage:null"
    );

    let mut final_choice_chunk = chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);
    final_choice_chunk.usage = Some(inference_providers::TokenUsage::new(10, 7));
    assert!(prepare_chat_stream_chunk_for_client_with_state(
        &mut final_choice_chunk,
        true,
        &mut final_usage
    ));
    assert!(final_choice_chunk.usage.is_none());
    assert_eq!(
        final_usage.as_ref().map(|usage| usage.total_tokens),
        Some(17)
    );

    let mut usage_only_chunk = chat_stream_chunk_with_usage(vec![]);
    usage_only_chunk.usage = Some(inference_providers::TokenUsage::new(10, 9));
    assert!(!prepare_chat_stream_chunk_for_client_with_state(
        &mut usage_only_chunk,
        true,
        &mut final_usage
    ));
    assert_eq!(
        final_usage.as_ref().map(|usage| usage.total_tokens),
        Some(19)
    );
}

#[test]
fn include_usage_preserves_converter_only_usage_for_tail() {
    let mut final_usage = None;
    let mut final_choice_chunk = chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);
    assert!(prepare_chat_stream_chunk_for_client_with_state(
        &mut final_choice_chunk,
        true,
        &mut final_usage
    ));
    assert!(final_choice_chunk.usage.is_none());
    assert_eq!(
        final_usage.as_ref().map(|usage| usage.total_tokens),
        Some(15)
    );
}

#[test]
fn final_usage_chunk_uses_stream_template_metadata() {
    let template = Some((
        "chatcmpl-test".to_string(),
        "test-model".to_string(),
        1234567890,
        Some("fp-test".to_string()),
    ));

    let bytes =
        build_final_usage_chunk_bytes(inference_providers::TokenUsage::new(10, 5), &template)
            .expect("final usage chunk should serialize")
            .expect("template should produce final usage chunk");

    let body = String::from_utf8(bytes.to_vec()).expect("SSE bytes should be UTF-8");
    assert!(body.starts_with("data: "));
    assert!(body.ends_with("\n\n"));
    let payload = body
        .trim_start_matches("data: ")
        .trim_end()
        .trim_end_matches('\n');
    let value: serde_json::Value =
        serde_json::from_str(payload).expect("final usage payload should be JSON");
    assert_eq!(value["id"], "chatcmpl-test");
    assert_eq!(value["model"], "test-model");
    assert_eq!(value["created"], 1234567890);
    assert_eq!(value["system_fingerprint"], "fp-test");
    assert!(value["choices"].as_array().is_some_and(Vec::is_empty));
    assert_eq!(value["usage"]["prompt_tokens"], 10);
    assert_eq!(value["usage"]["completion_tokens"], 5);
    // No reasoning count reported upstream, so none is invented.
    assert!(value["usage"].get("completion_tokens_details").is_none());
    assert!(value["usage"].get("reasoning_tokens").is_none());
}

#[test]
fn final_usage_chunk_reports_sglang_reasoning_count_in_standard_details() {
    let template = Some((
        "chatcmpl-test".to_string(),
        "test-model".to_string(),
        1234567890,
        None,
    ));
    // Last cumulative usage from an SGLang stream (top-level count).
    let mut chunk: inference_providers::models::ChatCompletionChunk =
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1234567890,
            "model": "test-model",
            "choices": [chat_stream_finish_choice()],
            "usage": {
                "prompt_tokens": 20,
                "total_tokens": 120,
                "completion_tokens": 100,
                "prompt_tokens_details": { "cached_tokens": 4 },
                "reasoning_tokens": 14
            }
        }))
        .expect("SGLang chunk should parse");
    let mut final_usage = None;
    assert!(prepare_chat_stream_chunk_for_client_with_state(
        &mut chunk,
        true,
        &mut final_usage
    ));

    let bytes = build_final_usage_chunk_bytes(
        final_usage.expect("usage should be kept for the final chunk"),
        &template,
    )
    .expect("final usage chunk should serialize")
    .expect("template should produce final usage chunk");
    let body = String::from_utf8(bytes.to_vec()).expect("SSE bytes should be UTF-8");
    let value: serde_json::Value =
        serde_json::from_str(body.trim_start_matches("data: ").trim_end())
            .expect("final usage payload should be JSON");
    assert_eq!(
        value["usage"],
        serde_json::json!({
            "prompt_tokens": 20,
            "completion_tokens": 100,
            "total_tokens": 120,
            "prompt_tokens_details": { "cached_tokens": 4 },
            "completion_tokens_details": { "reasoning_tokens": 14 },
            "reasoning_tokens": 14
        })
    );
}

#[test]
fn rewritten_control_events_keep_comments_and_drop_separators() {
    let blank = inference_providers::SSEEvent {
        raw_bytes: Bytes::from_static(b"\n"),
        chunk: None,
        raw_passthrough: true,
    };
    assert!(rewritten_control_event_bytes(&blank).is_none());

    let comment = inference_providers::SSEEvent {
        raw_bytes: Bytes::from_static(b": keepalive\n"),
        chunk: None,
        raw_passthrough: true,
    };
    assert_eq!(
        rewritten_control_event_bytes(&comment),
        Some(Bytes::from_static(b": keepalive\n\n"))
    );

    let done = inference_providers::SSEEvent {
        raw_bytes: Bytes::from_static(b"data: [DONE]\n"),
        chunk: None,
        raw_passthrough: true,
    };
    assert!(rewritten_control_event_bytes(&done).is_none());
}

#[test]
fn default_chat_stream_strips_usage_from_terminal_choice_chunk() {
    let mut final_choice_chunk = chat_stream_chunk_with_usage(vec![chat_stream_finish_choice()]);

    assert!(prepare_chat_stream_chunk_for_client(
        &mut final_choice_chunk,
        false
    ));
    assert!(final_choice_chunk.usage.is_none());
}

#[test]
fn test_extract_inference_id_from_chunk_valid() {
    let chunk = make_chat_chunk("chatcmpl-123abc");
    let uuid1 = extract_inference_id_from_chunk(&chunk);
    // UUID should be deterministic - same input produces same UUID
    let uuid2 = extract_inference_id_from_chunk(&chunk);
    assert_eq!(uuid1, uuid2);
}

#[test]
fn test_extract_inference_id_from_chunk_deterministic() {
    let chunk1 = make_chat_chunk("chatcmpl-test123");
    let chunk2 = make_chat_chunk("chatcmpl-test123");
    let uuid1 = extract_inference_id_from_chunk(&chunk1);
    let uuid2 = extract_inference_id_from_chunk(&chunk2);
    assert_eq!(uuid1, uuid2);
}

#[test]
fn test_extract_inference_id_from_chunk_different_ids() {
    let chunk1 = make_chat_chunk("chatcmpl-abc123");
    let chunk2 = make_chat_chunk("chatcmpl-xyz789");
    let uuid1 = extract_inference_id_from_chunk(&chunk1);
    let uuid2 = extract_inference_id_from_chunk(&chunk2);
    assert_ne!(uuid1, uuid2);
}

#[test]
fn test_extract_inference_id_from_chunk_empty_id() {
    // Given: Chutes omitted its provider inference id at the parsing boundary.
    let chunk = make_chat_chunk("");

    // When: the route derives the optional public inference id.
    let result = extract_inference_id_from_chunk(&chunk);

    // Then: absence stays absent instead of becoming a hash of the empty string.
    assert_eq!(result, None);
}

fn empty_delta() -> inference_providers::models::ChatDelta {
    inference_providers::models::ChatDelta {
        role: None,
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        extra: Default::default(),
    }
}

fn make_chat_chunk_with_choice(
    delta: Option<inference_providers::models::ChatDelta>,
    finish_reason: Option<inference_providers::models::FinishReason>,
) -> inference_providers::StreamChunk {
    inference_providers::StreamChunk::Chat(inference_providers::models::ChatCompletionChunk {
        id: "x".to_string(),
        object: "chat.completion.chunk".to_string(),
        created: 0,
        model: "m".to_string(),
        system_fingerprint: None,
        choices: vec![inference_providers::models::ChatChoice {
            index: 0,
            delta,
            logprobs: None,
            finish_reason,
            token_ids: None,
        }],
        usage: None,
        service_tier: None,
        prompt_token_ids: None,
        modality: None,
        extra: Default::default(),
    })
}

#[test]
fn finish_chunk_with_empty_delta_drains_held_tail_in_place() {
    // Reproduces the bug Pierre flagged on PR #599:
    // when a content chunk pushes bytes into the per-choice tail and
    // the next chunk is a finish chunk with `delta: {}` + `finish_reason`,
    // the previous code only drained states whose corresponding delta
    // field was present on the finish chunk. The held tail then leaked
    // out *after* the finish chunk via build_flush_chunks — invisible
    // to clients that stop reading on finish_reason.
    //
    // Fix: when finish_reason is present, drain ALL per-choice states
    // into the finish chunk's delta in place, even if the delta field
    // was absent. Assert: (a) the tail is on the finish chunk;
    // (b) `states.content` is empty so build_flush_chunks emits nothing.
    use services::auto_redact::RedactionMap;

    let mut m = RedactionMap::new();
    m.lookup_or_mint("private_email", "alice@example.com", |_| false);
    let map = Arc::new(m);
    let mut states = StreamUnredactStates::default();

    // Chunk 1: content shorter than hold_size — sits in the tail.
    let mut chunk1 = make_chat_chunk_with_choice(
        Some(inference_providers::models::ChatDelta {
            content: Some("hi redact".to_string()),
            ..empty_delta()
        }),
        None,
    );
    unredact_chunk_in_place(&mut chunk1, &mut states, &map);

    // Chunk 2: empty delta + finish_reason.
    let mut chunk2 = make_chat_chunk_with_choice(
        Some(empty_delta()),
        Some(inference_providers::models::FinishReason::Stop),
    );
    unredact_chunk_in_place(&mut chunk2, &mut states, &map);

    let inference_providers::StreamChunk::Chat(c2) = &chunk2 else {
        panic!("expected Chat chunk");
    };
    let drained_content = c2.choices[0]
        .delta
        .as_ref()
        .and_then(|d| d.content.as_deref())
        .unwrap_or("");
    assert_eq!(
        drained_content, "hi redact",
        "held tail must be attached to the finish chunk's delta.content, not emitted after",
    );
    assert!(
        !states.content.contains_key(&0),
        "after drain the state must be removed so build_flush_chunks emits nothing for this choice",
    );
}

#[test]
fn finish_chunk_with_empty_delta_drains_tool_call_arguments_in_place() {
    // Same bug as above, for tool-call arguments. A tool_calls fragment
    // leaves a partial JSON in the per-(choice,tc_idx) state; the next
    // chunk carries `finish_reason: tool_calls` with empty delta. The
    // held bytes must come out on THAT chunk, not after.
    use services::auto_redact::RedactionMap;

    let mut m = RedactionMap::new();
    m.lookup_or_mint("private_email", "alice@example.com", |_| false);
    let map = Arc::new(m);
    let mut states = StreamUnredactStates::default();

    // Chunk 1: tool_call args fragment that fits inside hold_size.
    let mut chunk1 = make_chat_chunk_with_choice(
        Some(inference_providers::models::ChatDelta {
            tool_calls: Some(vec![inference_providers::models::ToolCallDelta {
                id: None,
                type_: None,
                index: Some(0),
                function: Some(inference_providers::models::FunctionCallDelta {
                    name: None,
                    arguments: Some(r#"{"to":"x"}"#.to_string()),
                }),
                thought_signature: None,
            }]),
            ..empty_delta()
        }),
        None,
    );
    unredact_chunk_in_place(&mut chunk1, &mut states, &map);

    // Chunk 2: empty delta + finish_reason=tool_calls.
    let mut chunk2 = make_chat_chunk_with_choice(
        Some(empty_delta()),
        Some(inference_providers::models::FinishReason::ToolCalls),
    );
    unredact_chunk_in_place(&mut chunk2, &mut states, &map);

    let inference_providers::StreamChunk::Chat(c2) = &chunk2 else {
        panic!("expected Chat chunk");
    };
    let tool_calls = c2.choices[0]
        .delta
        .as_ref()
        .and_then(|d| d.tool_calls.as_ref())
        .expect("finish chunk must carry the drained tool_calls");
    assert_eq!(tool_calls.len(), 1);
    let args = tool_calls[0]
        .function
        .as_ref()
        .and_then(|f| f.arguments.as_deref())
        .unwrap_or("");
    assert_eq!(args, r#"{"to":"x"}"#);
    assert!(
        states.tool_call_arguments.is_empty(),
        "after drain the tool_call state must be removed",
    );
}

#[test]
fn test_classify_provider_error_404_surfaces_message() {
    let (status, error_type, message) =
        classify_provider_error(404, "model 'foo' not found".to_string());
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_type, "not_found_error");
    assert_eq!(message, "model 'foo' not found");
}

#[test]
fn test_classify_provider_error_429_surfaces_message() {
    let (status, error_type, message) =
        classify_provider_error(429, "too many concurrent requests".to_string());
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error_type, "rate_limit_error");
    assert_eq!(message, "too many concurrent requests");
}

#[test]
fn test_classify_provider_error_400_surfaces_message() {
    let (status, error_type, message) = classify_provider_error(
        400,
        "dimensions is not supported for this model".to_string(),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_type, "invalid_request_error");
    assert_eq!(message, "dimensions is not supported for this model");
}

#[test]
fn test_classify_provider_error_422_preserves_upstream_status() {
    let (status, error_type, message) =
        classify_provider_error(422, "validation failed".to_string());
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_type, "invalid_request_error");
    assert_eq!(message, "validation failed");
}

#[test]
fn test_classify_provider_error_401_masked_as_5xx() {
    // 401 from upstream means *our* credentials are wrong — the client
    // did nothing to cause it, so we must not echo the auth error.
    let (status, error_type, message) =
        classify_provider_error(401, "Invalid API key 'sk-***'".to_string());
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_type, "server_error");
    assert!(
        !message.contains("sk-"),
        "must not leak upstream credentials in surfaced message"
    );
    assert!(message.contains("try again later"));
}

#[test]
fn test_classify_provider_error_403_masked_as_5xx() {
    let (status, error_type, message) =
        classify_provider_error(403, "Forbidden: backend ACL denied".to_string());
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_type, "server_error");
    assert_eq!(
        message,
        "Embeddings request failed. Please try again later."
    );
}

#[test]
fn test_classify_provider_error_407_masked_as_5xx() {
    let (status, error_type, _) = classify_provider_error(407, "proxy auth required".to_string());
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_type, "server_error");
}

#[test]
fn test_classify_provider_error_5xx_masked() {
    // 5xx bodies may contain stack traces or internal details — never echo.
    let (status, error_type, message) = classify_provider_error(
        502,
        "RuntimeError: traceback...internal-host:9000".to_string(),
    );
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_type, "server_error");
    assert!(!message.contains("traceback"));
    assert!(!message.contains("internal-host"));
}

#[test]
fn test_stream_chunk_serialization_preserves_field_order() {
    // Verify that StreamChunk::Chat serializes with struct field order
    // (not alphabetical), matching what serde_json::to_string produces.
    // The server uses to_string(&StreamChunk) and the mock must match.
    let chunk = inference_providers::models::ChatCompletionChunk {
        id: "chatcmpl-test".to_string(),
        object: "chat.completion.chunk".to_string(),
        created: 1234567890,
        model: "test-model".to_string(),
        system_fingerprint: None,
        choices: vec![],
        usage: Some(inference_providers::models::TokenUsage::new(10, 5)),
        service_tier: None,
        prompt_token_ids: None,
        modality: None,
        extra: Default::default(),
    };

    let stream_chunk = inference_providers::StreamChunk::Chat(chunk.clone());

    // Both serialization paths must produce identical output
    let from_inner = serde_json::to_string(&chunk).expect("inner chunk should serialize");
    let from_enum =
        serde_json::to_string(&stream_chunk).expect("enum-wrapped chunk should serialize");
    assert_eq!(from_inner, from_enum);

    // Field order should be struct order, not alphabetical
    let id_pos = from_inner
        .find("\"id\"")
        .expect("serialized chunk should contain id field");
    let choices_pos = from_inner
        .find("\"choices\"")
        .expect("serialized chunk should contain choices field");
    assert!(
        id_pos < choices_pos,
        "id should appear before choices (struct field order)"
    );
}

#[test]
fn test_sse_error_frame_is_valid_json() {
    // Every stream-error variant must produce a frame whose `data:` payload
    // parses as JSON of shape {"error": {"message": ..., "type": ...}}.
    // The historical `data: error: <msg>\n\n` format broke clients that
    // parse the data payload as JSON (opencode, vercel/ai-sdk).
    let cases = vec![
        inference_providers::CompletionError::CompletionError("boom".into()),
        inference_providers::CompletionError::HttpError {
            status_code: 503,
            message: "overloaded".into(),
            is_external: false,
        },
        inference_providers::CompletionError::HttpError {
            status_code: 429,
            message: "rate limit".into(),
            is_external: false,
        },
        inference_providers::CompletionError::HttpError {
            status_code: 400,
            message: "bad request".into(),
            is_external: false,
        },
        inference_providers::CompletionError::InvalidResponse("Failed to parse event".into()),
        inference_providers::CompletionError::NoPubKeyProvider("abc".into()),
        inference_providers::CompletionError::Unknown("mystery".into()),
        inference_providers::CompletionError::Timeout {
            operation: "completion".into(),
            timeout_seconds: 30,
        },
    ];

    for e in &cases {
        let frame = sse_error_frame(e);
        let text = std::str::from_utf8(&frame).expect("frame is utf-8");
        assert!(
            text.starts_with("data: "),
            "frame missing 'data: ' prefix: {text:?}"
        );
        assert!(
            text.ends_with("\n\n"),
            "frame missing SSE terminator: {text:?}"
        );
        let payload = text
            .strip_prefix("data: ")
            .and_then(|s| s.strip_suffix("\n\n"))
            .expect("frame must have data: prefix and \\n\\n suffix");
        let json: serde_json::Value = serde_json::from_str(payload).unwrap_or_else(|err| {
            panic!("frame payload not valid JSON for {e:?}: err={err}, payload={payload}")
        });
        let obj = json.get("error").expect("payload has error key");
        assert!(obj
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()));
        assert!(obj
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()));
    }
}

#[test]
fn test_completion_stream_error_openai_type_http_status_mapping() {
    let rate_limited = inference_providers::CompletionError::HttpError {
        status_code: 429,
        message: "rl".into(),
        is_external: false,
    };
    assert_eq!(
        completion_stream_error_openai_type(&rate_limited),
        "rate_limit_exceeded"
    );

    let client_err = inference_providers::CompletionError::HttpError {
        status_code: 400,
        message: "bad".into(),
        is_external: false,
    };
    assert_eq!(
        completion_stream_error_openai_type(&client_err),
        "invalid_request_error"
    );

    let server_err = inference_providers::CompletionError::HttpError {
        status_code: 503,
        message: "down".into(),
        is_external: false,
    };
    assert_eq!(
        completion_stream_error_openai_type(&server_err),
        "server_error"
    );

    let parse_err = inference_providers::CompletionError::InvalidResponse("bad chunk".into());
    assert_eq!(
        completion_stream_error_openai_type(&parse_err),
        "server_error"
    );
}

#[test]
fn chat_response_to_text_response_maps_content_and_usage() {
    let response: inference_providers::ChatCompletionResponse =
        serde_json::from_value(serde_json::json!({
            "id": "cmpl-abc",
            "object": "chat.completion",
            "created": 42,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "The capital is Paris."},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 5,
                "total_tokens": 16,
                "prompt_tokens_details": {"cached_tokens": 4}
            }
        }))
        .unwrap();

    let text = chat_response_to_text_response(response);

    assert_eq!(text.object, "text_completion");
    assert_eq!(text.id, "cmpl-abc");
    assert_eq!(text.created, 42);
    assert_eq!(text.choices.len(), 1);
    assert_eq!(text.choices[0].index, 0);
    assert_eq!(text.choices[0].text, "The capital is Paris.");
    assert_eq!(text.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(text.usage.prompt_tokens, 11);
    assert_eq!(text.usage.completion_tokens, 5);
    assert_eq!(text.usage.total_tokens, 16);
    assert_eq!(
        text.usage.prompt_tokens_details.map(|d| d.cached_tokens),
        Some(4)
    );
}

#[test]
fn chat_response_to_text_response_handles_missing_content_and_no_cache() {
    let response: inference_providers::ChatCompletionResponse =
        serde_json::from_value(serde_json::json!({
            "id": "cmpl-empty",
            "object": "chat.completion",
            "created": 0,
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1}
        }))
        .unwrap();

    let text = chat_response_to_text_response(response);

    assert_eq!(text.choices[0].text, "");
    assert!(text.choices[0].finish_reason.is_none());
    // No cached tokens reported -> details omitted.
    assert!(text.usage.prompt_tokens_details.is_none());
}

#[test]
fn chat_chunk_to_text_chunk_maps_delta_content() {
    let chunk = make_chat_chunk_with_choice(
        Some(inference_providers::models::ChatDelta {
            content: Some("Hello".to_string()),
            ..empty_delta()
        }),
        Some(inference_providers::models::FinishReason::Stop),
    );

    let text = chat_chunk_to_text_chunk(chunk);

    assert_eq!(text.object, "text_completion");
    assert_eq!(text.choices.len(), 1);
    assert_eq!(text.choices[0].index, 0);
    assert_eq!(text.choices[0].text, "Hello");
    assert!(matches!(
        text.choices[0].finish_reason,
        Some(inference_providers::models::FinishReason::Stop)
    ));
}

#[test]
fn chat_chunk_to_text_chunk_empty_delta_yields_empty_text() {
    let chunk = make_chat_chunk_with_choice(Some(empty_delta()), None);
    let text = chat_chunk_to_text_chunk(chunk);
    assert_eq!(text.choices[0].text, "");
    assert!(text.choices[0].finish_reason.is_none());
}

#[test]
fn completion_request_deserializes_minimal_body() {
    // `extra` is #[serde(flatten)]: a request with only model+prompt must
    // parse (no required "extra" object) and leave `extra` empty.
    let req: CompletionRequest = serde_json::from_str(r#"{"model":"m","prompt":"hello"}"#).unwrap();
    assert_eq!(req.model, "m");
    assert_eq!(req.prompt.single_text().unwrap(), "hello");
    assert!(req.extra.is_empty());
}

#[test]
fn completion_request_flattens_unknown_fields_into_extra() {
    let req: CompletionRequest =
        serde_json::from_str(r#"{"model":"m","prompt":"hi","auto_redact":true}"#).unwrap();
    assert_eq!(
        req.extra.get("auto_redact"),
        Some(&serde_json::Value::Bool(true))
    );
}

fn parse_completion_request(json: &str) -> CompletionRequest {
    serde_json::from_str(json).unwrap()
}

#[test]
fn unsupported_params_are_rejected() {
    assert_eq!(
        unsupported_completion_param(&parse_completion_request(
            r#"{"model":"m","prompt":"p","echo":true}"#
        )),
        Some("echo")
    );
    assert_eq!(
        unsupported_completion_param(&parse_completion_request(
            r#"{"model":"m","prompt":"p","logprobs":5}"#
        )),
        Some("logprobs")
    );
    assert_eq!(
        unsupported_completion_param(&parse_completion_request(
            r#"{"model":"m","prompt":"p","best_of":3}"#
        )),
        Some("best_of")
    );
}

#[test]
fn supported_and_default_params_are_accepted() {
    // echo:false, best_of:1, and penalties are not rejected.
    assert_eq!(
        unsupported_completion_param(&parse_completion_request(
            r#"{"model":"m","prompt":"p","echo":false,"best_of":1,"presence_penalty":0.5,"frequency_penalty":-0.2}"#
        )),
        None
    );
    assert_eq!(
        unsupported_completion_param(&parse_completion_request(r#"{"model":"m","prompt":"p"}"#)),
        None
    );
}

#[test]
fn convert_forwards_penalties_into_extra() {
    let req = parse_completion_request(
        r#"{"model":"m","prompt":"p","presence_penalty":0.5,"frequency_penalty":-0.2}"#,
    );
    let body_hash = RequestBodyHash {
        hash: String::new(),
        body_bytes: Bytes::new(),
    };
    let svc = convert_text_request_to_service(
        &req,
        "p".to_string(),
        Uuid::nil(),
        "key".to_string(),
        Uuid::nil(),
        Uuid::nil(),
        true,
        0,
        body_hash,
        Uuid::nil(),
    );
    // Compare with tolerance: the typed field is f32, so the forwarded JSON
    // number widens to f64 (e.g. -0.2f32 -> -0.20000000298).
    let presence = svc
        .extra
        .get("presence_penalty")
        .and_then(|v| v.as_f64())
        .unwrap();
    let frequency = svc
        .extra
        .get("frequency_penalty")
        .and_then(|v| v.as_f64())
        .unwrap();
    assert!((presence - 0.5).abs() < 1e-6);
    assert!((frequency + 0.2).abs() < 1e-6);
}

#[test]
fn stop_accepts_string_or_array() {
    // Single string form (e.g. {"stop":"\n"}) must deserialize, not 422.
    let single = parse_completion_request(r#"{"model":"m","prompt":"p","stop":"\n"}"#);
    assert_eq!(single.stop.unwrap().into_vec(), vec!["\n".to_string()]);

    let many = parse_completion_request(r#"{"model":"m","prompt":"p","stop":["a","b"]}"#);
    assert_eq!(
        many.stop.unwrap().into_vec(),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn prompt_array_shapes_deserialize_without_422() {
    // The point: these all parse (so the handler can return a clean 400),
    // rather than failing JSON extraction at the framework layer.
    let single_elem = parse_completion_request(r#"{"model":"m","prompt":["solo"]}"#);
    assert_eq!(single_elem.prompt.single_text().unwrap(), "solo");

    let batch = parse_completion_request(r#"{"model":"m","prompt":["a","b"]}"#);
    assert!(batch.prompt.single_text().is_err());

    let tokens = parse_completion_request(r#"{"model":"m","prompt":[1,2,3]}"#);
    assert!(tokens.prompt.single_text().is_err());

    let token_batches = parse_completion_request(r#"{"model":"m","prompt":[[1,2],[3,4]]}"#);
    assert!(token_batches.prompt.single_text().is_err());
}
