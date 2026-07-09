use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use config::StakingFarmConfig;
use near_api::{Contract, Data, NetworkConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard},
};
use uuid::Uuid;

pub const CREDIT_TYPE_STAKING_FARM: &str = "staking_farm";
pub const CREDIT_SOURCE_HOUSE_OF_STAKE: &str = "house-of-stake";
const REWARD_UNIT_SCALE_24: u128 = 1_000_000_000_000_000_000_000_000;
const STAKING_FARM_RPC_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, thiserror::Error)]
#[error("NEAR account is already linked to another organization")]
pub struct StakingFarmSourceConflict;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StakingFarmSourceStatus {
    Active,
    Disconnected,
}

impl StakingFarmSourceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StakingSyncStatus {
    NeverSynced,
    Synced,
    Stale,
    Failed,
}

impl StakingSyncStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NeverSynced => "never_synced",
            Self::Synced => "synced",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FarmAccount {
    pub accumulated_reward_units: String,
    pub pending_reward_units: String,
    pub total_earned_reward_units: String,
    #[serde(default)]
    pub active_positions: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationStakingFarmSource {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub near_account_id: String,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub status: String,
    pub sync_status: String,
    pub last_sync_error: Option<String>,
    pub created_by_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub last_synced_accumulated_reward_units_24: Option<String>,
    pub last_synced_pending_reward_units_24: Option<String>,
    pub last_synced_reward_units_24: Option<String>,
    pub last_synced_credit_nano_usd: Option<i64>,
    pub active_positions: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct UpsertStakingFarmSourceRequest {
    pub organization_id: Uuid,
    pub near_account_id: String,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub created_by_user_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct StakingFarmSourceSyncUpdate {
    pub sync_status: StakingSyncStatus,
    pub last_sync_error: Option<String>,
    pub last_synced_accumulated_reward_units_24: Option<String>,
    pub last_synced_pending_reward_units_24: Option<String>,
    pub last_synced_reward_units_24: Option<String>,
    pub last_synced_credit_nano_usd: Option<i64>,
    pub active_positions: serde_json::Value,
}

#[async_trait]
pub trait StakingFarmRepository: Send + Sync {
    async fn upsert_source(
        &self,
        request: UpsertStakingFarmSourceRequest,
    ) -> anyhow::Result<OrganizationStakingFarmSource>;

    async fn get_source_by_organization(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationStakingFarmSource>>;

    async fn update_sync_state(
        &self,
        source_id: Uuid,
        update: StakingFarmSourceSyncUpdate,
    ) -> anyhow::Result<OrganizationStakingFarmSource>;

    async fn update_staking_farm_limit(
        &self,
        organization_id: Uuid,
        credit_nano_usd: i64,
        changed_by_user_id: Option<Uuid>,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait StakingFarmContractClient: Send + Sync {
    async fn get_farm_account(
        &self,
        account_id: &str,
        contract_id: &str,
    ) -> anyhow::Result<FarmAccount>;
}

#[derive(Debug, Clone)]
pub struct NearRpcStakingFarmClient {
    network_config: NetworkConfig,
}

impl NearRpcStakingFarmClient {
    pub fn new(rpc_url: String, network_id: String) -> anyhow::Result<Self> {
        let rpc_url = rpc_url.parse()?;
        Ok(Self {
            network_config: NetworkConfig::from_rpc_url(&network_id, rpc_url),
        })
    }
}

#[async_trait]
impl StakingFarmContractClient for NearRpcStakingFarmClient {
    async fn get_farm_account(
        &self,
        account_id: &str,
        contract_id: &str,
    ) -> anyhow::Result<FarmAccount> {
        let contract = Contract(contract_id.parse()?);
        let account: Data<FarmAccount> = tokio::time::timeout(
            std::time::Duration::from_secs(STAKING_FARM_RPC_TIMEOUT_SECS),
            contract
                .call_function(
                    "get_farm_account",
                    serde_json::json!({ "account_id": account_id }),
                )
                .read_only()
                .fetch_from(&self.network_config),
        )
        .await
        .map_err(|_| anyhow::anyhow!("staking farm RPC timed out"))??;
        Ok(account.data)
    }
}

#[derive(Clone)]
pub struct StakingFarmService {
    repository: Arc<dyn StakingFarmRepository>,
    contract_client: Arc<dyn StakingFarmContractClient>,
    config: StakingFarmConfig,
    active_syncs: Arc<Mutex<HashSet<Uuid>>>,
}

impl StakingFarmService {
    pub fn new(
        repository: Arc<dyn StakingFarmRepository>,
        contract_client: Arc<dyn StakingFarmContractClient>,
        config: StakingFarmConfig,
    ) -> Self {
        Self {
            repository,
            contract_client,
            config,
            active_syncs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn config(&self) -> &StakingFarmConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub async fn get_source(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationStakingFarmSource>> {
        self.repository
            .get_source_by_organization(organization_id)
            .await
    }

    pub async fn ensure_source_for_near_account(
        &self,
        organization_id: Uuid,
        near_account_id: String,
        created_by_user_id: Option<Uuid>,
    ) -> anyhow::Result<OrganizationStakingFarmSource> {
        ensure_configured(&self.config)?;
        self.repository
            .upsert_source(UpsertStakingFarmSourceRequest {
                organization_id,
                near_account_id,
                network_id: self.config.network_id.clone(),
                contract_id: self.config.contract_id.clone(),
                farm_product_id: self.config.farm_product_id.clone(),
                farm_price_id: self.config.farm_price_id.clone(),
                credit_nano_usd_per_reward_unit: self.config.credit_nano_usd_per_reward_unit,
                created_by_user_id,
            })
            .await
    }

    pub async fn sync_for_source(
        &self,
        source: OrganizationStakingFarmSource,
        changed_by_user_id: Option<Uuid>,
    ) -> anyhow::Result<OrganizationStakingFarmSource> {
        ensure_configured(&self.config)?;

        let sync_result = self
            .contract_client
            .get_farm_account(&source.near_account_id, &source.contract_id)
            .await;

        let farm_account = match sync_result {
            Ok(account) => account,
            Err(error) => {
                let updated = self
                    .repository
                    .update_sync_state(
                        source.id,
                        StakingFarmSourceSyncUpdate {
                            sync_status: StakingSyncStatus::Failed,
                            last_sync_error: Some(error.to_string()),
                            last_synced_accumulated_reward_units_24: source
                                .last_synced_accumulated_reward_units_24,
                            last_synced_pending_reward_units_24: source
                                .last_synced_pending_reward_units_24,
                            last_synced_reward_units_24: source.last_synced_reward_units_24,
                            last_synced_credit_nano_usd: source.last_synced_credit_nano_usd,
                            active_positions: source.active_positions,
                        },
                    )
                    .await?;
                return Ok(updated);
            }
        };

        let computed_credit_result =
            validate_farm_account_reward_units(&farm_account).and_then(|_| {
                reward_units_24_to_nano_usd(
                    &farm_account.total_earned_reward_units,
                    source.credit_nano_usd_per_reward_unit,
                )
            });
        let computed_credit = match computed_credit_result {
            Ok(credit) => credit,
            Err(error) => {
                let updated = self
                    .repository
                    .update_sync_state(
                        source.id,
                        StakingFarmSourceSyncUpdate {
                            sync_status: StakingSyncStatus::Failed,
                            last_sync_error: Some(error.to_string()),
                            last_synced_accumulated_reward_units_24: source
                                .last_synced_accumulated_reward_units_24,
                            last_synced_pending_reward_units_24: source
                                .last_synced_pending_reward_units_24,
                            last_synced_reward_units_24: source.last_synced_reward_units_24,
                            last_synced_credit_nano_usd: source.last_synced_credit_nano_usd,
                            active_positions: farm_account.active_positions,
                        },
                    )
                    .await?;
                return Ok(updated);
            }
        };
        // Farm rewards are cumulative. Keep granted staking-farm credits monotonic so a stale
        // or inconsistent farm response cannot reduce already-issued credits.
        let next_credit = computed_credit.max(source.last_synced_credit_nano_usd.unwrap_or(0));

        if Some(next_credit) != source.last_synced_credit_nano_usd {
            self.repository
                .update_staking_farm_limit(source.organization_id, next_credit, changed_by_user_id)
                .await?;
        }

        self.repository
            .update_sync_state(
                source.id,
                StakingFarmSourceSyncUpdate {
                    sync_status: StakingSyncStatus::Synced,
                    last_sync_error: None,
                    last_synced_accumulated_reward_units_24: Some(
                        farm_account.accumulated_reward_units,
                    ),
                    last_synced_pending_reward_units_24: Some(farm_account.pending_reward_units),
                    last_synced_reward_units_24: Some(farm_account.total_earned_reward_units),
                    last_synced_credit_nano_usd: Some(next_credit),
                    active_positions: farm_account.active_positions,
                },
            )
            .await
    }

    pub async fn sync_for_near_account(
        &self,
        organization_id: Uuid,
        near_account_id: String,
        user_id: Uuid,
    ) -> anyhow::Result<OrganizationStakingFarmSource> {
        let source = self
            .ensure_source_for_near_account(organization_id, near_account_id, Some(user_id))
            .await?;
        self.sync_for_source(source, Some(user_id)).await
    }

    pub async fn sync_organization_if_stale(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationStakingFarmSource>> {
        if !self.is_enabled() {
            return Ok(None);
        }

        let Some(source) = self.get_source(organization_id).await? else {
            return Ok(None);
        };

        if source.status != StakingFarmSourceStatus::Active.as_str() {
            return Ok(Some(source));
        }

        let stale_after = Duration::seconds(self.config.sync_staleness_seconds.max(0));
        let is_stale = source
            .last_synced_at
            .map(|synced_at| Utc::now().signed_duration_since(synced_at) >= stale_after)
            .unwrap_or(true)
            || source.sync_status == StakingSyncStatus::NeverSynced.as_str()
            || source.sync_status == StakingSyncStatus::Stale.as_str()
            || source.sync_status == StakingSyncStatus::Failed.as_str();

        if is_stale {
            let Some(_guard) = self.try_start_sync(organization_id) else {
                return Ok(Some(source));
            };
            self.sync_for_source(source, None).await.map(Some)
        } else {
            Ok(Some(source))
        }
    }

    fn try_start_sync(&self, organization_id: Uuid) -> Option<ActiveSyncGuard> {
        let mut active_syncs = lock_active_syncs(&self.active_syncs);
        if !active_syncs.insert(organization_id) {
            return None;
        }
        Some(ActiveSyncGuard {
            organization_id,
            active_syncs: self.active_syncs.clone(),
        })
    }
}

struct ActiveSyncGuard {
    organization_id: Uuid,
    active_syncs: Arc<Mutex<HashSet<Uuid>>>,
}

impl Drop for ActiveSyncGuard {
    fn drop(&mut self) {
        lock_active_syncs(&self.active_syncs).remove(&self.organization_id);
    }
}

fn lock_active_syncs(active_syncs: &Mutex<HashSet<Uuid>>) -> MutexGuard<'_, HashSet<Uuid>> {
    active_syncs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn validate_farm_account_reward_units(farm_account: &FarmAccount) -> anyhow::Result<()> {
    validate_reward_units_24_format(
        "accumulated_reward_units",
        &farm_account.accumulated_reward_units,
    )?;
    validate_reward_units_24_format("pending_reward_units", &farm_account.pending_reward_units)?;
    validate_reward_units_24_format(
        "total_earned_reward_units",
        &farm_account.total_earned_reward_units,
    )
}

fn validate_reward_units_24_format(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("{field} must be an unsigned integer string");
    }
    Ok(())
}

pub fn reward_units_24_to_nano_usd(
    reward_units_24: &str,
    credit_nano_usd_per_reward_unit: i64,
) -> anyhow::Result<i64> {
    if credit_nano_usd_per_reward_unit < 0 {
        anyhow::bail!("credit_nano_usd_per_reward_unit must be non-negative");
    }

    let reward_units = reward_units_24
        .parse::<u128>()
        .map_err(|_| anyhow::anyhow!("reward units must be an unsigned integer string"))?;
    let conversion = u128::try_from(credit_nano_usd_per_reward_unit)?;
    let whole_reward_units = reward_units / REWARD_UNIT_SCALE_24;
    let fractional_reward_units = reward_units % REWARD_UNIT_SCALE_24;
    let nano_usd = whole_reward_units
        .checked_mul(conversion)
        .and_then(|whole_credit| {
            fractional_reward_units
                .checked_mul(conversion)
                .and_then(|fractional_credit| {
                    whole_credit.checked_add(fractional_credit / REWARD_UNIT_SCALE_24)
                })
        })
        .ok_or_else(|| anyhow::anyhow!("reward unit conversion overflow"))?;

    i64::try_from(nano_usd).map_err(|_| anyhow::anyhow!("converted credit exceeds i64"))
}

fn ensure_configured(config: &StakingFarmConfig) -> anyhow::Result<()> {
    if !config.enabled {
        anyhow::bail!("staking farm is not configured");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let service = StakingFarmService::new(repo.clone(), client, enabled_config());

        let source = service
            .ensure_source_for_near_account(
                organization_id,
                "alice.near".to_string(),
                Some(user_id),
            )
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
        let service = StakingFarmService::new(repo.clone(), client, enabled_config());

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
        let service = StakingFarmService::new(repo.clone(), client, enabled_config());

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
        let service = StakingFarmService::new(repo.clone(), client, enabled_config());

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
        let service = StakingFarmService::new(repo.clone(), client, enabled_config());

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
    async fn stale_source_is_synced_automatically() {
        let organization_id = Uuid::new_v4();
        let source = source_fixture(organization_id);
        let repo = Arc::new(MockStakingFarmRepository::default());
        *repo.source.lock().unwrap() = Some(source);
        let client = Arc::new(MockStakingFarmContractClient::returning(farm_account(
            "3000000000000000000000000",
        )));
        let service = StakingFarmService::new(repo.clone(), client.clone(), enabled_config());

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
        let service = StakingFarmService::new(repo.clone(), client.clone(), enabled_config());

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
        let service = StakingFarmService::new(
            Arc::new(MockStakingFarmRepository::default()),
            Arc::new(MockStakingFarmContractClient::returning(farm_account("0"))),
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
        let service = StakingFarmService::new(repo.clone(), client.clone(), enabled_config());

        let result = service
            .sync_organization_if_stale(organization_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.last_synced_credit_nano_usd, Some(1_000_000_000));
        assert!(repo.limit_updates.lock().unwrap().is_empty());
        assert!(client.calls.lock().unwrap().is_empty());
    }
}
