//! Normalize the instruction prefix produced by the typed Responses adapter.

use crate::completions::ports::CompletionMessage;
use serde_json::Value;

/// Chat providers receive developer messages as system messages. Templates
/// such as Qwen's accept only one system message, at the beginning. Combine
/// the leading organization policy, request instructions, and caller-provided
/// system/developer messages in their existing order. Never move instructions
/// across a user, assistant, or tool turn.
pub(super) fn normalize_leading_instructions(
    messages: Vec<CompletionMessage>,
) -> Vec<CompletionMessage> {
    let mut messages = messages.into_iter().peekable();
    let Some(mut first) = messages.next() else {
        return Vec::new();
    };
    if !is_instruction(&first) {
        return std::iter::once(first).chain(messages).collect();
    }

    first.role = "system".to_string();
    while messages.peek().is_some_and(is_instruction) {
        if let Some(next) = messages.next() {
            first.content = join_content(first.content, next.content);
        }
    }
    std::iter::once(first).chain(messages).collect()
}

fn is_instruction(message: &CompletionMessage) -> bool {
    matches!(message.role.as_str(), "system" | "developer")
}

fn join_content(first: Value, next: Value) -> Value {
    match (first, next) {
        (Value::String(mut first), Value::String(next)) => {
            first.push_str("\n\n");
            first.push_str(&next);
            Value::String(first)
        }
        (first, next) => {
            let mut parts = into_parts(first);
            parts.push(serde_json::json!({"type": "text", "text": "\n\n"}));
            parts.extend(into_parts(next));
            Value::Array(parts)
        }
    }
}

fn into_parts(content: Value) -> Vec<Value> {
    match content {
        Value::Array(parts) => parts,
        Value::String(text) => vec![serde_json::json!({"type": "text", "text": text})],
        other => vec![other],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completions::ports::CompletionToolCall;
    use serde_json::json;

    fn message(role: &str, content: Value) -> CompletionMessage {
        CompletionMessage {
            role: role.to_string(),
            content,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn organization_instructions_and_codex_developers_form_one_system_prefix() {
        let history = vec![
            message("user", json!("previous question")),
            message("assistant", json!("previous answer")),
            message("user", json!("current question")),
        ];
        let mut messages = vec![
            message("system", json!("organization policy")),
            message("system", json!("request instructions\n\ntime context")),
            message("developer", json!("first developer instructions")),
            message("developer", json!("second developer instructions")),
        ];
        messages.extend(history.clone());

        let normalized = normalize_leading_instructions(messages);
        assert_eq!(normalized.len(), history.len() + 1);
        assert_eq!(normalized[0].role, "system");
        assert_eq!(
            normalized[0].content,
            "organization policy\n\nrequest instructions\n\ntime context\n\nfirst developer instructions\n\nsecond developer instructions"
        );
        assert_eq!(
            serde_json::to_value(&normalized[1..]).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }

    #[test]
    fn mixed_instruction_content_preserves_text_and_multimodal_parts() {
        let image = json!({"type": "image_url", "image_url": {
            "url": "data:image/png;base64,fixture", "detail": "low"
        }});
        let normalized = normalize_leading_instructions(vec![
            message("system", json!("  organization policy\n")),
            message(
                "developer",
                json!([
                    {"type": "text", "text": "developer text"}, image.clone()
                ]),
            ),
            message("developer", json!("last instruction")),
        ]);
        assert_eq!(normalized.len(), 1);
        assert_eq!(
            normalized[0].content,
            json!([
                {"type": "text", "text": "  organization policy\n"},
                {"type": "text", "text": "\n\n"},
                {"type": "text", "text": "developer text"},
                image,
                {"type": "text", "text": "\n\n"},
                {"type": "text", "text": "last instruction"}
            ])
        );
    }

    #[test]
    fn normalization_preserves_later_instructions_and_tool_replay_order() {
        let mut assistant = message("assistant", json!("checking"));
        assistant.reasoning_content = Some("prior reasoning".to_string());
        assistant.tool_calls = Some(vec![CompletionToolCall {
            id: "call_1".to_string(),
            name: "lookup".to_string(),
            arguments: "{ \"city\": \"上海\" }".to_string(),
            thought_signature: Some("opaque".to_string()),
        }]);
        let mut result = message("tool", json!("result"));
        result.tool_call_id = Some("call_1".to_string());
        let history = vec![
            message("user", json!("question")),
            assistant,
            result,
            message("developer", json!("later instruction")),
            message("user", json!("follow-up")),
        ];
        let mut messages = vec![
            message("system", json!("instructions")),
            message("developer", json!("initial developer instructions")),
        ];
        messages.extend(history.clone());
        let normalized = normalize_leading_instructions(messages);
        assert_eq!(
            serde_json::to_value(&normalized[1..]).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }

    #[test]
    fn absent_or_single_instruction_prefix_keeps_content_intact() {
        assert!(normalize_leading_instructions(Vec::new()).is_empty());
        let history = vec![
            message("user", json!("question")),
            message("developer", json!("later instruction")),
        ];
        assert_eq!(
            serde_json::to_value(normalize_leading_instructions(history.clone())).unwrap(),
            serde_json::to_value(history).unwrap()
        );
        let normalized = normalize_leading_instructions(vec![message(
            "developer",
            json!("  preserve whitespace\n"),
        )]);
        assert_eq!(normalized[0].role, "system");
        assert_eq!(normalized[0].content, "  preserve whitespace\n");
    }
}
