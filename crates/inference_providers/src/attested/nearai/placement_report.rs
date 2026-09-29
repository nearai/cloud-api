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
    /// `params.request_priority` (operator-set, never client JSON).
    pub(super) priority: i32,
}

impl PlacementRequest {
    /// Reads the affinity key (hex; invalid means none), its source and the
    /// tracing ids from `extra` without removing anything.
    pub(super) fn from_extra(
        model: &str,
        priority: i32,
        extra: &HashMap<String, serde_json::Value>,
    ) -> Self {
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
            priority,
        }
    }
}

/// Estimated prompt tokens: total message text bytes / 4. Text is a string
/// content, the `text` fields of content parts, each tool call's
/// `function.arguments`, and any echoed `reasoning_content` — all of it is
/// serialized upstream, so leaving tool calls or reasoning out of the
/// estimate would systematically undercount agent/tool-heavy traffic (image
/// or other non-text content parts cannot be estimated from bytes and stay
/// excluded).
pub(super) fn prompt_tokens_est(messages: &[crate::ChatMessage]) -> u64 {
    let bytes = messages
        .iter()
        .map(|message| {
            let mut bytes = match message.content.as_ref() {
                Some(serde_json::Value::String(text)) => text.len(),
                Some(serde_json::Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                    .map(str::len)
                    .fold(0usize, usize::saturating_add),
                _ => 0,
            };
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    bytes = bytes
                        .saturating_add(call.function.arguments.as_deref().map_or(0, str::len));
                }
            }
            bytes.saturating_add(message.reasoning_content.as_deref().map_or(0, str::len))
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
    use super::{logs_at_debug, prompt_tokens_est};
    use crate::models::{FunctionCall, ToolCall};
    use crate::{ChatMessage, MessageRole};
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

    #[test]
    fn prompt_tokens_est_counts_tool_call_arguments_and_reasoning_content() {
        let text_only = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::Value::String("a".repeat(40))),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }];
        let with_tool_and_reasoning = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::Value::String("a".repeat(40))),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: Some("call-1".to_string()),
                type_: Some("function".to_string()),
                function: FunctionCall {
                    name: Some("lookup".to_string()),
                    arguments: Some("b".repeat(40)),
                },
                index: None,
                thought_signature: None,
            }]),
            reasoning_content: Some("c".repeat(40)),
        }];

        // Then: the estimate grows to include the tool call arguments and
        // the reasoning content bytes, not just the text content.
        assert_eq!(prompt_tokens_est(&text_only), 10);
        assert_eq!(prompt_tokens_est(&with_tool_and_reasoning), 30);
    }
}
