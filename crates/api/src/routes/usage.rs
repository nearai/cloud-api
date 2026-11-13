use crate::{
    middleware::AuthenticatedUser,
    models::{CreditType, ErrorResponse},
    routes::{
        api::AppState,
        common::{analytics_error_response, format_amount},
    },
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use services::{
    organization::OrganizationError,
    usage::{InferenceUsageHistoryQuery, InferenceUsageReportRow, UsageServiceTrait},
};
use utoipa::ToSchema;
use uuid::Uuid;

pub(crate) type UsageError = (StatusCode, ResponseJson<ErrorResponse>);

async fn check_org_membership(
    app_state: &AppState,
    user: AuthenticatedUser,
    org_id: &str,
) -> Result<Uuid, UsageError> {
    let organization_id = Uuid::parse_str(org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let user_id = crate::conversions::authenticated_user_to_user_id(user);
    let is_member = app_state
        .organization_service
        .is_member(
            services::organization::OrganizationId(organization_id),
            user_id,
        )
        .await
        .map_err(|e| match e {
            OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Organization not found".to_string(),
                    "not_found".to_string(),
                )),
            ),
            _ => {
                tracing::error!("Failed to check organization membership");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to verify organization access".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    if !is_member {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "You are not authorized to access this organization.".to_string(),
                "forbidden".to_string(),
            )),
        ));
    }

    Ok(organization_id)
}

/// Get organization balance response
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrganizationBalanceResponse {
    pub organization_id: String,
    /// Historical consumption not yet assigned to a credit type, in nano-USD.
    pub legacy_unattributed_amount: i64,
    pub total_spent: i64,            // In nano-dollars (scale 9)
    pub total_spent_display: String, // Human readable, e.g., "$12.50"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<i64>, // In nano-dollars (scale 9), None if not set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit_display: Option<String>, // Human readable, e.g., "$100.00"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>, // Remaining credits in nano-dollars
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_display: Option<String>, // Human readable remaining
    pub last_usage_at: Option<String>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: String,
    pub credit_limits: Vec<CreditLimitBreakdownResponse>,
    /// Unresolved cost from completed requests that exceeded all capacity.
    pub unfunded_amount: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreditLimitBreakdownResponse {
    #[serde(rename = "type")]
    pub credit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Gross allocated amount before transfers.
    pub amount: i64,
    /// Nonnegative capacity transferred out to other organizations.
    pub transferred_out: i64,
    /// Net capacity for this source: `max(amount - transferred_out, 0)`.
    pub effective_capacity: i64,
    pub consumed: i64,
    /// Remaining spendable capacity: `max(effective_capacity - consumed, 0)`.
    pub available: i64,
    pub currency: String,
}

pub async fn compute_organization_balance_response(
    usage_service: &(dyn UsageServiceTrait + Send + Sync),
    organization_id: Uuid,
) -> Result<OrganizationBalanceResponse, UsageError> {
    let (balance, limit, credit_limits) = tokio::try_join!(
        usage_service.get_balance(organization_id),
        usage_service.get_limit(organization_id),
        usage_service.get_credit_limits(organization_id)
    )
    .map_err(|e| {
        tracing::error!(
            "Failed to get organization balance, limit, or credit limits: {:?}",
            e
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Failed to retrieve balance, limit, or credit limits".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    })?;

    let credit_limits: Vec<CreditLimitBreakdownResponse> = credit_limits
        .into_iter()
        .map(|limit| CreditLimitBreakdownResponse {
            credit_type: limit.credit_type,
            source: limit.source,
            amount: limit.amount,
            transferred_out: limit.transferred_out,
            effective_capacity: limit.effective_capacity,
            consumed: limit.consumed,
            available: limit.available,
            currency: limit.currency,
        })
        .collect();

    match balance {
        Some(balance) => {
            let (spend_limit, spend_limit_display, remaining, remaining_display) =
                if let Some(limit_info) = limit.as_ref() {
                    let remaining_amount = limit_info.available;
                    (
                        Some(limit_info.spend_limit),
                        Some(format_amount(limit_info.spend_limit)),
                        Some(remaining_amount),
                        Some(format_amount(remaining_amount)),
                    )
                } else {
                    (None, None, None, None)
                };

            Ok(OrganizationBalanceResponse {
                organization_id: balance.organization_id.to_string(),
                total_spent: balance.total_spent,
                legacy_unattributed_amount: balance.legacy_unattributed_amount,
                total_spent_display: format_amount(balance.total_spent),
                spend_limit,
                spend_limit_display,
                remaining,
                remaining_display,
                last_usage_at: balance.last_usage_at.map(|dt| dt.to_rfc3339()),
                total_requests: balance.total_requests,
                total_tokens: balance.total_tokens,
                updated_at: balance.updated_at.to_rfc3339(),
                credit_limits,
                unfunded_amount: limit.as_ref().map_or(0, |value| value.unfunded),
            })
        }
        None => {
            if let Some(limit_info) = limit {
                Ok(OrganizationBalanceResponse {
                    organization_id: organization_id.to_string(),
                    total_spent: 0,
                    legacy_unattributed_amount: 0,
                    total_spent_display: format_amount(0),
                    spend_limit: Some(limit_info.spend_limit),
                    spend_limit_display: Some(format_amount(limit_info.spend_limit)),
                    remaining: Some(limit_info.available),
                    remaining_display: Some(format_amount(limit_info.available)),
                    last_usage_at: None,
                    total_requests: 0,
                    total_tokens: 0,
                    updated_at: Utc::now().to_rfc3339(),
                    credit_limits,
                    unfunded_amount: limit_info.unfunded,
                })
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "No usage data or limit found for organization".to_string(),
                        "not_found".to_string(),
                    )),
                ))
            }
        }
    }
}

