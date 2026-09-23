//! TypeSafe-compatible System One protocol. Capability ("decisions") is
//! independent of whether the serving provider is external or attested.
use crate::CompletionError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const OUTPUT_MODALITY: &str = "decisions";

/// Text or structured context accepted by the System One protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", schema(as = SystemOneContent))]
pub enum Content {
    Text(String),
    Object(serde_json::Map<String, serde_json::Value>),
    Array(Vec<serde_json::Value>),
}

/// A choice may be identified solely by its name, with a null description.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
#[cfg_attr(feature = "openapi", schema(as = SystemOneChoiceCriterion))]
pub struct ChoiceCriterion(pub Option<Content>);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Content,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
#[cfg_attr(feature = "openapi", schema(as = SystemOneQuestion))]
pub enum Question {
    Noul {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Content>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Content>,
        criteria: BTreeMap<String, ChoiceCriterion>,
    },
    Score {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Content>,
        criteria: Vec<Content>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", schema(as = SystemOneNoulCriteria))]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Content>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Content>,
}

impl SystemOneRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.trim().is_empty() {
            return Err("model must not be empty");
        }
        if self.questions.is_empty() {
            return Err("questions must contain at least one question");
        }
        for question in self.questions.values() {
            match question {
                Question::Choice { criteria, .. } if !(1..=255).contains(&criteria.len()) => {
                    return Err("choice criteria must contain between 1 and 255 options");
                }
                // The published OpenAPI accepts one level; the prose guide
                // recommends two. Preserve the wire protocol's lower bound.
                Question::Score { criteria, .. } if !(1..=10).contains(&criteria.len()) => {
                    return Err("score criteria must contain between 1 and 10 levels");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SystemOneResponse {
    /// Optional upstream ID. Required by our self-hosted TEE signing contract.
    /// TypeSafe's hosted API omits it; the gateway exposes X-Signature-Id instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", schema(as = SystemOneAnswer))]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
        legend: BTreeMap<String, Content>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(as = SystemOneUsage))]
pub struct Usage {
    pub input_tokens: i32,
    pub output_tokens: i32,
}

/// Always return raw_bytes to clients: reserialization would invalidate the
/// provider signature and would discard future fields in the upstream response.
#[derive(Debug, Clone)]
pub struct SystemOneResponseWithBytes {
    pub response: SystemOneResponse,
    pub raw_bytes: Vec<u8>,
}

impl SystemOneResponseWithBytes {
    /// Provider IDs become URL path segments and response headers.
    pub fn provider_signature_id(&self) -> Result<&str, CompletionError> {
        self.response
            .id
            .as_deref()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            })
            .ok_or_else(|| {
                CompletionError::InvalidResponse(
                    "TEE System One response requires a valid signature id".into(),
                )
            })
    }

    pub fn parse(raw_bytes: Vec<u8>, request: &SystemOneRequest) -> Result<Self, CompletionError> {
        let invalid = || CompletionError::InvalidResponse("Invalid System One response".into());
        let response: SystemOneResponse =
            serde_json::from_slice(&raw_bytes).map_err(|_| invalid())?;
        let usage = &response.usage;
        if response.model.trim().is_empty()
            || usage.input_tokens < 0
            || usage.output_tokens < 0
            || usage
                .input_tokens
                .checked_add(usage.output_tokens)
                .is_none()
            || response.answers.len() != request.questions.len()
        {
            return Err(invalid());
        }
        let probability = |n: &f64| n.is_finite() && (0.0..=1.0).contains(n);
        for (key, question) in &request.questions {
            let valid = match (question, response.answers.get(key)) {
                (Question::Noul { .. }, Some(Answer::Noul { noul })) => probability(noul),
                (
                    Question::Choice { criteria, .. },
                    Some(Answer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    }),
                ) => {
                    criteria.contains_key(choice)
                        && criteria.keys().eq(probabilities.keys())
                        && probability(confidence)
                        && probabilities.values().all(probability)
                }
                (
                    Question::Score { criteria, .. },
                    Some(Answer::Score {
                        score,
                        confidence,
                        probabilities,
                        legend,
                    }),
                ) => {
                    score.is_finite()
                        && *score >= 0.0
                        && *score <= criteria.len().saturating_sub(1) as f64
                        && probability(confidence)
                        && probabilities.len() == criteria.len()
                        && legend.len() == criteria.len()
                        && (0..criteria.len()).all(|i| {
                            let key = i.to_string();
                            legend.contains_key(&key)
                                && probabilities.get(&key).is_some_and(probability)
                        })
                }
                _ => false,
            };
            if !valid {
                return Err(invalid());
            }
        }
        Ok(Self {
            response,
            raw_bytes,
        })
    }
}

/// Shared HTTP response handling for the TypeSafe and self-hosted transports.
pub(crate) async fn read_response(
    mut response: reqwest::Response,
    request: &SystemOneRequest,
    is_external: bool,
) -> Result<SystemOneResponseWithBytes, CompletionError> {
    if !response.status().is_success() {
        return Err(CompletionError::HttpError {
            status_code: response.status().as_u16(),
            // Do not retain upstream error bodies: validation errors can echo state.
            message: "System One provider rejected the request".into(),
            is_external,
        });
    }
    let mut raw_bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        CompletionError::InvalidResponse("Failed to read System One response".into())
    })? {
        if raw_bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(CompletionError::InvalidResponse(
                "System One response is too large".into(),
            ));
        }
        raw_bytes.extend_from_slice(&chunk);
    }
    SystemOneResponseWithBytes::parse(raw_bytes, request)
}

pub(crate) fn transport_error(error: reqwest::Error, timeout_seconds: u64) -> CompletionError {
    if error.is_timeout() {
        CompletionError::Timeout {
            operation: "systemone".into(),
            timeout_seconds,
        }
    } else {
        CompletionError::CompletionError("System One connection error".into())
    }
}

#[cfg(test)]
mod tests;
