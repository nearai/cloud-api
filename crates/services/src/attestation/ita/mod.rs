pub mod client;
pub(in crate::attestation) mod client_error_mapping;
pub mod evidence;
pub mod models;
pub(in crate::attestation) mod service;

pub use client::{ItaClient, ItaClientError};
pub(in crate::attestation) use client_error_mapping::ita_client_error_class;
pub use evidence::{
    build_gateway_attest_request, build_gateway_runtime_data, build_model_attest_request,
    build_tdx_report_data, derive_gpu_nonce, ItaEvidenceError, ItaGatewayEvidenceInput,
    ItaGatewayRuntimeDataInput, ItaModelEvidenceInput,
};
pub use models::{
    ItaAttestRequest, ItaAttestResponse, ItaAttestationToken, ItaAttestationType,
    ItaGatewaySigningAlg, ItaModelAliasResolved, ItaModelToken, ItaNonceResponse, ItaNvgpuEvidence,
    ItaNvgpuEvidenceItem, ItaTdxEvidence, ItaTokenQuery, ItaTokenResponse, ItaTokenType,
    ItaVerifierNonce, ItaVerifierNonceDecodeError,
};
pub(in crate::attestation) use service::ProviderPoolModelAttestationCollector;
pub use service::{ModelAttestationCollector, ModelAttestationInput};

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
