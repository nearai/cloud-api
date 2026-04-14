use crate::middleware::{AdminUser, AuthenticatedUser};
use crate::models::{
    ClaimCreditsRequest, ClaimCreditsResponse, CreateCreditEventRequest, CreditEventCodeResponse,
    CreditEventResponse, ErrorResponse, GenerateCodesRequest, GenerateCodesResponse,
};
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::Json as ResponseJson;
use axum::Extension;
use services::credit_events::ports::{CreditEventError, CreditEventServiceTrait};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct CreditEventAppState {
    pub credit_event_service: Arc<dyn CreditEventServiceTrait>,
}

fn map_error(e: CreditEventError) -> (StatusCode, ResponseJson<ErrorResponse>) {
    match e {
        CreditEventError::NotFound(msg) => (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(msg, "not_found".to_string())),
        ),
        CreditEventError::EventInactive => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Event is no longer active".to_string(),
                "event_inactive".to_string(),
            )),
        ),
        CreditEventError::ClaimPeriodNotStarted => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Claim period has not started yet".to_string(),
                "claim_not_started".to_string(),
            )),
        ),
        CreditEventError::ClaimPeriodEnded => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Claim period has ended".to_string(),
                "claim_deadline_passed".to_string(),
            )),
        ),
        CreditEventError::MaxClaimsReached => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Maximum claims reached for this event".to_string(),
                "max_claims_reached".to_string(),
            )),
        ),
        CreditEventError::InvalidCode => (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Invalid promo code".to_string(),
                "code_not_found".to_string(),
            )),
        ),
        CreditEventError::CodeAlreadyClaimed => (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "Promo code has already been claimed".to_string(),
                "code_already_claimed".to_string(),
            )),
        ),
        CreditEventError::UserAlreadyClaimed => (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "User has already claimed credits for this event".to_string(),
                "user_already_claimed".to_string(),
            )),
        ),
        CreditEventError::Unauthorized(msg) => (
            StatusCode::UNAUTHORIZED,
            ResponseJson(ErrorResponse::new(msg, "unauthorized".to_string())),
        ),
        CreditEventError::ValidationError(msg) => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(msg, "validation_error".to_string())),
        ),
        CreditEventError::InternalError(msg) => {
            tracing::error!(error = %msg, "Credit event internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Internal server error".to_string(),
                    "internal_error".to_string(),
                )),
            )
        }
    }
}

fn event_to_response(
    event: &services::credit_events::ports::CreditEventInfo,
) -> CreditEventResponse {
    CreditEventResponse {
        id: event.id.to_string(),
        name: event.name.clone(),
        description: event.description.clone(),
        credit_amount: event.credit_amount,
        currency: event.currency.clone(),
        max_claims: event.max_claims,
        claim_count: event.claim_count,
        starts_at: event.starts_at.to_rfc3339(),
        claim_deadline: event.claim_deadline.map(|dt| dt.to_rfc3339()),
        credit_expires_at: event.credit_expires_at.to_rfc3339(),
        is_active: event.is_active,
        created_at: event.created_at.to_rfc3339(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/admin/credit-events",
    tag = "Credit Events",
    request_body = CreateCreditEventRequest,
    responses(
        (status = 201, description = "Credit event created", body = CreditEventResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - admin only", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("admin_token" = [])
    )
)]
pub async fn create_credit_event(
    State(app_state): State<CreditEventAppState>,
    Extension(admin_user): Extension<AdminUser>,
    ResponseJson(request): ResponseJson<CreateCreditEventRequest>,
) -> Result<ResponseJson<CreditEventResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let starts_at = match request.starts_at {
        Some(ref s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            "Invalid startsAt format, expected ISO 8601".to_string(),
                            "validation_error".to_string(),
                        )),
                    )
                })?
                .to_utc(),
        ),
        None => None,
    };
    let claim_deadline = match request.claim_deadline {
        Some(ref s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            "Invalid claimDeadline format, expected ISO 8601".to_string(),
                            "validation_error".to_string(),
                        )),
                    )
                })?
                .to_utc(),
        ),
        None => None,
    };
    let credit_expires_at = chrono::DateTime::parse_from_rfc3339(&request.credit_expires_at)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "Invalid creditExpiresAt format, expected ISO 8601".to_string(),
                    "validation_error".to_string(),
                )),
            )
        })?
        .to_utc();

    let service_request = services::credit_events::ports::CreateEventRequest {
        name: request.name,
        description: request.description,
        credit_amount: request.credit_amount,
        currency: request.currency,
        max_claims: request.max_claims,
        starts_at,
        claim_deadline,
        credit_expires_at,
        created_by_user_id: Some(admin_user.0.id),
    };

    let event = app_state
        .credit_event_service
        .create_event(service_request)
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(event_to_response(&event)))
}

