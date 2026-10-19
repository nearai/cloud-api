//! Request requirements shared by protocol adapters and routing. These are
//! caller intent, never a declaration that a deployment has been verified.
use crate::CompletionError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum TeeRequirement {
    Required,
    Any,
}

impl TeeRequirement {
    pub fn from_value(value: Option<&Value>) -> Result<Option<Self>, CompletionError> {
        value
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|_| invalid("tee must be 'required' or 'any'"))
            })
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn from_fields(fields: &HashMap<String, Value>) -> Result<Option<Self>, CompletionError> {
        for name in ["reasoning", "output_config"] {
            if fields
                .get(name)
                .is_some_and(|value| !value.is_null() && !value.is_object())
            {
                return Err(invalid("reasoning and output_config must be objects"));
            }
        }
        let mut selected = None;
        for value in [
            fields.get("reasoning_effort"),
            fields.get("reasoning").and_then(|v| v.get("effort")),
            fields.get("output_config").and_then(|v| v.get("effort")),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_null())
        {
            let effort: Self = serde_json::from_value(value.clone())
                .map_err(|_| invalid("Invalid reasoning effort"))?;
            if selected.is_some_and(|previous| previous != effort) {
                return Err(invalid("Conflicting reasoning effort fields"));
            }
            selected = Some(effort);
        }
        Ok(selected)
    }
}

pub fn invalid(message: &str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 400,
        message: message.into(),
        is_external: false,
    }
}

/// Keep transport classification structured and avoid echoing URLs or bodies.
pub fn transport_error(error: reqwest::Error) -> CompletionError {
    CompletionError::HttpError {
        status_code: if error.is_timeout() { 408 } else { 503 },
        message: if error.is_timeout() {
            "Upstream request timed out"
        } else {
            "Upstream transport failed"
        }
        .into(),
        is_external: true,
    }
}

/// Valid Retry-After delays are bounded to five minutes. HTTP dates use the
/// response's receipt time, never the caller's clock.
pub fn retry_after_error(response: &reqwest::Response) -> Option<CompletionError> {
    retry_after_headers(response.status().as_u16(), response.headers())
}

pub fn retry_after_headers(
    status_code: u16,
    headers: &reqwest::header::HeaderMap,
) -> Option<CompletionError> {
    if status_code != 429 && !(500..600).contains(&status_code) {
        return None;
    }
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    let seconds = value.parse::<u64>().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| (date.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
    })?;
    Some(CompletionError::RetryableUpstream {
        status_code,
        retry_after_ms: seconds.min(300).saturating_mul(1000),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCategory {
    Overload,
    Transient,
    InvalidRequest,
    Configuration,
    Unsupported,
    Verification,
    Cancelled,
}

impl FailureCategory {
    pub fn from_error(error: &CompletionError) -> Self {
        match error {
            CompletionError::HttpError {
                status_code: 429, ..
            }
            | CompletionError::RetryableUpstream {
                status_code: 429, ..
            } => Self::Overload,
            CompletionError::HttpError {
                status_code: 401 | 403,
                ..
            } => Self::Configuration,
            CompletionError::HttpError { status_code, .. }
                if (400..500).contains(status_code) && *status_code != 408 =>
            {
                Self::InvalidRequest
            }
            CompletionError::NoPubKeyProvider(_) => Self::Verification,
            CompletionError::ClientMediaError(_) => Self::InvalidRequest,
            _ => Self::Transient,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_requirements_reject_ambiguous_or_malformed_intent() {
        assert_eq!(
            TeeRequirement::from_value(Some(&json!("required"))).unwrap(),
            Some(TeeRequirement::Required)
        );
        assert!(TeeRequirement::from_value(Some(&json!(null))).is_err());
        assert!(TeeRequirement::from_value(Some(&json!("preferred"))).is_err());
        let mut fields = HashMap::from([
            ("reasoning_effort".into(), json!("high")),
            ("output_config".into(), json!({"effort":"high"})),
        ]);
        assert_eq!(
            ReasoningEffort::from_fields(&fields).unwrap(),
            Some(ReasoningEffort::High)
        );
        fields.insert("reasoning".into(), json!({"effort":"low"}));
        assert!(ReasoningEffort::from_fields(&fields).is_err());
        fields.insert("reasoning".into(), json!(false));
        assert!(ReasoningEffort::from_fields(&fields).is_err());
    }
}
