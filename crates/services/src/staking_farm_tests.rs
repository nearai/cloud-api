use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct MockStakingFarmRepository {
    source: Mutex<Option<OrganizationStakingFarmSource>>,
    upserts: Mutex<Vec<UpsertStakingFarmSourceRequest>>,
    sync_updates: Mutex<Vec<StakingFarmSourceSyncUpdate>>,
    limit_updates: Mutex<Vec<(Uuid, i64, Option<Uuid>)>>,
}

#[async_trait]
impl StakingFarmRepository for MockStakingFarmRepository {
    async fn upsert_source(
        &self,
        request: UpsertStakingFarmSourceRequest,
    ) -> anyhow::Result<OrganizationStakingFarmSource> {
        self.upserts.lock().unwrap().push(request.clone());
        let source = OrganizationStakingFarmSource {
            id: Uuid::new_v4(),
            organization_id: request.organization_id,
            near_account_id: request.near_account_id,
            network_id: request.network_id,
            contract_id: request.contract_id,
            farm_product_id: request.farm_product_id,
            farm_price_id: request.farm_price_id,
            credit_nano_usd_per_reward_unit: request.credit_nano_usd_per_reward_unit,
            status: StakingFarmSourceStatus::Active.as_str().to_string(),
            sync_status: StakingSyncStatus::NeverSynced.as_str().to_string(),
            last_sync_error: None,
            created_by_user_id: request.created_by_user_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
            last_synced_accumulated_reward_units_24: None,
            last_synced_pending_reward_units_24: None,
            last_synced_reward_units_24: None,
            last_synced_credit_nano_usd: None,
            active_positions: serde_json::json!([]),
        };
        *self.source.lock().unwrap() = Some(source.clone());
        Ok(source)
    }

    async fn get_source_by_organization(
        &self,
        _organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationStakingFarmSource>> {
        Ok(self.source.lock().unwrap().clone())
    }

    async fn update_sync_state(
        &self,
        source_id: Uuid,
        update: StakingFarmSourceSyncUpdate,
    ) -> anyhow::Result<OrganizationStakingFarmSource> {
        self.sync_updates.lock().unwrap().push(update.clone());
        let mut source = self
            .source
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| source_fixture(Uuid::new_v4()));
        source.id = source_id;
        source.sync_status = update.sync_status.as_str().to_string();
        source.last_sync_error = update.last_sync_error;
        source.last_synced_accumulated_reward_units_24 =
            update.last_synced_accumulated_reward_units_24;
        source.last_synced_pending_reward_units_24 = update.last_synced_pending_reward_units_24;
        source.last_synced_reward_units_24 = update.last_synced_reward_units_24;
        source.last_synced_credit_nano_usd = update.last_synced_credit_nano_usd;
        source.active_positions = update.active_positions;
        if update.sync_status == StakingSyncStatus::Synced {
            source.last_synced_at = Some(Utc::now());
        }
        *self.source.lock().unwrap() = Some(source.clone());
        Ok(source)
    }

    async fn update_staking_farm_limit(
        &self,
        organization_id: Uuid,
        credit_nano_usd: i64,
        changed_by_user_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        self.limit_updates.lock().unwrap().push((
            organization_id,
            credit_nano_usd,
            changed_by_user_id,
        ));
        Ok(())
    }
}

struct MockStakingFarmContractClient {
    result: Mutex<anyhow::Result<FarmAccount>>,
    calls: Mutex<Vec<(String, String)>>,
    delay_millis: u64,
}

impl MockStakingFarmContractClient {
    fn returning(account: FarmAccount) -> Self {
        Self {
            result: Mutex::new(Ok(account)),
            calls: Mutex::new(vec![]),
            delay_millis: 0,
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            result: Mutex::new(Err(anyhow::anyhow!(message.to_string()))),
            calls: Mutex::new(vec![]),
            delay_millis: 0,
        }
    }

    fn with_delay(mut self, delay_millis: u64) -> Self {
        self.delay_millis = delay_millis;
        self
    }
}

#[async_trait]
impl StakingFarmContractClient for MockStakingFarmContractClient {
    async fn get_farm_account(
        &self,
        account_id: &str,
        contract_id: &str,
    ) -> anyhow::Result<FarmAccount> {
        self.calls
            .lock()
            .unwrap()
            .push((account_id.to_string(), contract_id.to_string()));
        if self.delay_millis > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_millis)).await;
        }
        self.result
            .lock()
            .unwrap()
            .as_ref()
            .map(Clone::clone)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

