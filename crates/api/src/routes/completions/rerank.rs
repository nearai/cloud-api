use super::*;

fn apply_rerank_response_options(
    request: &crate::models::RerankRequest,
    response: &mut inference_providers::RerankResponse,
) {
    response
        .results
        .sort_by(|a, b| b.relevance_score.total_cmp(&a.relevance_score));

    if let Some(top_n) = request.top_n {
        response.results.truncate(top_n);
    }

    for result in &mut response.results {
        if request.return_documents == Some(false) {
            result.document = None;
            continue;
        }

        if let Some(serde_json::Value::Object(document)) = result.document.as_mut() {
            if document
                .get("multi_modal")
                .is_some_and(serde_json::Value::is_null)
            {
                document.remove("multi_modal");
            }
        }
    }
}

#[cfg(test)]
mod rerank_response_options_tests {
    use super::*;
    use serde_json::json;

    fn request(
        top_n: Option<usize>,
        return_documents: Option<bool>,
    ) -> crate::models::RerankRequest {
        crate::models::RerankRequest {
            model: "Qwen/Qwen3-Reranker-0.6B".to_string(),
            query: "capital of France".to_string(),
            documents: vec![
                "Paris".to_string(),
                "Berlin".to_string(),
                "Tokyo".to_string(),
            ],
            top_n,
            return_documents,
        }
    }

    fn response() -> inference_providers::RerankResponse {
        inference_providers::RerankResponse {
            id: "rerank-test".to_string(),
            model: "Qwen/Qwen3-Reranker-0.6B".to_string(),
            results: vec![
                inference_providers::RerankResult {
                    index: 2,
                    relevance_score: 0.4,
                    document: Some(json!({"text": "Tokyo", "multi_modal": null})),
                },
                inference_providers::RerankResult {
                    index: 0,
                    relevance_score: 0.9,
                    document: Some(json!({"text": "Paris", "multi_modal": null})),
                },
                inference_providers::RerankResult {
                    index: 1,
                    relevance_score: 0.1,
                    document: Some(json!({"text": "Berlin", "multi_modal": null})),
                },
            ],
            usage: Some(inference_providers::RerankUsage {
                prompt_tokens: Some(10),
                total_tokens: Some(15),
            }),
        }
    }

