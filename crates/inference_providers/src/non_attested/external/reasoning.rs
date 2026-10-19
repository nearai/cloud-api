use crate::CompletionError;
use serde_json::Value;
use std::collections::HashMap;

fn invalid(message: &str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 400,
        message: message.into(),
        is_external: false,
    }
}

pub(crate) fn effort(fields: &HashMap<String, Value>) -> Result<Option<String>, CompletionError> {
    Ok(
        crate::routing::ReasoningEffort::from_fields(fields)?
            .map(|value| value.as_str().to_owned()),
    )
}

pub(crate) fn validate(
    fields: &HashMap<String, Value>,
    supported: Option<&[String]>,
) -> Result<(), CompletionError> {
    if let Some(effort) = effort(fields)? {
        if supported.is_some_and(|values| !values.contains(&effort)) {
            return Err(invalid(
                "Requested reasoning effort is not supported by this provider model",
            ));
        }
    }
    Ok(())
}

pub(crate) fn normalize_chat(
    fields: &mut HashMap<String, Value>,
    base_url: &str,
) -> Result<(), CompletionError> {
    let Some(effort) = effort(fields)? else {
        return Ok(());
    };
    let openrouter = url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .as_deref()
        == Some("openrouter.ai");
    if openrouter {
        let reasoning = fields
            .entry("reasoning".into())
            .or_insert_with(|| serde_json::json!({}));
        let object = reasoning
            .as_object_mut()
            .ok_or_else(|| invalid("reasoning must be an object"))?;
        object.insert("effort".into(), Value::String(effort));
        fields.remove("reasoning_effort");
    } else {
        fields.insert("reasoning_effort".into(), Value::String(effort));
        if let Some(reasoning) = fields.get_mut("reasoning") {
            let object = reasoning
                .as_object_mut()
                .ok_or_else(|| invalid("reasoning must be an object"))?;
            object.remove("effort");
            if object.is_empty() {
                fields.remove("reasoning");
            }
        }
    }
    if let Some(output_config) = fields
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    {
        output_config.remove("effort");
        if output_config.is_empty() {
            fields.remove("output_config");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::non_attested::external::reasoning::{effort, normalize_chat, validate};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn preserves_effort_and_rejects_conflicts() {
        let mut fields = HashMap::from([("reasoning_effort".into(), json!("high"))]);
        normalize_chat(&mut fields, "https://openrouter.ai/api/v1").unwrap();
        assert_eq!(fields["reasoning"]["effort"], "high");
        assert!(!fields.contains_key("reasoning_effort"));
        normalize_chat(&mut fields, "https://api.deepseek.com/v1").unwrap();
        assert_eq!(fields["reasoning_effort"], "high");
        assert!(!fields.contains_key("reasoning"));
        fields.insert("output_config".into(), json!({"effort":"low"}));
        assert!(effort(&fields).is_err());
    }

    #[test]
    fn unknown_and_unsupported_capabilities_are_distinct() {
        let fields = HashMap::from([("reasoning_effort".into(), json!("high"))]);
        assert!(validate(&fields, None).is_ok());
        assert!(validate(&fields, Some(&[])).is_err());
        assert!(validate(&fields, Some(&["high".into()])).is_ok());
        assert!(validate(
            &HashMap::from([("reasoning_effort".into(), json!(13))]),
            None
        )
        .is_err());
    }
}
