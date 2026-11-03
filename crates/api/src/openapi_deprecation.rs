//! Marks the routes that are moving to cloud-platform as deprecated in the
//! generated OpenAPI document. Documentation only: no handler, header or
//! status changes.
//!
//! utoipa 5 has no `deprecated` key in `#[utoipa::path]`; the only per-handler
//! form is Rust's `#[deprecated]`, which would warn at every registration site.
//! A `Modify` hook sets the OpenAPI flag from one list instead.

use utoipa::openapi::path::Operation;
use utoipa::openapi::{Deprecated, OpenApi};
use utoipa::Modify;

/// Added at the start of each deprecated operation's description.
pub const DEPRECATION_NOTE: &str = "**Deprecated:** this endpoint is moving to cloud-platform at the same path; the cloud-platform base URL will be announced. Behaviour may differ in edge cases.";

/// Lowercase HTTP method and OpenAPI path of every operation to deprecate.
///
/// Not listed: `/v1/auth/*`, which moves too but has no `#[utoipa::path]`
/// annotation, so it is not in the generated spec for the hook to mark.
pub const MOVED_TO_CLOUD_PLATFORM: &[(&str, &str)] = &[
    ("get", "/v1/admin/access-tokens"),
    ("post", "/v1/admin/access-tokens"),
    ("delete", "/v1/admin/access-tokens/{token_id}"),
    ("get", "/v1/admin/aml/allowlist"),
    ("post", "/v1/admin/aml/allowlist"),
    ("delete", "/v1/admin/aml/allowlist/{account_id}"),
    ("get", "/v1/admin/aml/reports"),
    ("patch", "/v1/admin/aml/reports/{report_id}/status"),
    ("get", "/v1/admin/api-keys"),
    ("get", "/v1/admin/feature-requests"),
    ("get", "/v1/admin/invitation-email-deliveries"),
    (
        "post",
        "/v1/admin/invitation-email-deliveries/{invitation_id}/resend",
    ),
    ("get", "/v1/admin/models"),
    ("get", "/v1/admin/organizations"),
    ("patch", "/v1/admin/models"),
    ("post", "/v1/admin/models/deprecate"),
    ("get", "/v1/admin/models/pricing-changes"),
    ("post", "/v1/admin/models/pricing-changes/confirm"),
    ("post", "/v1/admin/models/pricing-changes/preview"),
    ("delete", "/v1/admin/models/pricing-changes/{id}"),
    ("delete", "/v1/admin/models/{model_name}"),
    ("get", "/v1/admin/models/{model_name}/access"),
    ("put", "/v1/admin/models/{model_name}/access/{org_id}"),
    ("delete", "/v1/admin/models/{model_name}/access/{org_id}"),
    ("post", "/v1/admin/models/{model_name}/deprecation/confirm"),
    ("post", "/v1/admin/models/{model_name}/deprecation/preview"),
    ("get", "/v1/admin/models/{model_name}/history"),
    ("get", "/v1/admin/organizations/{org_id}"),
    ("get", "/v1/admin/organizations/{org_id}/concurrent-limit"),
    ("patch", "/v1/admin/organizations/{org_id}/concurrent-limit"),
    (
        "post",
        "/v1/admin/organizations/{org_id}/credit/reconciliations",
    ),
    (
        "get",
        "/v1/admin/organizations/{org_id}/credit/transfer/reconciliation",
    ),
    ("get", "/v1/admin/organizations/{org_id}/fallback"),
    ("patch", "/v1/admin/organizations/{org_id}/fallback"),
    ("patch", "/v1/admin/organizations/{org_id}/limits"),
    ("get", "/v1/admin/organizations/{org_id}/limits/history"),
    ("get", "/v1/admin/organizations/{org_id}/members"),
    ("put", "/v1/admin/organizations/{org_id}/members/{user_id}"),
    ("get", "/v1/admin/organizations/{org_id}/metrics"),
    ("get", "/v1/admin/organizations/{org_id}/metrics/timeseries"),
    ("get", "/v1/admin/organizations/{org_id}/model-access"),
    ("get", "/v1/admin/organizations/{org_id}/policy"),
    ("put", "/v1/admin/organizations/{org_id}/policy"),
    ("get", "/v1/admin/organizations/{org_id}/priority"),
    ("patch", "/v1/admin/organizations/{org_id}/priority"),
    ("get", "/v1/admin/organizations/{org_id}/staking/farm"),
    ("post", "/v1/admin/organizations/{org_id}/staking/farm/sync"),
    ("get", "/v1/admin/organizations/{org_id}/usage-discount"),
    ("put", "/v1/admin/organizations/{org_id}/usage-discount"),
    ("get", "/v1/admin/organizations/{org_id}/usage/balance"),
    ("get", "/v1/admin/platform/billing-summary"),
    ("get", "/v1/admin/platform/metrics"),
    ("get", "/v1/admin/platform/metrics/timeseries"),
    ("get", "/v1/admin/platform/model-revenue"),
    ("get", "/v1/admin/platform/model-consumption-timeseries"),
    ("get", "/v1/admin/platform/org-revenue"),
    ("get", "/v1/admin/platform/performance-timeseries"),
    ("get", "/v1/admin/platform/revenue-density"),
    ("post", "/v1/admin/services"),
    ("patch", "/v1/admin/services/{id}"),
    ("get", "/v1/admin/settings"),
    ("get", "/v1/admin/settings/{key}"),
    ("patch", "/v1/admin/settings/{key}"),
    ("post", "/v1/admin/usage-hourly/recompute"),
    ("post", "/v1/admin/user-erasures/lookup"),
    ("get", "/v1/admin/users"),
    ("post", "/v1/admin/users/{user_id}/erasure"),
    ("post", "/v1/admin/users/{user_id}/erasure/preview"),
    ("post", "/v1/billing/costs"),
    ("post", "/v1/check_api_key"),
    ("post", "/v1/feature-requests"),
    ("get", "/v1/invitations/{token}"),
    ("post", "/v1/invitations/{token}/accept"),
    ("get", "/v1/organizations"),
    ("post", "/v1/organizations"),
    ("get", "/v1/organizations/{org_id}"),
    ("put", "/v1/organizations/{org_id}"),
    ("delete", "/v1/organizations/{org_id}"),
    ("get", "/v1/organizations/{org_id}/credentials"),
    ("post", "/v1/organizations/{org_id}/credentials"),
    ("put", "/v1/organizations/{org_id}/credentials/{id}"),
    ("delete", "/v1/organizations/{org_id}/credentials/{id}"),
    ("post", "/v1/organizations/{org_id}/credentials/{id}/rotate"),
    ("post", "/v1/organizations/{org_id}/credit/transfer"),
    ("get", "/v1/organizations/{org_id}/credit/transfer/capacity"),
    ("get", "/v1/organizations/{org_id}/credit/transfers"),
    ("get", "/v1/organizations/{org_id}/credit/transfers/{id}"),
    (
        "delete",
        "/v1/organizations/{org_id}/invitations/{invitation_id}",
    ),
    ("get", "/v1/organizations/{org_id}/members"),
    ("post", "/v1/organizations/{org_id}/members"),
    ("get", "/v1/organizations/{org_id}/members/invitations"),
    (
        "delete",
        "/v1/organizations/{org_id}/members/invitations/{invitation_id}",
    ),
    ("post", "/v1/organizations/{org_id}/members/invite-by-email"),
    ("put", "/v1/organizations/{org_id}/members/{user_id}"),
    ("delete", "/v1/organizations/{org_id}/members/{user_id}"),
    ("get", "/v1/organizations/{org_id}/reporting-tokens"),
    ("get", "/v1/organizations/{org_id}/service-usage/history"),
    ("get", "/v1/organizations/{org_id}/settings"),
    ("patch", "/v1/organizations/{org_id}/settings"),
    ("post", "/v1/organizations/{org_id}/reporting-tokens"),
    (
        "delete",
        "/v1/organizations/{org_id}/reporting-tokens/{token_id}",
    ),
    ("get", "/v1/organizations/{org_id}/staking/farm"),
    ("post", "/v1/organizations/{org_id}/staking/farm/bind"),
    ("post", "/v1/organizations/{org_id}/staking/farm/sync"),
    ("get", "/v1/organizations/{org_id}/usage/balance"),
    ("get", "/v1/organizations/{org_id}/usage/by-model"),
    ("get", "/v1/organizations/{org_id}/usage/export"),
    ("get", "/v1/organizations/{org_id}/usage/history"),
    ("get", "/v1/organizations/{org_id}/usage/metrics"),
    ("get", "/v1/organizations/{org_id}/usage/summary"),
    ("get", "/v1/organizations/{org_id}/usage/timeseries"),
    ("get", "/v1/organizations/{org_id}/workspaces"),
    ("post", "/v1/organizations/{org_id}/workspaces"),
    ("get", "/v1/services"),
    ("get", "/v1/services/{service_name}"),
    ("get", "/v1/staking/farm/config"),
    ("post", "/v1/users/contact-email/verify"),
    ("get", "/v1/users/me"),
    ("post", "/v1/users/me/access-tokens"),
    ("post", "/v1/users/me/contact-email"),
    ("delete", "/v1/users/me/contact-email"),
    ("get", "/v1/users/me/invitations"),
    ("put", "/v1/users/me/profile"),
    ("post", "/v1/users/me/invitations/{invitation_id}/accept"),
    ("post", "/v1/users/me/invitations/{invitation_id}/decline"),
    ("get", "/v1/users/me/refresh-tokens"),
    ("delete", "/v1/users/me/refresh-tokens/{refresh_token_id}"),
    ("get", "/v1/users/me/status"),
    ("delete", "/v1/users/me/tokens"),
    ("get", "/v1/workspaces/{workspace_id}"),
    ("put", "/v1/workspaces/{workspace_id}"),
    ("delete", "/v1/workspaces/{workspace_id}"),
    ("get", "/v1/workspaces/{workspace_id}/api-keys"),
    ("post", "/v1/workspaces/{workspace_id}/api-keys"),
    (
        "get",
        "/v1/workspaces/{workspace_id}/api-keys/{api_key_id}/usage/history",
    ),
    ("patch", "/v1/workspaces/{workspace_id}/api-keys/{key_id}"),
    ("delete", "/v1/workspaces/{workspace_id}/api-keys/{key_id}"),
    (
        "patch",
        "/v1/workspaces/{workspace_id}/api-keys/{key_id}/spend-limit",
    ),
];