/// Usage history entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency.
/// `cache_read_tokens` is meaningful only for token-based chat-style models; for other
/// inference types (e.g., rerank, audio transcription, image), it will typically be 0
/// and is not used in billing calculations.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model: String, // Model name (canonical name from models table)
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Number of prompt tokens that were cache hits
    pub cache_read_tokens: i32,
    /// Number of prompt tokens written to provider cache
    pub cache_write_tokens: i32,
    #[serde(rename = "serviceTier", skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(rename = "contextBand", skip_serializing_if = "Option::is_none")]
    pub context_band: Option<String>,
    #[serde(rename = "billingDetails", skip_serializing_if = "Option::is_none")]
    pub billing_details: Option<serde_json::Value>,
    pub total_tokens: i32,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    pub inference_type: String,
    pub created_at: String,
    /// Why the inference ended (e.g., "completed", "length", "client_disconnect")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Response ID when called from Responses API
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Raw request ID from the inference provider
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Inference UUID (hashed from provider_request_id)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_id: Option<String>,
    /// Number of images generated (for image generation requests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_count: Option<i32>,
    /// None denotes legacy usage whose funding was not historically captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_allocations: Option<Vec<services::usage::CreditAllocation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funded_amount: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unfunded_amount: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation_policy_version: Option<String>,
}

/// Usage history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryResponse {
    pub data: Vec<UsageHistoryEntryResponse>,
    /// Number of matching records. Without filters this is the organization's
    /// recorded request count (`total_requests` in the balance), which can exceed
    /// the rows still stored, so a final page may come back short or empty.
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}

/// Query parameters for usage history
#[derive(Debug, Deserialize)]
pub struct UsageHistoryQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub credit_type: Option<String>,
}

impl UsageHistoryQuery {
    const fn has_filters(&self) -> bool {
        self.start_date.is_some()
            || self.end_date.is_some()
            || self.start_time.is_some()
            || self.end_time.is_some()
            || self.workspace_id.is_some()
            || self.api_key_id.is_some()
            || self.credit_type.is_some()
    }

    const fn has_time_filters(&self) -> bool {
        self.start_date.is_some()
            || self.end_date.is_some()
            || self.start_time.is_some()
            || self.end_time.is_some()
    }
}