struct MockStakingFarmAmlGate {
    blocked: bool,
    calls: Mutex<Vec<(Option<Uuid>, String, AmlFlow)>>,
}

impl MockStakingFarmAmlGate {
    fn blocking() -> Self {
        Self {
            blocked: true,
            calls: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl StakingFarmAmlGate for MockStakingFarmAmlGate {
    async fn check_near_account(
        &self,
        user_id: Option<Uuid>,
        account_id: &str,
        flow: AmlFlow,
    ) -> Result<(), AmlError> {
        self.calls
            .lock()
            .unwrap()
            .push((user_id, account_id.to_string(), flow));
        if self.blocked {
            Err(AmlError::AccountBlocked)
        } else {
            Ok(())
        }
    }
}

fn enabled_config() -> StakingFarmConfig {
    StakingFarmConfig {
        enabled: true,
        network_id: "testnet".to_string(),
        contract_id: "stake.testnet".to_string(),
        farm_product_id: "cloud-credits".to_string(),
        farm_price_id: Some("price-1".to_string()),
        credit_nano_usd_per_reward_unit: 1_000_000_000,
        sync_staleness_seconds: 300,
    }
}

fn farm_account(total_earned_reward_units: &str) -> FarmAccount {
    FarmAccount {
        accumulated_reward_units: "100000000000000000000000".to_string(),
        pending_reward_units: "200000000000000000000000".to_string(),
        total_earned_reward_units: total_earned_reward_units.to_string(),
        active_positions: serde_json::json!([{"amount": "10"}]),
    }
}

fn source_fixture(organization_id: Uuid) -> OrganizationStakingFarmSource {
    OrganizationStakingFarmSource {
        id: Uuid::new_v4(),
        organization_id,
        near_account_id: "alice.near".to_string(),
        network_id: "testnet".to_string(),
        contract_id: "stake.testnet".to_string(),
        farm_product_id: "cloud-credits".to_string(),
        farm_price_id: Some("price-1".to_string()),
        credit_nano_usd_per_reward_unit: 1_000_000_000,
        status: StakingFarmSourceStatus::Active.as_str().to_string(),
        sync_status: StakingSyncStatus::Synced.as_str().to_string(),
        last_sync_error: None,
        created_by_user_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_synced_at: Some(Utc::now() - Duration::seconds(600)),
        last_synced_accumulated_reward_units_24: Some("0".to_string()),
        last_synced_pending_reward_units_24: Some("0".to_string()),
        last_synced_reward_units_24: Some("0".to_string()),
        last_synced_credit_nano_usd: Some(2_000_000_000),
        active_positions: serde_json::json!([]),
    }
}

#[test]
fn converts_24_decimal_reward_units() {
    assert_eq!(
        reward_units_24_to_nano_usd("1000000000000000000000000", 1_000_000_000).unwrap(),
        1_000_000_000
    );
    assert_eq!(
        reward_units_24_to_nano_usd("1500000000000000000000000", 1_000_000_000).unwrap(),
        1_500_000_000
    );
}

#[test]
fn conversion_floors_fractional_nano_usd() {
    assert_eq!(
        reward_units_24_to_nano_usd("999999999999999", 1_000_000_000).unwrap(),
        0
    );
}

#[test]
fn conversion_handles_large_reward_totals_without_intermediate_overflow() {
    assert_eq!(
        reward_units_24_to_nano_usd("1000000000000000000000000000000", 1_000_000_000).unwrap(),
        1_000_000_000_000_000
    );
}

#[test]
fn conversion_rejects_overflow() {
    let error = reward_units_24_to_nano_usd(&u128::MAX.to_string(), i64::MAX)
        .expect_err("conversion should overflow");
    assert!(error.to_string().contains("overflow"));
}

#[tokio::test]
async fn ensure_source_uses_server_config_values() {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let repo = Arc::new(MockStakingFarmRepository::default());
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account("0")));
    let service = StakingFarmService::new_for_tests(repo.clone(), client, None, enabled_config());

    let source = service
        .ensure_source_for_near_account(organization_id, "alice.near".to_string(), Some(user_id))
        .await
        .unwrap();

    assert_eq!(source.organization_id, organization_id);
    assert_eq!(source.near_account_id, "alice.near");
    assert_eq!(source.network_id, "testnet");
    assert_eq!(source.contract_id, "stake.testnet");
    assert_eq!(source.farm_product_id, "cloud-credits");
    assert_eq!(source.farm_price_id.as_deref(), Some("price-1"));
    assert_eq!(source.created_by_user_id, Some(user_id));
}

#[tokio::test]
async fn sync_does_not_decrement_existing_farm_credit() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source.clone());
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "1000000000000000000000000",
    )));
    let service = StakingFarmService::new_for_tests(repo.clone(), client, None, enabled_config());

    let synced = service
        .sync_for_source(source, Some(Uuid::new_v4()))
        .await
        .unwrap();

    assert_eq!(synced.last_synced_credit_nano_usd, Some(2_000_000_000));
    let limit_updates = repo.limit_updates.lock().unwrap();
    assert!(limit_updates.is_empty());
}

