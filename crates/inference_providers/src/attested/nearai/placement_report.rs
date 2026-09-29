//! Placement decision reporting: the routing-only request view read from
//! `ChatCompletionParams`, and turning a [`DecisionRecord`] into metrics +
//! one log line. None of this touches `Fleet` state — it is pure translation
//! from a placer decision (or the request params) to observability output,
//! extracted out of `fleet.rs` to keep that file to routing/reservation state.

use super::tracing_headers;
use crate::placement_io::{
    PlacementHandles, METRIC_AFFINITY, METRIC_CHOSEN_BACKLOG, METRIC_DECISIONS, METRIC_EXCLUDED,
};
use crate::ChatCompletionParams;
use placement::affinity::AffinityKey;
use placement::decision::{AffinitySource, DecisionRecord};
use placement::rules::Rule;

/// Routing-only placement inputs: the pool's typed `params.placement`, the
/// operator priority and the tracing ids (read from `params.extra` before
/// the tracing helper strips them). Holds an [`AffinityKey`], so it has no
/// `Debug`; the key is never logged.
pub(super) struct PlacementRequest {
    pub(super) model: String,
    /// Tracing ids for the decision log line (empty when absent).
    pub(super) request_id: String,
    pub(super) org_id: String,
    /// The pool's estimate (input only; 0 when the pool gave none).
    pub(super) prompt_tokens: u64,
    /// The pool's context requirement (input plus output reserve).
    pub(super) context_tokens: Option<u64>,
    /// The pool's class: the requirement exceeds the base tier.
    pub(super) heavy: bool,
    pub(super) affinity: Option<AffinityKey>,
    pub(super) affinity_source: AffinitySource,
    /// `params.request_priority` (operator-set, never client JSON).
    pub(super) priority: i32,
}

impl PlacementRequest {
    /// Reads `params.placement`, `params.request_priority` and the tracing
    /// ids without removing anything. Placement never re-estimates: the pool
    /// (`inference_provider_pool::context_routing`) owns the size estimate.
    pub(super) fn from_params(params: &ChatCompletionParams) -> Self {
        let text = |key: &str| params.extra.get(key).and_then(|value| value.as_str());
        let placement = &params.placement;
        Self {
            model: params.model.clone(),
            request_id: text(tracing_headers::REQUEST_ID)
                .unwrap_or_default()
                .to_string(),
            org_id: text(tracing_headers::ORG_ID)
                .unwrap_or_default()
                .to_string(),
            prompt_tokens: placement.prompt_tokens.unwrap_or(0),
            context_tokens: placement.context_tokens,
            heavy: placement.heavy,
            affinity: placement.affinity.clone(),
            // A source without a key would mislabel the decision record.
            affinity_source: if placement.affinity.is_some() {
                placement.affinity_source
            } else {
                AffinitySource::None
            },
            priority: params.request_priority,
        }
    }
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
    if logs_at_debug(record) {
        // Placement has no usable state (Valkey unreachable, placeholder
        // endpoint, stale snapshot): every covered request would log the same
        // line. The decision metric above still counts each one.
        tracing::debug!(
            request_id = %request.request_id,
            org_id = %request.org_id,
            model = %request.model,
            outcome = record.outcome,
            reason = record.reason.unwrap_or(""),
            snapshot_age_ms = record.snapshot_age_ms,
            "Placement decision"
        );
        return;
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
        slot = record.slot.as_deref().unwrap_or(""),
        replica = ?record.replica,
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

/// Legacy decisions made because placement has no usable snapshot, or is
/// switched off by the data-plane kill switch, are logged at debug; they
/// would otherwise repeat on every covered request while the shared state is
/// down, unconfigured or disabled.
fn logs_at_debug(record: &DecisionRecord) -> bool {
    record.outcome == "legacy" && matches!(record.reason, Some("no_state" | "stale" | "disabled"))
}

fn outcome_tag(outcome: &str) -> &'static str {
    match outcome {
        "place" => "outcome:place",
        "legacy" => "outcome:legacy",
        "refused" => "outcome:refused",
        _ => "outcome:unknown",
    }
}

/// `reason:{..}` for a legacy record, `selection:{..}` for a placed one.
fn detail_tag(record: &DecisionRecord) -> &'static str {
    match (record.reason, record.selection) {
        (Some(reason), _) => match reason {
            "disabled" => "reason:disabled",
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
        Rule::Lifecycle => "rule:lifecycle",
        Rule::Freshness => "rule:freshness",
        Rule::Capacity => "rule:capacity",
        Rule::Context => "rule:context",
        Rule::Lane => "rule:lane",
    }
}

#[cfg(test)]
mod tests {
    use super::logs_at_debug;
    use placement::decision::{AffinitySource, Decision, DecisionRecord, PlaceInput, Placer};
    use placement::policy::Tier;
    use placement::snapshot::Snapshot;
    use std::collections::HashMap;

    fn legacy_record(snap: &Snapshot) -> DecisionRecord {
        let input = PlaceInput {
            model: placement::consts::COVERED_MODELS[0].to_string(),
            prompt_tokens: 10,
            context_tokens: None,
            heavy: false,
            priority: 0,
            affinity: None,
            affinity_source: AffinitySource::None,
            now_ms: 10_000,
        };
        let mut rng = rand::rng();
        match Placer::new([1u8; 32], Tier::Base).place(&input, snap, &HashMap::new(), &mut rng) {
            Decision::Legacy { record, .. } => record,
            Decision::Place { .. } | Decision::Refused { .. } => {
                panic!("an empty snapshot is always legacy")
            }
        }
    }

    fn no_state_record() -> DecisionRecord {
        legacy_record(&Snapshot::default())
    }

    #[test]
    fn no_state_and_stale_decisions_log_at_debug() {
        let mut record = no_state_record();
        assert_eq!(record.reason, Some("no_state"));
        assert!(logs_at_debug(&record));
        record.reason = Some("stale");
        assert!(logs_at_debug(&record));
    }

    #[test]
    fn kill_switch_decisions_log_at_debug_with_their_own_tag() {
        let record = legacy_record(&Snapshot {
            disabled: true,
            ..Snapshot::default()
        });
        assert_eq!(record.reason, Some("disabled"));
        assert!(logs_at_debug(&record));
        assert_eq!(super::detail_tag(&record), "reason:disabled");
    }

    #[test]
    fn other_decisions_log_at_info() {
        let mut record = no_state_record();
        record.reason = Some("incomplete");
        assert!(!logs_at_debug(&record));
        record.outcome = "place";
        record.reason = None;
        assert!(!logs_at_debug(&record));
    }
}
