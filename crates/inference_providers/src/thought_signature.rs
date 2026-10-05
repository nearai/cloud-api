//! Serde adapter for a flattened tool-call signature field.
//!
//! Google-compatible clients use `extra_content.google.thought_signature`.
//! Keep the legacy top-level field for existing clients, with the nested value
//! taking precedence on input. Both output fields use the same internal value.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
struct SignatureFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra_content: Option<ExtraContent>,
}

#[derive(Serialize, Deserialize)]
struct ExtraContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    google: Option<GoogleContent>,
}

#[derive(Serialize, Deserialize)]
struct GoogleContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
}

pub fn serialize<S>(signature: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    SignatureFields {
        thought_signature: signature.clone(),
        extra_content: signature.as_ref().map(|signature| ExtraContent {
            google: Some(GoogleContent {
                thought_signature: Some(signature.clone()),
            }),
        }),
    }
    .serialize(serializer)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let fields = SignatureFields::deserialize(deserializer)?;
    Ok(fields
        .extra_content
        .and_then(|extra| extra.google)
        .and_then(|google| google.thought_signature)
        .or(fields.thought_signature))
}

#[cfg(test)]
mod tests {
    use crate::models::ToolCallDelta;
    use serde_json::json;

    #[test]
    fn signature_only_stream_delta_preserves_nested_metadata() {
        let delta: ToolCallDelta = serde_json::from_value(json!({
            "index": 1,
            "extra_content": {"google": {"thought_signature": "opaque+/=="}}
        }))
        .unwrap();
        assert_eq!(delta.thought_signature.as_deref(), Some("opaque+/=="));
        assert!(delta.function.is_none());
        let wire = serde_json::to_value(delta).unwrap();
        assert_eq!(wire["index"], 1);
        assert_eq!(wire["thought_signature"], "opaque+/==");
        assert_eq!(
            wire["extra_content"]["google"]["thought_signature"],
            "opaque+/=="
        );
        assert!(wire.get("function").is_none());
    }
}