/// Get organization balance
///
/// Returns the current spending balance for an organization
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/balance",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization balance", body = OrganizationBalanceResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_balance(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> Result<ResponseJson<OrganizationBalanceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get balance request for org {} by user {}",
        org_id,
        user.0.id
    );

    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    compute_organization_balance_response(&*app_state.usage_service, organization_id)
        .await
        .map(ResponseJson)
}

/// Get organization usage history
///
/// Returns paginated usage history for an organization
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/history",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)"),
        ("start_date" = Option<String>, Query, description = "Inclusive UTC start date in YYYY-MM-DD or RFC3339 format."),
        ("end_date" = Option<String>, Query, description = "Inclusive UTC end date in YYYY-MM-DD or RFC3339 format."),
        ("start_time" = Option<String>, Query, description = "Inclusive RFC3339 start timestamp. Takes precedence over start_date."),
        ("end_time" = Option<String>, Query, description = "Inclusive RFC3339 end timestamp. Takes precedence over end_date."),
        ("workspace_id" = Option<Uuid>, Query, description = "Filter by workspace ID."),
        ("api_key_id" = Option<Uuid>, Query, description = "Filter by API key ID."),
        ("credit_type" = Option<CreditType>, Query, description = "Filter costs by their saved credit allocation.")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 504, description = "Usage query timed out", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(mut query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for org {} by user {}, limit: {}, offset: {}",
        org_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;
    query.credit_type = normalize_credit_type_filter(query.credit_type.as_deref())?;

    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    if query.has_filters() {
        return get_filtered_organization_usage_history(&app_state, organization_id, &query).await;
    }

    let (history, total) = app_state
        .usage_service
        .get_usage_history(organization_id, Some(query.limit), Some(query.offset))
        .await
        .map_err(|error| match error {
            services::usage::UsageError::ReportingTimeout => {
                usage_query_timeout_error(organization_id)
            }
            _ => {
                tracing::error!("Failed to get usage history");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve usage history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            cache_write_tokens: entry.cache_write_tokens,
            service_tier: entry.service_tier,
            context_band: entry.context_band,
            billing_details: entry.billing_details,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            inference_type: entry.inference_type.to_string(),
            created_at: entry.created_at.to_rfc3339(),
            stop_reason: entry.stop_reason.map(|r| r.to_string()),
            response_id: entry.response_id.map(|id| id.to_string()),
            provider_request_id: entry.provider_request_id,
            inference_id: entry.inference_id.map(|id| id.to_string()),
            image_count: entry.image_count,
            credit_allocations: entry.credit_allocations,
            funded_amount: entry.funded_amount,
            unfunded_amount: entry.unfunded_amount,
            allocation_policy_version: entry.allocation_policy_version,
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

async fn get_filtered_organization_usage_history(
    app_state: &AppState,
    organization_id: Uuid,
    query: &UsageHistoryQuery,
) -> Result<ResponseJson<UsageHistoryResponse>, UsageError> {
    let report_query = usage_history_report_query(organization_id, query)?;
    let (history, total) = app_state
        .usage_service
        .list_inference_usage_history(report_query)
        .await
        .map_err(|error| match error {
            services::usage::UsageError::ReportingTimeout => {
                usage_query_timeout_error(organization_id)
            }
            _ => internal_usage_history_error("Failed to retrieve usage history"),
        })?;

    let data = history
        .into_iter()
        .map(usage_report_row_response)
        .collect::<Result<Vec<_>, _>>()?;
    let total = usize::try_from(total)
        .map_err(|_| internal_usage_history_error("Invalid usage history total"))?;

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total,
        limit: query.limit,
        offset: query.offset,
    }))
}

fn usage_history_report_query(
    organization_id: Uuid,
    query: &UsageHistoryQuery,
) -> Result<InferenceUsageHistoryQuery, UsageError> {
    if !query.has_time_filters() {
        return Ok(InferenceUsageHistoryQuery {
            organization_id,
            start_time: None,
            end_time: None,
            workspace_id: query.workspace_id,
            api_key_id: query.api_key_id,
            credit_type: query.credit_type.clone(),
            limit: query.limit,
            offset: query.offset,
        });
    }

    let (start_time, end_time) = normalize_usage_history_range(
        parse_usage_history_time(usage_history_start_time(query)?.as_deref())?,
        parse_usage_history_time(usage_history_end_time(query)?.as_deref())?,
    )
    .map_err(usage_history_query_error)?;

    Ok(InferenceUsageHistoryQuery {
        organization_id,
        start_time: Some(start_time),
        end_time: Some(end_time),
        workspace_id: query.workspace_id,
        api_key_id: query.api_key_id,
        credit_type: query.credit_type.clone(),
        limit: query.limit,
        offset: query.offset,
    })
}

fn normalize_credit_type_filter(value: Option<&str>) -> Result<Option<String>, UsageError> {
    value
        .map(|value| {
            value
                .parse::<CreditType>()
                .map(|credit_type| credit_type.as_str().to_string())
                .map_err(|_| usage_history_query_bad_request("Invalid credit type"))
        })
        .transpose()
}

fn usage_history_start_time(query: &UsageHistoryQuery) -> Result<Option<String>, UsageError> {
    match query.start_time.as_ref() {
        Some(value) => Ok(Some(value.clone())),
        None => normalize_usage_history_date(query.start_date.as_deref(), DateBoundary::Start),
    }
}

fn usage_history_end_time(query: &UsageHistoryQuery) -> Result<Option<String>, UsageError> {
    match query.end_time.as_ref() {
        Some(value) => Ok(Some(value.clone())),
        None => normalize_usage_history_date(query.end_date.as_deref(), DateBoundary::End),
    }
}

#[derive(Clone, Copy)]
enum DateBoundary {
    Start,
    End,
}

fn normalize_usage_history_date(
    value: Option<&str>,
    boundary: DateBoundary,
) -> Result<Option<String>, UsageError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if DateTime::parse_from_rfc3339(raw).is_ok() {
        return Ok(Some(raw.to_string()));
    }

    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| usage_history_query_error(UsageHistoryRangeError::InvalidTimestamp))?;
    let time = match boundary {
        DateBoundary::Start => date.and_hms_nano_opt(0, 0, 0, 0),
        DateBoundary::End => date.and_hms_nano_opt(23, 59, 59, 999_999_999),
    }
    .ok_or_else(|| usage_history_query_error(UsageHistoryRangeError::InvalidTimestamp))?;

    Ok(Some(
        DateTime::<Utc>::from_naive_utc_and_offset(time, Utc).to_rfc3339(),
    ))
}

