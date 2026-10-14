use super::*;
use services::auth::{SessionId, UserId};

#[derive(Deserialize)]
pub struct OAuthLinkInit {
    provider: String,
    frontend_callback: String,
}

#[derive(Deserialize)]
pub struct OAuthLinkComplete {
    code: String,
}

async fn original_session(
    auth: &dyn AuthServiceTrait,
    config: &ApiConfig,
    headers: &HeaderMap,
    user_id: UserId,
) -> Result<SessionId, AuthError> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(AuthError::Unauthorized)?;
    let claims = auth
        .validate_session_access_token(token.into(), config.auth.encoding_key.clone())?
        .ok_or(AuthError::Unauthorized)?;
    if claims.sub != user_id {
        return Err(AuthError::Unauthorized);
    }
    let session_id = claims.sid.ok_or(AuthError::OAuthLinkInvalid)?;
    auth.validate_oauth_link_session(user_id, session_id.clone())
        .await?;
    Ok(session_id)
}

pub async fn list_oauth_identities(
    Extension(user): Extension<AuthenticatedUser>,
    State((_, _, auth, _)): State<AuthState>,
) -> Response {
    match auth.list_oauth_identities(UserId(user.0.id)).await {
        Ok(identities) => Json(serde_json::json!({
            "can_link": matches!(user.0.auth_provider.as_str(), "github" | "google"),
            "identities": identities.into_iter().map(|identity| serde_json::json!({
                "provider": identity.provider,
                "email": identity.email,
                "is_primary": identity.provider == user.0.auth_provider,
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(error) => link_error_response(error, None, &config::CorsConfig::default()),
    }
}

pub async fn start_oauth_link(
    Extension(user): Extension<AuthenticatedUser>,
    State((oauth, store, auth, config)): State<AuthState>,
    headers: HeaderMap,
    Json(params): Json<OAuthLinkInit>,
) -> Response {
    let user_id = UserId(user.0.id);
    let result = async {
        let session = original_session(auth.as_ref(), &config, &headers, user_id.clone()).await?;
        let callback = validate_frontend_callback(&params.frontend_callback, &config.cors)
            .map_err(|_| AuthError::OAuthLinkInvalid)?;
        if callback.path() != "/auth/link/callback" {
            return Err(AuthError::OAuthLinkInvalid);
        }
        if auth
            .list_oauth_identities(user_id.clone())
            .await?
            .iter()
            .any(|identity| identity.provider == params.provider)
        {
            return Err(AuthError::OAuthLinkConflict);
        }
        let (url, state, verifier) = match params.provider.as_str() {
            "github" => oauth.github_link_auth_url()?,
            "google" => oauth.google_link_auth_url()?,
            _ => return Err(AuthError::OAuthLinkNotAllowed),
        };
        store
            .create_link(
                state,
                params.provider,
                verifier,
                params.frontend_callback,
                user_id.0,
                session.0,
            )
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to store link state: {e}")))?;
        Ok(url)
    }
    .await;
    match result {
        Ok(url) => Json(serde_json::json!({"auth_url":url})).into_response(),
        Err(error) => link_error_response(error, None, &config.cors),
    }
}

pub async fn complete_oauth_link(
    Extension(user): Extension<AuthenticatedUser>,
    State((_, _, auth, config)): State<AuthState>,
    headers: HeaderMap,
    Json(params): Json<OAuthLinkComplete>,
) -> Response {
    let user_id = UserId(user.0.id);
    let result = async {
        let session = original_session(auth.as_ref(), &config, &headers, user_id.clone()).await?;
        auth.complete_oauth_link(user_id, session, &params.code)
            .await
    }
    .await;
    match result {
        Ok(identity) => {
            Json(serde_json::json!({"provider":identity.provider,"linked":true})).into_response()
        }
        Err(error) => link_error_response(error, None, &config.cors),
    }
}

pub(super) fn link_error_response(
    error: AuthError,
    callback: Option<&str>,
    cors: &config::CorsConfig,
) -> Response {
    let (status, code) = match error {
        AuthError::AuthFailed(_) => (StatusCode::BAD_REQUEST, "oauth_link_canceled"),
        AuthError::OAuthLinkConflict => (StatusCode::CONFLICT, "oauth_link_conflict"),
        AuthError::OAuthEmailUnverified => (StatusCode::FORBIDDEN, "oauth_email_unverified"),
        AuthError::OAuthLinkNotAllowed => (StatusCode::FORBIDDEN, "oauth_link_not_allowed"),
        AuthError::OAuthLinkInvalid
        | AuthError::SessionNotFound
        | AuthError::Unauthorized
        | AuthError::UserNotFound => (StatusCode::UNAUTHORIZED, "oauth_link_invalid"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "oauth_link_failed"),
    };
    if let Some(url) = callback.and_then(|url| validate_frontend_callback(url, cors).ok()) {
        let mut url = url;
        let fragment = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("link_error", code)
            .finish();
        url.set_fragment(Some(&fragment));
        return Redirect::temporary(url.as_str()).into_response();
    }
    (status, Json(serde_json::json!({"error":code}))).into_response()
}

pub(super) fn link_ready_response(
    code: &str,
    callback: &str,
    cors: &config::CorsConfig,
) -> Response {
    let Ok(mut url) = validate_frontend_callback(callback, cors) else {
        return link_error_response(AuthError::OAuthLinkInvalid, None, cors);
    };
    let fragment = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("link_code", code)
        .finish();
    url.set_fragment(Some(&fragment));
    Redirect::temporary(url.as_str()).into_response()
}
