use crate::middleware::{AdminUser, AuthenticatedUser};
use crate::models::ErrorResponse;
use crate::routes::common::{validate_limit_offset, validate_max_length, validate_non_empty_field};
use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use database::repositories::{
    FeatureRequestRepository, FeatureRequestSummary, FeatureRequestTarget,
    FeatureRequestVoteSummary, SubmitFeatureRequestParams,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::error;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

const MAX_KEY_LENGTH: usize = 255;
const MAX_TITLE_LENGTH: usize = 255;
const MAX_NOTE_LENGTH: usize = 2000;
const MAX_SOURCE_LENGTH: usize = 100;

#[derive(Clone)]
pub struct FeatureRequestsRouteState {
    pub repository: Arc<FeatureRequestRepository>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FeatureRequestKind {
    Model,
    Feature,
}

impl FeatureRequestKind {
    fn as_str(&self) -> &'static str {
        match self {
            FeatureRequestKind::Model => "model",
            FeatureRequestKind::Feature => "feature",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "model" => Some(FeatureRequestKind::Model),
            "feature" => Some(FeatureRequestKind::Feature),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubmitFeatureRequest {
    pub kind: String,
    pub key: String,
    pub title: Option<String>,
    pub note: Option<String>,
    pub organization_id: Option<Uuid>,
    pub source: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeatureRequestTargetResponse {
    pub id: Uuid,
    pub kind: String,
    pub key: String,
    pub title: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubmitFeatureRequestResponse {
    pub target: FeatureRequestTargetResponse,
    pub unique_user_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeatureRequestVoteResponse {
    pub user_id: Uuid,
    pub user_email: String,
    pub user_display_name: Option<String>,
    pub organization_id: Option<Uuid>,
    pub organization_name: Option<String>,
    pub note: Option<String>,
    pub source: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminFeatureRequestSummaryResponse {
    pub target: FeatureRequestTargetResponse,
    pub unique_user_count: i64,
    pub unique_organization_count: i64,
    pub latest_requested_at: chrono::DateTime<chrono::Utc>,
    pub recent_votes: Vec<FeatureRequestVoteResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminFeatureRequestListResponse {
    pub requests: Vec<AdminFeatureRequestSummaryResponse>,
    pub limit: i64,
    pub offset: i64,
    pub total: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct AdminFeatureRequestListQuery {
    pub kind: Option<String>,
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Submit or update the current user's interest in a feature request target.
#[utoipa::path(
    post,
    path = "/v1/feature-requests",
    tag = "Feature Requests",
    request_body = SubmitFeatureRequest,
    responses(
        (status = 200, description = "Feature request recorded", body = SubmitFeatureRequestResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Organization membership required", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn submit_feature_request(
    State(state): State<FeatureRequestsRouteState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<SubmitFeatureRequest>,
) -> Result<ResponseJson<SubmitFeatureRequestResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let kind = parse_kind(&request.kind)?;
    let key = normalize_key(&request.key);
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| request.key.trim())
        .to_string();
    let note = request
        .note
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let source = request
        .source
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    validate_input(&key, &title, note.as_deref(), source.as_deref())?;

    if let Some(org_id) = request.organization_id {
        let belongs = state
            .repository
            .user_belongs_to_organization(user.0.id, org_id)
            .await
            .map_err(internal_error)?;
        if !belongs {
            return Err((
                StatusCode::FORBIDDEN,
                ResponseJson(ErrorResponse::new(
                    "User does not belong to the specified organization".to_string(),
                    "forbidden".to_string(),
                )),
            ));
        }
    }

    let result = state
        .repository
        .submit(SubmitFeatureRequestParams {
            kind: kind.as_str().to_string(),
            key,
            title,
            user_id: user.0.id,
            organization_id: request.organization_id,
            note,
            source,
        })
        .await
        .map_err(internal_error)?;

    Ok(ResponseJson(SubmitFeatureRequestResponse {
        target: target_to_response(result.target),
        unique_user_count: result.unique_user_count,
    }))
}

/// List aggregated feature requests for admins.
#[utoipa::path(
    get,
    path = "/v1/admin/feature-requests",
    tag = "Admin",
    params(AdminFeatureRequestListQuery),
    responses(
        (status = 200, description = "Aggregated feature requests", body = AdminFeatureRequestListResponse),
        (status = 400, description = "Invalid parameters", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_admin_feature_requests(
    State(state): State<FeatureRequestsRouteState>,
    Extension(_admin_user): Extension<AdminUser>,
    Query(params): Query<AdminFeatureRequestListQuery>,
) -> Result<ResponseJson<AdminFeatureRequestListResponse>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    validate_limit_offset(params.limit, params.offset)?;

    let kind = match params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let normalized = value.to_ascii_lowercase();
            if FeatureRequestKind::parse(&normalized).is_none() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "kind must be 'model' or 'feature'".to_string(),
                        "invalid_parameter".to_string(),
                    )),
                ));
            }
            Some(normalized)
        }
        None => None,
    };

    let (requests, total) = state
        .repository
        .list_admin(kind.as_deref(), params.limit, params.offset)
        .await
        .map_err(internal_error)?;

    Ok(ResponseJson(AdminFeatureRequestListResponse {
        requests: requests.into_iter().map(summary_to_response).collect(),
        limit: params.limit,
        offset: params.offset,
        total,
    }))
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn parse_kind(
    value: &str,
) -> Result<FeatureRequestKind, (StatusCode, ResponseJson<ErrorResponse>)> {
    let normalized = value.trim().to_ascii_lowercase();
    FeatureRequestKind::parse(&normalized).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "kind must be 'model' or 'feature'".to_string(),
                "invalid_request".to_string(),
            )),
        )
    })
}