fn usage_report_row_response(
    row: InferenceUsageReportRow,
) -> Result<UsageHistoryEntryResponse, UsageError> {
    Ok(UsageHistoryEntryResponse {
        id: row.id.to_string(),
        workspace_id: row.workspace_id.to_string(),
        api_key_id: row.api_key_id.to_string(),
        model: row.model,
        input_tokens: checked_usage_history_i32(row.input_tokens, "input_tokens")?,
        output_tokens: checked_usage_history_i32(row.output_tokens, "output_tokens")?,
        cache_read_tokens: checked_usage_history_i32(row.cache_read_tokens, "cache_read_tokens")?,
        cache_write_tokens: checked_usage_history_i32(
            row.cache_write_tokens,
            "cache_write_tokens",
        )?,
        service_tier: row.service_tier,
        context_band: row.context_band,
        billing_details: row.billing_details,
        total_tokens: checked_usage_history_i32(row.total_tokens, "total_tokens")?,
        total_cost: row.total_cost_nano_usd,
        total_cost_display: format_amount(row.total_cost_nano_usd),
        inference_type: row.inference_type,
        created_at: row.created_at.to_rfc3339(),
        stop_reason: row.stop_reason,
        response_id: row.response_id.map(|id| id.to_string()),
        provider_request_id: row.provider_request_id,
        inference_id: row.inference_id.map(|id| id.to_string()),
        image_count: row.image_count,
        credit_allocations: row.credit_allocations,
        funded_amount: row.funded_amount,
        unfunded_amount: row.unfunded_amount,
        allocation_policy_version: row.allocation_policy_version,
    })
}

