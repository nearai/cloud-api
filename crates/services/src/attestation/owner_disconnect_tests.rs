//! A stored signature is returned to any caller, whichever workspace stored it.
//! A missing signature is reported as `STREAM_DISCONNECTED` only when the
//! caller's own usage row for that id ended with a client disconnect; another
//! workspace's disconnect is not found. The usage fake answers
//! `ClientDisconnect` from every global stop-reason lookup, so a lookup that
//! ignores the owner makes these tests fail.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use config::ExternalProvidersConfig;
use inference_providers::mock::MockProvider;
use uuid::Uuid;

use super::{
    chat_signature_lifecycle_tests::{lifecycle_service, owner, RecordingRepository},
    chat_signatures::STREAM_DISCONNECTED,
    models::{ChatSignature, SignatureKind, SignatureLookupResult, SignatureOwner},
    ports::AttestationServiceTrait,
    AttestationError, AttestationService,
};
use crate::{
    inference_provider_pool::InferenceProviderPool,
    usage::{
        InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportRow,
        OrganizationBalanceInfo, RecordUsageDbRequest, StopReason, UsageByModelEntry,
        UsageLogEntry, UsageRepository,
    },
};

/// One usage row: organization, workspace, id, stop reason.
type UsageRow = (Uuid, Uuid, String, StopReason);

#[derive(Clone, Default)]
struct OwnerUsageRepository {
    rows: Arc<Mutex<Vec<UsageRow>>>,
    calls: Arc<Mutex<Vec<(Uuid, Uuid, String)>>>,
    fail: bool,
}

impl OwnerUsageRepository {
    fn with_row(owner: SignatureOwner, id: &str, reason: StopReason) -> Self {
        let repository = Self::default();
        repository.rows.lock().unwrap().push((
            owner.organization_id,
            owner.workspace_id,
            id.to_string(),
            reason,
        ));
        repository
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<(Uuid, Uuid, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl UsageRepository for OwnerUsageRepository {
    async fn record_usage(&self, _request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        anyhow::bail!("unused")
    }

    async fn get_balance(
        &self,
        _organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        Ok(None)
    }

    async fn get_usage_history(
        &self,
        _organization_id: Uuid,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_usage_history_by_api_key(
        &self,
        _api_key_id: Uuid,
        _credit_type: Option<&str>,
        _limit: Option<i64>,
        _offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        Ok((Vec::new(), 0))
    }

    async fn get_api_key_spend(&self, _api_key_id: Uuid) -> anyhow::Result<i64> {
        Ok(0)
    }

    async fn get_costs_by_inference_ids(
        &self,
        _organization_id: Uuid,
        _inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        Ok(Vec::new())
    }

    async fn has_client_disconnect_usage(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        id: &str,
    ) -> anyhow::Result<bool> {
        self.calls
            .lock()
            .unwrap()
            .push((organization_id, workspace_id, id.to_string()));
        if self.fail {
            anyhow::bail!("simulated usage lookup failure");
        }
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|(org, ws, row_id, reason)| {
                *org == organization_id
                    && *ws == workspace_id
                    && row_id == id
                    && *reason == StopReason::ClientDisconnect
            }))
    }

    async fn get_usage_by_model(
        &self,
        _organization_id: Uuid,
        _start_date: chrono::DateTime<chrono::Utc>,
        _end_date: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        Ok(Vec::new())
    }

    async fn list_inference_usage_history(
        &self,
        _query: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        Ok((Vec::new(), 0))
    }
}

fn other_org() -> SignatureOwner {
    SignatureOwner {
        organization_id: Uuid::from_u128(30),
        workspace_id: Uuid::from_u128(40),
    }
}

fn sibling_workspace() -> SignatureOwner {
    SignatureOwner {
        workspace_id: Uuid::from_u128(99),
        ..owner()
    }
}

fn empty_pool() -> Arc<InferenceProviderPool> {
    Arc::new(InferenceProviderPool::new(
        None,
        ExternalProvidersConfig::default(),
    ))
}

fn service(signatures: RecordingRepository, usage: OwnerUsageRepository) -> AttestationService {
    let mut service = lifecycle_service(Arc::new(signatures), empty_pool());
    service.usage_repository = Arc::new(usage);
    service
}

fn assert_disconnected(result: Result<SignatureLookupResult, AttestationError>) {
    match result {
        Ok(SignatureLookupResult::Unavailable { error_code, .. }) => {
            assert_eq!(error_code, STREAM_DISCONNECTED);
        }
        other => panic!("expected STREAM_DISCONNECTED, got {other:?}"),
    }
}

fn assert_not_found(result: Result<SignatureLookupResult, AttestationError>) {
    match result {
        Err(AttestationError::SignatureNotFound(_)) => {}
        other => panic!("expected SignatureNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn own_disconnected_stream_without_signature_is_stream_disconnected() {
    let chat_id = "chatcmpl-own-disconnect";
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ClientDisconnect);
    let service = service(RecordingRepository::default(), usage.clone());

    for algo in [None, Some("ecdsa".to_string()), Some("ED25519".to_string())] {
        assert_disconnected(service.get_chat_signature(owner(), chat_id, algo).await);
    }
    let expected = (
        owner().organization_id,
        owner().workspace_id,
        chat_id.to_string(),
    );
    assert_eq!(usage.calls(), vec![expected; 3]);
}

#[tokio::test]
async fn another_organizations_disconnect_is_not_found() {
    let chat_id = "chatcmpl-other-org-disconnect";
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ClientDisconnect);
    let service = service(RecordingRepository::default(), usage.clone());

    assert_not_found(service.get_chat_signature(other_org(), chat_id, None).await);
    assert_eq!(
        usage.calls(),
        vec![(
            other_org().organization_id,
            other_org().workspace_id,
            chat_id.to_string()
        )],
        "the lookup must use the caller's organization and workspace"
    );
}

#[tokio::test]
async fn sibling_workspaces_disconnect_is_not_found() {
    let chat_id = "chatcmpl-sibling-disconnect";
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ClientDisconnect);
    let service = service(RecordingRepository::default(), usage);

    assert_not_found(
        service
            .get_chat_signature(sibling_workspace(), chat_id, None)
            .await,
    );
}

#[tokio::test]
async fn own_row_with_another_stop_reason_is_not_found() {
    let chat_id = "chatcmpl-own-provider-error";
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ProviderError);
    let service = service(RecordingRepository::default(), usage);

