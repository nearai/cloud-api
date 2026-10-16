use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::admin_settings::AdminSettingsService;
use services::attestation::{
    tinfoil::vetted_tinfoil_pins, tinfoil_pins_view::tinfoil_pins_view,
    tinfoil_trust::TinfoilTrustKey,
};
use std::sync::Arc;

use crate::models::ErrorResponse;

/// Which Tinfoil trust inputs are live
///
/// Public endpoint, a deliberate and documented exception like
/// `/v1/attestation/ita-token` (see `build_public_attestation_routes` in
/// `routes/attestation.rs`). It lists the compiled Tinfoil pins, the admin
/// switches, and the admin-added model pins with their Sigstore provenance
/// (Rekor log index, integration time, signed registers). It carries no
/// customer data, and raw Sigstore bundles are never included. Admin pins are
/// re-verified here, so `verified` reflects what the bundle proves today.
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
pub async fn get_tinfoil_pins(State(settings): State<Arc<AdminSettingsService>>) -> Response {
    let Ok(pins) = vetted_tinfoil_pins() else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Tinfoil pins unavailable".to_string(),
                "internal_server_error".to_string(),
            )),
        )
            .into_response();
    };
    let trust = settings.current::<TinfoilTrustKey>();
    // Verification is CPU-bound (cached per bundle); keep it off the executor.
    let view = tokio::task::spawn_blocking(move || tinfoil_pins_view(&pins, &trust)).await;
    let Ok(view) = view else {
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = ResponseJson(view).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=30"),
    );
    response
}
