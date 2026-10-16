use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::admin_settings::AdminSettingsService;
use services::attestation::{
    tinfoil::compiled_tinfoil_pins,
    tinfoil_pins_view::{tinfoil_pins_view, PinsViewMemo},
    tinfoil_trust::TinfoilTrustKey,
};
use std::sync::Arc;

use crate::models::ErrorResponse;

/// State of the pins route: the settings service and the rendered body for
/// the latest `tinfoil_trust` version.
pub struct TinfoilPinsState {
    settings: Arc<AdminSettingsService>,
    memo: Arc<PinsViewMemo>,
}

impl TinfoilPinsState {
    pub fn new(settings: Arc<AdminSettingsService>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            memo: Arc::new(PinsViewMemo::default()),
        })
    }
}

/// Which Tinfoil trust inputs are live
///
/// Public endpoint, a deliberate and documented exception like
/// `/v1/attestation/ita-token` (see `build_public_attestation_routes` in
/// `routes/attestation.rs`). It lists the compiled Tinfoil pins, the admin
/// switches, and the admin-added model pins with their Sigstore provenance
/// (Rekor log index, integration time, signed registers). It carries no
/// customer data, and raw Sigstore bundles are never included. Admin pins are
/// verified here, so `verified` reflects what the bundle proves today. The
/// response is rendered once per `tinfoil_trust` version and served from memory
/// until it changes.
#[utoipa::path(
    get,
    path = "/v1/attestation/tinfoil/pins",
    responses(
        (status = 200, description = "Live Tinfoil trust inputs", body = serde_json::Value),
        (status = 500, description = "Compiled pins unreadable", body = ErrorResponse)
    ),
    security(()),
    tag = "Attestation"
)]
pub async fn get_tinfoil_pins(State(state): State<Arc<TinfoilPinsState>>) -> Response {
    let version = state.settings.version::<TinfoilTrustKey>();
    let body = match state.memo.get(version) {
        Some(hit) => hit,
        None => {
            let Ok(pins) = compiled_tinfoil_pins() else {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Tinfoil pins unavailable".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
                    .into_response();
            };
            // Read the trust value after the version, so the body is never
            // older than the version it is stored under. Verification is
            // CPU-bound; keep it off the executor and outside any lock.
            let trust = state.settings.current::<TinfoilTrustKey>();
            let memo = state.memo.clone();
            let built = tokio::task::spawn_blocking(move || {
                memo.get_or_build(version, || {
                    serde_json::to_value(tinfoil_pins_view(pins, &trust)).unwrap_or_default()
                })
            })
            .await;
            let Ok(body) = built else {
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            body
        }
    };
    let view = body;
    let mut response = ResponseJson(view).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=30"),
    );
    response
}
