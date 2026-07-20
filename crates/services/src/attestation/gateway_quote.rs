use async_trait::async_trait;
use dstack_sdk::dstack_client;

use super::{models::DstackCpuQuote, AttestationError, VpcInfo};

#[derive(Debug)]
pub struct GatewayQuoteInput {
    pub signing_address: String,
    pub signing_algo: String,
    pub report_data: Vec<u8>,
    pub request_nonce: String,
    pub vpc: Option<VpcInfo>,
    pub tls_cert_fingerprint: Option<String>,
}

#[async_trait]
pub trait GatewayQuoteCollector: Send + Sync {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError>;
}

pub struct DstackGatewayQuoteCollector;

#[async_trait]
impl GatewayQuoteCollector for DstackGatewayQuoteCollector {
    async fn collect_gateway_quote(
        &self,
        input: GatewayQuoteInput,
    ) -> Result<DstackCpuQuote, AttestationError> {
        #[cfg(debug_assertions)]
        {
            if std::env::var("DEV").is_ok() {
                return Ok(DstackCpuQuote {
                    signing_address: input.signing_address,
                    signing_algo: input.signing_algo,
                    intel_quote: "0x1234567890abcdef".to_string(),
                    event_log: "[]".to_string(),
                    report_data: hex::encode(input.report_data),
                    request_nonce: input.request_nonce,
                    info: serde_json::json!({
                        "app_id": "dev-app-id",
                        "instance_id": "dev-instance-id",
                        "app_cert": "dev-app-cert",
                        "tcb_info": {},
                        "app_name": "dev-app-name",
                        "device_id": "dev-device-id",
                        "mr_aggregated": "dev-mr-aggregated",
                        "os_image_hash": "dev-os-image-id",
                        "key_provider_info": "dev-key-provider-info",
                        "compose_hash": "dev-compose-hash",
                        "vm_config": {},
                    }),
                    vpc: input.vpc,
                    tls_cert_fingerprint: input.tls_cert_fingerprint,
                });
            }
        }

        let client = dstack_client::DstackClient::new(None);
        let info = client.info().await.map_err(|e| {
            tracing::error!(
                "Failed to get cloud API attestation info, are you running in a CVM?: {e:?}"
            );
            AttestationError::InternalError("failed to get cloud API attestation info".to_string())
        })?;

        let cpu_quote = client.get_quote(input.report_data).await.map_err(|e| {
            tracing::error!(
                "Failed to get cloud API attestation, are you running in a CVM?: {:?}",
                e
            );
            AttestationError::InternalError("failed to get cloud API attestation".to_string())
        })?;

        Ok(DstackCpuQuote::from_quote_and_nonce(
            input.signing_address,
            input.signing_algo,
            input.vpc,
            info,
            cpu_quote,
            input.request_nonce,
            input.tls_cert_fingerprint,
        ))
    }
}