pub struct DeprecatedRoutesAddon;

impl Modify for DeprecatedRoutesAddon {
    fn modify(&self, openapi: &mut OpenApi) {
        for (method, path) in MOVED_TO_CLOUD_PLATFORM {
            let Some(item) = openapi.paths.paths.get_mut(*path) else {
                continue;
            };
            let operation = match *method {
                "get" => item.get.as_mut(),
                "post" => item.post.as_mut(),
                "put" => item.put.as_mut(),
                "patch" => item.patch.as_mut(),
                "delete" => item.delete.as_mut(),
                _ => None,
            };
            if let Some(operation) = operation {
                deprecate(operation);
            }
        }
    }
}

fn deprecate(operation: &mut Operation) {
    operation.deprecated = Some(Deprecated::True);
    operation.description = Some(match operation.description.take() {
        Some(description) if !description.is_empty() => {
            format!(
                "{DEPRECATION_NOTE}

{description}"
            )
        }
        _ => DEPRECATION_NOTE.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::HashSet;
    use utoipa::OpenApi as _;

    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

    fn spec() -> Value {
        serde_json::to_value(crate::openapi::ApiDoc::openapi()).unwrap()
    }

    #[test]
    fn every_listed_route_is_documented_and_deprecated() {
        let doc = spec();
        for (method, path) in MOVED_TO_CLOUD_PLATFORM {
            let operation = &doc["paths"][*path][*method];
            assert!(
                operation.is_object(),
                "{method} {path}: listed route not found in the spec: was it renamed (update the list) or removed (delete the entry)?"
            );
            assert_eq!(operation["deprecated"], true, "{method} {path}");
            assert!(
                operation["description"]
                    .as_str()
                    .is_some_and(|d| d.starts_with(DEPRECATION_NOTE)),
                "{method} {path} lacks the deprecation note"
            );
        }
    }

    #[test]
    fn nothing_else_is_deprecated() {
        let doc = spec();
        let listed: HashSet<(&str, &str)> = MOVED_TO_CLOUD_PLATFORM.iter().copied().collect();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for method in METHODS {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                let deprecated = operation["deprecated"] == true;
                assert_eq!(
                    deprecated,
                    listed.contains(&(method, path.as_str())),
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn content_paths_stay_in_cloud_api() {
        const CONTENT: [&str; 13] = [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/v1/conversations",
            "/v1/embeddings",
            "/v1/images",
            "/v1/audio",
            "/v1/files",
            "/v1/attestation",
            "/v1/signature",
            "/v1/models",
            "/v1/model",
            "/v1/messages",
        ];
        for (method, path) in MOVED_TO_CLOUD_PLATFORM {
            assert!(
                !CONTENT
                    .iter()
                    .any(|c| path == c || path.starts_with(&format!("{c}/"))),
                "{method} {path} is a content path"
            );
        }
    }

    #[test]
    fn deprecation_keeps_the_existing_description() {
        let mut operation = Operation::new();
        operation.description = Some("Original.".into());
        deprecate(&mut operation);
        assert_eq!(
            operation.description.as_deref(),
            Some(
                format!(
                    "{DEPRECATION_NOTE}

Original."
                )
                .as_str()
            )
        );
        let mut bare = Operation::new();
        deprecate(&mut bare);
        assert_eq!(bare.description.as_deref(), Some(DEPRECATION_NOTE));
    }

    /// Every `$ref` in the generated spec names a registered component, so newly
    /// documented operations cannot publish dangling schema references.
    #[test]
    fn every_schema_reference_resolves() {
        fn refs(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(target)) = map.get("$ref") {
                        out.push(target.clone());
                    }
                    map.values().for_each(|v| refs(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
                _ => {}
            }
        }
        let spec = spec();
        let mut found = Vec::new();
        refs(&spec, &mut found);
        let mut missing: Vec<String> = found
            .into_iter()
            .filter(|target| spec.pointer(target.trim_start_matches('#')).is_none())
            .collect();
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "unresolved $ref: {missing:?}");
    }
}
