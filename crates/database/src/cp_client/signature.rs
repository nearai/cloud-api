//! Chat-signature reads, served by cloud-platform's
//! `GET /v1/internal/chat-signatures/{chat_id}` when the client is configured.
//!
//! A stored signature can be read by any API key, whichever workspace stored
//! it, so that a response can be verified cryptographically by anyone holding
//! its id. Signatures stored before owners were recorded are readable too.
//! cloud-platform returns stored signatures the same way, and the local
//! repository read is not filtered by owner either.
//!
//! The read is sent with the caller's own key ([`with_caller_credential`]),
//! never the service key. When no caller key is in scope, the local read
//! answers instead.
//!
//! A miss is a 200 whose `signature` is null. That maps to the same
//! `SignatureNotFound` the local repository returns; the attestation service
//! then runs its own caller-scoped disconnect check. Signature reads are not
//! cached. When cloud-platform cannot answer (any client error), the read
//! falls back to the local repository, which reads the same table.
//!
//! This relies on the shared database. cloud-api writes `chat_signatures`
//! locally, and cloud-platform reads the same table (cloud-platform owns it;
//! see `crates/schema/ownership.toml` in cloud-platform). A signature cloud-api
//! stores is therefore visible to cloud-platform's read. Do not configure the
//! client if the two services are ever given separate databases, unless the
//! writes also go to cloud-platform (`PUT /v1/internal/chat-signatures`).
//!
//! [`with_caller_credential`]: super::client::with_caller_credential

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use services::attestation::models::{
    AttestationError, ChatSignature, SignatureKind, SignatureOwner,
};
use services::attestation::ports::AttestationRepository;

use super::client::{current_caller_credential, CallType, CpClient, CpError, Credential};

/// `GET /v1/internal/chat-signatures/{chat_id}` body. `stop_reason` and any
/// other field are not read: a miss is reported as not found, with no reason.
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

    /// The stored signature, or `None` for a miss.
    async fn remote(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<Option<ChatSignature>, CpError> {
        let token = current_caller_credential().ok_or(CpError::NoCallerCredential)?;
        let response = self
            .client
            .send(
                CallType::SignatureRead,
                Method::GET,
                &["v1", "internal", "chat-signatures", chat_id],
                &[("signing_algo", signing_algo)],
                Credential::Caller(&token),
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
        owner: SignatureOwner,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        self.local
            .add_chat_signature(owner, chat_id, signature)
            .await
    }

    async fn add_chat_signatures(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        self.local
            .add_chat_signatures(owner, chat_id, signatures)
            .await
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        match self.remote(chat_id, signing_algo).await {
            Ok(Some(signature)) => Ok(signature),
            Ok(None) => Err(not_found(chat_id, signing_algo)),
            // Both services read the same table, so an outage falls back to
            // the local read instead of failing the lookup.
            Err(error) => {
                tracing::warn!(
                    error_kind = error.kind(),
                    "cloud-platform signature read failed; reading locally"
                );
                self.local.get_chat_signature(chat_id, signing_algo).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::tests::client;
    use super::super::client::with_caller_credential;
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{any, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CALLER_KEY: &str = "sk-caller";

    /// A local repository that holds at most one signature, readable by any
    /// caller, like the database read.
    struct FakeLocal {
        stored: Option<ChatSignature>,
    }

    #[async_trait]
    impl AttestationRepository for FakeLocal {
        async fn add_chat_signature(
            &self,
            _: SignatureOwner,
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

    /// A read made inside an API-key request, with the caller's key in scope.
    async fn read(
        adapter: &ChatSignatureReads,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        with_caller_credential(
            CALLER_KEY.to_string(),
            adapter.get_chat_signature(chat_id, signing_algo),
        )
        .await
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
            .and(header("authorization", "Bearer sk-caller"))
            .respond_with(ResponseTemplate::new(200).set_body_json(found_body()))
            .expect(1)
            .mount(&server)
            .await;
        let signature = read(&reads(&server, None), "chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature_kind, Some(SignatureKind::Gateway));
        assert_eq!(signature.signing_address, "0xaddr");
    }

    #[tokio::test]
    async fn a_null_signature_is_the_local_miss() {
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
        let error = read(&reads(&server, None), "chatcmpl-9", "ecdsa")
            .await
            .unwrap_err();
        assert!(matches!(error, AttestationError::SignatureNotFound(_)));
    }

    /// A found answer is used whatever else the body says, including an
    /// `owner_scoped` flag of either value: stored signatures are readable by
    /// any caller, so the flag gates nothing.
    #[tokio::test]
    async fn a_found_answer_is_used_whatever_its_scope_flag() {
        for owner_scoped in [None, Some(false), Some(true)] {
            let server = MockServer::start().await;
            let mut body = found_body();
            if let Some(flag) = owner_scoped {
                body["owner_scoped"] = json!(flag);
            }
            Mock::given(any())
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let signature = read(&reads(&server, Some(local_row())), "chatcmpl-1", "ecdsa")
                .await
                .unwrap();
            assert_eq!(
                signature.signature, "0xsig",
                "owner_scoped = {owner_scoped:?}"
            );
        }
    }

    #[tokio::test]
    async fn without_the_caller_key_cloud_platform_is_not_asked() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(found_body()))
            .expect(0)
            .mount(&server)
            .await;
        let adapter = reads(&server, Some(local_row()));
        let signature = adapter
            .get_chat_signature("chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature, "0xlocal");
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
        let signature = read(&reads(&server, None), "chatcmpl-1", "ecdsa")
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
        read(&adapter, "chatcmpl-1", "ecdsa").await.unwrap();
        read(&adapter, "chatcmpl-1", "ecdsa").await.unwrap();
    }

    fn local_row() -> ChatSignature {
        ChatSignature {
            text: "local:text".to_string(),
            signature: "0xlocal".to_string(),
            signing_address: "0xlocaladdr".to_string(),
            signing_algo: "ecdsa".to_string(),
            signature_kind: Some(SignatureKind::ProviderTee),
        }
    }

    #[tokio::test]
    async fn an_unavailable_cloud_platform_falls_back_to_the_local_row() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let signature = read(&reads(&server, Some(local_row())), "chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature, "0xlocal");
    }

    #[tokio::test]
    async fn a_timed_out_read_falls_back_to_the_local_row() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(found_body())
                    .set_delay(std::time::Duration::from_millis(1500)),
            )
            .mount(&server)
            .await;
        let signature = read(&reads(&server, Some(local_row())), "chatcmpl-1", "ecdsa")
            .await
            .unwrap();
        assert_eq!(signature.signature, "0xlocal");
    }

    #[tokio::test]
    async fn an_outage_with_no_local_row_is_the_local_miss() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let error = read(&reads(&server, None), "chatcmpl-1", "ecdsa")
            .await
            .unwrap_err();
        assert!(matches!(error, AttestationError::SignatureNotFound(_)));
    }
}
