#[allow(dead_code)]
mod support;

use database::repositories::{
    credit_email_notification::CreditEmailNotificationRepository,
    OrganizationLimitsRepository as DatabaseOrganizationLimitsRepository,
};
use services::usage::ports::{
    CreditEmailClaimResult,
    CreditEmailNotificationRepository as CreditEmailNotificationRepositoryPort,
    OrganizationLimitsRepository as OrganizationLimitsRepositoryPort,
};
use support::test_pool;
use uuid::Uuid;

async fn insert_credit_fixture(
    pool: &database::DbPool,
    consumed: i64,
) -> anyhow::Result<(Uuid, Uuid)> {
    let client = pool.get().await?;
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let suffix = organization_id.simple().to_string();
    client
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id) VALUES ($1, $2, $3, 'test', $4)",
            &[
                &user_id,
                &format!("credit-alert-{suffix}@example.test"),
                &format!("credit-alert-{suffix}"),
                &format!("provider-{suffix}"),
            ],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organizations (id, name, is_active) VALUES ($1, $2, true)",
            &[&organization_id, &format!("credit-alert-{suffix}")],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&organization_id, &user_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_limits_history (organization_id, spend_limit, credit_type) VALUES ($1, 100, 'payment')",
            &[&organization_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_credit_consumption (organization_id, credit_type, amount) VALUES ($1, 'payment', $2)",
            &[&organization_id, &consumed],
        )
        .await?;
    Ok((organization_id, user_id))
}

async fn snapshot_version(
    pool: &database::DbPool,
    organization_id: Uuid,
) -> anyhow::Result<String> {
    let repository = DatabaseOrganizationLimitsRepository::new(pool.clone());
    Ok(repository
        .get_current_credit_snapshot(organization_id)
        .await?
        .version)
}

fn claimed(result: CreditEmailClaimResult) -> Vec<services::usage::ports::CreditEmailRecipient> {
    match result {
        CreditEmailClaimResult::Claimed(recipients) => recipients,
        CreditEmailClaimResult::SnapshotChanged => panic!("snapshot unexpectedly changed"),
    }
}

#[tokio::test]
async fn claims_deduplicate_recover_and_release() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let version = snapshot_version(&pool, organization_id).await?;

    let initial_claim = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    assert_eq!(initial_claim.len(), 1);
    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?
    )
    .is_empty());

    pool.get()
        .await?
        .execute(
            "UPDATE email_notification_deliveries SET claimed_at = NOW() - INTERVAL '31 minutes' WHERE resource_id = $1",
            &[&format!("{organization_id}:combined")],
        )
        .await?;
    let reclaimed = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    assert_eq!(reclaimed.len(), 1);
    assert_ne!(reclaimed[0].claim_token, initial_claim[0].claim_token);
    let claim_token = reclaimed[0].claim_token;

    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, claim_token)
            .await?
    );
    repository
        .mark_sent(organization_id, 10, user_id, claim_token)
        .await?;
    pool.get()
        .await?
        .execute(
            "UPDATE email_notification_deliveries SET claimed_at = NOW() - INTERVAL '31 minutes' WHERE resource_id = $1",
            &[&format!("{organization_id}:combined")],
        )
        .await?;
    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?
    )
    .is_empty());

    repository
        .reconcile_claims(organization_id, &version, None, &[])
        .await?;
    let retryable = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    assert_eq!(retryable.len(), 1);
    let released_token = retryable[0].claim_token;
    repository
        .release_claim(organization_id, 10, user_id, released_token)
        .await?;
    let retried = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    assert_eq!(retried.len(), 1);
    assert_ne!(retried[0].claim_token, released_token);
    repository
        .reconcile_claims(organization_id, &version, None, &[])
        .await?;
    Ok(())
}

