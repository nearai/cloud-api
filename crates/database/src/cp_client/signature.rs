//! Signature area (`CP_SIGNATURE_READ`): the chat-signature read, served by
//! cloud-platform's `GET /v1/internal/chat-signatures/{chat_id}`.
//!
//! - `off`: the local repository answers, and cloud-platform is never called.
//! - `shadow`: the local answer is served and compared with cloud-platform's;
//!   a difference is counted by field (`presence` for a hit on one side only).
//! - `on`: cloud-platform answers. Signature reads are not cached.
//!
//! A miss is a 200 whose `signature` is null. That maps to the same
//! `SignatureNotFound` the local repository returns, so the provider-pool
//! fallback in the attestation service runs unchanged. Writes always stay
//! local. `on` relies on the shared database: see [`ChatSignatureReads`].

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use services::attestation::models::{AttestationError, ChatSignature, SignatureKind};
use services::attestation::ports::AttestationRepository;
use services::metrics::MetricsServiceTrait;

use super::client::{CallType, CpClient, CpError, Credential, METRIC_CP_SHADOW_MISMATCH};
use super::config::AreaMode;

const AREA_TAG: &str = "area:signature";

/// `GET /v1/internal/chat-signatures/{chat_id}` body. `stop_reason` is not
/// read: the attestation service takes it from usage, as before.
#[derive(Debug, Deserialize)]
struct SignatureLookupDto {
    #[serde(default)]
    signature: Option<SignatureDto>,
}

#[derive(Debug, Deserialize)]
struct SignatureDto {
    text: String,
    signature: String,
    signing_address: String,
    signing_algo: String,
    #[serde(default)]
    signature_kind: Option<WireKind>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireKind {
    ProviderTee,
    Gateway,
    /// A kind added in a later contract: its provenance is unknown, so it is
    /// surfaced as absent rather than guessed.
    #[serde(other)]
    Unknown,
}

impl SignatureDto {
    fn into_signature(self) -> ChatSignature {
        let signature_kind = match self.signature_kind {
            Some(WireKind::ProviderTee) => Some(SignatureKind::ProviderTee),
            Some(WireKind::Gateway) => Some(SignatureKind::Gateway),
            Some(WireKind::Unknown) | None => None,
        };
        ChatSignature {
            text: self.text,
            signature: self.signature,
            signing_address: self.signing_address,
            signing_algo: self.signing_algo,
            signature_kind,
        }
    }
}

/// The chat-signature read adapter. Other `AttestationRepository` methods are
/// the local repository's.
///
/// `on` relies on the shared database. cloud-api writes `chat_signatures`
/// locally, and cloud-platform reads the same table (cloud-platform owns it;
/// see `crates/schema/ownership.toml` in cloud-platform). A signature cloud-api
/// stores is therefore visible to cloud-platform's read. Do not enable `on` if
/// the two services are ever given separate databases, unless the writes also
/// go to cloud-platform (`PUT /v1/internal/chat-signatures`). Shadow reports a
/// gap between the two as `presence` mismatches.
pub struct ChatSignatureReads {
    mode: AreaMode,
    local: Arc<dyn AttestationRepository>,
    client: Arc<CpClient>,
    metrics: Option<Arc<dyn MetricsServiceTrait>>,
}

impl ChatSignatureReads {
    pub fn new(
        mode: AreaMode,
        local: Arc<dyn AttestationRepository>,
        client: Arc<CpClient>,
        metrics: Option<Arc<dyn MetricsServiceTrait>>,
    ) -> Self {
        Self {
            mode,
            local,
            client,
            metrics,
        }
    }

    fn record_mismatch(&self, field: &'static str) {
        if let Some(metrics) = &self.metrics {
            let field_tag = format!("field:{field}");
            metrics.record_count(METRIC_CP_SHADOW_MISMATCH, 1, &[AREA_TAG, &field_tag]);
        }
    }

    async fn remote(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<Option<ChatSignature>, CpError> {
        let response = self
            .client
            .send(
                CallType::SignatureRead,
                Method::GET,
                &["v1", "internal", "chat-signatures", chat_id],
                &[("signing_algo", signing_algo)],
                Credential::Service,
                None,
            )
            .await?;
        if response.status != StatusCode::OK {
            return Err(CpError::Status(response.status));
        }
        let lookup: SignatureLookupDto = response.json()?;
        Ok(lookup.signature.map(SignatureDto::into_signature))
    }

    /// Cloud-platform's answer as the local repository would report it.
    async fn remote_result(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        match self.remote(chat_id, signing_algo).await {
            Ok(Some(signature)) => Ok(signature),
            Ok(None) => Err(not_found(chat_id, signing_algo)),
            Err(error) => Err(AttestationError::RepositoryError(format!(
                "cloud-platform signature read failed: {}",
                error.kind()
            ))),
        }
    }
}

fn not_found(chat_id: &str, signing_algo: &str) -> AttestationError {
    AttestationError::SignatureNotFound(format!("{chat_id}:{signing_algo}"))
}

#[async_trait]
impl AttestationRepository for ChatSignatureReads {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        self.local.add_chat_signature(chat_id, signature).await
    }

