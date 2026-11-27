use crate::models::ModelWithPricing;
use inference_providers::{ChatMessage, MessageRole};
use std::collections::{HashMap, HashSet};

/// The Gemini conversion and the Qwen3.8-27B template identify tool results by position.
pub(super) fn order_for_model(model: &ModelWithPricing, messages: &mut [ChatMessage]) {
    let gemini = model.provider_type == "external"
        && model
            .provider_config
            .as_ref()
            .and_then(|c| c["backend"].as_str())
            == Some("gemini");
    if !gemini && model.model_name != "Qwen/Qwen3.8-27B" {
        return;
    }

    // Each group contains a message and the tool results immediately following it.
    for turn in messages.chunk_by_mut(|_, next| next.role == MessageRole::Tool) {
        let [assistant, results @ ..] = turn else {
            continue;
        };
        let Some(calls) = assistant.tool_calls.as_deref() else {
            continue;
        };
        if assistant.role != MessageRole::Assistant
            || results.len() < 2
            || results.len() != calls.len()
        {
            continue;
        }
        if calls
            .iter()
            .zip(results.iter())
            .all(|(c, r)| c.id == r.tool_call_id)
        {
            continue;
        }

        let positions: HashMap<_, _> = calls
            .iter()
            .enumerate()
            .filter_map(|(index, call)| {
                Some((call.id.as_deref().filter(|id| !id.is_empty())?, index))
            })
            .collect();
        let position =
            |result: &ChatMessage| positions.get(result.tool_call_id.as_deref()?).copied();
        let mut seen = HashSet::with_capacity(results.len());
        if results
            .iter()
            .all(|r| position(r).is_some_and(|index| seen.insert(index)))
        {
            results.sort_unstable_by_key(position);
        }
    }
}
