//! Responses reasoning uses typed text arrays on the wire. Keep legacy stored
//! string values readable while the internal representation remains text.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ReasoningSummaryPart {
    #[serde(rename = "summary_text")]
    SummaryText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum ReasoningTextPart {
    #[serde(rename = "reasoning_text")]
    ReasoningText { text: String },
    #[serde(rename = "text")]
    Text { text: String },
}

pub(super) mod summary {
    use super::*;

    pub fn serialize<S: Serializer>(text: &str, serializer: S) -> Result<S::Ok, S::Error> {
        let parts: Vec<_> = (!text.is_empty())
            .then(|| ReasoningSummaryPart::SummaryText { text: text.into() })
            .into_iter()
            .collect();
        parts.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Text {
            Legacy(String),
            Parts(Vec<ReasoningSummaryPart>),
        }
        Ok(match Text::deserialize(deserializer)? {
            Text::Legacy(text) => text,
            Text::Parts(parts) => parts
                .into_iter()
                .map(|part| match part {
                    ReasoningSummaryPart::SummaryText { text } => text,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        })
    }
}

pub(super) mod content {
    use super::*;

    pub fn serialize<S: Serializer>(text: &str, serializer: S) -> Result<S::Ok, S::Error> {
        let parts: Vec<_> = (!text.is_empty())
            .then(|| ReasoningTextPart::ReasoningText { text: text.into() })
            .into_iter()
            .collect();
        parts.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Text {
            Legacy(String),
            Parts(Vec<ReasoningTextPart>),
        }
        Ok(match Option::<Text>::deserialize(deserializer)? {
            None => String::new(),
            Some(Text::Legacy(text)) => text,
            Some(Text::Parts(parts)) => parts
                .into_iter()
                .map(|part| match part {
                    ReasoningTextPart::ReasoningText { text }
                    | ReasoningTextPart::Text { text } => text,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::responses::models::{CreateResponseRequest, ResponseInputItem, ResponseOutputItem};
    use serde_json::json;

    #[test]
    fn legacy_reasoning_strings_serialize_as_standard_parts_and_round_trip() {
        let item: ResponseOutputItem = serde_json::from_value(json!({
            "type": "reasoning", "id": "rs_test", "status": "completed", "model": "test",
            "summary": "A concise summary.", "content": "  reasoning\nwith whitespace "
        }))
        .unwrap();
        let wire = serde_json::to_value(&item).unwrap();
        assert_eq!(
            wire["summary"],
            json!([{"type": "summary_text", "text": "A concise summary."}])
        );
        assert_eq!(
            wire["content"],
            json!([{"type": "reasoning_text", "text": "  reasoning\nwith whitespace "}])
        );
        let restored: ResponseOutputItem = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), wire);
        let replay: ResponseInputItem = serde_json::from_value(wire).unwrap();
        assert!(
            matches!(replay, ResponseInputItem::Reasoning { content, summary, .. }
            if content == "  reasoning\nwith whitespace " && summary == "A concise summary.")
        );
    }

    #[test]
    fn empty_reasoning_has_required_summary_array_and_optional_content() {
        let item: ResponseOutputItem = serde_json::from_value(json!({
            "type": "reasoning", "id": "rs_test", "status": "in_progress", "model": "test",
            "summary": "", "content": ""
        }))
        .unwrap();
        let wire = serde_json::to_value(item).unwrap();
        assert_eq!(wire["summary"], json!([]));
        assert!(wire.get("content").is_none());
    }

    #[test]
    fn plaintext_reasoning_accepts_standard_parts_and_legacy_strings() {
        for (summary, content, expected_summary, expected_content) in [
            (
                json!([{"type":"summary_text", "text":"summary"}]),
                json!([{"type":"reasoning_text", "text":"first"}, {"type":"text", "text":"second"}]),
                "summary",
                "first\n\nsecond",
            ),
            (
                json!("legacy summary"),
                json!("legacy content"),
                "legacy summary",
                "legacy content",
            ),
            (json!([]), json!(null), "", ""),
        ] {
            let request: CreateResponseRequest = serde_json::from_value(json!({
                "model":"test", "store":false,
                "input":[{"type":"reasoning", "summary":summary, "content":content}]
            }))
            .unwrap();
            assert!(request.validate_stateless().is_ok());
            let Some(crate::responses::models::ResponseInput::Items(items)) = request.input else {
                panic!("expected reasoning input");
            };
            assert!(
                matches!(&items[0], ResponseInputItem::Reasoning { summary, content, .. }
                if summary == expected_summary && content == expected_content)
            );
        }
    }

    #[test]
    fn encrypted_only_reasoning_is_explicitly_rejected_by_typed_adapter() {
        let request: CreateResponseRequest = serde_json::from_value(json!({
            "model":"test", "store":false,
            "input":[{"type":"reasoning", "summary":[], "encrypted_content":"opaque"}]
        }))
        .unwrap();
        assert!(request
            .validate_stateless()
            .unwrap_err()
            .contains("native Responses"));
        assert!(serde_json::from_value::<ResponseInputItem>(json!({
            "type":"reasoning", "summary":[{"type":"unexpected", "text":"must not be dropped"}]
        }))
        .is_err());
    }
}