#[tokio::test]
async fn stale_worker_cannot_release_or_complete_a_reclaimed_delivery() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let version = snapshot_version(&pool, organization_id).await?;

    let first_claim = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    let first_token = first_claim[0].claim_token;
    pool.get()
        .await?
        .execute(
            "UPDATE email_notification_deliveries SET claimed_at = NOW() - INTERVAL '31 minutes' WHERE resource_id = $1",
            &[&format!("{organization_id}:combined")],
        )
        .await?;
    let second_claim = claimed(
        repository
            .reconcile_claims(organization_id, &version, Some(10), &[])
            .await?,
    );
    let second_token = second_claim[0].claim_token;
    assert_ne!(first_token, second_token);

    assert!(
        !repository
            .begin_dispatch(organization_id, 10, user_id, first_token)
            .await?
    );
    repository
        .release_claim(organization_id, 10, user_id, first_token)
        .await?;
    repository
        .mark_sent(organization_id, 10, user_id, first_token)
        .await?;
    let row = pool
        .get()
        .await?
        .query_one(
            "SELECT claim_token, sent_at IS NOT NULL AS sent FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10' AND recipient_user_id = $2",
            &[&format!("{organization_id}:combined"), &user_id],
        )
        .await?;
    assert_eq!(row.get::<_, Uuid>("claim_token"), second_token);
    assert!(!row.get::<_, bool>("sent"));

    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, second_token)
            .await?
    );
    repository
        .mark_sent(organization_id, 10, user_id, second_token)
        .await?;
    let sent: bool = pool
        .get()
        .await?
        .query_one(
            "SELECT sent_at IS NOT NULL FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10' AND recipient_user_id = $2",
            &[&format!("{organization_id}:combined"), &user_id],
        )
        .await?
        .get(0);
    assert!(sent);
    Ok(())
}

#[tokio::test]
async fn stale_snapshot_cannot_remove_a_newer_threshold_claim() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 85).await?;
    let stale_version = snapshot_version(&pool, organization_id).await?;

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 90, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let current_version = snapshot_version(&pool, organization_id).await?;
    assert_ne!(stale_version, current_version);
    let current_claim = claimed(
        repository
            .reconcile_claims(organization_id, &current_version, Some(10), &[])
            .await?,
    );
    assert_eq!(current_claim.len(), 1);
    let claim_token = current_claim[0].claim_token;
    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, claim_token)
            .await?
    );
    repository
        .mark_sent(organization_id, 10, user_id, claim_token)
        .await?;

    assert_eq!(
        repository
            .reconcile_claims(organization_id, &stale_version, Some(20), &[0, 10])
            .await?,
        CreditEmailClaimResult::SnapshotChanged
    );
    let count: i64 = pool
        .get()
        .await?
        .query_one(
            "SELECT COUNT(*) FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10' AND sent_at IS NOT NULL",
            &[&format!("{organization_id}:combined")],
        )
        .await?
        .get(0);
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn partial_replenishment_rearms_lower_alert_without_sending_higher_alert(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;

    let low_version = snapshot_version(&pool, organization_id).await?;
    let low_claim = claimed(
        repository
            .reconcile_claims(organization_id, &low_version, Some(10), &[0])
            .await?,
    );
    assert_eq!(low_claim.len(), 1);
    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, low_claim[0].claim_token,)
            .await?
    );
    repository
        .mark_sent(organization_id, 10, user_id, low_claim[0].claim_token)
        .await?;

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 85, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let recovered_version = snapshot_version(&pool, organization_id).await?;

    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &recovered_version, Some(20), &[0, 10],)
            .await?
    )
    .is_empty());
    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &recovered_version, Some(20), &[0, 10],)
            .await?
    )
    .is_empty());

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 90, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let low_again_version = snapshot_version(&pool, organization_id).await?;
    assert_eq!(
        claimed(
            repository
                .reconcile_claims(organization_id, &low_again_version, Some(10), &[0])
                .await?
        )
        .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn failed_lower_alert_still_suppresses_a_higher_alert_during_replenishment(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let low_version = snapshot_version(&pool, organization_id).await?;
    let low_claim = claimed(
        repository
            .reconcile_claims(organization_id, &low_version, Some(10), &[0])
            .await?,
    );
    let failed_token = low_claim[0].claim_token;
    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, failed_token)
            .await?
    );
    repository
        .release_claim(organization_id, 10, user_id, failed_token)
        .await?;

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 85, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let recovered_version = snapshot_version(&pool, organization_id).await?;
    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &recovered_version, Some(20), &[0, 10],)
            .await?
    )
    .is_empty());

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 90, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let low_again_version = snapshot_version(&pool, organization_id).await?;
    let low_again = claimed(
        repository
            .reconcile_claims(organization_id, &low_again_version, Some(10), &[0])
            .await?,
    );
    assert_eq!(low_again.len(), 1);
    assert_eq!(low_again[0].user_id, user_id);
    Ok(())
}

