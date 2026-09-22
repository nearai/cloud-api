//! Bounded `client` metric label derived from organization id.
//!
//! Per-org TTFT is only useful for SLA reporting when it stays low-cardinality:
//! a handful of named clients (e.g. `brave`, `openrouter`) get their own label,
//! and every other organization collapses into `other`. The mapping is
//! configured via `METRICS_CLIENT_ORG_LABELS` so it can change per-deployment
//! without a code change, but the parser still enforces the cardinality bound
//! so a bad env value cannot blow up the metric's tag set.
//!
//! Format: `label=org_uuid,label=org_uuid,...`. A label may repeat to cover
//! several org ids (e.g. `brave=96c6...,brave=fb0f...`).

use std::collections::HashMap;
use std::sync::OnceLock;

use uuid::Uuid;

/// Label reported for an organization id with no configured mapping.
const OTHER_LABEL: &str = "other";

/// Hard cap on distinct labels, to keep the `client` tag low-cardinality.
const MAX_DISTINCT_LABELS: usize = 16;

/// Parse `METRICS_CLIENT_ORG_LABELS` into an org id -> label map.
///
/// Invalid entries (bad label syntax, unparseable uuid, the reserved `other`
/// label, or a distinct label beyond the cardinality cap) are skipped with a
/// `tracing::warn!` rather than failing the whole parse — one bad entry
/// should not take down every other configured client label.
fn parse_client_org_labels(raw: &str) -> HashMap<Uuid, String> {
    let mut labels: HashMap<Uuid, String> = HashMap::new();
    let mut distinct_labels: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let Some((label, org_id)) = entry.split_once('=') else {
            tracing::warn!(
                entry,
                "METRICS_CLIENT_ORG_LABELS: entry missing '=', skipping"
            );
            continue;
        };
        let label = label.trim();
        let org_id = org_id.trim();

        if !is_valid_label(label) {
            tracing::warn!(label, "METRICS_CLIENT_ORG_LABELS: invalid label, skipping");
            continue;
        }
        if label == OTHER_LABEL {
            tracing::warn!("METRICS_CLIENT_ORG_LABELS: label 'other' is reserved, skipping");
            continue;
        }

        let Ok(org_id) = Uuid::parse_str(org_id) else {
            tracing::warn!(
                label,
                "METRICS_CLIENT_ORG_LABELS: invalid org uuid, skipping"
            );
            continue;
        };

        if !distinct_labels.contains(label) && distinct_labels.len() >= MAX_DISTINCT_LABELS {
            tracing::warn!(
                label,
                max = MAX_DISTINCT_LABELS,
                "METRICS_CLIENT_ORG_LABELS: exceeded max distinct labels, skipping"
            );
            continue;
        }

        distinct_labels.insert(label.to_string());
        labels.insert(org_id, label.to_string());
    }

    labels
}

/// `^[a-z0-9_]{1,32}$`, checked by hand to avoid pulling in a regex crate for
/// one small, fixed pattern.
fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 32
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn client_org_labels() -> &'static HashMap<Uuid, String> {
    static LABELS: OnceLock<HashMap<Uuid, String>> = OnceLock::new();
    LABELS.get_or_init(|| {
        let raw = std::env::var("METRICS_CLIENT_ORG_LABELS").unwrap_or_default();
        parse_client_org_labels(&raw)
    })
}

/// Bounded `client` label for an organization id, for the `client` metric tag.
/// Returns the configured label, or `"other"` when the org has none.
pub fn client_label(organization_id: Uuid) -> &'static str {
    client_org_labels()
        .get(&organization_id)
        .map(String::as_str)
        .unwrap_or(OTHER_LABEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_entry_with_repeated_label() {
        let org1 = Uuid::new_v4();
        let org2 = Uuid::new_v4();
        let raw = format!("brave={org1},brave={org2}");
        let labels = parse_client_org_labels(&raw);
        assert_eq!(labels.get(&org1).map(String::as_str), Some("brave"));
        assert_eq!(labels.get(&org2).map(String::as_str), Some("brave"));
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn trims_whitespace_around_entries_and_equals() {
        let org = Uuid::new_v4();
        let raw = format!("  brave = {org}  ");
        let labels = parse_client_org_labels(&raw);
        assert_eq!(labels.get(&org).map(String::as_str), Some("brave"));
    }

    #[test]
    fn skips_invalid_label_characters() {
        let org = Uuid::new_v4();
        let raw = format!("Brave-1={org}");
        let labels = parse_client_org_labels(&raw);
        assert!(labels.is_empty());
    }

    #[test]
    fn skips_invalid_uuid() {
        let raw = "brave=not-a-uuid".to_string();
        let labels = parse_client_org_labels(&raw);
        assert!(labels.is_empty());
    }

    #[test]
    fn skips_reserved_other_label() {
        let org = Uuid::new_v4();
        let raw = format!("other={org}");
        let labels = parse_client_org_labels(&raw);
        assert!(labels.is_empty());
    }

    #[test]
    fn truncates_beyond_max_distinct_labels() {
        let mut raw = String::new();
        let mut orgs = Vec::new();
        // MAX_DISTINCT_LABELS distinct labels, each with a distinct org id.
        for i in 0..MAX_DISTINCT_LABELS {
            let org = Uuid::new_v4();
            orgs.push(org);
            raw.push_str(&format!("label{i}={org},"));
        }
        // One more distinct label beyond the cap.
        let extra_org = Uuid::new_v4();
        raw.push_str(&format!("label_extra={extra_org}"));

        let labels = parse_client_org_labels(&raw);
        assert_eq!(labels.len(), MAX_DISTINCT_LABELS);
        for org in &orgs {
            assert!(labels.contains_key(org));
        }
        assert!(!labels.contains_key(&extra_org));
    }

    #[test]
    fn empty_string_yields_empty_map() {
        assert!(parse_client_org_labels("").is_empty());
        assert!(parse_client_org_labels("   ").is_empty());
    }

    #[test]
    fn client_label_falls_back_to_other_when_env_unset() {
        // The OnceLock-backed path does not depend on the process env var
        // being set in this test process; regardless of its value, an
        // unmapped random org id must fall back to "other".
        assert_eq!(client_label(Uuid::new_v4()), "other");
    }
}
