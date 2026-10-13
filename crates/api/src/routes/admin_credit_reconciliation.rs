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
    /// Whether this credit type has a currently effective limit.
    /// Inactive types are retained when they have recorded consumption.
    pub active: bool,
    /// Sum of active limits for this type, in nano-USD; zero when inactive.
    pub spend_limit: i64,
    /// Lifetime consumption assigned to this type, including reconciled history,
    /// in nano-USD. This can remain nonzero after the type's limits expire.
    pub consumed: i64,
    /// Remaining capacity in nano-USD: max(spend_limit - consumed, 0) for
    /// active types, or zero for inactive types.
    pub available: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AccountingPositionResponse {
    /// Positions keyed by credit type (grant, payment, staking_farm, or postpay).
    /// Includes active limits and recorded consumption for inactive types.
    pub credits: BTreeMap<String, CreditPositionResponse>,
    /// Historical consumption still lacking a credit type, in nano-USD.
    /// Reconciliation reduces this by the sum of the requested increments.
    pub legacy_unattributed_amount: i64,
    /// Unresolved usage that exceeded available funding, in nano-USD.
    /// This debt is separate from legacy consumption and is not reconciled here.
    pub unresolved_unfunded_amount: i64,
    /// Sum of per-type available capacity minus legacy unattributed consumption,
    /// clamped to zero, in nano-USD. Reconciliation preserves this value.
    /// It is measured before billing holds or the unresolved-debt admission gate.
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
                            spend_limit: credit.spend_limit,
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
    /// Version of the stored result schema, currently 1.
    pub schema_version: u32,
    /// Audit ledger ID for a committed operation; null for an unsaved preview.
    pub reconciliation_id: Option<Uuid>,
    /// True when this result is a preview and no adjustment was saved.
    pub dry_run: bool,
    /// Accounting position immediately before this operation. Idempotent replays
    /// return the original snapshot, not the organization's current balances.
    pub before: AccountingPositionResponse,
    /// Projected position for a preview, or saved position for a commit.
    /// Commits revalidate live balances, so they may differ from an earlier preview.
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
/// invoices, alter limits, or change allocation policy. Dry runs are point-in-time
/// previews; commits revalidate under the organization accounting lock.
#[utoipa::path(
    post,
    path = "/v1/admin/organizations/{org_id}/credit/reconciliations",
    tag = "Admin",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = CreditReconciliationRequest,
    responses(
        (status = 200, description = "Saved reconciliation, idempotent replay, or dry-run preview", body = CreditReconciliationResponse),
        (status = 400, description = "Invalid amounts, key or reason", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Admin write permission required", body = ErrorResponse),
        (status = 404, description = "Organization not found or inactive", body = ErrorResponse),
        (status = 409, description = "Conflicting key or unreviewed accounting position", body = ErrorResponse),
    ),
    security(("session_token" = [])),
)]
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
