use super::{
    super::{get_attestation_report, AttestationRouteState},
    ita_token_route_test_support::TestModelsService,
};
use crate::models::ErrorResponse;
use async_trait::async_trait;
use axum::{http::StatusCode, Router};
use services::{
    attestation::{
        ita::{ItaTokenQuery, ItaTokenResponse},
        models::AttestationReport,
        AttestationError, SignatureLookupResult,
    },
    models::ModelCaller,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

const NONCE: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Stands in for evidence collection: counts report builds, and holds each
/// build until the test releases it.
#[derive(Clone, Default)]
struct HeldReportService {
    builds: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl HeldReportService {
    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl services::attestation::ports::AttestationServiceTrait for HeldReportService {
    async fn get_chat_signature(
        &self,
        _chat_id: &str,
        _signing_algo: Option<String>,
    ) -> Result<SignatureLookupResult, AttestationError> {
        Err(AttestationError::InternalError("unused".to_string()))
    }

    async fn store_chat_signature_from_provider(
        &self,
        _chat_id: &str,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn store_chat_signature(
        &self,
        _chat_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn store_response_signature(
        &self,
        _response_id: &str,
        _request_hash: String,
        _response_hash: String,
    ) -> Result<(), AttestationError> {
        Ok(())
    }

    async fn get_attestation_report(
        &self,
        _model: Option<String>,
        _signing_algo: Option<String>,
        _nonce: Option<String>,
        _signing_address: Option<String>,
        _include_tls_fingerprint: bool,
        _provider_filter: Option<inference_providers::ProviderFilter>,
        _caller: ModelCaller,
    ) -> Result<AttestationReport, AttestationError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Err(AttestationError::ProviderError("test report".to_string()))
    }

    async fn get_ita_attestation_token(
        &self,
        _query: ItaTokenQuery,
    ) -> Result<ItaTokenResponse, AttestationError> {
        Err(AttestationError::InternalError("unused".to_string()))
    }

    async fn verify_vpc_signature(
        &self,
        _timestamp: i64,
        _signature: String,
    ) -> Result<bool, AttestationError> {
        Ok(false)
    }
}

/// The report route with no auth layer, so every request is anonymous.
fn anonymous_report_server(service: HeldReportService, permits: usize) -> axum_test::TestServer {
    let state = AttestationRouteState {
        attestation_service: Arc::new(service),
        models_service: Arc::new(TestModelsService::default()),
        ohttp_attestation: None,
        anonymous_reports: Arc::new(Semaphore::new(permits)),
    };
    let app = Router::new()
        .route(
            "/attestation/report",
            axum::routing::get(get_attestation_report),
        )
        .with_state(state);
    axum_test::TestServer::new(app)
}

/// With every anonymous permit taken, any other anonymous request is refused
/// with 429 before evidence is collected: one with a nonce, and one without a
/// nonce whose new `signing_address` would miss the report cache. The permit
/// returns once the held build finishes.
#[tokio::test]
async fn anonymous_requests_past_the_limit_are_refused_before_evidence_collection() {
    let service = HeldReportService::default();
    let server = anonymous_report_server(service.clone(), 1);
    let with_address = |address: &'static str| {
        server
            .get("/attestation/report")
            .add_query_param("signing_address", address)
    };
    let with_nonce = || {
        server
            .get("/attestation/report")
            .add_query_param("nonce", NONCE)
    };

    let (held, (refused, builds_after_refusal)) = tokio::join!(with_address("0x01"), async {
        service.entered.notified().await;
        // Without the limit these requests would wait on the held build.
        let refused = tokio::time::timeout(Duration::from_secs(5), async {
            [with_address("0x02").await, with_nonce().await]
        })
        .await
        .unwrap_or_else(|_| panic!("a request past the limit reached evidence collection"));
        let builds_after_refusal = service.builds();
        service.release.notify_one();
        (refused, builds_after_refusal)
    });

    for response in &refused {
        assert_eq!(response.status_code(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );
        assert_eq!(
            response.json::<ErrorResponse>().error.r#type,
            "rate_limit_exceeded"
        );
    }
    assert_eq!(
        builds_after_refusal, 1,
        "a refused request must not reach evidence collection"
    );
    assert_ne!(held.status_code(), StatusCode::TOO_MANY_REQUESTS);

    // The permit is back: the next request is served.
    service.release.notify_one();
    let next = with_address("0x03").await;
    assert_ne!(next.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.builds(), 2);
}