#[tokio::test]
async fn partial_replenishment_does_not_revoke_an_active_dispatch() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let low_version = snapshot_version(&pool, organization_id).await?;
    let low_claim = claimed(
        repository
            .reconcile_claims(organization_id, &low_version, Some(10), &[0])
            .await?,
    );
    let claim_token = low_claim[0].claim_token;
    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, claim_token)
            .await?
    );

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 85, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let recovered_version = snapshot_version(&pool, organization_id).await?;
    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &recovered_version, Some(20), &[0, 10],)
            .await?
    )
    .is_empty());

    let active: bool = pool
        .get()
        .await?
        .query_one(
            "SELECT dispatch_started_at IS NOT NULL AND sent_at IS NULL FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10' AND recipient_user_id = $2 AND claim_token = $3",
            &[&format!("{organization_id}:combined"), &user_id, &claim_token],
        )
        .await?
        .get(0);
    assert!(active);
    repository
        .mark_sent(organization_id, 10, user_id, claim_token)
        .await?;

    assert!(claimed(
        repository
            .reconcile_claims(organization_id, &recovered_version, Some(20), &[0, 10],)
            .await?
    )
    .is_empty());
    let lower_claims: i64 = pool
        .get()
        .await?
        .query_one(
            "SELECT COUNT(*) FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10'",
            &[&format!("{organization_id}:combined")],
        )
        .await?
        .get(0);
    assert_eq!(lower_claims, 0);
    Ok(())
}

#[tokio::test]
async fn full_replenishment_does_not_revoke_an_active_dispatch() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let low_version = snapshot_version(&pool, organization_id).await?;
    let claim = claimed(
        repository
            .reconcile_claims(organization_id, &low_version, Some(10), &[0])
            .await?,
    );
    let claim_token = claim[0].claim_token;
    assert!(
        repository
            .begin_dispatch(organization_id, 10, user_id, claim_token)
            .await?
    );

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 0, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let recovered_version = snapshot_version(&pool, organization_id).await?;
    repository
        .reconcile_claims(organization_id, &recovered_version, None, &[])
        .await?;

    let active_and_rearmed: bool = pool
        .get()
        .await?
        .query_one(
            "SELECT dispatch_started_at IS NOT NULL AND sent_at IS NULL AND rearm_after_dispatch FROM email_notification_deliveries WHERE resource_id = $1 AND event_key = '10' AND recipient_user_id = $2 AND claim_token = $3",
            &[&format!("{organization_id}:combined"), &user_id, &claim_token],
        )
        .await?
        .get(0);
    assert!(active_and_rearmed);
    repository
        .mark_sent(organization_id, 10, user_id, claim_token)
        .await?;

    pool.get()
        .await?
        .execute(
            "UPDATE organization_credit_consumption SET amount = 90, updated_at = clock_timestamp() WHERE organization_id = $1 AND credit_type = 'payment'",
            &[&organization_id],
        )
        .await?;
    let low_again_version = snapshot_version(&pool, organization_id).await?;
    let low_again = claimed(
        repository
            .reconcile_claims(organization_id, &low_again_version, Some(10), &[0])
            .await?,
    );
    assert_eq!(low_again.len(), 1);
    assert_eq!(low_again[0].user_id, user_id);
    Ok(())
}

#[tokio::test]
async fn claims_store_user_identity_and_are_removed_with_the_user() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let (organization_id, user_id) = insert_credit_fixture(&pool, 90).await?;
    let version = snapshot_version(&pool, organization_id).await?;
    assert_eq!(
        claimed(
            repository
                .reconcile_claims(organization_id, &version, Some(10), &[])
                .await?
        )[0]
        .user_id,
        user_id
    );

    let client = pool.get().await?;
    let recipient_email_column_exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'email_notification_deliveries' AND column_name = 'recipient_email')",
            &[],
        )
        .await?
        .get(0);
    assert!(!recipient_email_column_exists);
    client
        .execute("DELETE FROM users WHERE id = $1", &[&user_id])
        .await?;
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM email_notification_deliveries WHERE recipient_user_id = $1",
            &[&user_id],
        )
        .await?
        .get(0);
    assert_eq!(count, 0);
    Ok(())
}
