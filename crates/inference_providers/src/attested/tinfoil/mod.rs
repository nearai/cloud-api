//! Tinfoil — attested backup provider (`ProviderTier::Attested3p`).
//!
//! Tinfoil's router enclave (SEV-SNP) is the trust anchor: the session verifies
//! its attestation through the [`verifier_port::TinfoilVerifier`] seam, pins the
//! TLS key bound in that attestation, and only then talks to it. Per-model
//! measurements are the router-enforced registers it publishes, checked against
//! pins (`trust: "router_attested"`). Chat completions only.

mod availability;
mod config;
mod session;
#[cfg(test)]
mod tests;
pub mod verifier_port;
mod wire;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

pub use self::availability::{map_upstream_status, unavailable, UpstreamDisposition};
pub use self::config::{Config, ATC_URL, BASE_URL, PROXY_REREAD, ROUTER_REVERIFY};
pub use self::session::{TinfoilRouterSession, VerifiedState};
use crate::attested::chutes::request_body;
use crate::{
    AttestationError, AudioTranscriptionError, AudioTranscriptionParams,
    AudioTranscriptionResponse, ChatCompletionParams, ChatCompletionResponseWithBytes,
    ChatSignature, CompletionError, CompletionParams, EmbeddingError, ImageEditError,
    ImageEditParams, ImageEditResponseWithBytes, ImageGenerationError, ImageGenerationParams,
    ImageGenerationResponseWithBytes, InferenceProvider, ListModelsError, ModelInfo,
    ModelsResponse, PrivacyClassifyError, RerankError, RerankParams, RerankResponse, ScoreError,
    ScoreParams, ScoreResponse, StreamingResult,
};

const CHAT_PATH: &str = "/v1/chat/completions";
const UNSUPPORTED: &str =
    "operation not supported by the attested Tinfoil provider (chat completions only)";

/// One provider per canonical model; all share one [`TinfoilRouterSession`].
pub struct Provider {
    session: Arc<TinfoilRouterSession>,
    slug: String,
    canonical_id: String,
    api_key: String,
    timeout: std::time::Duration,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("slug", &self.slug)
            .field("canonical_id", &self.canonical_id)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Provider {
    pub fn new(
        session: Arc<TinfoilRouterSession>,
        cfg: &Config,
        slug: String,
        canonical_id: String,
    ) -> Self {
        Self {
            session,
            slug,
            canonical_id,
            api_key: cfg.api_key().to_string(),
            timeout: cfg.timeout,
        }
    }

    /// Closed unless the router is verified and this slug is pinned.
    fn ensure_available(&self) -> Result<(), CompletionError> {
        let snap = self.session.snapshot();
        let Some(state) = &*snap else {
            return Err(unavailable("not_verified"));
        };
        match state.models.get(&self.slug) {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(unavailable(e.reason())),
            None => Err(unavailable("model_not_published")),
        }
    }

    fn reject_client_e2ee(params: &ChatCompletionParams) -> Result<(), CompletionError> {
        use crate::attested::nearai::encryption_headers as eh;
        if params.extra.contains_key(eh::CLIENT_PUB_KEY) {
            return Err(CompletionError::CompletionError(
                "client-facing E2EE is not supported on the attested Tinfoil path".to_string(),
            ));
        }
        Ok(())
    }

    /// Map a non-success upstream status per spec §3.5. Never logs the body.
    fn status_error(&self, status: u16, body: &str) -> CompletionError {
        match map_upstream_status(status) {
            UpstreamDisposition::Retryable503 => {
                if matches!(status, 401..=403) {
                    self.session.record_auth_failure();
                    tracing::error!(
                        status,
                        "Tinfoil upstream rejected our credentials or billing"
                    );
                    return unavailable("upstream_auth");
                }
                tracing::warn!(status, "Tinfoil upstream unavailable");
                unavailable("upstream_error")
            }
            UpstreamDisposition::Passthrough429 => CompletionError::HttpError {
                status_code: 429,
                message: crate::extract_error_message(body),
                is_external: true,
            },
            UpstreamDisposition::ReturnAs4xx(code) => CompletionError::HttpError {
                status_code: code,
                message: crate::extract_error_message(body),
                is_external: true,
            },
        }
    }

    async fn post_chat(
        &self,
        mut params: ChatCompletionParams,
        stream: bool,
    ) -> Result<reqwest::Response, CompletionError> {
        Self::reject_client_e2ee(&params)?;
        self.ensure_available()?;
        crate::strip_cache_control(&mut params.messages);
        crate::strip_reasoning_content(&mut params.messages);
        let body =
            request_body(&self.slug, &params, stream).map_err(CompletionError::CompletionError)?;

        let generation = self.session.generation();
        let client = self.session.client();
        let send = client
            .post(format!("{}{CHAT_PATH}", self.session.base_url()))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let resp = match tokio::time::timeout(self.timeout, send).await {
            Err(_) => {
                tracing::warn!("Tinfoil request timed out");
                return Err(unavailable("timeout"));
            }
            Ok(Err(e)) => {
                if e.is_connect() {
                    // Includes TLS failures such as an SPKI mismatch after a
                    // key rotation: re-verify once, never retry unpinned.
                    tracing::warn!("Tinfoil connection failed; re-verifying router");
                    self.session.on_connect_failure(generation).await;
                    return Err(unavailable("connect_failed"));
                }
                tracing::warn!("Tinfoil transport error");
                return Err(unavailable("transport"));
            }
            Ok(Ok(r)) => r,
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.status_error(status.as_u16(), &text));
        }
        Ok(resp)
    }
}

