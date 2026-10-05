//! Announces a planned model deprecation on inference responses.
//!
//! When the model named in a request has a planned deprecation, successful
//! responses carry:
//!
//! - `x-model-deprecation-date`: the planned deprecation date, in the same
//!   form as `deprecation_date` on `GET /v1/models`.
//! - `x-model-successor`: the recommended replacement model, when one is set.
//!
//! These are deliberately not the standard `Deprecation` (RFC 9745) and
//! `Sunset` (RFC 8594) headers. Those describe the requested URL, so on a
//! shared endpoint such as `/v1/chat/completions` they would announce that the
//! endpoint itself is going away. The model is only a field of the request
//! body; the header names say what is actually being deprecated.
//!
//! Only headers are added. The response body is left untouched, so
//! response-hash verification is unaffected. Browser clients can read the
//! headers: the app-wide CORS layer exposes every response header.

use std::sync::{Arc, OnceLock};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{
        header::{HeaderMap, HeaderValue},
        request::Parts,
    },
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use services::models::{ModelWithPricing, ModelsServiceTrait};

pub const HEADER_MODEL_DEPRECATION_DATE: &str = "x-model-deprecation-date";
pub const HEADER_MODEL_SUCCESSOR: &str = "x-model-successor";

/// Upper bound on the catalog lookup made while a finished response waits.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// The model a request named, recorded by the handler once it has parsed the
/// request so [`model_deprecation_middleware`] can annotate the response.
///
/// Extracting it never fails: on a route without the middleware the handler
/// gets a detached slot and recording the model is a no-op.
#[derive(Clone, Default)]
pub struct RequestedModel(Arc<OnceLock<String>>);

impl RequestedModel {
    /// Record the model identifier (canonical name or alias) the client sent.
    /// The first call wins.
    pub fn set(&self, model: &str) {
        let _ = self.0.set(model.to_string());
    }

    fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestedModel {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<RequestedModel>()
            .cloned()
            .unwrap_or_default())
    }
}

#[derive(Clone)]
pub struct ModelDeprecationState {
    pub models_service: Arc<dyn ModelsServiceTrait>,
}

/// What the response headers say about a model's planned deprecation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDeprecationNotice {
    deprecation_date: DateTime<Utc>,
    successor: Option<String>,
}

impl ModelDeprecationNotice {
    /// `None` when the model has no planned deprecation.
    pub fn from_model(model: &ModelWithPricing) -> Option<Self> {
        Some(Self {
            deprecation_date: model.deprecation_date?,
            successor: model.successor_model_name.clone(),
        })
    }

    pub fn apply(&self, headers: &mut HeaderMap) {
        // Guarded construction: a header-invalid byte in a model name drops
        // that one header instead of failing the request.
        let date = crate::routes::admin::format_deprecation_date(&self.deprecation_date);
        if let Ok(value) = HeaderValue::from_str(&date) {
            headers.insert(HEADER_MODEL_DEPRECATION_DATE, value);
        }
        if let Some(value) = self
            .successor
            .as_deref()
            .and_then(|successor| HeaderValue::from_str(successor).ok())
        {
            headers.insert(HEADER_MODEL_SUCCESSOR, value);
        }
    }
}

/// Adds the deprecation headers to successful responses for a model with a
/// planned deprecation. The handler names the model through
/// [`RequestedModel`]; requests that never reach that point are left alone.
pub async fn model_deprecation_middleware(
    State(state): State<ModelDeprecationState>,
    mut request: Request,
    next: Next,
) -> Response {
    let requested = RequestedModel::default();
    request.extensions_mut().insert(requested.clone());

    let mut response = next.run(request).await;

    if !response.status().is_success() {
        return response;
    }
    let Some(model_name) = requested.get() else {
        return response;
    };
    // Cache-backed: serving the request resolved the same identifier moments
    // ago. An alias resolves to its canonical model. Advisory only: a failed
    // or slow lookup (cache miss while the database is struggling) leaves the
    // response as it is instead of holding it back.
    let lookup = state.models_service.resolve_and_get_model(model_name);
    let Ok(Ok(model)) = tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await else {
        return response;
    };
    if let Some(notice) = ModelDeprecationNotice::from_model(&model) {
        notice.apply(response.headers_mut());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn example(successor: Option<&str>) -> ModelDeprecationNotice {
        ModelDeprecationNotice {
            deprecation_date: Utc.with_ymd_and_hms(2026, 12, 1, 13, 0, 0).unwrap(),
            successor: successor.map(str::to_string),
        }
    }

    #[test]
    fn headers_carry_the_catalog_date_and_the_successor() {
        let mut headers = HeaderMap::new();
        example(Some("zai-org/GLM-5.3")).apply(&mut headers);

        // Same form as `deprecation_date` on `GET /v1/models`.
        assert_eq!(
            headers.get(HEADER_MODEL_DEPRECATION_DATE).unwrap(),
            "2026-12-01T13:00:00Z"
        );
        assert_eq!(
            headers.get(HEADER_MODEL_SUCCESSOR).unwrap(),
            "zai-org/GLM-5.3"
        );
        // The endpoint itself is not deprecated: no standard headers.
        assert!(headers.get("deprecation").is_none());
        assert!(headers.get("sunset").is_none());
        assert!(headers.get("link").is_none());
    }

    #[test]
    fn successor_header_is_omitted_when_none_is_set() {
        let mut headers = HeaderMap::new();
        example(None).apply(&mut headers);

        assert!(headers.get(HEADER_MODEL_DEPRECATION_DATE).is_some());
        assert!(headers.get(HEADER_MODEL_SUCCESSOR).is_none());
    }

    #[test]
    fn header_invalid_successor_drops_only_that_header() {
        let mut headers = HeaderMap::new();
        example(Some("bad\nname")).apply(&mut headers);

        assert!(headers.get(HEADER_MODEL_DEPRECATION_DATE).is_some());
        assert!(headers.get(HEADER_MODEL_SUCCESSOR).is_none());
    }

    fn model(deprecation_date: Option<DateTime<Utc>>) -> ModelWithPricing {
        ModelWithPricing {
            id: uuid::Uuid::nil(),
            model_name: "nearai/old-model".to_string(),
            model_display_name: "Old Model".to_string(),
            model_description: String::new(),
            model_icon: None,
            input_cost_per_token: 1,
            output_cost_per_token: 1,
            cost_per_image: 0,
            cache_read_cost_per_token: None,
            text_pricing: None,
            context_length: 4096,
            verifiable: true,
            aliases: Vec::new(),
            owned_by: "nearai".to_string(),
            provider_type: "vllm".to_string(),
            provider_config: None,
            attestation_supported: true,
            input_modalities: None,
            output_modalities: None,
            inference_url: None,
            hugging_face_id: None,
            quantization: None,
            max_output_length: None,
            supported_sampling_parameters: Vec::new(),
            supported_features: Vec::new(),
            datacenters: None,
            is_ready: None,
            deprecation_date,
            successor_model_name: Some("nearai/new-model".to_string()),
            openrouter_slug: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn no_planned_deprecation_means_no_notice() {
        // A successor alone announces nothing.
        assert_eq!(ModelDeprecationNotice::from_model(&model(None)), None);
    }

    #[test]
    fn notice_is_built_from_the_catalog_row() {
        let date = Utc.with_ymd_and_hms(2026, 12, 1, 13, 0, 0).unwrap();
        assert_eq!(
            ModelDeprecationNotice::from_model(&model(Some(date))),
            Some(ModelDeprecationNotice {
                deprecation_date: date,
                successor: Some("nearai/new-model".to_string()),
            })
        );
    }
}
