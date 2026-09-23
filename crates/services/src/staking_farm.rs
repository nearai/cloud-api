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

use crate::aml::{AmlError, AmlFlow};
use crate::usage::admission::AdmissionCoordinator;

pub const CREDIT_TYPE_STAKING_FARM: &str = "staking_farm";
pub const CREDIT_SOURCE_HOUSE_OF_STAKE: &str = "house-of-stake";
const REWARD_UNIT_SCALE_24: u128 = 1_000_000_000_000_000_000_000_000;
const NEAR_RPC_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, thiserror::Error)]
#[error("NEAR account is already linked to another organization")]
pub struct StakingFarmSourceConflict;

#[derive(Debug, thiserror::Error)]
#[error("organization is inactive and cannot be linked to a NEAR staking wallet")]
pub struct StakingFarmOrganizationInactive;

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

#[async_trait]
pub trait StakingFarmAmlGate: Send + Sync {
    async fn check_near_account(
        &self,
        user_id: Option<Uuid>,
        account_id: &str,
        flow: AmlFlow,
    ) -> Result<(), AmlError>;
}

#[async_trait]
impl StakingFarmAmlGate for crate::aml::AmlService {
    async fn check_near_account(
        &self,
        user_id: Option<Uuid>,
        account_id: &str,
        flow: AmlFlow,
    ) -> Result<(), AmlError> {
        crate::aml::AmlService::check_near_account(self, user_id, account_id, flow)
            .await
            .map(|_| ())
    }
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
            std::time::Duration::from_secs(NEAR_RPC_TIMEOUT_SECS),
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
    aml_gate: Option<Arc<dyn StakingFarmAmlGate>>,
    config: StakingFarmConfig,
    active_syncs: Arc<Mutex<HashSet<Uuid>>>,
    admission_coordinator: Arc<AdmissionCoordinator>,
}

impl StakingFarmService {
    pub fn new(
        repository: Arc<dyn StakingFarmRepository>,
        contract_client: Arc<dyn StakingFarmContractClient>,
        aml_gate: Option<Arc<dyn StakingFarmAmlGate>>,
        config: StakingFarmConfig,
        admission_coordinator: Arc<AdmissionCoordinator>,
    ) -> Self {
        Self {
            repository,
            contract_client,
            aml_gate,
            config,
            active_syncs: Arc::new(Mutex::new(HashSet::new())),
            admission_coordinator,
        }
    }

    pub fn config(&self) -> &StakingFarmConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    #[cfg(test)]
    fn new_for_tests(
        repository: Arc<dyn StakingFarmRepository>,
        contract_client: Arc<dyn StakingFarmContractClient>,
        aml_gate: Option<Arc<dyn StakingFarmAmlGate>>,
        config: StakingFarmConfig,
    ) -> Self {
        Self::new(
            repository,
            contract_client,
            aml_gate,
            config,
            Arc::new(AdmissionCoordinator::new_for_tests()),
        )
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
        self.enforce_aml_for_source(&source, changed_by_user_id)
            .await?;

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
            self.admission_coordinator
                .refresh_organization(source.organization_id)
                .await;
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

    async fn enforce_aml_for_source(
        &self,
        source: &OrganizationStakingFarmSource,
        changed_by_user_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        let Some(aml_gate) = &self.aml_gate else {
            return Ok(());
        };

        match aml_gate
            .check_near_account(
                changed_by_user_id,
                &source.near_account_id,
                AmlFlow::StakingFarmSync,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => Err(anyhow::Error::new(error)),
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
#[path = "staking_farm_tests.rs"]
mod tests;
