use crate::middleware::auth::{auth_middleware, AuthState, AuthenticatedUser};
use crate::models::ErrorResponse;
use axum::{
    extract::{Extension, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
    Json,
};
use database::repositories::organization_credentials::{
    hash_credential, OrganizationActor, OrganizationOperationError,
};

pub(crate) type ApiError = (StatusCode, Json<ErrorResponse>);

pub(crate) fn error_code(error: &anyhow::Error) -> &'static str {
    let database_busy = error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(|error| error.code())
            .is_some_and(|code| matches!(code.code(), "55P03" | "57014" | "40P01"))
    });

    error
        .downcast_ref::<OrganizationOperationError>()
        .map(|e| e.0)
        .unwrap_or(if database_busy {
            "accounting_busy"
        } else {
            "transfer_unavailable"
        })
}

pub(crate) fn error(error: anyhow::Error) -> ApiError {
    let code = error_code(&error);
    let status = match code {
        "credential_refused" | "sender_not_enabled" | "credential_management_disabled" => {
            StatusCode::FORBIDDEN
        }
        "organization_unavailable" | "credential_not_found" => StatusCode::NOT_FOUND,
        "insufficient_posted_capacity"
        | "sender_not_reconciled"
        | "idempotency_conflict"
        | "capacity_overflow"
        | "credential_transfer_limit_exceeded"
        | "organization_transfer_limit_exceeded" => StatusCode::CONFLICT,
        "transfer_rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        "transfer_unavailable" | "accounting_busy" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };

    // Never log arbitrary terms, metadata, token hashes, or database bind values.
    tracing::info!(outcome = code, "Organization operation rejected");

    (status, Json(ErrorResponse::new(code.into(), code.into())))
}

pub(crate) async fn authenticate(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer oc_"));

    if let Some(token) = token {
        if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(error(
                OrganizationOperationError("credential_refused").into(),
            ));
        }

        let actor = OrganizationActor::Credential(hash_credential(&format!("oc_{token}")));
        request.extensions_mut().insert(actor);
        return Ok(next.run(request).await);
    }

    auth_middleware(State(state), request, next).await
}

pub(crate) fn actor(
    user: Option<Extension<AuthenticatedUser>>,
    token: Option<Extension<OrganizationActor>>,
) -> Result<OrganizationActor, ApiError> {
    token
        .map(|Extension(actor)| actor)
        .or_else(|| user.map(|Extension(user)| OrganizationActor::Owner(user.0.id)))
        .ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))
}
