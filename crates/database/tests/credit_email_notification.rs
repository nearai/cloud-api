#[allow(dead_code)]
mod support;

use database::repositories::credit_email_notification::CreditEmailNotificationRepository;
use services::usage::ports::CreditEmailNotificationRepository as CreditEmailNotificationRepositoryPort;
use support::test_pool;
use uuid::Uuid;

#[tokio::test]
async fn claims_deduplicate_recover_and_release() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool.clone());
    let organization_id = Uuid::new_v4();
    let recipient = "credit-claim@example.test";

    assert!(repository.try_claim(organization_id, 20, recipient).await?);
    assert!(!repository.try_claim(organization_id, 20, recipient).await?);

    pool.get()
        .await?
        .execute(
            "UPDATE email_notification_deliveries SET claimed_at = NOW() - INTERVAL '31 minutes' WHERE resource_id = $1",
            &[&format!("{organization_id}:combined")],
        )
        .await?;
    assert!(repository.try_claim(organization_id, 20, recipient).await?);

    repository.mark_sent(organization_id, 20, recipient).await?;
    pool.get()
        .await?
        .execute(
            "UPDATE email_notification_deliveries SET claimed_at = NOW() - INTERVAL '31 minutes' WHERE resource_id = $1",
            &[&format!("{organization_id}:combined")],
        )
        .await?;
    assert!(!repository.try_claim(organization_id, 20, recipient).await?);

    repository
        .release_claim(organization_id, 20, recipient)
        .await?;
    assert!(repository.try_claim(organization_id, 20, recipient).await?);
    repository.clear_claims(organization_id).await?;
    Ok(())
}

#[tokio::test]
async fn threshold_and_full_clears_rearm_claims() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = CreditEmailNotificationRepository::new(pool);
    let organization_id = Uuid::new_v4();
    let recipient = "credit-rearm@example.test";

    assert!(repository.try_claim(organization_id, 10, recipient).await?);
    assert!(repository.try_claim(organization_id, 20, recipient).await?);
    repository
        .clear_threshold_claims(organization_id, &[10])
        .await?;
    assert!(repository.try_claim(organization_id, 10, recipient).await?);
    assert!(!repository.try_claim(organization_id, 20, recipient).await?);

    repository.clear_claims(organization_id).await?;
    assert!(repository.try_claim(organization_id, 20, recipient).await?);
    repository.clear_claims(organization_id).await?;
    Ok(())
}