#[tokio::test]
async fn sync_failure_marks_source_failed_without_limit_update() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source.clone());
    let client = Arc::new(MockStakingFarmContractClient::failing("rpc unavailable"));
    let service = StakingFarmService::new_for_tests(repo.clone(), client, None, enabled_config());

    let synced = service.sync_for_source(source, None).await.unwrap();

    assert_eq!(synced.sync_status, StakingSyncStatus::Failed.as_str());
    assert_eq!(synced.last_sync_error.as_deref(), Some("rpc unavailable"));
    assert!(repo.limit_updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn conversion_overflow_marks_source_failed_without_limit_update() {
    let organization_id = Uuid::new_v4();
    let mut source = source_fixture(organization_id);
    source.credit_nano_usd_per_reward_unit = i64::MAX;
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source.clone());
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        &u128::MAX.to_string(),
    )));
    let service = StakingFarmService::new_for_tests(repo.clone(), client, None, enabled_config());

    let synced = service.sync_for_source(source, None).await.unwrap();

    assert_eq!(synced.sync_status, StakingSyncStatus::Failed.as_str());
    assert!(synced
        .last_sync_error
        .as_deref()
        .unwrap_or_default()
        .contains("overflow"));
    assert!(repo.limit_updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_farm_account_reward_units_fail_before_limit_update() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source.clone());
    let mut account = farm_account("3000000000000000000000000");
    account.accumulated_reward_units = "not-a-number".to_string();
    let client = Arc::new(MockStakingFarmContractClient::returning(account));
    let service = StakingFarmService::new_for_tests(repo.clone(), client, None, enabled_config());

    let synced = service.sync_for_source(source, None).await.unwrap();

    assert_eq!(synced.sync_status, StakingSyncStatus::Failed.as_str());
    assert!(synced
        .last_sync_error
        .as_deref()
        .unwrap_or_default()
        .contains("accumulated_reward_units"));
    assert!(repo.limit_updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sync_for_source_blocks_aml_rejected_source_before_contract_call() {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source.clone());
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "3000000000000000000000000",
    )));
    let aml_gate = Arc::new(MockStakingFarmAmlGate::blocking());
    let service = StakingFarmService::new_for_tests(
        repo.clone(),
        client.clone(),
        Some(aml_gate.clone()),
        enabled_config(),
    );

    let error = service
        .sync_for_source(source, Some(user_id))
        .await
        .expect_err("AML-rejected source should block sync");

    assert!(error.chain().any(|cause| matches!(
        cause.downcast_ref::<AmlError>(),
        Some(AmlError::AccountBlocked)
    )));
    assert!(client.calls.lock().unwrap().is_empty());
    assert!(repo.limit_updates.lock().unwrap().is_empty());
    assert_eq!(
        aml_gate.calls.lock().unwrap().as_slice(),
        &[(
            Some(user_id),
            "alice.near".to_string(),
            AmlFlow::StakingFarmSync
        )]
    );
}

#[tokio::test]
async fn stale_preflight_blocks_aml_rejected_source_before_contract_call() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source);
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "3000000000000000000000000",
    )));
    let aml_gate = Arc::new(MockStakingFarmAmlGate::blocking());
    let service = StakingFarmService::new_for_tests(
        repo.clone(),
        client.clone(),
        Some(aml_gate.clone()),
        enabled_config(),
    );

    let error = service
        .sync_organization_if_stale(organization_id)
        .await
        .expect_err("AML-rejected stale source should block preflight sync");

    assert!(error.chain().any(|cause| matches!(
        cause.downcast_ref::<AmlError>(),
        Some(AmlError::AccountBlocked)
    )));
    assert!(client.calls.lock().unwrap().is_empty());
    assert!(repo.limit_updates.lock().unwrap().is_empty());
    assert_eq!(
        aml_gate.calls.lock().unwrap().as_slice(),
        &[(None, "alice.near".to_string(), AmlFlow::StakingFarmSync)]
    );
}

