use api::routes::attestation::{AttestationQuery, SignatureQuery, VerifyRequest};

// Tests focus on data structure validation and serialization since
// endpoint functions require real VLLM infrastructure

#[tokio::test]
async fn test_attestation_data_structures() {
    // Test that all the data structures serialize/deserialize properly

    let signature_query = SignatureQuery {
        model: Some("gpt-4".to_string()),
        signing_algo: Some("ecdsa".to_string()),
    };

    // Test serialization
    let serialized = serde_json::to_string(&signature_query).unwrap();
    assert!(serialized.contains("gpt-4"));
    assert!(serialized.contains("ecdsa"));

    let attestation_query = AttestationQuery {
        model: Some("test-model".to_string()),
    };

    let serialized = serde_json::to_string(&attestation_query).unwrap();
    assert!(serialized.contains("test-model"));

    let verify_request = VerifyRequest {
        request_hash: Some("abc123".to_string()),
    };

    let serialized = serde_json::to_string(&verify_request).unwrap();
    assert!(serialized.contains("abc123"));
}

/// Note: Integration tests with real VLLM proxy would require:
/// 1. A running VLLM proxy with attestation support
/// 2. Environment variables set for VLLM_BASE_URL and VLLM_API_KEY  
/// 3. A valid chat completion ID from a previous request
/// 4. TEE environment (Intel TDX + NVIDIA GPU attestation)
///
/// These tests focus on data structure validation instead.

/// Test OpenAPI schema compatibility
#[test]
fn test_openapi_schemas() {
    // Test that all our types can be used in OpenAPI schemas
    // This is a compile-time test - if it compiles, the schemas are valid

    use api::routes::attestation::*;
    use utoipa::OpenApi;

    #[derive(OpenApi)]
    #[openapi(components(schemas(
        SignatureResponse,
        AttestationResponse,
        VerifyRequest,
        VerifyResponse,
        Evidence,
        NvidiaPayload,
        Attestation,
    )))]
    struct TestApiDoc;

    let doc = TestApiDoc::openapi();
    assert!(doc.components.is_some());

    let components = doc.components.unwrap();
    assert!(components.schemas.contains_key("SignatureResponse"));
    assert!(components.schemas.contains_key("AttestationResponse"));
    assert!(components.schemas.contains_key("VerifyRequest"));
    assert!(components.schemas.contains_key("VerifyResponse"));
}
