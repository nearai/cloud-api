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
}

pub(crate) fn admin_operation(method: &Method, matched_path: &str) -> AdminOperation {
    // Axum includes the nesting prefix in MatchedPath. The builders are also
    // used without /v1 in tests; support both without matching arbitrary suffixes.
    let path = if matched_path.starts_with("/v1/") {
        &matched_path[3..]
    } else {
        matched_path
    };

    // Explicitly approve GET and its Axum HEAD fallback on these routes only.
    if matches!(*method, Method::GET | Method::HEAD)
        && matches!(
            path,
            "/admin/organizations/{org_id}/metrics"
                | "/admin/platform/infra-summary"
                | "/admin/database-encryption/jobs/{id}"
                | "/admin/database-encryption/keys"
                | "/admin/object-encryption/jobs/{id}"
                | "/admin/object-encryption/keys"
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
            "/admin/database-encryption/scan" | "/admin/object-encryption/scan"
        )
    {
        return AdminOperation::Read;
    }

    AdminOperation::Write
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_encryption_keys_are_read_only_and_jobs_require_write() {
        for prefix in ["/v1", ""] {
            for method in [Method::GET, Method::HEAD] {
                assert_eq!(
                    admin_operation(&method, &format!("{prefix}/admin/database-encryption/keys")),
                    AdminOperation::Read
                );
            }
            assert_eq!(
                admin_operation(
                    &Method::POST,
                    &format!("{prefix}/admin/database-encryption/jobs")
                ),
                AdminOperation::Write
            );
        }
    }

    #[test]
    fn object_encryption_reads_and_mutations_have_separate_permissions() {
        for prefix in ["/v1", ""] {
            for (method, suffix, expected) in [
                (Method::GET, "keys", AdminOperation::Read),
                (Method::HEAD, "keys", AdminOperation::Read),
                (Method::GET, "jobs/{id}", AdminOperation::Read),
                (Method::POST, "scan", AdminOperation::Read),
                (Method::POST, "jobs", AdminOperation::Write),
                (Method::POST, "jobs/{id}/cancel", AdminOperation::Write),
                (Method::POST, "verify", AdminOperation::Write),
                (Method::GET, "unknown", AdminOperation::Write),
            ] {
                assert_eq!(
                    admin_operation(
                        &method,
                        &format!("{prefix}/admin/object-encryption/{suffix}")
                    ),
                    expected,
                    "{method} {suffix}"
                );
            }
        }
    }

    #[test]
    fn removed_admin_routes_are_not_reads() {
        for path in [
            "/v1/admin/models",
            "/v1/admin/models/{model_name}/access",
            "/v1/admin/organizations/{org_id}/model-access",
            "/v1/admin/organizations/{org_id}/limits/history",
            "/v1/admin/settings",
            "/v1/admin/settings/{key}",
            "/v1/admin/aml/reports",
            "/v1/admin/feature-requests",
            "/v1/admin/api-keys",
            "/v1/admin/organizations/{org_id}/members",
            "/v1/admin/organizations/{org_id}/policy",
            "/v1/admin/organizations/{org_id}/staking/farm",
            "/v1/admin/organizations/{org_id}/usage/balance",
            "/v1/admin/organizations/{org_id}/credit/transfer/reconciliation",
        ] {
            assert_eq!(admin_operation(&Method::GET, path), AdminOperation::Write);
        }
    }
}