#[utoipa::path(
    get,
    path = "/v1/credit-events/{event_id}",
    tag = "Credit Events",
    params(
        ("event_id" = String, Path, description = "Credit event ID")
    ),
    responses(
        (status = 200, description = "Credit event details", body = CreditEventResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn get_credit_event(
    State(app_state): State<CreditEventAppState>,
    Path(event_id): Path<String>,
) -> Result<ResponseJson<CreditEventResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let event_uuid = Uuid::parse_str(&event_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid event ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let event = app_state
        .credit_event_service
        .get_event(event_uuid)
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(event_to_response(&event)))
}

#[utoipa::path(
    get,
    path = "/v1/credit-events",
    tag = "Credit Events",
    responses(
        (status = 200, description = "List of active credit events", body = Vec<CreditEventResponse>),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    )
)]
pub async fn list_credit_events(
    State(app_state): State<CreditEventAppState>,
) -> Result<ResponseJson<Vec<CreditEventResponse>>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let events = app_state
        .credit_event_service
        .list_events()
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(events.iter().map(event_to_response).collect()))
}

#[utoipa::path(
    patch,
    path = "/v1/admin/credit-events/{event_id}",
    tag = "Credit Events",
    params(
        ("event_id" = String, Path, description = "Credit event ID")
    ),
    responses(
        (status = 200, description = "Credit event deactivated", body = CreditEventResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - admin only", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("admin_token" = [])
    )
)]
pub async fn deactivate_credit_event(
    State(app_state): State<CreditEventAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(event_id): Path<String>,
) -> Result<ResponseJson<CreditEventResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let event_uuid = Uuid::parse_str(&event_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid event ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let event = app_state
        .credit_event_service
        .deactivate_event(event_uuid)
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(event_to_response(&event)))
}

#[utoipa::path(
    post,
    path = "/v1/admin/credit-events/{event_id}/codes",
    tag = "Credit Events",
    params(
        ("event_id" = String, Path, description = "Credit event ID")
    ),
    request_body = GenerateCodesRequest,
    responses(
        (status = 200, description = "Promo codes generated", body = GenerateCodesResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - admin only", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("admin_token" = [])
    )
)]
pub async fn generate_promo_codes(
    State(app_state): State<CreditEventAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(event_id): Path<String>,
    ResponseJson(request): ResponseJson<GenerateCodesRequest>,
) -> Result<ResponseJson<GenerateCodesResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let event_uuid = Uuid::parse_str(&event_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid event ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let codes = app_state
        .credit_event_service
        .generate_codes(services::credit_events::ports::GenerateCodesRequest {
            event_id: event_uuid,
            count: request.count,
        })
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(GenerateCodesResponse { codes }))
}

#[utoipa::path(
    get,
    path = "/v1/admin/credit-events/{event_id}/codes",
    tag = "Credit Events",
    params(
        ("event_id" = String, Path, description = "Credit event ID")
    ),
    responses(
        (status = 200, description = "List of promo codes", body = Vec<CreditEventCodeResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - admin only", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("admin_token" = [])
    )
)]
pub async fn list_credit_event_codes(
    State(app_state): State<CreditEventAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(event_id): Path<String>,
) -> Result<ResponseJson<Vec<CreditEventCodeResponse>>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let event_uuid = Uuid::parse_str(&event_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid event ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let codes = app_state
        .credit_event_service
        .get_codes(event_uuid)
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(
        codes
            .into_iter()
            .map(|c| CreditEventCodeResponse {
                id: c.id.to_string(),
                code: c.code,
                is_claimed: c.is_claimed,
                claimed_by_user_id: c.claimed_by_user_id.map(|id| id.to_string()),
                claimed_by_near_account_id: c.claimed_by_near_account_id,
                claimed_at: c.claimed_at.map(|dt| dt.to_rfc3339()),
            })
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/v1/credit-events/{event_id}/claim",
    tag = "Credit Events",
    params(
        ("event_id" = String, Path, description = "Credit event ID")
    ),
    request_body = ClaimCreditsRequest,
    responses(
        (status = 200, description = "Credits claimed successfully", body = ClaimCreditsResponse),
        (status = 400, description = "Invalid request or claim period ended", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Event or code not found", body = ErrorResponse),
        (status = 409, description = "Code already claimed", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn claim_credits(
    State(app_state): State<CreditEventAppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(event_id): Path<String>,
    Json(request): Json<ClaimCreditsRequest>,
) -> Result<ResponseJson<ClaimCreditsResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let event_uuid = Uuid::parse_str(&event_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid event ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let user_id = user.0.id;
    let claimer_id = format!("user:{}", user_id);

    let result = app_state
        .credit_event_service
        .claim_credits(services::credit_events::ports::ClaimCreditsRequest {
            event_id: event_uuid,
            code: request.code,
            near_account_id: claimer_id,
            user_id,
        })
        .await
        .map_err(map_error)?;

    Ok(ResponseJson(ClaimCreditsResponse {
        claim_id: result.claim_id.to_string(),
        event_id: result.event_id.to_string(),
        near_account_id: result.near_account_id,
        organization_id: result.organization_id.to_string(),
        credit_amount: result.credit_amount,
        api_key: result.api_key,
        credit_expires_at: result.credit_expires_at.to_rfc3339(),
    }))
}
