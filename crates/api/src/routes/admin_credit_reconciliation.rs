//! Audited opening-consumption reconciliation for existing organizations.
use crate::{middleware::AdminUser, models::ErrorResponse};
use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    Json,
};
use database::repositories::credit_reconciliation::{
    AccountingPosition, CreditReconciliationRepository, ReconciliationError, ReconciliationResult,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreditReconciliationRequest {
    /// Unique per organization. Repeating identical terms returns the original result.
    pub idempotency_key: String,
    /// Positive nano-USD amounts to move from unknown consumption into credit types.
    pub amounts: BTreeMap<String, i64>,
    /// Explanation and reference to supporting billing/contract records.
    pub reason: String,
    /// Validate and preview without saving an adjustment or reserving the key.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreditPositionResponse {
    /// False for consumption whose credit type has no active ceiling.
    pub active: bool,
    pub ceiling: i64,
    pub consumed: i64,
    pub available: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AccountingPositionResponse {
    pub credits: BTreeMap<String, CreditPositionResponse>,
    pub legacy_unattributed_amount: i64,
    pub unresolved_unfunded_amount: i64,
    /// Before billing holds or the unresolved-debt admission gate.
    pub aggregate_available: i64,
}

impl From<AccountingPosition> for AccountingPositionResponse {
    fn from(value: AccountingPosition) -> Self {
        Self {
            credits: value
                .credits
                .into_iter()
                .map(|(kind, credit)| {
                    (
                        kind,
                        CreditPositionResponse {
                            active: credit.active,
                            ceiling: credit.ceiling,
                            consumed: credit.consumed,
                            available: credit.available,
                        },
                    )
                })
                .collect(),
            legacy_unattributed_amount: value.legacy_unattributed_amount,
            unresolved_unfunded_amount: value.unresolved_unfunded_amount,
            aggregate_available: value.aggregate_available,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreditReconciliationResponse {
    pub schema_version: u32,
    pub reconciliation_id: Option<Uuid>,
    pub dry_run: bool,
    pub before: AccountingPositionResponse,
    pub after: AccountingPositionResponse,
}

impl From<ReconciliationResult> for CreditReconciliationResponse {
    fn from(value: ReconciliationResult) -> Self {
        Self {
            schema_version: value.schema_version,
            reconciliation_id: value.reconciliation_id,
            dry_run: value.dry_run,
            before: value.before.into(),
            after: value.after.into(),
        }
    }
}

type ApiError = (StatusCode, Json<ErrorResponse>);
fn api_error(error: anyhow::Error) -> ApiError {
    let (status, code, message) = match error.downcast_ref::<ReconciliationError>() {
        Some(ReconciliationError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        ),
        Some(ReconciliationError::NotFound) => {
            (StatusCode::NOT_FOUND, "not_found", error.to_string())
        }
        Some(ReconciliationError::Conflict(_)) => {
            (StatusCode::CONFLICT, "conflict", error.to_string())
        }
        None => {
            tracing::error!(error = %error, "Credit reconciliation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                "Failed to reconcile organization credits".into(),
            )
        }
    };
    (status, Json(ErrorResponse::new(message, code.into())))
}

/// Reclassify reviewed legacy consumption. Does not rewrite usage logs, settle
/// invoices, alter ceilings, or change allocation policy. Dry runs are point-in-time
/// previews; commits revalidate under the organization accounting lock.
#[utoipa::path(post, path="/v1/admin/organizations/{org_id}/credit-reconciliations", tag="Admin",
    params(("org_id" = Uuid, Path, description="Organization ID")), request_body=CreditReconciliationRequest,
    responses((status=200, description="Saved reconciliation, idempotent replay, or dry-run preview", body=CreditReconciliationResponse),
        (status=400, description="Invalid amounts, key or reason", body=ErrorResponse),
        (status=401, description="Unauthorized", body=ErrorResponse),
        (status=403, description="Admin write permission required", body=ErrorResponse),
        (status=404, description="Organization not found or inactive", body=ErrorResponse),
        (status=409, description="Conflicting key or unreviewed accounting position", body=ErrorResponse)),
    security(("session_token"=[])))]
pub async fn reconcile_organization_credits(
    Extension(repository): Extension<CreditReconciliationRepository>,
    Extension(admin): Extension<AdminUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreditReconciliationRequest>,
) -> Result<Json<CreditReconciliationResponse>, ApiError> {
    let result = repository
        .reconcile(
            org_id,
            &request.amounts,
            &request.idempotency_key,
            &request.reason,
            admin.0.id,
            request.dry_run,
        )
        .await
        .map_err(api_error)?;
    Ok(Json(result.into()))
}