#[tokio::test]
async fn sync_for_near_account_blocks_aml_rejected_source_before_contract_call() {
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let repo = Arc::new(MockStakingFarmRepository::default());
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "3000000000000000000000000",
    )));
    let aml_gate = Arc::new(MockStakingFarmAmlGate::blocking());
    let service = StakingFarmService::new_for_tests(
        repo.clone(),
        client.clone(),
        Some(aml_gate.clone()),
        enabled_config(),
    );

    let error = service
        .sync_for_near_account(organization_id, "alice.near".to_string(), user_id)
        .await
        .expect_err("AML-rejected user source should block sync");

    assert!(error.chain().any(|cause| matches!(
        cause.downcast_ref::<AmlError>(),
        Some(AmlError::AccountBlocked)
    )));
    assert_eq!(repo.upserts.lock().unwrap().len(), 1);
    assert!(client.calls.lock().unwrap().is_empty());
    assert!(repo.limit_updates.lock().unwrap().is_empty());
    assert_eq!(
        aml_gate.calls.lock().unwrap().as_slice(),
        &[(
            Some(user_id),
            "alice.near".to_string(),
            AmlFlow::StakingFarmSync
        )]
    );
}

#[tokio::test]
async fn stale_source_is_synced_automatically() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source);
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "3000000000000000000000000",
    )));
    let service =
        StakingFarmService::new_for_tests(repo.clone(), client.clone(), None, enabled_config());

    let synced = service
        .sync_organization_if_stale(organization_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(synced.sync_status, StakingSyncStatus::Synced.as_str());
    assert_eq!(repo.limit_updates.lock().unwrap()[0].1, 3_000_000_000);
    assert_eq!(client.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_stale_syncs_share_in_flight_sync() {
    let organization_id = Uuid::new_v4();
    let source = source_fixture(organization_id);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source);
    let client = Arc::new(
        MockStakingFarmContractClient::returning(farm_account("3000000000000000000000000"))
            .with_delay(50),
    );
    let service =
        StakingFarmService::new_for_tests(repo.clone(), client.clone(), None, enabled_config());

    let (first, second) = tokio::join!(
        service.sync_organization_if_stale(organization_id),
        service.sync_organization_if_stale(organization_id)
    );

    assert!(first.unwrap().is_some());
    assert!(second.unwrap().is_some());
    assert_eq!(client.calls.lock().unwrap().len(), 1);
    assert_eq!(repo.limit_updates.lock().unwrap().len(), 1);
}

#[test]
fn active_sync_guard_recovers_poisoned_mutex() {
    let service = StakingFarmService::new_for_tests(
        Arc::new(MockStakingFarmRepository::default()),
        Arc::new(MockStakingFarmContractClient::returning(farm_account("0"))),
        None,
        enabled_config(),
    );
    let organization_id = Uuid::new_v4();
    let active_syncs = service.active_syncs.clone();

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = active_syncs.lock().unwrap();
        panic!("poison active sync lock");
    }));

    let guard = service
        .try_start_sync(organization_id)
        .expect("poisoned mutex should be recovered");
    assert!(service.try_start_sync(organization_id).is_none());
    drop(guard);
    assert!(service.try_start_sync(organization_id).is_some());
}

#[tokio::test]
async fn fresh_source_is_not_synced_automatically() {
    let organization_id = Uuid::new_v4();
    let mut source = source_fixture(organization_id);
    source.last_synced_at = Some(Utc::now());
    source.last_synced_credit_nano_usd = Some(1_000_000_000);
    let repo = Arc::new(MockStakingFarmRepository::default());
    *repo.source.lock().unwrap() = Some(source);
    let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
        "3000000000000000000000000",
    )));
    let service =
        StakingFarmService::new_for_tests(repo.clone(), client.clone(), None, enabled_config());

    let result = service
        .sync_organization_if_stale(organization_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(result.last_synced_credit_nano_usd, Some(1_000_000_000));
    assert!(repo.limit_updates.lock().unwrap().is_empty());
    assert!(client.calls.lock().unwrap().is_empty());
}