fn validate_input(
    key: &str,
    title: &str,
    note: Option<&str>,
    source: Option<&str>,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    let validation = || -> Result<(), String> {
        validate_non_empty_field(key, "key")?;
        validate_max_length(key, "key", MAX_KEY_LENGTH)?;
        validate_non_empty_field(title, "title")?;
        validate_max_length(title, "title", MAX_TITLE_LENGTH)?;
        if let Some(note) = note {
            validate_max_length(note, "note", MAX_NOTE_LENGTH)?;
        }
        if let Some(source) = source {
            validate_max_length(source, "source", MAX_SOURCE_LENGTH)?;
        }
        Ok(())
    };

    validation().map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(message, "invalid_request".to_string())),
        )
    })
}

fn target_to_response(target: FeatureRequestTarget) -> FeatureRequestTargetResponse {
    FeatureRequestTargetResponse {
        id: target.id,
        kind: target.kind,
        key: target.key,
        title: target.title,
        status: target.status,
        created_at: target.created_at,
        updated_at: target.updated_at,
    }
}

fn vote_to_response(vote: FeatureRequestVoteSummary) -> FeatureRequestVoteResponse {
    FeatureRequestVoteResponse {
        user_id: vote.user_id,
        user_email: vote.user_email,
        user_display_name: vote.user_display_name,
        organization_id: vote.organization_id,
        organization_name: vote.organization_name,
        note: vote.note,
        source: vote.source,
        updated_at: vote.updated_at,
    }
}

fn summary_to_response(summary: FeatureRequestSummary) -> AdminFeatureRequestSummaryResponse {
    AdminFeatureRequestSummaryResponse {
        target: target_to_response(summary.target),
        unique_user_count: summary.unique_user_count,
        unique_organization_count: summary.unique_organization_count,
        latest_requested_at: summary.latest_requested_at,
        recent_votes: summary
            .recent_votes
            .into_iter()
            .map(vote_to_response)
            .collect(),
    }
}

fn internal_error(error: anyhow::Error) -> (StatusCode, ResponseJson<ErrorResponse>) {
    error!("Feature request operation failed: {:?}", error);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            "Internal server error".to_string(),
            "internal_server_error".to_string(),
        )),
    )
}
