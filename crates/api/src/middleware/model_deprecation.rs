//! Announces a planned model deprecation on inference responses.
//!
//! When the model named in a request has a planned deprecation, successful
//! responses carry the standard headers:
//!
//! - `Deprecation` (RFC 9745): when the deprecation was announced, as a
//!   structured-field date (`@<unix seconds>`).
//! - `Sunset` (RFC 8594): the planned deprecation date, i.e. when the model
//!   stops being served under that name, as an HTTP date.
//! - `Link` with `rel="deprecation"` (human documentation, when configured)
//!   and `rel="successor-version"` (the replacement model's catalog entry).
//!
//! Both RFCs scope these headers to the requested URL by default and let a
//! service document a different scope. Here they describe the model named in
//! the request, not the endpoint; the linked documentation says so.
//!
//! Only headers are added. The response body is left untouched, so
//! response-hash verification is unaffected.

use std::sync::{Arc, OnceLock};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{
        header::{self, HeaderMap, HeaderName, HeaderValue},
        request::Parts,
    },
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use services::models::{ModelWithPricing, ModelsServiceTrait};

pub const HEADER_DEPRECATION: HeaderName = HeaderName::from_static("deprecation");
pub const HEADER_SUNSET: HeaderName = HeaderName::from_static("sunset");

/// Upper bound on the catalog lookup made while a finished response waits.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Names appended to an explicit `Access-Control-Expose-Headers` list so
/// browser clients can read the announcement.
const EXPOSED_HEADER_NAMES: &str = "Deprecation, Sunset, Link";

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
    /// Documentation page linked with `rel="deprecation"`. `None` omits the
    /// link.
    pub docs_url: Option<String>,
}

/// What the response headers say about a model's planned deprecation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDeprecationNotice {
    announced_at: DateTime<Utc>,
    sunset: DateTime<Utc>,
    successor: Option<String>,
}

impl ModelDeprecationNotice {
    /// `None` when the model has no planned deprecation.
    pub fn from_model(model: &ModelWithPricing) -> Option<Self> {
        let sunset = model.deprecation_date?;
        // `Sunset` must not be earlier than `Deprecation` (RFC 9745 section 4).
        // A planned date earlier than the recorded announcement, or a row with
        // no recorded announcement, is announced as deprecated at that date.
        let announced_at = model
            .deprecation_announced_at
            .map_or(sunset, |announced| announced.min(sunset));
        Some(Self {
            announced_at,
            sunset,
            successor: model.successor_model_name.clone(),
        })
    }

    fn deprecation_value(&self) -> String {
        format!("@{}", self.announced_at.timestamp())
    }

    fn sunset_value(&self) -> String {
        self.sunset.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
    }

    /// One `Link` field value carrying every applicable relation, or `None`
    /// when there is neither a documentation page nor a successor.
    fn link_value(&self, docs_url: Option<&str>) -> Option<String> {
        let mut links = Vec::new();
        if let Some(url) = docs_url {
            links.push(format!("<{url}>; rel=\"deprecation\"; type=\"text/html\""));
        }
        if let Some(successor) = &self.successor {
            // Relative reference: resolves against the request URL, so it
            // points at this deployment's public catalog entry.
            links.push(format!(
                "</v1/model/{}>; rel=\"successor-version\"",
                urlencoding::encode(successor)
            ));
        }
        (!links.is_empty()).then(|| links.join(", "))
    }

    pub fn apply(&self, headers: &mut HeaderMap, docs_url: Option<&str>) {
        // Guarded construction: a header-invalid byte in a model name or a
        // configured URL drops that one header instead of failing the request.
        if let Ok(value) = HeaderValue::from_str(&self.deprecation_value()) {
            headers.insert(HEADER_DEPRECATION, value);
        }
        if let Ok(value) = HeaderValue::from_str(&self.sunset_value()) {
            headers.insert(HEADER_SUNSET, value);
        }
        if let Some(value) = self
            .link_value(docs_url)
            .and_then(|link| HeaderValue::from_str(&link).ok())
        {
            headers.append(header::LINK, value);
        }
        // Handlers that set an explicit CORS expose list name each header;
        // keep the announcement readable from browsers.
        if let Some(exposed) = headers
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.trim() != "*")
        {
            if let Ok(value) = HeaderValue::from_str(&format!("{exposed}, {EXPOSED_HEADER_NAMES}"))
            {
                headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
            }
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
        notice.apply(response.headers_mut(), state.docs_url.as_deref());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn notice(
        announced_at: DateTime<Utc>,
        sunset: DateTime<Utc>,
        successor: Option<&str>,
    ) -> ModelDeprecationNotice {
        ModelDeprecationNotice {
            announced_at,
            sunset,
            successor: successor.map(str::to_string),
        }
    }

    fn example() -> ModelDeprecationNotice {
        notice(
            Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 12, 1, 13, 0, 0).unwrap(),
            Some("zai-org/GLM-5.3"),
        )
    }