    async fn add_chat_signatures(
        &self,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        self.local.add_chat_signatures(chat_id, signatures).await
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        match self.mode {
            AreaMode::Off => self.local.get_chat_signature(chat_id, signing_algo).await,
            AreaMode::Shadow => {
                let served = self.local.get_chat_signature(chat_id, signing_algo).await;
                let remote = self.remote_result(chat_id, signing_algo).await;
                match (&served, &remote) {
                    (Ok(local), Ok(remote)) => {
                        if local.text != remote.text
                            || local.signature != remote.signature
                            || local.signing_address != remote.signing_address
                            || local.signing_algo != remote.signing_algo
                            || local.signature_kind != remote.signature_kind
                        {
                            self.record_mismatch("signature");
                        }
                    }
                    (Ok(_), Err(AttestationError::SignatureNotFound(_)))
                    | (Err(AttestationError::SignatureNotFound(_)), Ok(_)) => {
                        self.record_mismatch("presence");
                    }
                    // A cloud-platform failure is not a difference.
                    _ => {}
                }
                served
            }
            AreaMode::On => self.remote_result(chat_id, signing_algo).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::*;
    use serde_json::json;
    use services::metrics::capturing::CapturingMetricsService;
    use wiremock::matchers::{any, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A local repository that either holds one signature or misses.
    struct FakeLocal {
        stored: Option<ChatSignature>,
    }

    #[async_trait]
    impl AttestationRepository for FakeLocal {
        async fn add_chat_signature(
            &self,
            _: &str,
            _: ChatSignature,
        ) -> Result<(), AttestationError> {
            Ok(())
        }
        async fn get_chat_signature(
            &self,
            chat_id: &str,
            signing_algo: &str,
        ) -> Result<ChatSignature, AttestationError> {
            self.stored
                .clone()
                .ok_or_else(|| not_found(chat_id, signing_algo))
        }
    }

    fn stored() -> ChatSignature {
        ChatSignature {
            text: "a:b".to_string(),
            signature: "0xsig".to_string(),
            signing_address: "0xaddr".to_string(),
            signing_algo: "ecdsa".to_string(),
            signature_kind: Some(SignatureKind::Gateway),
        }
    }

    fn reads(
        server: &MockServer,
        mode: AreaMode,
        local: Option<ChatSignature>,
        metrics: Option<Arc<CapturingMetricsService>>,
    ) -> ChatSignatureReads {
        let metrics: Option<Arc<dyn MetricsServiceTrait>> =
            metrics.map(|m| m as Arc<dyn MetricsServiceTrait>);
        ChatSignatureReads::new(
            mode,
            Arc::new(FakeLocal { stored: local }),
            Arc::new(client(server)),
            metrics,
        )
    }

    fn found_body() -> serde_json::Value {
        json!({
            "signature": {
                "chat_id": "chatcmpl-1",
                "signing_algo": "ecdsa",
                "text": "a:b",
                "signature": "0xsig",
                "signing_address": "0xaddr",
                "signature_kind": "gateway"
            },
            "stop_reason": null
        })
    }

    #[tokio::test]
    async fn off_never_calls_cloud_platform() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let signature = reads(&server, AreaMode::Off, Some(stored()), None)
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature, "0xsig");
    }

    #[tokio::test]
    async fn on_maps_a_found_row() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/chat-signatures/chatcmpl-1"))
            .and(query_param("signing_algo", "ecdsa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(found_body()))
            .expect(1)
            .mount(&server)
            .await;
        let signature = reads(&server, AreaMode::On, None, None)
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature_kind, Some(SignatureKind::Gateway));
        assert_eq!(signature.signing_address, "0xaddr");
    }

    #[tokio::test]
    async fn a_null_signature_is_the_local_miss_so_the_fallback_runs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/chat-signatures/chatcmpl-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "signature": null,
                "stop_reason": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = reads(&server, AreaMode::On, None, None)
            .get_chat_signature("chatcmpl-9", "ecdsa")
            .await
            .unwrap_err();
        assert!(matches!(error, AttestationError::SignatureNotFound(_)));
    }

    #[tokio::test]
    async fn an_unknown_kind_is_surfaced_as_absent() {
        let server = MockServer::start().await;
        let mut body = found_body();
        body["signature"]["signature_kind"] = json!("a_future_kind");
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let signature = reads(&server, AreaMode::On, None, None)
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature_kind, None);
    }

    #[tokio::test]
    async fn reads_are_not_cached() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(found_body()))
            .expect(2)
            .mount(&server)
            .await;
        let adapter = reads(&server, AreaMode::On, None, None);
        adapter
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        adapter
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_cloud_platform_is_a_repository_error_not_a_miss() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let error = reads(&server, AreaMode::On, None, None)
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap_err();
        assert!(matches!(error, AttestationError::RepositoryError(_)));
    }

    #[tokio::test]
    async fn shadow_serves_local_and_counts_a_presence_difference() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"signature": null})))
            .mount(&server)
            .await;
        let metrics = Arc::new(CapturingMetricsService::new());
        let served = reads(
            &server,
            AreaMode::Shadow,
            Some(stored()),
            Some(metrics.clone()),
        )
        .get_chat_signature("chatcmpl-1", "ecdsa")
        .await
        .unwrap();
        assert_eq!(served.signature, "0xsig");
        let fields: Vec<String> = metrics
            .get_metrics()
            .into_iter()
            .filter(|metric| metric.name == METRIC_CP_SHADOW_MISMATCH)
            .flat_map(|metric| metric.tags)
            .filter(|tag| tag.starts_with("field:"))
            .collect();
        assert_eq!(fields, ["field:presence"]);
    }
}