fn checked_usage_history_i32(value: i64, field: &str) -> Result<i32, UsageError> {
    i32::try_from(value)
        .map_err(|_| internal_usage_history_error(&format!("Invalid usage history {field}")))
}

const MAX_USAGE_HISTORY_RANGE_DAYS: i64 = 366;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum UsageHistoryRangeError {
    #[error("start_time and end_time must be RFC3339 timestamps")]
    InvalidTimestamp,
    #[error("end_time must be greater than or equal to start_time")]
    InvalidTimeRange,
    #[error("time range must not exceed {max_days} days")]
    TimeRangeTooLarge { max_days: i64 },
}

fn parse_usage_history_time(value: Option<&str>) -> Result<Option<DateTime<Utc>>, UsageError> {
    value
        .map(|timestamp| {
            DateTime::parse_from_rfc3339(timestamp)
                .map(|parsed| parsed.with_timezone(&Utc))
                .map_err(|_| usage_history_query_error(UsageHistoryRangeError::InvalidTimestamp))
        })
        .transpose()
}

/// Default to the trailing maximum window ending now, then bound the range.
fn normalize_usage_history_range(
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), UsageHistoryRangeError> {
    let end_time = end_time.unwrap_or_else(Utc::now);
    let start_time =
        start_time.unwrap_or_else(|| end_time - Duration::days(MAX_USAGE_HISTORY_RANGE_DAYS));
    if end_time < start_time {
        return Err(UsageHistoryRangeError::InvalidTimeRange);
    }
    if end_time - start_time > Duration::days(MAX_USAGE_HISTORY_RANGE_DAYS) {
        return Err(UsageHistoryRangeError::TimeRangeTooLarge {
            max_days: MAX_USAGE_HISTORY_RANGE_DAYS,
        });
    }
    Ok((start_time, end_time))
}

fn usage_history_query_error(error: UsageHistoryRangeError) -> UsageError {
    (
        StatusCode::BAD_REQUEST,
        ResponseJson(ErrorResponse::new(
            error.to_string(),
            "invalid_reporting_usage_query".to_string(),
        )),
    )
}

fn usage_history_query_bad_request(message: &str) -> UsageError {
    (
        StatusCode::BAD_REQUEST,
        ResponseJson(ErrorResponse::new(
            message.to_string(),
            "invalid_parameter".to_string(),
        )),
    )
}

fn internal_usage_history_error(message: &str) -> UsageError {
    tracing::error!("{message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            message.to_string(),
            "internal_server_error".to_string(),
        )),
    )
}

/// The database cancelled a usage query at the reporting statement timeout.
fn usage_query_timeout_error(organization_id: Uuid) -> UsageError {
    tracing::warn!(%organization_id, "Usage query timed out");
    (
        StatusCode::GATEWAY_TIMEOUT,
        ResponseJson(ErrorResponse::new(
            "Usage query timed out".to_string(),
            "usage_query_timeout".to_string(),
        )),
    )
}

