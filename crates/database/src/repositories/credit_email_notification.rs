use crate::pool::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use services::usage::ports::{
    CreditEmailClaimResult,
    CreditEmailNotificationRepository as CreditEmailNotificationRepositoryPort,
    CreditEmailOrganization, CreditEmailRecipient,
};
use uuid::Uuid;

use super::OrganizationLimitsRepository;

const EVENT_TYPE: &str = "credit_balance";
const RESOURCE_SCOPE: &str = "combined";

#[derive(Clone)]
pub struct CreditEmailNotificationRepository {
    pool: DbPool,
}

impl CreditEmailNotificationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn resource_id(organization_id: Uuid) -> String {
        format!("{organization_id}:{RESOURCE_SCOPE}")
    }
}

#[async_trait::async_trait]
impl CreditEmailNotificationRepositoryPort for CreditEmailNotificationRepository {
    async fn active_organizations(&self) -> Result<Vec<CreditEmailOrganization>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, name FROM organizations WHERE is_active = true ORDER BY id",
                &[],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| CreditEmailOrganization {
                id: row.get("id"),
                name: row.get("name"),
            })
            .collect())
    }

    async fn reconcile_claims(
        &self,
        organization_id: Uuid,
        snapshot_version: &str,
        threshold: Option<i16>,
        lower_thresholds: &[i16],
    ) -> Result<CreditEmailClaimResult> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        let active = transaction
            .query_opt(
                "SELECT id FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
                &[&organization_id],
            )
            .await?;
        if active.is_none() {
            transaction.rollback().await?;
            return Ok(CreditEmailClaimResult::SnapshotChanged);
        }
        let (_, _, _, current_version) = OrganizationLimitsRepository::load_current_credit_status(
            &*transaction,
            organization_id,
        )
        .await?;
        if current_version != snapshot_version {
            transaction.rollback().await?;
            return Ok(CreditEmailClaimResult::SnapshotChanged);
        }

        let resource_id = Self::resource_id(organization_id);
        let Some(threshold) = threshold else {
            transaction
                .execute(
                    r#"
                    UPDATE email_notification_deliveries
                    SET rearm_after_dispatch = TRUE
                    WHERE event_type = $1 AND resource_id = $2
                      AND sent_at IS NULL
                      AND dispatch_started_at IS NOT NULL
                      AND claimed_at >= NOW() - INTERVAL '30 minutes'
                    "#,
                    &[&EVENT_TYPE, &resource_id],
                )
                .await?;
            transaction
                .execute(
                    r#"
                    DELETE FROM email_notification_deliveries
                    WHERE event_type = $1 AND resource_id = $2
                      AND NOT (
                          sent_at IS NULL
                          AND dispatch_started_at IS NOT NULL
                          AND claimed_at >= NOW() - INTERVAL '30 minutes'
                      )
                    "#,
                    &[&EVENT_TYPE, &resource_id],
                )
                .await?;
            transaction.commit().await?;
            return Ok(CreditEmailClaimResult::Claimed(Vec::new()));
        };

        let mut recovered_from_lower_threshold = false;
        if !lower_thresholds.is_empty() {
            let event_keys = lower_thresholds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            recovered_from_lower_threshold = transaction
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM email_notification_deliveries WHERE event_type = $1 AND resource_id = $2 AND event_key = ANY($3))",
                    &[&EVENT_TYPE, &resource_id, &event_keys],
                )
                .await?
                .get(0);
            transaction
                .execute(
                    r#"
                    UPDATE email_notification_deliveries
                    SET rearm_after_dispatch = TRUE
                    WHERE event_type = $1 AND resource_id = $2 AND event_key = ANY($3)
                      AND sent_at IS NULL
                      AND dispatch_started_at IS NOT NULL
                      AND claimed_at >= NOW() - INTERVAL '30 minutes'
                    "#,
                    &[&EVENT_TYPE, &resource_id, &event_keys],
                )
                .await?;
            transaction
                .execute(
                    r#"
                    DELETE FROM email_notification_deliveries
                    WHERE event_type = $1 AND resource_id = $2 AND event_key = ANY($3)
                      AND NOT (
                          sent_at IS NULL
                          AND dispatch_started_at IS NOT NULL
                          AND claimed_at >= NOW() - INTERVAL '30 minutes'
                      )
                    "#,
                    &[&EVENT_TYPE, &resource_id, &event_keys],
                )
                .await?;
        }

        let rows = transaction
            .query(
                r#"
                SELECT u.id, u.email, u.contact_email, u.contact_email_verified_at
                FROM organization_members om
                JOIN users u ON u.id = om.user_id
                WHERE om.organization_id = $1
                  AND om.role IN ('owner', 'admin')
                  AND u.is_active = true
                ORDER BY lower(u.email)
                "#,
                &[&organization_id],
            )
            .await?;
        let mut claimed_recipients = Vec::new();
        for row in rows {
            let recipient_user_id = row.get("id");
            let claim_token = Uuid::new_v4();
            if recovered_from_lower_threshold {
                // Crossing a threshold upward re-arms the lower alerts, but must not
                // emit the current higher alert. Persist the suppression so the next
                // scan at the same balance does not mistake it for a downward crossing.
                transaction
                    .execute(
                        r#"
                INSERT INTO email_notification_deliveries
                    (event_type, resource_id, event_key, recipient_user_id, claim_token, sent_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                ON CONFLICT (event_type, resource_id, event_key, recipient_user_id)
                DO UPDATE SET claimed_at = NOW(), claim_token = EXCLUDED.claim_token,
                              sent_at = COALESCE(email_notification_deliveries.sent_at, NOW()),
                              dispatch_started_at = NULL, rearm_after_dispatch = FALSE
                WHERE NOT (
                    email_notification_deliveries.sent_at IS NULL
                    AND email_notification_deliveries.dispatch_started_at IS NOT NULL
                    AND email_notification_deliveries.claimed_at >= NOW() - INTERVAL '30 minutes'
                )
                "#,
                        &[
                            &EVENT_TYPE,
                            &resource_id,
                            &threshold.to_string(),
                            &recipient_user_id,
                            &claim_token,
                        ],
                    )
                    .await?;
                continue;
            }
            let claimed = transaction
                .query_opt(
                    r#"
                INSERT INTO email_notification_deliveries
                    (event_type, resource_id, event_key, recipient_user_id, claim_token)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (event_type, resource_id, event_key, recipient_user_id)
                DO UPDATE SET claimed_at = NOW(), claim_token = EXCLUDED.claim_token,
                              dispatch_started_at = NULL, rearm_after_dispatch = FALSE
                WHERE email_notification_deliveries.sent_at IS NULL
                  AND email_notification_deliveries.claimed_at < NOW() - INTERVAL '30 minutes'
                RETURNING recipient_user_id, claim_token
                "#,
                    &[
                        &EVENT_TYPE,
                        &resource_id,
                        &threshold.to_string(),
                        &recipient_user_id,
                        &claim_token,
                    ],
                )
                .await?;
            if claimed.is_some() {
                claimed_recipients.push(CreditEmailRecipient {
                    user_id: recipient_user_id,
                    claim_token,
                    email: row.get("email"),
                    contact_email: row.get("contact_email"),
                    contact_email_verified_at: row
                        .get::<_, Option<DateTime<Utc>>>("contact_email_verified_at"),
                });
            }
        }
        transaction.commit().await?;
        Ok(CreditEmailClaimResult::Claimed(claimed_recipients))
    }

    async fn begin_dispatch(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_user_id: Uuid,
        claim_token: Uuid,
    ) -> Result<bool> {
        let client = self.pool.get().await?;
        let updated = client
            .execute(
                r#"
                UPDATE email_notification_deliveries
                SET dispatch_started_at = NOW(), claimed_at = NOW()
                WHERE event_type = $1 AND resource_id = $2 AND event_key = $3
                  AND recipient_user_id = $4 AND claim_token = $5
                  AND sent_at IS NULL AND dispatch_started_at IS NULL
                "#,
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_user_id,
                    &claim_token,
                ],
            )
            .await?;
        Ok(updated == 1)
    }

    async fn release_claim(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_user_id: Uuid,
        claim_token: Uuid,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                r#"
                UPDATE email_notification_deliveries
                SET claimed_at = NOW() - INTERVAL '31 minutes',
                    dispatch_started_at = NULL
                WHERE event_type = $1 AND resource_id = $2 AND event_key = $3
                  AND recipient_user_id = $4 AND claim_token = $5
                  AND sent_at IS NULL
                "#,
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_user_id,
                    &claim_token,
                ],
            )
            .await?;
        Ok(())
    }

    async fn mark_sent(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_user_id: Uuid,
        claim_token: Uuid,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                r#"
                UPDATE email_notification_deliveries
                SET sent_at = CASE WHEN rearm_after_dispatch THEN NULL ELSE NOW() END,
                    claimed_at = CASE
                        WHEN rearm_after_dispatch THEN NOW() - INTERVAL '31 minutes'
                        ELSE claimed_at
                    END,
                    dispatch_started_at = NULL,
                    rearm_after_dispatch = FALSE
                WHERE event_type = $1 AND resource_id = $2 AND event_key = $3
                  AND recipient_user_id = $4 AND claim_token = $5
                  AND sent_at IS NULL AND dispatch_started_at IS NOT NULL
                "#,
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_user_id,
                    &claim_token,
                ],
            )
            .await?;
        Ok(())
    }
}
