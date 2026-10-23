use inference_providers::{InferenceProvider, StreamingResult};
use std::sync::Arc;

pub struct AttributedChatCompletion {
    pub response: inference_providers::ChatCompletionResponseWithBytes,
    pub provider_attribution: crate::usage::ProviderAttribution,
}

pub struct AttributedChatCompletionStream {
    pub stream: StreamingResult,
    pub provider_attribution: crate::usage::ProviderAttribution,
    /// Callback to report observed TTFT back to the pool for latency-aware
    /// routing (see [`super::ProviderLatencyReporter`]). Invoked once by the
    /// caller's `InterceptStream` on drop with the backend TTFT.
    pub latency_reporter: super::ProviderLatencyReporter,
}

pub struct AttributedImageGeneration {
    pub response: inference_providers::ImageGenerationResponseWithBytes,
    pub provider_attribution: crate::usage::ProviderAttribution,
}

pub struct AttributedImageEdit {
    pub response: inference_providers::ImageEditResponseWithBytes,
    pub provider_attribution: crate::usage::ProviderAttribution,
}

pub struct AttributedSystemOne {
    pub response: inference_providers::SystemOneResponseWithBytes,
    pub provider_attribution: crate::usage::ProviderAttribution,
    pub decision_id: String,
    pub signature_kind: crate::attestation::SignatureKind,
}

pub struct AttributedAnthropicRawResponse {
    pub response: inference_providers::AnthropicRawResponse,
    pub provider_attribution: crate::usage::ProviderAttribution,
}

pub(super) struct ServedProviderResult<T> {
    pub(super) value: T,
    pub(super) provider: Arc<dyn InferenceProvider + Send + Sync>,
    pub(super) provider_attribution: crate::usage::ProviderAttribution,
}

pub(super) fn served_provider_attribution(
    provider: &(dyn InferenceProvider + Send + Sync),
    served_via_fallback: bool,
) -> crate::usage::ProviderAttribution {
    crate::usage::ProviderAttribution {
        served_provider_tier: Some(provider.tier().into()),
        served_provider_type: Some(provider.provider_source().into()),
        served_via_fallback,
    }
}