/// Get API key usage history
///
/// Returns paginated usage history for a specific API key
#[utoipa::path(
    get,
    path = "/v1/workspaces/{workspace_id}/api-keys/{api_key_id}/usage/history",
    tag = "Usage",
    params(
        ("workspace_id" = String, Path, description = "Workspace ID"),
        ("api_key_id" = String, Path, description = "API Key ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)"),
        ("credit_type" = Option<CreditType>, Query, description = "Filter costs by their saved credit allocation.")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_api_key_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(String, String)>,
    Query(mut query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for API key {} in workspace {} by user {}, limit: {}, offset: {}",
        api_key_id,
        workspace_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;
    query.credit_type = normalize_credit_type_filter(query.credit_type.as_deref())?;

    let workspace_uuid = Uuid::parse_str(&workspace_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid workspace ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let api_key_uuid = Uuid::parse_str(&api_key_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid API key ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Get usage history with permission checking handled by the service
    let (history, total) = app_state
        .usage_service
        .get_api_key_usage_history_with_permissions(
            workspace_uuid,
            api_key_uuid,
            user.0.id,
            query.credit_type.as_deref(),
            Some(query.limit),
            Some(query.offset),
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to get usage history");
            match e {
                services::usage::UsageError::Unauthorized(_) => (
                    StatusCode::FORBIDDEN,
                    ResponseJson(ErrorResponse::new(
                        "Access denied to this workspace".to_string(),
                        "forbidden".to_string(),
                    )),
                ),
                services::usage::UsageError::NotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "API key not found in this workspace".to_string(),
                        "not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve usage history".to_string(),
                        "internal_server_error".to_string(),
                    )),
                ),
            }
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model: entry.model,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            cache_read_tokens: entry.cache_read_tokens,
            cache_write_tokens: entry.cache_write_tokens,
            service_tier: entry.service_tier,
            context_band: entry.context_band,
            billing_details: entry.billing_details,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            inference_type: entry.inference_type.to_string(),
            created_at: entry.created_at.to_rfc3339(),
            stop_reason: entry.stop_reason.map(|r| r.to_string()),
            response_id: entry.response_id.map(|id| id.to_string()),
            provider_request_id: entry.provider_request_id,
            inference_id: entry.inference_id.map(|id| id.to_string()),
            image_count: entry.image_count,
            credit_allocations: entry.credit_allocations,
            funded_amount: entry.funded_amount,
            unfunded_amount: entry.unfunded_amount,
            allocation_policy_version: entry.allocation_policy_version,
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

// ============= User-facing analytics endpoints =============

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserMetricsSummary {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserModelMetrics {
    pub model_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserWorkspaceMetrics {
    pub workspace_id: String,
    pub workspace_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserOrganizationMetrics {
    pub organization_id: String,
    pub period_start: String,
    pub period_end: String,
    pub summary: UserMetricsSummary,
    pub by_model: Vec<UserModelMetrics>,
    pub by_workspace: Vec<UserWorkspaceMetrics>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserTimeSeriesPoint {
    pub date: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UserTimeSeriesMetrics {
    pub organization_id: String,
    pub period_start: String,
    pub period_end: String,
    pub granularity: String,
    pub data: Vec<UserTimeSeriesPoint>,
}

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TimeSeriesQuery {
    pub start: Option<String>,
    pub end: Option<String>,
    pub granularity: Option<String>,
}

const MAX_DATE_RANGE_DAYS: i64 = 366;

fn validate_date_range(
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) -> Result<(), UsageError> {
    if start >= end {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "start must be before end".to_string(),
                "invalid_date_range".to_string(),
            )),
        ));
    }
    if end - start > Duration::days(MAX_DATE_RANGE_DAYS) {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                format!("Date range must not exceed {MAX_DATE_RANGE_DAYS} days."),
                "date_range_too_large".to_string(),
            )),
        ));
    }
    Ok(())
}

fn parse_datetime_or_default(
    value: &Option<String>,
    default: chrono::DateTime<Utc>,
) -> Result<chrono::DateTime<Utc>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match value {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Invalid date format. Use ISO 8601 (e.g. 2024-01-01T00:00:00Z)."
                            .to_string(),
                        "invalid_date".to_string(),
                    )),
                )
            }),
        None => Ok(default),
    }
}

/// `end` defaults to now and `start` to 30 days before `end`.
fn parse_date_range(
    start: &Option<String>,
    end: &Option<String>,
) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), UsageError> {
    let end = parse_datetime_or_default(end, Utc::now())?;
    let start = parse_datetime_or_default(start, end - Duration::days(30))?;
    validate_date_range(start, end)?;
    Ok((start, end))
}

