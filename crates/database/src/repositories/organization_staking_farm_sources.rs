use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::common::RepositoryError;
use services::staking_farm::{
    OrganizationStakingFarmSource, StakingFarmRepository, StakingFarmSourceConflict,
    StakingFarmSourceStatus, StakingFarmSourceSyncUpdate, StakingSyncStatus,
    UpsertStakingFarmSourceRequest, CREDIT_SOURCE_HOUSE_OF_STAKE, CREDIT_TYPE_STAKING_FARM,
};
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OrganizationStakingFarmSourcesRepository {
    pool: DbPool,
}

impl OrganizationStakingFarmSourcesRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_source(row: &Row) -> OrganizationStakingFarmSource {
        OrganizationStakingFarmSource {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            near_account_id: row.get("near_account_id"),
            network_id: row.get("network_id"),
            contract_id: row.get("contract_id"),
            farm_product_id: row.get("farm_product_id"),
            farm_price_id: row.get("farm_price_id"),
            credit_nano_usd_per_reward_unit: row.get("credit_nano_usd_per_reward_unit"),
            status: row.get("status"),
            sync_status: row.get("sync_status"),
            last_sync_error: row.get("last_sync_error"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            last_synced_at: row.get("last_synced_at"),
            last_synced_accumulated_reward_units_24: row
                .get("last_synced_accumulated_reward_units_24"),
            last_synced_pending_reward_units_24: row.get("last_synced_pending_reward_units_24"),
            last_synced_reward_units_24: row.get("last_synced_reward_units_24"),
            last_synced_credit_nano_usd: row.get("last_synced_credit_nano_usd"),
            active_positions: row.get("active_positions"),
        }
    }
}

