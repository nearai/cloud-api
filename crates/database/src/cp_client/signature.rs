//! Chat-signature reads, served by cloud-platform's
//! `GET /v1/internal/chat-signatures/{chat_id}` when the client is configured.
//!
//! A miss is a 200 whose `signature` is null. That maps to the same
//! `SignatureNotFound` the local repository returns, so the provider-pool
//! fallback in the attestation service runs unchanged. Signature reads are not
//! cached.
//!
//! This relies on the shared database. cloud-api writes `chat_signatures`
//! locally, and cloud-platform reads the same table (cloud-platform owns it;
//! see `crates/schema/ownership.toml` in cloud-platform). A signature cloud-api
//! stores is therefore visible to cloud-platform's read. Do not configure the
//! client if the two services are ever given separate databases, unless the
//! writes also go to cloud-platform (`PUT /v1/internal/chat-signatures`).

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use services::attestation::models::{AttestationError, ChatSignature, SignatureKind};
use services::attestation::ports::AttestationRepository;

use super::client::{CallType, CpClient, CpError, Credential};

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

/// The chat-signature read adapter. Writes and the other `AttestationRepository`
/// methods are the local repository's.
pub struct ChatSignatureReads {
    local: Arc<dyn AttestationRepository>,
    client: Arc<CpClient>,
}

impl ChatSignatureReads {
    pub fn new(local: Arc<dyn AttestationRepository>, client: Arc<CpClient>) -> Self {
        Self { local, client }
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

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::*;
    use serde_json::json;
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

    fn reads(server: &MockServer, local: Option<ChatSignature>) -> ChatSignatureReads {
        ChatSignatureReads::new(
            Arc::new(FakeLocal { stored: local }),
            Arc::new(client(server)),
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
    async fn on_maps_a_found_row() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/internal/chat-signatures/chatcmpl-1"))
            .and(query_param("signing_algo", "ecdsa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(found_body()))
            .expect(1)
            .mount(&server)
            .await;
        let signature = reads(&server, None)
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
        let error = reads(&server, None)
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
        let signature = reads(&server, None)
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
        let adapter = reads(&server, None);
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
        let error = reads(&server, None)
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap_err();
        assert!(matches!(error, AttestationError::RepositoryError(_)));
    }
}