/// Organization usage metrics.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/metrics",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("start" = Option<String>, Query, description = "Start date (ISO 8601, default: 30 days ago)"),
        ("end" = Option<String>, Query, description = "End date (ISO 8601, default: now)")
    ),
    responses(
        (status = 200, description = "Organization usage metrics", body = UserOrganizationMetrics),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 504, description = "Analytics statement budget exceeded", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_organization_metrics(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<MetricsQuery>,
) -> Result<ResponseJson<UserOrganizationMetrics>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let (start, end) = parse_date_range(&query.start, &query.end)?;

    let metrics = app_state
        .analytics_service
        .get_organization_metrics(organization_id, start, end, None)
        .await
        .map_err(|e| {
            analytics_error_response(e, "Failed to retrieve organization metrics", false)
        })?;

    Ok(ResponseJson(UserOrganizationMetrics {
        organization_id: metrics.organization_id.to_string(),
        period_start: metrics.period_start.to_rfc3339(),
        period_end: metrics.period_end.to_rfc3339(),
        summary: UserMetricsSummary {
            total_requests: metrics.summary.total_requests,
            total_input_tokens: metrics.summary.total_input_tokens,
            total_output_tokens: metrics.summary.total_output_tokens,
            total_cache_read_tokens: metrics.summary.total_cache_read_tokens,
            total_cost_usd: metrics.summary.total_cost_usd,
        },
        by_model: metrics
            .by_model
            .into_iter()
            .map(|m| UserModelMetrics {
                model_name: m.model_name,
                requests: m.requests,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                cache_read_tokens: m.cache_read_tokens,
                cost_usd: m.cost_usd,
            })
            .collect(),
        by_workspace: metrics
            .by_workspace
            .into_iter()
            .map(|w| UserWorkspaceMetrics {
                workspace_id: w.workspace_id.to_string(),
                workspace_name: w.workspace_name,
                requests: w.requests,
                input_tokens: w.input_tokens,
                output_tokens: w.output_tokens,
                cache_read_tokens: w.cache_read_tokens,
                cost_usd: w.cost_usd,
            })
            .collect(),
    }))
}

/// Organization usage timeseries.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/timeseries",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("start" = Option<String>, Query, description = "Start date (ISO 8601, default: 30 days ago)"),
        ("end" = Option<String>, Query, description = "End date (ISO 8601, default: now)"),
        ("granularity" = Option<String>, Query, description = "Time bucket size: hour, day, week (default: day)")
    ),
    responses(
        (status = 200, description = "Organization usage timeseries", body = UserTimeSeriesMetrics),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 504, description = "Analytics statement budget exceeded", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_user_organization_timeseries(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<TimeSeriesQuery>,
) -> Result<ResponseJson<UserTimeSeriesMetrics>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;

    let granularity = query.granularity.as_deref().unwrap_or("day");
    if !["hour", "day", "week"].contains(&granularity) {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid granularity. Must be one of: hour, day, week.".to_string(),
                "invalid_granularity".to_string(),
            )),
        ));
    }

    let (start, end) = parse_date_range(&query.start, &query.end)?;

    let timeseries = app_state
        .analytics_service
        .get_organization_timeseries(organization_id, start, end, granularity, None)
        .await
        .map_err(|e| {
            analytics_error_response(e, "Failed to retrieve organization timeseries", false)
        })?;

    Ok(ResponseJson(UserTimeSeriesMetrics {
        organization_id: timeseries.organization_id.to_string(),
        period_start: timeseries.period_start.to_rfc3339(),
        period_end: timeseries.period_end.to_rfc3339(),
        granularity: timeseries.granularity,
        data: timeseries
            .data
            .into_iter()
            .map(|p| UserTimeSeriesPoint {
                date: p.date,
                requests: p.requests,
                input_tokens: p.input_tokens,
                output_tokens: p.output_tokens,
                cache_read_tokens: p.cache_read_tokens,
                cost_usd: p.cost_usd,
            })
            .collect(),
    }))
}

/// Period selector for the by-model usage breakdown.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageByModelPeriod {
    Day,
    Week,
    Month,
    /// The `start`/`end` range instead of a rolling window.
    Custom,
}