#[async_trait]
impl StakingFarmRepository for OrganizationStakingFarmSourcesRepository {
    async fn upsert_source(
        &self,
        request: UpsertStakingFarmSourceRequest,
    ) -> Result<OrganizationStakingFarmSource> {
        let row = retry_db!("upsert_organization_staking_farm_source", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    INSERT INTO organization_staking_farm_sources (
                        id,
                        organization_id,
                        near_account_id,
                        network_id,
                        contract_id,
                        farm_product_id,
                        farm_price_id,
                        credit_nano_usd_per_reward_unit,
                        status,
                        sync_status,
                        created_by_user_id,
                        created_at,
                        updated_at
                    )
                    VALUES (
                        uuid_generate_v4(), $1, $2, $3, $4, $5, $6, $7,
                        'active', 'never_synced', $8, now(), now()
                    )
                    ON CONFLICT (near_account_id, network_id, contract_id)
                    DO UPDATE SET
                        organization_id = CASE
                            WHEN organization_staking_farm_sources.organization_id = EXCLUDED.organization_id
                            THEN EXCLUDED.organization_id
                            ELSE organization_staking_farm_sources.organization_id
                        END,
                        farm_product_id = EXCLUDED.farm_product_id,
                        farm_price_id = EXCLUDED.farm_price_id,
                        credit_nano_usd_per_reward_unit = EXCLUDED.credit_nano_usd_per_reward_unit,
                        status = 'active',
                        updated_at = now()
                    WHERE organization_staking_farm_sources.organization_id = EXCLUDED.organization_id
                    RETURNING id, organization_id, near_account_id, network_id, contract_id,
                              farm_product_id, farm_price_id, credit_nano_usd_per_reward_unit,
                              status, sync_status, last_sync_error, created_by_user_id,
                              created_at, updated_at, last_synced_at,
                              last_synced_accumulated_reward_units_24::text AS last_synced_accumulated_reward_units_24,
                              last_synced_pending_reward_units_24::text AS last_synced_pending_reward_units_24,
                              last_synced_reward_units_24::text AS last_synced_reward_units_24,
                              last_synced_credit_nano_usd, active_positions
                    "#,
                    &[
                        &request.organization_id,
                        &request.near_account_id,
                        &request.network_id,
                        &request.contract_id,
                        &request.farm_product_id,
                        &request.farm_price_id,
                        &request.credit_nano_usd_per_reward_unit,
                        &request.created_by_user_id,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        let row = row.ok_or_else(|| anyhow::anyhow!(StakingFarmSourceConflict))?;
        Ok(Self::row_to_source(&row))
    }

    async fn get_source_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationStakingFarmSource>> {
        let row = retry_db!("get_organization_staking_farm_source", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
                    SELECT id, organization_id, near_account_id, network_id, contract_id,
                           farm_product_id, farm_price_id, credit_nano_usd_per_reward_unit,
                           status, sync_status, last_sync_error, created_by_user_id,
                           created_at, updated_at, last_synced_at,
                           last_synced_accumulated_reward_units_24::text AS last_synced_accumulated_reward_units_24,
                           last_synced_pending_reward_units_24::text AS last_synced_pending_reward_units_24,
                           last_synced_reward_units_24::text AS last_synced_reward_units_24,
                           last_synced_credit_nano_usd, active_positions
                    FROM organization_staking_farm_sources
                    WHERE organization_id = $1
                    ORDER BY created_at ASC
                    LIMIT 1
                    "#,
                    &[&organization_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(|row| Self::row_to_source(&row)))
    }

    async fn update_sync_state(
        &self,
        source_id: Uuid,
        update: StakingFarmSourceSyncUpdate,
    ) -> Result<OrganizationStakingFarmSource> {
        let row = retry_db!("update_organization_staking_farm_source_sync_state", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    UPDATE organization_staking_farm_sources
                    SET sync_status = $1,
                        last_sync_error = $2,
                        last_synced_at = CASE WHEN $1 = 'synced' THEN now() ELSE last_synced_at END,
                        last_synced_accumulated_reward_units_24 = $3::numeric,
                        last_synced_pending_reward_units_24 = $4::numeric,
                        last_synced_reward_units_24 = $5::numeric,
                        last_synced_credit_nano_usd = $6,
                        active_positions = $7,
                        updated_at = now()
                    WHERE id = $8
                    RETURNING id, organization_id, near_account_id, network_id, contract_id,
                              farm_product_id, farm_price_id, credit_nano_usd_per_reward_unit,
                              status, sync_status, last_sync_error, created_by_user_id,
                              created_at, updated_at, last_synced_at,
                              last_synced_accumulated_reward_units_24::text AS last_synced_accumulated_reward_units_24,
                              last_synced_pending_reward_units_24::text AS last_synced_pending_reward_units_24,
                              last_synced_reward_units_24::text AS last_synced_reward_units_24,
                              last_synced_credit_nano_usd, active_positions
                    "#,
                    &[
                        &update.sync_status.as_str(),
                        &update.last_sync_error,
                        &update.last_synced_accumulated_reward_units_24,
                        &update.last_synced_pending_reward_units_24,
                        &update.last_synced_reward_units_24,
                        &update.last_synced_credit_nano_usd,
                        &update.active_positions,
                        &source_id,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(Self::row_to_source(&row))
    }

    async fn update_staking_farm_limit(
        &self,
        organization_id: Uuid,
        credit_nano_usd: i64,
        changed_by_user_id: Option<Uuid>,
    ) -> Result<()> {
        retry_db!("update_staking_farm_organization_limit", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;
            let now = Utc::now();
            let advisory_key = format!("{organization_id}:{CREDIT_TYPE_STAKING_FARM}");

            transaction
                .query_one(
                    "SELECT pg_advisory_xact_lock(hashtext($1))",
                    &[&advisory_key],
                )
                .await
                .map_err(map_db_error)?;

            transaction
                .execute(
                    r#"
                    UPDATE organization_limits_history
                    SET effective_until = $1
                    WHERE organization_id = $2
                      AND credit_type = $3
                      AND effective_until IS NULL
                    "#,
                    &[&now, &organization_id, &CREDIT_TYPE_STAKING_FARM],
                )
                .await
                .map_err(map_db_error)?;

            transaction
                .execute(
                    r#"
                    INSERT INTO organization_limits_history (
                        organization_id,
                        spend_limit,
                        credit_type,
                        source,
                        currency,
                        effective_from,
                        changed_by,
                        change_reason,
                        changed_by_user_id
                    ) VALUES ($1, $2, $3, $4, 'USD', $5, $6, $7, $8)
                    "#,
                    &[
                        &organization_id,
                        &credit_nano_usd,
                        &CREDIT_TYPE_STAKING_FARM,
                        &Some(CREDIT_SOURCE_HOUSE_OF_STAKE.to_string()),
                        &now,
                        &Some("staking_farm_sync".to_string()),
                        &Some("House of Stake farm reward unit sync".to_string()),
                        &changed_by_user_id,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;
            Ok::<(), RepositoryError>(())
        })?;

        Ok(())
    }
}

pub fn parse_source_status(value: &str) -> Option<StakingFarmSourceStatus> {
    match value {
        "active" => Some(StakingFarmSourceStatus::Active),
        "disconnected" => Some(StakingFarmSourceStatus::Disconnected),
        _ => None,
    }
}

pub fn parse_sync_status(value: &str) -> Option<StakingSyncStatus> {
    match value {
        "never_synced" => Some(StakingSyncStatus::NeverSynced),
        "synced" => Some(StakingSyncStatus::Synced),
        "stale" => Some(StakingSyncStatus::Stale),
        "failed" => Some(StakingSyncStatus::Failed),
        _ => None,
    }
}
