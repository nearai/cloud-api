use async_trait::async_trait;

use super::{
    ita::{ItaTokenQuery, ItaTokenResponse},
    models::AttestationReport,
    ports, AttestationError, AttestationService, SignatureLookupResult,
};
use inference_providers::ProviderTier;

#[async_trait]
impl ports::AttestationServiceTrait for AttestationService {
    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        self.get_chat_signature_impl(chat_id, signing_algo).await
    }

    async fn store_chat_signature_from_provider(
        &self,
        chat_id: &str,
    ) -> Result<(), AttestationError> {
        self.store_chat_signature_from_provider_impl(chat_id).await
    }

    async fn store_chat_signature(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_chat_signature_impl(chat_id, request_hash, response_hash)
            .await
    }

    async fn store_chat_signature_and_unpin(
        &self,
        chat_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_chat_signature_and_unpin_impl(chat_id, request_hash, response_hash)
            .await
    }

    async fn release_chat_signature_pin(&self, chat_id: &str) {
        self.release_chat_signature_pin_impl(chat_id).await
    }

    async fn store_response_signature(
        &self,
        response_id: &str,
        request_hash: String,
        response_hash: String,
    ) -> Result<(), AttestationError> {
        self.store_response_signature_impl(response_id, request_hash, response_hash)
            .await
    }

    async fn get_attestation_report(
        &self,
        model: Option<String>,
        signing_algo: Option<String>,
        nonce: Option<String>,
        signing_address: Option<String>,
        include_tls_fingerprint: bool,
        provider_filter: Option<ProviderTier>,
    ) -> Result<AttestationReport, AttestationError> {
        self.get_attestation_report_impl(
            model,
            signing_algo,
            nonce,
            signing_address,
            include_tls_fingerprint,
            provider_filter,
        )
        .await
    }

    async fn get_ita_attestation_token(
        &self,
        query: ItaTokenQuery,
    ) -> Result<ItaTokenResponse, AttestationError> {
        self.create_ita_attestation_token(query).await
    }

    async fn verify_vpc_signature(
        &self,
        timestamp: i64,
        signature: String,
    ) -> Result<bool, AttestationError> {
        self.verify_vpc_signature_impl(timestamp, signature).await
    }
}