    #[test]
    fn top_n_sorts_by_relevance_and_truncates_results() {
        let mut response = response();

        apply_rerank_response_options(&request(Some(2), None), &mut response);

        let indices: Vec<i32> = response.results.iter().map(|result| result.index).collect();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn return_documents_false_omits_documents() {
        let mut response = response();

        apply_rerank_response_options(&request(None, Some(false)), &mut response);

        assert!(response
            .results
            .iter()
            .all(|result| result.document.is_none()));
    }

    #[test]
    fn null_multi_modal_is_stripped_from_document_objects() {
        let mut response = response();

        apply_rerank_response_options(&request(None, None), &mut response);

        for result in response.results {
            let document = result.document.expect("document should be returned");
            assert!(document.get("text").is_some());
            assert!(document.get("multi_modal").is_none());
        }
    }

    #[test]
    fn top_n_zero_is_invalid() {
        assert_eq!(
            request(Some(0), None).validate().unwrap_err(),
            "top_n must be at least 1"
        );
    }
}

/// Document reranking endpoint
///
/// Ranks documents by relevance to a query using a reranker model.
///
/// **Concurrent Request Limits:** Each organization has a per-model concurrent request limit (default: 64).
/// When the limit is reached, new requests will fail with 429 status code. Wait for in-flight requests to complete before retrying.
#[utoipa::path(
    post,
    path = "/v1/rerank",
    tag = "Rerank",
    request_body = crate::models::RerankRequest,
    responses(
        (status = 200, description = "Successful rerank", body = crate::models::RerankResponse),
        (status = 400, description = "Invalid request (empty documents, invalid model, etc.)", body = ErrorResponse),
        (status = 401, description = "Unauthorized (missing or invalid API key)", body = ErrorResponse),
        (status = 404, description = "Model not found", body = ErrorResponse),
        (status = 429, description = "Rate limited or overloaded — retry with backoff. Check error.type: rate_limit_exceeded or service_overloaded.", body = ErrorResponse),
        (status = 500, description = "Server error (billing failure, provider error)", body = ErrorResponse),
    ),
    security(("ApiKeyAuth" = []))
)]
pub async fn rerank(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(_body_hash): Extension<RequestBodyHash>,
    Extension(correlation): Extension<RequestCorrelation>,
    headers: header::HeaderMap,
    requested_model: RequestedModel,
    OpenAiJson(request): OpenAiJson<crate::models::RerankRequest>,
) -> axum::response::Response {
    requested_model.set(&request.model);
    debug!(
        "Rerank request: model={}, org={}, workspace={}",
        request.model, api_key.organization.id, api_key.workspace.id.0
    );

    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                error,
                "invalid_request_error".to_string(),
            )),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking - fail fast if not found
    // Models must be registered in the database to be available for use
    let model = match app_state
        .models_service
        .get_model_by_name(&request.model)
        .await
    {
        Ok(model) => model,
        Err(services::models::ModelsError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "not_found_error".to_string(),
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to resolve model for rerank");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to resolve model".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };
    let model_id = model.id;
    let organization_id = api_key.organization.id.0;

    // Extract and validate encryption headers if present
    let encryption_headers = match crate::routes::common::validate_encryption_headers(&headers) {
        Ok(headers) => headers,
        Err(err) => return err.into_response(),
    };

    // Convert API request to provider params
    let mut extra = std::collections::HashMap::new();
    insert_encryption_headers(&encryption_headers, &mut extra);
    insert_request_id_header(correlation, &mut extra);

    let params = inference_providers::RerankParams {
        model: request.model.clone(),
        query: request.query.clone(),
        documents: request.documents.clone(),
        extra,
    };

    // Call completion service which handles concurrent request limiting
    // Each organization has a per-model concurrent request limit (default: 64 concurrent requests).
    // This prevents resource exhaustion and ensures fair usage. Returns 429 if limit exceeded.
    match app_state
        .completion_service
        .try_rerank(organization_id, model_id, &request.model, params)
        .await
    {
        Ok(mut response) => {
            apply_rerank_response_options(&request, &mut response);

            // Record usage for rerank SYNCHRONOUSLY to ensure billing accuracy
            // This is critical for revenue tracking and must complete before returning response
            let organization_id = api_key.organization.id.0;
            let workspace_id = api_key.workspace.id.0;
            let mut token_count = response
                .usage
                .as_ref()
                .and_then(|u| u.total_tokens)
                .unwrap_or(0);

            // Validate token count is reasonable (prevent provider misreporting)
            const MAX_REASONABLE_TOKENS: i32 = 1_000_000; // 1M tokens max
            let mut token_anomaly_detected = false;

            if token_count > MAX_REASONABLE_TOKENS {
                // Log at ERROR level with full context for monitoring/alerting
                // This indicates a provider bug that needs investigation
                tracing::error!(
                    token_count = token_count,
                    max_expected = MAX_REASONABLE_TOKENS,
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned unreasonable token count - capping to prevent billing errors. This may indicate provider misconfiguration or a bug."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting
                let model_tag = format!("model:{}", request.model);
                let reason_tag = format!(
                    "reason:{}",
                    services::metrics::consts::REASON_TOKEN_OVERFLOW
                );
                let anomaly_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_TOKEN_ANOMALIES,
                    1,
                    &anomaly_tags,
                );

                // Cap to maximum to prevent billing errors
                token_count = MAX_REASONABLE_TOKENS;
            }

            // Warn if provider didn't return usage data
            if token_count == 0 {
                tracing::warn!(
                    model = %request.model,
                    organization_id = %organization_id,
                    "Provider returned zero tokens for rerank - no cost will be charged. This may indicate provider misconfiguration or incomplete response."
                );
                token_anomaly_detected = true;

                // Record metrics for monitoring and alerting on missing usage data
                let model_tag = format!("model:{}", request.model);
                let reason_tag =
                    format!("reason:{}", services::metrics::consts::REASON_MISSING_USAGE);
                let zero_tokens_tags = [model_tag.as_str(), reason_tag.as_str()];
                app_state.metrics_service.record_count(
                    services::metrics::consts::METRIC_PROVIDER_ZERO_TOKENS,
                    1,
                    &zero_tokens_tags,
                );
            }

            // If anomaly detected, log additional context for debugging
            if token_anomaly_detected {
                tracing::info!(
                    model = %request.model,
                    organization_id = %organization_id,
                    final_token_count = token_count,
                    "Token count anomaly: Provider data quality issue detected. Recommendation: Check provider logs and configuration."
                );
            }

            // Parse API key ID to UUID
            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Invalid API key ID for usage tracking");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to record usage".to_string(),
                            "server_error".to_string(),
                        )),
                    )
                        .into_response();
                }
            };

            let inference_id = uuid::Uuid::new_v4();
            let usage_request = services::usage::RecordUsageServiceRequest {
                input_tokens: token_count,
                inference_id: Some(inference_id),
                stop_reason: Some(services::usage::StopReason::Completed),
                ..services::usage::RecordUsageServiceRequest::base(
                    organization_id,
                    workspace_id,
                    api_key_id,
                    model_id,
                    services::usage::ports::InferenceType::Rerank,
                )
            };

            // Record usage synchronously - this is billing-critical and must succeed
            // If usage recording fails, we must fail the request to prevent revenue loss
            if let Err(e) = app_state.usage_service.record_usage(usage_request).await {
                tracing::error!(error = %e, "Failed to record rerank usage - blocking request to ensure billing accuracy");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to record usage - please retry".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response();
            }

            (StatusCode::OK, ResponseJson(response)).into_response()
        }
        Err(e) => {
            let (status_code, error_type, message) = match e {
                services::completions::ports::CompletionError::RateLimitExceeded(msg) => {
                    tracing::warn!("Concurrent request limit exceeded for rerank");
                    (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", msg)
                }
                services::completions::ports::CompletionError::ProviderError {
                    status_code,
                    message,
                } => {
                    tracing::error!(
                        model = %request.model,
                        upstream_status = status_code,
                        detail = %message,
                        "Rerank provider error"
                    );
                    let http_status = StatusCode::from_u16(status_code)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    (
                        http_status,
                        "server_error",
                        "Reranking failed. Please try again later.".to_string(),
                    )
                }
                services::completions::ports::CompletionError::InvalidModel(msg) => {
                    tracing::warn!("Rerank model not found");
                    (StatusCode::NOT_FOUND, "not_found_error", msg)
                }
                services::completions::ports::CompletionError::ServiceOverloaded(_) => {
                    tracing::warn!("Rerank service overloaded");
                    (
                        crate::routes::common::status_overloaded(),
                        "service_overloaded",
                        "All inference backends are overloaded. Please retry with exponential backoff.".to_string(),
                    )
                }
                _ => {
                    tracing::error!("Unexpected rerank error");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "Reranking failed".to_string(),
                    )
                }
            };

            (
                status_code,
                ResponseJson(ErrorResponse::new(message, error_type.to_string())),
            )
                .into_response()
        }
    }
}
