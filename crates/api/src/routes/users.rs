use crate::{middleware::AuthenticatedUser, models::ErrorResponse, routes::api::AppState};
use axum::{
    extract::{Extension, Json, State},
    http::{HeaderMap, StatusCode},
};
use services::auth::AuthError;
use tracing::{debug, error};

/// Create a new access token
///
/// Creates a new short-lived access token using the current refresh token.
#[utoipa::path(
    post,
    path = "/v1/users/me/access-tokens",
    tag = "Users",
    responses(
        (status = 200, description = "Access token created successfully", body = crate::models::AccessAndRefreshTokenResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("refresh_token" = [])
    )
)]
pub async fn create_access_token(
    State(app_state): State<AppState>,
    headers: HeaderMap,
    Extension((session, user)): Extension<(services::auth::Session, AuthenticatedUser)>,
) -> Result<Json<crate::models::AccessAndRefreshTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Creating access token & rotating refresh token for user: {}",
        user.0.id
    );

    // Rotate the refresh token session
    let old_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "Refresh token is required".to_string(),
                    "unauthorized".to_string(),
                )),
            )
        })?;
    let expires_in_hours = 7 * 24;
    let result = app_state
        .auth_service
        .rotate_session(
            session.user_id,
            session.id,
            old_token,
            app_state.config.auth.encoding_key.to_string(),
            1,                // access token expires in 1 hour
            expires_in_hours, // refresh token expires in 7 days
        )
        .await;

    match result {
        Ok((access_token, refresh_session, refresh_token)) => {
            Ok(Json(crate::models::AccessAndRefreshTokenResponse {
                access_token,
                refresh_token,
                refresh_token_expiration: refresh_session.expires_at,
            }))
        }
        Err(AuthError::Unauthorized) => {
            tracing::warn!("Token rotation failed: invalid or already rotated token");
            Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "Invalid or expired refresh token".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
        }
        Err(_) => {
            error!(
                error_kind = "token_rotation_failed",
                "Failed to create access token and rotate refresh token"
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create access token and rotate refresh token".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}
