use inference_providers::{InferenceProvider, ProviderSource, ProviderTier, StreamingResult};
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
        served_provider_tier: Some(served_provider_tier(provider.tier())),
        served_provider_type: Some(served_provider_type(provider.provider_source())),
        served_via_fallback,
    }
}

fn served_provider_tier(tier: ProviderTier) -> crate::usage::ServedProviderTier {
    match tier {
        ProviderTier::Near => crate::usage::ServedProviderTier::Near,
        ProviderTier::Attested3p => crate::usage::ServedProviderTier::Attested3p,
        ProviderTier::NonAttested => crate::usage::ServedProviderTier::NonAttested,
    }
}

fn served_provider_type(source: ProviderSource) -> crate::usage::ServedProviderType {
    match source {
        ProviderSource::Vllm => crate::usage::ServedProviderType::Vllm,
        ProviderSource::External => crate::usage::ServedProviderType::External,
        ProviderSource::Chutes => crate::usage::ServedProviderType::Chutes,
    }
}
