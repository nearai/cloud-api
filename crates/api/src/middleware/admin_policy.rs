//! Admin authorization shared by every router using `admin_middleware`.
//!
//! Classify matched route templates, never client-provided paths or HTTP
//! methods alone. New/unclassified operations require write permission. Audit
//! the handler and its service calls before adding an operation to this list.

use axum::http::Method;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminOperation {
    Read,
    Write,
    SessionOnly,
}

pub(crate) fn admin_operation(method: &Method, matched_path: &str) -> AdminOperation {
    // Axum includes the nesting prefix in MatchedPath. The builders are also
    // used without /v1 in tests; support both without matching arbitrary suffixes.
    let path = if matched_path.starts_with("/v1/") {
        &matched_path[3..]
    } else {
        matched_path
    };

    if path == "/admin/access-tokens" || path.starts_with("/admin/access-tokens/") {
        return AdminOperation::SessionOnly;
    }

    // Explicitly approve GET and its Axum HEAD fallback on these routes only.
    if matches!(*method, Method::GET | Method::HEAD)
        && matches!(
            path,
            "/admin/models"
                | "/admin/models/pricing-changes"
                | "/admin/models/{model_name}/history"
                | "/admin/organizations/{org_id}/limits/history"
                | "/admin/organizations/{org_id}/usage/balance"
                | "/admin/organizations/{org_id}/staking/farm"
                | "/admin/aml/reports"
                | "/admin/aml/allowlist"
                | "/admin/organizations/{org_id}/concurrent-limit"
                | "/admin/organizations/{org_id}/fallback"
                | "/admin/organizations/{org_id}/priority"
                | "/admin/organizations/{org_id}/metrics"
                | "/admin/organizations/{org_id}/metrics/timeseries"
                | "/admin/platform/metrics"
                | "/admin/platform/metrics/timeseries"
                | "/admin/platform/billing-summary"
                | "/admin/platform/model-revenue"
                | "/admin/platform/org-revenue"
                | "/admin/platform/infra-summary"
                | "/admin/platform/model-consumption-timeseries"
                | "/admin/platform/performance-timeseries"
                | "/admin/platform/revenue-density"
                | "/admin/invitation-email-deliveries"
                | "/admin/users"
                | "/admin/organizations"
                | "/admin/organizations/{org_id}"
                | "/admin/organizations/{org_id}/members"
                | "/admin/feature-requests"
                | "/admin/database-encryption/jobs/{id}"
        )
    {
        return AdminOperation::Read;
    }

    // These POST handlers validate inputs and read snapshots/recipients/counts;
    // they do not persist changes, create jobs, or send notifications. In
    // particular, database-encryption/verify creates a job and is NOT a read.
    if *method == Method::POST
        && matches!(
            path,
            "/admin/models/pricing-changes/preview"
                | "/admin/models/{model_name}/deprecation/preview"
                | "/admin/database-encryption/scan"
        )
    {
        return AdminOperation::Read;
    }

    AdminOperation::Write
}
