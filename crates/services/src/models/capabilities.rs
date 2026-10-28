//! Endpoint compatibility for protocols that require a dedicated HTTP API.
use super::ModelWithPricing;

#[derive(Clone, Copy)]
pub enum InferenceEndpoint {
    ChatCompletions,
    Responses,
    SystemOne,
}

impl ModelWithPricing {
    pub fn has_output_modality(&self, modality: &str) -> bool {
        self.output_modalities
            .as_ref()
            .is_some_and(|modalities| modalities.iter().any(|value| value == modality))
    }

    /// External model served through the Anthropic Messages backend.
    pub fn is_anthropic_backend(&self) -> bool {
        self.provider_type == "external"
            && self
                .provider_config
                .as_ref()
                .and_then(|config| config.get("backend"))
                .and_then(serde_json::Value::as_str)
                == Some("anthropic")
    }

    /// Check an already resolved model; never perform another catalog lookup.
    /// Image/audio output alone does not imply a dedicated protocol, and legacy
    /// models with missing modality metadata retain their existing behavior.
    pub fn validate_endpoint(&self, endpoint: InferenceEndpoint) -> Result<(), &'static str> {
        let decisions = self.has_output_modality(inference_providers::systemone::OUTPUT_MODALITY);
        match endpoint {
            InferenceEndpoint::SystemOne if !decisions => {
                Err("This model does not support the decisions output modality")
            }
            InferenceEndpoint::ChatCompletions | InferenceEndpoint::Responses if decisions => {
                Err("Decision models require /v1/systemone")
            }
            _ => Ok(()),
        }
    }
}