    assert_not_found(service.get_chat_signature(owner(), chat_id, None).await);
}

#[tokio::test]
async fn unknown_id_is_not_found_even_when_global_lookups_report_a_disconnect() {
    let service = service(
        RecordingRepository::default(),
        OwnerUsageRepository::default(),
    );

    assert_not_found(
        service
            .get_chat_signature(owner(), "chatcmpl-unknown-id", None)
            .await,
    );
}

#[tokio::test]
async fn usage_lookup_failure_is_not_found() {
    let usage = OwnerUsageRepository::failing();
    let service = service(RecordingRepository::default(), usage.clone());

    assert_not_found(
        service
            .get_chat_signature(owner(), "chatcmpl-lookup-fails", None)
            .await,
    );
    assert_eq!(usage.calls().len(), 1);
}

#[tokio::test]
async fn legacy_unowned_signature_is_found_despite_another_owners_disconnect() {
    let chat_id = "chatcmpl-legacy-disconnect";
    let signatures = RecordingRepository::default();
    signatures.insert_legacy(
        chat_id,
        ChatSignature {
            text: "legacy".to_string(),
            signature: "0xlegacy".to_string(),
            signing_address: "0xaddr".to_string(),
            signing_algo: "ecdsa".to_string(),
            signature_kind: None,
        },
    );
    let usage = OwnerUsageRepository::with_row(other_org(), chat_id, StopReason::ClientDisconnect);
    let service = service(signatures, usage.clone());

    match service.get_chat_signature(owner(), chat_id, None).await {
        Ok(SignatureLookupResult::Found(signature)) => {
            assert_eq!(signature.signature, "0xlegacy");
        }
        other => panic!("expected the legacy signature, got {other:?}"),
    }
    assert!(
        usage.calls().is_empty(),
        "usage is checked only after a miss"
    );
}

#[tokio::test]
async fn another_workspaces_signature_is_found_without_a_disconnect_check() {
    let chat_id = "chatcmpl-cross-workspace-signed";
    // The writer's own stream also has a disconnect row; neither caller
    // should see it, because a signature is stored.
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ClientDisconnect);
    let service = service(RecordingRepository::default(), usage.clone());
    service
        .store_chat_signature(owner(), chat_id, "req".to_string(), "resp".to_string())
        .await
        .expect("gateway signature stored");

    for caller in [other_org(), sibling_workspace()] {
        match service.get_chat_signature(caller, chat_id, None).await {
            Ok(SignatureLookupResult::Found(signature)) => {
                assert_eq!(signature.signature_kind, Some(SignatureKind::Gateway));
            }
            other => panic!("expected the stored signature, got {other:?}"),
        }
    }
    assert!(
        usage.calls().is_empty(),
        "usage is checked only after a miss"
    );
}

#[tokio::test]
async fn stored_signature_wins_over_a_disconnect_row() {
    let chat_id = "chatcmpl-signed-and-disconnected";
    let usage = OwnerUsageRepository::with_row(owner(), chat_id, StopReason::ClientDisconnect);
    let signatures = RecordingRepository::default();
    let service = service(signatures, usage.clone());
    service
        .store_chat_signature(owner(), chat_id, "req".to_string(), "resp".to_string())
        .await
        .expect("gateway signature stored");

    match service.get_chat_signature(owner(), chat_id, None).await {
        Ok(SignatureLookupResult::Found(signature)) => {
            assert_eq!(signature.signature_kind, Some(SignatureKind::Gateway));
        }
        other => panic!("expected the stored signature, got {other:?}"),
    }
    assert!(
        usage.calls().is_empty(),
        "usage is checked only after a miss"
    );
}

#[tokio::test]
async fn pinned_provider_without_signatures_does_not_report_another_owners_disconnect() {
    let chat_id = "chatcmpl-pinned-unsupported-disconnect";
    let pool = empty_pool();
    let provider = Arc::new(MockProvider::new().with_chat_signature_support(false));
    pool.store_chat_id_mapping(chat_id.to_string(), provider)
        .await;
    let mut service = lifecycle_service(Arc::new(RecordingRepository::default()), pool);
    service.usage_repository = Arc::new(OwnerUsageRepository::with_row(
        owner(),
        chat_id,
        StopReason::ClientDisconnect,
    ));

    assert_not_found(service.get_chat_signature(other_org(), chat_id, None).await);
}
