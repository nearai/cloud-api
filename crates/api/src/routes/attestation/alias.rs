use crate::models::ErrorResponse;
use axum::{
    http::{HeaderMap, StatusCode},
    response::{Json as ResponseJson, Response},
};
use services::models::ModelsServiceTrait;
use std::sync::Arc;

pub(super) async fn resolve_attestation_alias(
    model: Option<&str>,
    models_service: &Arc<dyn ModelsServiceTrait>,
    headers: &HeaderMap,
) -> Result<Option<(String, String)>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let Some(requested) = model else {
        return Ok(None);
    };

    match models_service.resolve_and_get_model(requested).await {
        Ok(m) if m.model_name != requested => {
            if crate::routes::common::no_aliasing_requested(headers) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::with_param(
                        format!(
                            "Model '{requested}' is an alias of '{}' and the request set \
                             {}. Use the canonical model name '{}'.",
                            m.model_name,
                            crate::routes::common::HEADER_NO_ALIASING,
                            m.model_name
                        ),
                        "invalid_request_error".to_string(),
                        "model".to_string(),
                    )),
                ));
            }
            Ok(Some((requested.to_string(), m.model_name)))
        }
        Ok(_) | Err(services::models::ModelsError::NotFound(_)) => Ok(None),
        Err(_) => {
            if crate::routes::common::no_aliasing_requested(headers) {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to resolve model for x-no-aliasing check".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ));
            }
            Ok(None)
        }
    }
}

pub(super) fn attach_alias_header(response: &mut Response, requested: &str, canonical: &str) {
    if let Ok(value) = axum::http::HeaderValue::from_str(&format!("{requested} -> {canonical}")) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED),
            value,
        );
        let expose_name = axum::http::HeaderName::from_static("access-control-expose-headers");
        let exposed = match response
            .headers()
            .get(&expose_name)
            .and_then(|v| v.to_str().ok())
        {
            Some(existing) => format!(
                "{existing}, {}",
                crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED
            ),
            None => crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED.to_string(),
        };
        if let Ok(exposed) = axum::http::HeaderValue::from_str(&exposed) {
            response.headers_mut().insert(expose_name, exposed);
        }
    }
}
