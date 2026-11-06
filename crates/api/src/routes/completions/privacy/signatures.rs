//! Gateway receipts over the original request and final Privacy response bytes.
use super::*;

pub(super) struct PrivacyIdentity {
    pub response_id: Option<String>,
    pub inference_id: Uuid,
}

impl PrivacyIdentity {
    pub fn from_response(bytes: &[u8]) -> Self {
        #[derive(serde::Deserialize)]
        struct Identity {
            id: Option<String>,
        }
        let response_id = serde_json::from_slice::<Identity>(bytes)
            .ok()
            .and_then(|value| value.id)
            // Usage stores provider_request_id in VARCHAR(255). Do not let
            // malformed metadata break otherwise usable classification data.
            .filter(|id| !id.is_empty() && id.len() <= 255 && !id.chars().any(char::is_control));
        let inference_id = response_id
            .as_deref()
            .map(hash_inference_id_to_uuid)
            .unwrap_or_else(Uuid::new_v4);
        Self {
            response_id,
            inference_id,
        }
    }
}

pub(super) struct PrivacyResponse {
    pub identity: PrivacyIdentity,
    pub request_hash: String,
    pub body: Bytes,
}

/// After inference has completed, persist usage and the receipt independently
/// of the caller's lifetime. Privacy does not pin a provider for signature
/// fetching: even a TEE classifier currently exposes no per-response signing
/// contract here, and redact signs bytes rewritten by this gateway.
pub(super) async fn finalize(
    state: AppState,
    api_key: AuthenticatedApiKey,
    model_id: Uuid,
    model_name: String,
    endpoint: PrivacyEndpoint,
    tokens: i64,
    served: PrivacyResponse,
) -> Response {
    tokio::spawn(async move {
        if let Err(response) = settle_privacy_usage(
            &state,
            &api_key,
            model_id,
            &model_name,
            endpoint,
            tokens,
            &served.identity,
        )
        .await
        {
            return response;
        }
        if let Some(id) = served.identity.response_id.as_deref() {
            let stored = tokio::time::timeout(
                SIGNATURE_STORE_TIMEOUT,
                state.attestation_service.store_chat_signature(
                    id,
                    served.request_hash,
                    hex::encode(Sha256::digest(&served.body)),
                ),
            )
            .await;
            if !matches!(stored, Ok(Ok(()))) {
                // Never include upstream response content or error payloads.
                // A receipt failure must not trigger another inference call.
                tracing::error!(
                    endpoint = endpoint.label(),
                    "Privacy gateway signature storage failed or timed out"
                );
            }
        } else {
            tracing::warn!(
                endpoint = endpoint.label(),
                "Privacy response has no valid ID; gateway receipt unavailable"
            );
        }
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                HEADER_INFERENCE_ID,
                served.identity.inference_id.to_string(),
            )
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, HEADER_INFERENCE_ID)
            .body(Body::from(served.body))
            .unwrap_or_else(|_| finalization_error())
    })
    .await
    .unwrap_or_else(|_| finalization_error())
}

fn finalization_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            "Failed to finalize privacy response".to_string(),
            "server_error".to_string(),
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_preserves_upstream_id_and_derives_billing_id() {
        let id = "privacy-upstream-123";
        let identity =
            PrivacyIdentity::from_response(br#"{ "data":[], "id":"privacy-upstream-123" }"#);
        assert_eq!(identity.response_id.as_deref(), Some(id));
        assert_eq!(identity.inference_id, hash_inference_id_to_uuid(id));
    }

    #[test]
    fn invalid_ids_do_not_become_receipt_keys_or_break_billing() {
        for body in [
            b"{}".to_vec(),
            b"not json".to_vec(),
            br#"{"id":null}"#.to_vec(),
            br#"{"id":123}"#.to_vec(),
            br#"{"id":""}"#.to_vec(),
            br#"{"id":"bad\nvalue"}"#.to_vec(),
            br#"{"id":"first","id":"second"}"#.to_vec(),
            serde_json::to_vec(&serde_json::json!({"id":"x".repeat(256)})).unwrap(),
        ] {
            let identity = PrivacyIdentity::from_response(&body);
            assert!(identity.response_id.is_none());
            assert_eq!(identity.inference_id.get_version_num(), 4);
        }
    }
}
