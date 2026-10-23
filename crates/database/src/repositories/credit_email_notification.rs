use crate::pool::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use services::usage::ports::{
    CreditEmailNotificationRepository as CreditEmailNotificationRepositoryPort,
    CreditEmailOrganization, CreditEmailRecipient,
};
use uuid::Uuid;

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

    async fn owner_admin_recipients(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<CreditEmailRecipient>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                r#"
                SELECT u.email, u.contact_email, u.contact_email_verified_at
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
        Ok(rows
            .into_iter()
            .map(|row| CreditEmailRecipient {
                email: row.get("email"),
                contact_email: row.get("contact_email"),
                contact_email_verified_at: row
                    .get::<_, Option<DateTime<Utc>>>("contact_email_verified_at"),
            })
            .collect())
    }

    async fn clear_claims(&self, organization_id: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM email_notification_deliveries WHERE event_type = $1 AND resource_id = $2",
                &[&EVENT_TYPE, &Self::resource_id(organization_id)],
            )
            .await?;
        Ok(())
    }

    async fn clear_threshold_claims(
        &self,
        organization_id: Uuid,
        thresholds: &[i16],
    ) -> Result<()> {
        if thresholds.is_empty() {
            return Ok(());
        }
        let event_keys = thresholds
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM email_notification_deliveries WHERE event_type = $1 AND resource_id = $2 AND event_key = ANY($3)",
                &[&EVENT_TYPE, &Self::resource_id(organization_id), &event_keys],
            )
            .await?;
        Ok(())
    }

    async fn try_claim(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_email: &str,
    ) -> Result<bool> {
        let client = self.pool.get().await?;
        let claimed = client
            .query_opt(
                r#"
                INSERT INTO email_notification_deliveries
                    (event_type, resource_id, event_key, recipient_email)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (event_type, resource_id, event_key, recipient_email)
                DO UPDATE SET claimed_at = NOW()
                WHERE email_notification_deliveries.sent_at IS NULL
                  AND email_notification_deliveries.claimed_at < NOW() - INTERVAL '30 minutes'
                RETURNING resource_id
                "#,
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_email,
                ],
            )
            .await?;
        Ok(claimed.is_some())
    }

    async fn release_claim(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_email: &str,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "DELETE FROM email_notification_deliveries WHERE event_type = $1 AND resource_id = $2 AND event_key = $3 AND recipient_email = $4",
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_email,
                ],
            )
            .await?;
        Ok(())
    }

    async fn mark_sent(
        &self,
        organization_id: Uuid,
        threshold: i16,
        recipient_email: &str,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "UPDATE email_notification_deliveries SET sent_at = NOW() WHERE event_type = $1 AND resource_id = $2 AND event_key = $3 AND recipient_email = $4",
                &[
                    &EVENT_TYPE,
                    &Self::resource_id(organization_id),
                    &threshold.to_string(),
                    &recipient_email,
                ],
            )
            .await?;
        Ok(())
    }
}