    #[test]
    fn deprecation_is_a_structured_field_date() {
        assert_eq!(example().deprecation_value(), "@1791201600");
    }

    #[test]
    fn sunset_is_an_http_date() {
        assert_eq!(example().sunset_value(), "Tue, 01 Dec 2026 13:00:00 GMT");
    }

    #[test]
    fn link_carries_documentation_and_percent_encoded_successor() {
        assert_eq!(
            example()
                .link_value(Some("https://docs.example.com/model-deprecations"))
                .as_deref(),
            Some(
                "<https://docs.example.com/model-deprecations>; rel=\"deprecation\"; \
                 type=\"text/html\", </v1/model/zai-org%2FGLM-5.3>; rel=\"successor-version\""
            )
        );
    }

    #[test]
    fn link_is_omitted_without_documentation_or_successor() {
        let mut bare = example();
        bare.successor = None;
        assert_eq!(bare.link_value(None), None);

        let mut headers = HeaderMap::new();
        bare.apply(&mut headers, None);
        assert_eq!(headers.get(HEADER_DEPRECATION).unwrap(), "@1791201600");
        assert_eq!(
            headers.get(HEADER_SUNSET).unwrap(),
            "Tue, 01 Dec 2026 13:00:00 GMT"
        );
        assert!(headers.get(header::LINK).is_none());
    }

    #[test]
    fn apply_extends_an_explicit_cors_expose_list() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("Inference-Id"),
        );
        example().apply(&mut headers, None);
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_EXPOSE_HEADERS).unwrap(),
            "Inference-Id, Deprecation, Sunset, Link"
        );

        let mut wildcard = HeaderMap::new();
        wildcard.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("*"),
        );
        example().apply(&mut wildcard, None);
        assert_eq!(
            wildcard.get(header::ACCESS_CONTROL_EXPOSE_HEADERS).unwrap(),
            "*"
        );

        let mut none = HeaderMap::new();
        example().apply(&mut none, None);
        assert!(none.get(header::ACCESS_CONTROL_EXPOSE_HEADERS).is_none());
    }

    #[test]
    fn apply_keeps_an_existing_link_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::LINK,
            HeaderValue::from_static("<https://example.com/next>; rel=\"next\""),
        );
        example().apply(&mut headers, None);
        let links: Vec<_> = headers.get_all(header::LINK).iter().collect();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], "<https://example.com/next>; rel=\"next\"");
        assert_eq!(
            links[1],
            "</v1/model/zai-org%2FGLM-5.3>; rel=\"successor-version\""
        );
    }

    fn model(
        deprecation_date: Option<DateTime<Utc>>,
        announced_at: Option<DateTime<Utc>>,
    ) -> ModelWithPricing {
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
            deprecation_announced_at: announced_at,
            successor_model_name: Some("nearai/new-model".to_string()),
            openrouter_slug: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn no_planned_deprecation_means_no_notice() {
        let announced = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(
            ModelDeprecationNotice::from_model(&model(None, Some(announced))),
            None
        );
    }

    #[test]
    fn notice_uses_the_recorded_announcement() {
        let announced = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        let sunset = Utc.with_ymd_and_hms(2026, 12, 1, 13, 0, 0).unwrap();
        assert_eq!(
            ModelDeprecationNotice::from_model(&model(Some(sunset), Some(announced))),
            Some(notice(announced, sunset, Some("nearai/new-model")))
        );
    }

    #[test]
    fn deprecation_is_clamped_so_sunset_is_never_earlier() {
        let announced = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        let backdated = Utc.with_ymd_and_hms(2026, 9, 1, 13, 0, 0).unwrap();
        assert_eq!(
            ModelDeprecationNotice::from_model(&model(Some(backdated), Some(announced))),
            Some(notice(backdated, backdated, Some("nearai/new-model")))
        );
        // No recorded announcement: deprecated as of the planned date.
        assert_eq!(
            ModelDeprecationNotice::from_model(&model(Some(backdated), None)),
            Some(notice(backdated, backdated, Some("nearai/new-model")))
        );
    }
}
