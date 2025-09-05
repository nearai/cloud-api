use axum::{extract::{Json, State}, http::StatusCode, response::Json as ResponseJson};
use crate::{models::*, conversions::*};
use domain::{Domain, ChatCompletionParams, CompletionParams};
use std::sync::Arc;

// Application state containing the domain service
pub type AppState = Arc<Domain>;

pub async fn chat_completions(
    State(domain): State<AppState>,
    Json(request): Json<ChatCompletionRequest>,
) -> Result<ResponseJson<ChatCompletionResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // Validate the request
    if let Err(error) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ));
    }

    // Convert HTTP request to domain parameters
    let domain_params: ChatCompletionParams = (&request).into();
    
    // Call the domain service
    match domain.chat_completion(domain_params).await {
        Ok(result) => {
            let response = chat_completion_to_http_response(
                result,
                &request.model,
                format!("chatcmpl-{}", generate_completion_id()),
                current_unix_timestamp(),
            );
            Ok(ResponseJson(response))
        }
        Err(domain_error) => {
            let status_code = match domain_error {
                domain::CompletionError::InvalidModel(_) | domain::CompletionError::InvalidParams(_) => StatusCode::BAD_REQUEST,
                domain::CompletionError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
                domain::CompletionError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            Err((status_code, ResponseJson(domain_error.into())))
        }
    }
}

pub async fn completions(
    State(domain): State<AppState>,
    Json(request): Json<CompletionRequest>,
) -> Result<ResponseJson<CompletionResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    // Validate the request
    if let Err(error) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ));
    }

    // Convert HTTP request to domain parameters
    let domain_params: CompletionParams = (&request).into();
    
    // Call the domain service
    match domain.text_completion(domain_params).await {
        Ok(result) => {
            let response = completion_to_http_response(
                result,
                &request.model,
                format!("cmpl-{}", generate_completion_id()),
                current_unix_timestamp(),
            );
            Ok(ResponseJson(response))
        }
        Err(domain_error) => {
            let status_code = match domain_error {
                domain::CompletionError::InvalidModel(_) | domain::CompletionError::InvalidParams(_) => StatusCode::BAD_REQUEST,
                domain::CompletionError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
                domain::CompletionError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            Err((status_code, ResponseJson(domain_error.into())))
        }
    }
}

pub async fn models(
    State(domain): State<AppState>,
) -> Result<ResponseJson<ModelsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match domain.get_available_models().await {
        Ok(models) => {
            let response = ModelsResponse {
                object: "list".to_string(),
                data: models.into_iter().map(|model| ModelInfo {
                    id: model.id,
                    object: "model".to_string(),
                    created: model.created.unwrap_or(current_unix_timestamp()),
                    owned_by: model.owned_by.unwrap_or_else(|| model.provider.clone()),
                }).collect(),
            };
            Ok(ResponseJson(response))
        }
        Err(domain_error) => {
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(domain_error.into())
            ))
        }
    }
}

// Legacy struct for backwards compatibility
pub struct Routes {
    pub test: String,
}

impl Routes {
    pub fn new() -> Self {
        Self { test: "test".to_string() }
    }
}