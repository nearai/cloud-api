//! Placement decision reporting: the routing-only request view read out of
//! `params.extra`, and turning a [`DecisionRecord`] into metrics + one log
//! line. None of this touches `Fleet` state — it is pure translation from a
//! placer decision (or the extra map) to observability output, extracted out
//! of `fleet.rs` to keep that file to routing/reservation state.

use super::{placement_headers, tracing_headers};
use crate::placement_io::{
    PlacementHandles, METRIC_AFFINITY, METRIC_CHOSEN_BACKLOG, METRIC_DECISIONS, METRIC_EXCLUDED,
};
use placement::affinity::AffinityKey;
use placement::decision::{AffinitySource, DecisionRecord};
use placement::rules::Rule;
use std::collections::HashMap;

/// Routing-only placement inputs read from `params.extra` before the
/// tracing and encryption helpers strip it. Holds an [`AffinityKey`], so it
/// has no `Debug`; the key is never logged.
pub(super) struct PlacementRequest {
    pub(super) model: String,
    /// Tracing ids for the decision log line (empty when absent).
    pub(super) request_id: String,
    pub(super) org_id: String,
    pub(super) affinity: Option<AffinityKey>,
    pub(super) affinity_source: AffinitySource,
}

impl PlacementRequest {
    /// Reads the affinity key (hex; invalid means none), its source and the
    /// tracing ids from `extra` without removing anything.
    pub(super) fn from_extra(model: &str, extra: &HashMap<String, serde_json::Value>) -> Self {
        let text = |key: &str| extra.get(key).and_then(|value| value.as_str());
        let affinity = text(placement_headers::AFFINITY).and_then(AffinityKey::from_hex);
        let affinity_source = match (&affinity, text(placement_headers::AFFINITY_SOURCE)) {
            (Some(_), Some("client")) => AffinitySource::Client,
            (Some(_), Some("prefix")) => AffinitySource::Prefix,
            _ => AffinitySource::None,
        };
        Self {
            model: model.to_string(),
            request_id: text(tracing_headers::REQUEST_ID)
                .unwrap_or_default()
                .to_string(),
            org_id: text(tracing_headers::ORG_ID)
                .unwrap_or_default()
                .to_string(),
            affinity,
            affinity_source,
        }
    }
}

/// Estimated prompt tokens: total message text bytes / 4. Text is a string
/// content or the `text` fields of content parts.
pub(super) fn prompt_tokens_est(messages: &[crate::ChatMessage]) -> u64 {
    let bytes = messages
        .iter()
        .map(|message| match message.content.as_ref() {
            Some(serde_json::Value::String(text)) => text.len(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .map(str::len)
                .fold(0usize, usize::saturating_add),
            _ => 0,
        })
        .fold(0usize, usize::saturating_add);
    u64::try_from(bytes / 4).unwrap_or(u64::MAX)
}

/// Decision metrics and one info line per decision. IDs and numbers only:
/// never the affinity key, pin id or content. Metric tags are static strings
/// (no per-decision allocation) and stay low-cardinality.
pub(super) fn report_decision(
    handles: &PlacementHandles,
    record: &DecisionRecord,
    request: &PlacementRequest,
) {
    let metrics = handles.io.metrics();
    let outcome = outcome_tag(record.outcome);
    metrics.record_count(METRIC_DECISIONS, 1, &[outcome, detail_tag(record)]);
    metrics.record_count(
        METRIC_AFFINITY,
        1,
        &[affinity_tag(record.affinity), outcome],
    );
    for (rule, n) in record.excluded.iter().filter(|(_, n)| *n > 0) {
        metrics.record_count(METRIC_EXCLUDED, i64::from(*n), &[rule_tag(*rule)]);
    }
    if let Some(backlog) = record.chosen_backlog_tokens {
        metrics.record_histogram(METRIC_CHOSEN_BACKLOG, backlog as f64, &[]);
    }
    let excluded = record
        .excluded
        .iter()
        .map(|(rule, n)| format!("{}:{n}", rule.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(
        request_id = %request.request_id,
        org_id = %request.org_id,
        model = %request.model,
        outcome = record.outcome,
        reason = record.reason.unwrap_or(""),
        selection = record.selection.unwrap_or(""),
        rank = ?record.rank,
        affinity = record.affinity,
        host = record.host.as_deref().unwrap_or(""),
        home = record.home.as_deref().unwrap_or(""),
        pinned = record.pinned.as_deref().unwrap_or(""),
        eligible = record.eligible,
        excluded = %excluded,
        chosen_score = ?record.chosen_score,
        home_score = ?record.home_score,
        best_score = ?record.best_score,
        snapshot_age_ms = record.snapshot_age_ms,
        pending_req = record.pending_req,
        backlog_tokens = ?record.chosen_backlog_tokens,
        "Placement decision"
    );
}

fn outcome_tag(outcome: &str) -> &'static str {
    match outcome {
        "place" => "outcome:place",
        "legacy" => "outcome:legacy",
        _ => "outcome:unknown",
    }
}

/// `reason:{..}` for a legacy record, `selection:{..}` for a placed one.
fn detail_tag(record: &DecisionRecord) -> &'static str {
    match (record.reason, record.selection) {
        (Some(reason), _) => match reason {
            "not_covered" => "reason:not_covered",
            "no_state" => "reason:no_state",
            "stale" => "reason:stale",
            "none_eligible" => "reason:none_eligible",
            "host_unmapped" => "reason:host_unmapped",
            "key_group" => "reason:key_group",
            "incomplete" => "reason:incomplete",
            _ => "reason:unknown",
        },
        (None, Some(selection)) => match selection {
            "pinned" => "selection:pinned",
            "home" => "selection:home",
            "spill" => "selection:spill",
            "best_of_two" => "selection:best_of_two",
            _ => "selection:unknown",
        },
        (None, None) => "reason:unknown",
    }
}

fn affinity_tag(affinity: &str) -> &'static str {
    match affinity {
        "client" => "affinity:client",
        "prefix" => "affinity:prefix",
        "none" => "affinity:none",
        _ => "affinity:unknown",
    }
}

fn rule_tag(rule: Rule) -> &'static str {
    match rule {
        Rule::Model => "rule:model",
        Rule::Lifecycle => "rule:lifecycle",
        Rule::Freshness => "rule:freshness",
        Rule::Capacity => "rule:capacity",
        Rule::Context => "rule:context",
    }
}