impl UsageByModelPeriod {
    /// Start of the rolling window; `None` for `Custom`.
    fn since(self) -> Option<chrono::DateTime<Utc>> {
        let now = Utc::now();
        match self {
            UsageByModelPeriod::Day => Some(now - Duration::days(1)),
            UsageByModelPeriod::Week => Some(now - Duration::days(7)),
            UsageByModelPeriod::Month => Some(now - Duration::days(30)),
            UsageByModelPeriod::Custom => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            UsageByModelPeriod::Day => "day",
            UsageByModelPeriod::Week => "week",
            UsageByModelPeriod::Month => "month",
            UsageByModelPeriod::Custom => "custom",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UsageByModelQuery {
    /// `day` (last 24h), `week` (last 7d), `month` (last 30d), or `custom` for the
    /// `start`/`end` range. Defaults to `month`.
    #[serde(default = "default_period")]
    pub period: UsageByModelPeriod,
    /// Custom range start (ISO 8601). Setting `start` or `end` selects the custom range
    /// whatever `period` says.
    pub start: Option<String>,
    /// Custom range end, exclusive (ISO 8601). Defaults to now.
    pub end: Option<String>,
}

fn default_period() -> UsageByModelPeriod {
    UsageByModelPeriod::Month
}

/// Per-model usage aggregation entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageByModelEntryResponse {
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    pub request_count: i64,
}

/// Per-model usage breakdown response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageByModelResponse {
    /// `day`, `week` or `month` for a rolling window; `custom` for a `start`/`end` range.
    pub period: String,
    pub start_date: String,
    /// Exclusive end of a custom range. Absent for rolling windows, which end now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    pub data: Vec<UsageByModelEntryResponse>,
}

/// Get organization usage broken down by model.
///
/// Returns one row per model, summed over a rolling window ending now:
/// `day` = last 24h, `week` = last 7 days, `month` = last 30 days (NOT calendar
/// day/week/month-to-date). Setting `start` or `end` (or `period=custom`) sums a custom
/// range instead; a range longer than 366 days is rejected with 400. Used by the
/// dashboard pie chart to show which models drive spend.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/by-model",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("period" = Option<String>, Query, description = "Rolling window: `day` (last 24h), `week` (last 7d), or `month` (last 30d); or `custom` for the `start`/`end` range. Default: `month`. Setting `start` or `end` selects the custom range whatever `period` says"),
        ("start" = Option<String>, Query, description = "Custom range start (ISO 8601). Defaults to `end` minus 30 days"),
        ("end" = Option<String>, Query, description = "Custom range end, exclusive (ISO 8601). Defaults to now")
    ),
    responses(
        (status = 200, description = "Per-model usage breakdown", body = UsageByModelResponse),
        (status = 400, description = "Invalid date or date range", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 504, description = "Usage query timed out", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_usage_by_model(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<UsageByModelQuery>,
) -> Result<ResponseJson<UsageByModelResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let organization_id = check_org_membership(&app_state, user, &org_id).await?;
    let period = if query.start.is_some() || query.end.is_some() {
        UsageByModelPeriod::Custom
    } else {
        query.period
    };
    let (start_date, end_date) = match period.since() {
        Some(since) => (since, None),
        None => {
            let (start, end) = parse_date_range(&query.start, &query.end)?;
            (start, Some(end))
        }
    };

    let entries = app_state
        .usage_service
        .get_usage_by_model(organization_id, start_date, end_date)
        .await
        .map_err(|e| match e {
            services::usage::UsageError::ReportingTimeout => {
                usage_query_timeout_error(organization_id)
            }
            _ => {
                tracing::error!(error = ?e, "Failed to get usage by model");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve usage breakdown".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            }
        })?;

    let data = entries
        .into_iter()
        .map(|e| UsageByModelEntryResponse {
            model: e.model,
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            total_tokens: e.total_tokens,
            total_cost: e.total_cost,
            total_cost_display: format_amount(e.total_cost),
            request_count: e.request_count,
        })
        .collect();

    Ok(ResponseJson(UsageByModelResponse {
        period: period.as_str().to_string(),
        start_date: start_date.to_rfc3339(),
        end_date: end_date.map(|d| d.to_rfc3339()),
        data,
    }))
}
