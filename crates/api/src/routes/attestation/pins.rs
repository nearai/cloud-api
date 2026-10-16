use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::attestation::pins_view::{
    render_pins_view, source_versions, PinsViewMemo, ProviderPinsSource,
};
use std::sync::Arc;

use crate::models::ErrorResponse;

/// State of the pins route: the backup providers' pin sources and the rendered
/// body for the latest version of each.
pub struct AttestedPinsState {
    sources: Vec<Arc<dyn ProviderPinsSource>>,
    memo: Arc<PinsViewMemo>,
}

impl AttestedPinsState {
    pub fn new(sources: Vec<Arc<dyn ProviderPinsSource>>) -> Arc<Self> {
        Arc::new(Self {
            sources,
            memo: Arc::new(PinsViewMemo::default()),
        })
    }
}

/// Which attested backup-provider trust inputs are live
///
/// Public endpoint, a deliberate and documented exception like
/// `/v1/attestation/ita-token` (see `build_public_attestation_routes` in
/// `routes/attestation.rs`). The body is an object keyed by provider name
/// (`tinfoil`, `chutes`). Each section lists the provider's compiled pins and
/// any runtime trust it accepts (Tinfoil: the admin switches and admin-added
/// model pins with their Sigstore provenance; Chutes: `runtime` is `null`,
/// compiled-only). It carries no customer data, and raw Sigstore bundles are
/// never included. Admin pins are verified here, so `verified` reflects what
/// the bundle proves today. The response is rendered once per set of provider
/// trust versions and served from memory until one changes.
#[utoipa::path(
    get,
    path = "/v1/attestation/pins",
    responses(
        (status = 200, description = "Live backup-provider trust inputs, keyed by provider", body = serde_json::Value),
        (status = 500, description = "Pins view could not be built", body = ErrorResponse)
    ),
    security(()),
    tag = "Attestation"
)]
pub async fn get_attested_pins(State(state): State<Arc<AttestedPinsState>>) -> Response {
    // Versions are read before the views, so a body is never older than the
    // versions it is stored under.
    let versions = source_versions(&state.sources);
    let body = match state.memo.get(&versions) {
        Some(hit) => hit,
        None => {
            // Verification is CPU-bound; keep it off the executor and outside
            // any lock.
            let sources = state.sources.clone();
            let memo = state.memo.clone();
            let built = tokio::task::spawn_blocking(move || {
                memo.get_or_build(versions, || render_pins_view(&sources))
            })
            .await;
            let Ok(body) = built else {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Pins unavailable".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
                    .into_response();
            };
            body
        }
    };
    let mut response = ResponseJson(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=30"),
    );
    response
}
