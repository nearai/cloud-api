#[allow(dead_code)]
mod support;

use database::repositories::OrganizationStakingFarmSourcesRepository;
use services::staking_farm::{
    StakingFarmRepository, StakingFarmSourceSyncUpdate, StakingSyncStatus,
};
use support::{cleanup_usage_fixtures, insert_org_fixture, test_pool};
use uuid::Uuid;

#[tokio::test]
async fn disconnected_source_on_inactive_org_is_not_written() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let source_id = Uuid::new_v4();
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO organization_staking_farm_sources (
                id, organization_id, near_account_id, network_id, contract_id,
                farm_product_id, credit_nano_usd_per_reward_unit, status, sync_status
            ) VALUES ($1, $2, $3, 'testnet', 'stake.test', 'cloud-credits', 1, 'disconnected', 'never_synced')",
            &[&source_id, &org.org_id, &format!("inactive-{}.test", source_id.simple())],
        )
        .await?;
    client
        .execute(
            "UPDATE organizations SET is_active = false WHERE id = $1",
            &[&org.org_id],
        )
        .await?;

    let repo = OrganizationStakingFarmSourcesRepository::new(pool.clone());
    let mut failures: Vec<String> = Vec::new();

    match repo.update_staking_farm_limit(org.org_id, 42, None).await {
        Ok(false) => {}
        Ok(true) => failures.push("limit write applied for inactive source".to_string()),
        Err(e) => failures.push(format!("limit write errored: {e}")),
    }
    match client
        .query_one(
            "SELECT count(*) FROM organization_limits_history
             WHERE organization_id = $1 AND credit_type = 'staking_farm'",
            &[&org.org_id],
        )
        .await
    {
        Ok(row) => {
            let count: i64 = row.get(0);
            if count != 0 {
                failures.push(format!("expected no history rows, found {count}"));
            }
        }
        Err(e) => failures.push(format!("history query failed: {e}")),
    }

    let update = StakingFarmSourceSyncUpdate {
        sync_status: StakingSyncStatus::Synced,
        last_sync_error: None,
        last_synced_accumulated_reward_units_24: Some("1".to_string()),
        last_synced_pending_reward_units_24: Some("1".to_string()),
        last_synced_reward_units_24: Some("1".to_string()),
        last_synced_credit_nano_usd: Some(42),
        active_positions: serde_json::json!([]),
    };
    match repo.update_sync_state(source_id, update).await {
        Ok(source) => {
            if source.sync_status != "never_synced" || source.last_synced_credit_nano_usd.is_some()
            {
                failures.push("update_sync_state modified a disconnected source".to_string());
            }
        }
        Err(e) => failures.push(format!("update_sync_state errored: {e}")),
    }

    client
        .execute(
            "DELETE FROM organization_staking_farm_sources WHERE id = $1",
            &[&source_id],
        )
        .await?;
    cleanup_usage_fixtures(&pool, &[org.org_id], &[]).await?;
    assert!(failures.is_empty(), "{failures:?}");
    Ok(())
}
