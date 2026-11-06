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

    // Confirms whether an email was ever a customer, so no admin tokens.
    if path == "/admin/users/{user_id}/erasure" || path == "/admin/user-erasures/lookup" {
        return AdminOperation::SessionOnly;
    }

    if path == "/admin/access-tokens" || path.starts_with("/admin/access-tokens/") {
        return AdminOperation::SessionOnly;
    }

    // Explicitly approve GET and its Axum HEAD fallback on these routes only.
    if matches!(*method, Method::GET | Method::HEAD)
        && matches!(
            path,
            "/admin/platform/infra-summary"
                | "/admin/invitation-email-deliveries"
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
            "/admin/database-encryption/scan"
                | "/admin/object-encryption/scan"
                | "/admin/users/{user_id}/erasure/preview"
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
    fn user_erasure_preview_is_read_and_execute_is_session_only() {
        for prefix in ["/v1", ""] {
            let preview = format!("{prefix}/admin/users/{{user_id}}/erasure/preview");
            let execute = format!("{prefix}/admin/users/{{user_id}}/erasure");
            assert_eq!(
                admin_operation(&Method::POST, &preview),
                AdminOperation::Read
            );
            assert_eq!(
                admin_operation(&Method::POST, &execute),
                AdminOperation::SessionOnly
            );
        }
    }

    #[test]
    fn user_erasure_lookup_is_session_only() {
        for path in [
            "/v1/admin/user-erasures/lookup",
            "/admin/user-erasures/lookup",
        ] {
            assert_eq!(
                admin_operation(&Method::POST, path),
                AdminOperation::SessionOnly
            );
        }
    }

    #[test]
    fn retired_admin_routes_are_not_reads() {
        // Routes that moved to cloud-platform must not keep a Read classification:
        // a route re-added at one of these paths is a Write until it is audited.
        for path in [
            "/v1/admin/api-keys",
            "/v1/admin/users",
            "/v1/admin/organizations",
            "/v1/admin/organizations/{org_id}/usage/balance",
            "/v1/admin/platform/metrics",
            "/v1/admin/models",
            "/v1/admin/models/{model_name}/access",
            "/v1/admin/organizations/{org_id}/model-access",
            "/v1/admin/organizations/{org_id}/limits/history",
            "/v1/admin/settings",
            "/v1/admin/settings/{key}",
            "/v1/admin/aml/reports",
            "/v1/admin/feature-requests",
        ] {
            assert_eq!(admin_operation(&Method::GET, path), AdminOperation::Write);
        }
    }
}