#[async_trait]
impl InferenceProvider for Provider {
    async fn models(&self) -> Result<ModelsResponse, ListModelsError> {
        Ok(ModelsResponse {
            object: "list".to_string(),
            data: vec![ModelInfo {
                created: 0,
                id: self.canonical_id.clone(),
                object: "model".to_string(),
                owned_by: "tinfoil".to_string(),
                context_length: None,
                max_model_len: None,
                max_output_length: None,
                top_provider: None,
            }],
        })
    }

    async fn chat_completion(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<ChatCompletionResponseWithBytes, CompletionError> {
        let resp = self.post_chat(params, false).await?;
        let bytes = tokio::time::timeout(self.timeout, resp.bytes())
            .await
            .map_err(|_| unavailable("timeout"))?
            .map_err(|_| unavailable("transport"))?;
        let (raw_bytes, response) = wire::map_response(&bytes, &self.canonical_id)?;
        Ok(ChatCompletionResponseWithBytes {
            response,
            raw_bytes,
            serving: crate::ServingProvider::of(self),
        })
    }

    async fn chat_completion_stream(
        &self,
        params: ChatCompletionParams,
        _request_hash: String,
    ) -> Result<StreamingResult, CompletionError> {
        // Same contract as Chutes: usage is always requested upstream so billing
        // sees it; only what reaches the client follows the client's request.
        let client_wants_usage = params.stream_options.as_ref().is_some_and(|so| {
            so.include_usage == Some(true) || so.continuous_usage_stats == Some(true)
        });
        let resp = self.post_chat(params, true).await?;
        let sse = crate::sse_parser::new_external_sse_parser(resp.bytes_stream(), true);
        Ok(wire::map_stream(
            Box::pin(sse),
            self.canonical_id.clone(),
            client_wants_usage,
        ))
    }

    async fn text_completion_stream(
        &self,
        _params: CompletionParams,
    ) -> Result<StreamingResult, CompletionError> {
        Err(CompletionError::CompletionError(UNSUPPORTED.to_string()))
    }

    async fn image_generation(
        &self,
        _params: ImageGenerationParams,
        _request_hash: String,
    ) -> Result<ImageGenerationResponseWithBytes, ImageGenerationError> {
        Err(ImageGenerationError::GenerationError(
            UNSUPPORTED.to_string(),
        ))
    }

    async fn image_edit(
        &self,
        _params: Arc<ImageEditParams>,
        _request_hash: String,
    ) -> Result<ImageEditResponseWithBytes, ImageEditError> {
        Err(ImageEditError::EditError(UNSUPPORTED.to_string()))
    }

    async fn audio_transcription(
        &self,
        _params: AudioTranscriptionParams,
        _request_hash: String,
    ) -> Result<AudioTranscriptionResponse, AudioTranscriptionError> {
        Err(AudioTranscriptionError::TranscriptionError(
            UNSUPPORTED.to_string(),
        ))
    }

    async fn score(
        &self,
        _params: ScoreParams,
        _request_hash: String,
    ) -> Result<ScoreResponse, ScoreError> {
        Err(ScoreError::GenerationError(UNSUPPORTED.to_string()))
    }

    async fn rerank(&self, _params: RerankParams) -> Result<RerankResponse, RerankError> {
        Err(RerankError::GenerationError(UNSUPPORTED.to_string()))
    }

    async fn embeddings_raw(
        &self,
        _body: bytes::Bytes,
        _extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, EmbeddingError> {
        Err(EmbeddingError::RequestFailed(UNSUPPORTED.to_string()))
    }

    async fn privacy_classify_raw(
        &self,
        _body: bytes::Bytes,
        _extra: HashMap<String, Value>,
    ) -> Result<bytes::Bytes, PrivacyClassifyError> {
        Err(PrivacyClassifyError::RequestFailed(UNSUPPORTED.to_string()))
    }

    /// Spec §3.6 payload. Tinfoil's report binds the router's TLS key, not a
    /// caller nonce, so the nonce is deliberately ignored here: a client nonce
    /// binds only through the gateway quote returned alongside.
    async fn get_attestation_report(
        &self,
        _model: String,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
        _include_tls_fingerprint: bool,
    ) -> Result<serde_json::Map<String, Value>, AttestationError> {
        let snap = self.session.snapshot();
        let state = (*snap)
            .as_ref()
            .ok_or_else(|| AttestationError::FetchError("Tinfoil router not verified".into()))?;
        let pinned = match state.models.get(&self.slug) {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                return Err(AttestationError::FetchError(format!(
                    "Tinfoil model not pinned ({})",
                    e.reason()
                )))
            }
            None => {
                return Err(AttestationError::FetchError(
                    "Tinfoil model not published".into(),
                ))
            }
        };
        let replicas: Vec<Value> = pinned
            .entry
            .enclaves
            .iter()
            .map(|(host, e)| json!({"host": host, "tls_key_fp": e.tls_key_fp, "predicate": e.predicate}))
            .collect();
        let mut m = serde_json::Map::new();
        m.insert("provider".into(), json!("tinfoil"));
        m.insert("model".into(), json!(self.canonical_id));
        m.insert("verified".into(), json!(true));
        m.insert("trust".into(), json!("router_attested"));
        m.insert(
            "router".into(),
            json!({
                "format": state.bundle.report.format,
                "report_b64": state.bundle.report.body,
                "vcek_b64": state.bundle.vcek,
                "cert_b64": state.bundle.enclave_cert,
                "measurement": state.router.measurement_hex,
                "tag": state.router.tag,
                "spki_sha256": hex::encode(state.router.spki_sha256),
            }),
        );
        m.insert(
            "model_entry".into(),
            json!({
                "slug": pinned.slug,
                "repo": pinned.repo,
                "tag": pinned.tag,
                "registers": pinned.entry.measurement.registers,
                "replicas": replicas,
            }),
        );
        Ok(m)
    }

    /// The gateway signs; Tinfoil provides no per-response signature.
    fn supports_chat_signatures(&self) -> bool {
        false
    }

    fn tier(&self) -> crate::ProviderTier {
        crate::ProviderTier::Attested3p
    }

    fn provider_source(&self) -> crate::ProviderSource {
        crate::ProviderSource::Tinfoil
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_client_e2ee(&self) -> bool {
        false
    }

    fn supports_per_request_pubkey_routing(&self, _public_key: &str) -> bool {
        false
    }

    async fn get_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<ChatSignature, CompletionError> {
        Err(CompletionError::CompletionError(
            "Tinfoil provides no separate response signature (the gateway signs)".to_string(),
        ))
    }
}
