use crate::middleware::AuthenticatedUser;
use crate::models::ErrorResponse;
use crate::routes::api::AppState;
use axum::{extract::State, http::StatusCode, response::Json as ResponseJson, Extension};
use services::kyt::KytCheckResponse;

type RouteResult<T> = Result<ResponseJson<T>, (StatusCode, ResponseJson<ErrorResponse>)>;

/// Check the authenticated user's connected NEAR wallet KYT risk
///
/// Runs a server-side KYT check for the NEAR account associated with the
/// authenticated session and returns a provider-independent risk response.
#[utoipa::path(
    get,
    path = "/v1/users/me/kyt/near",
    tag = "Users",
    responses(
        (status = 200, description = "NEAR wallet KYT result", body = KytCheckResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "NEAR wallet authentication required", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_current_user_near_kyt(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> RouteResult<KytCheckResponse> {
    if user.0.auth_provider != "near" || user.0.provider_user_id.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "KYT checks require NEAR wallet authentication".to_string(),
                "near_auth_required".to_string(),
            )),
        ));
    }

    Ok(ResponseJson(
        app_state
            .kyt_service
            .check_near_account(
                &app_state.config.auth.near.network_id,
                &user.0.provider_user_id,
            )
            .await,
    ))
}
