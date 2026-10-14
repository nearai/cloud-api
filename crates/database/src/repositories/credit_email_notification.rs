use crate::pool::DbPool;
use anyhow::Result;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CreditEmailCandidate {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub credit_type: String,
    pub remaining: i64,
    pub limit: i64,
    pub threshold_percent: i16,
    pub recipient_emails: Vec<String>,
}

#[derive(Clone)]
pub struct CreditEmailNotificationRepository {
    pool: DbPool,
    thresholds: Vec<i16>,
}

impl CreditEmailNotificationRepository {
    pub fn new(pool: DbPool, mut thresholds: Vec<i16>) -> Self {
        thresholds.retain(|threshold| (0..=100).contains(threshold));
        thresholds.sort_unstable();
        thresholds.dedup();
        Self { pool, thresholds }
    }

    pub async fn claim_crossings(&self) -> Result<Vec<CreditEmailCandidate>> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        let rows = transaction.query(r#"
            WITH balances AS (
                SELECT o.id, o.name, olh.credit_type, olh.spend_limit,
                       GREATEST(olh.spend_limit - COALESCE(occ.amount, 0), 0)::BIGINT AS remaining
                FROM organizations o
                JOIN organization_limits_history olh ON olh.organization_id = o.id AND olh.effective_until IS NULL
                LEFT JOIN organization_credit_consumption occ
                  ON occ.organization_id = o.id AND occ.credit_type = olh.credit_type
                WHERE o.is_active = true AND olh.spend_limit > 0
            )
            SELECT id, name, credit_type, spend_limit, remaining
            FROM balances
        "#, &[]).await?;
        let mut candidates = Vec::new();
        for row in rows {
            let organization_id: Uuid = row.get("id");
            let credit_type: String = row.get("credit_type");
            let remaining: i64 = row.get("remaining");
            let limit: i64 = row.get("spend_limit");
            let percent = if remaining == 0 {
                0
            } else {
                remaining.saturating_mul(100) / limit
            };
            let threshold = self
                .thresholds
                .iter()
                .copied()
                .find(|threshold| percent <= i64::from(*threshold));
            if threshold.is_none() {
                let resource_id = format!("{organization_id}:{credit_type}");
                transaction.execute("DELETE FROM email_notification_deliveries WHERE event_type = 'credit_balance' AND resource_id = $1", &[&resource_id]).await?;
                continue;
            }
            let threshold = threshold.unwrap();
            let resource_id = format!("{organization_id}:{credit_type}");
            transaction.execute("DELETE FROM email_notification_deliveries WHERE event_type = 'credit_balance' AND resource_id = $1 AND event_key::SMALLINT < $2", &[&resource_id, &threshold]).await?;
            let event_key = threshold.to_string();
            let recipients = transaction.query(r#"
                SELECT CASE WHEN u.contact_email_verified_at IS NOT NULL THEN u.contact_email ELSE u.email END AS email
                FROM organization_members om JOIN users u ON u.id = om.user_id
                WHERE om.organization_id = $1 AND om.role IN ('owner', 'admin') AND u.is_active = true
                  AND (u.contact_email_verified_at IS NOT NULL OR u.email NOT LIKE '%@near')
                ORDER BY lower(CASE WHEN u.contact_email_verified_at IS NOT NULL THEN u.contact_email ELSE u.email END)
            "#, &[&organization_id]).await?;
            let mut recipient_emails = Vec::new();
            for recipient in recipients {
                let recipient_email: String = recipient.get("email");
                let inserted = transaction.query_opt(
                    "INSERT INTO email_notification_deliveries (event_type, resource_id, event_key, recipient_email) VALUES ('credit_balance', $1, $2, $3) ON CONFLICT DO NOTHING RETURNING resource_id",
                    &[&resource_id, &event_key, &recipient_email],
                ).await?;
                if inserted.is_some() {
                    recipient_emails.push(recipient_email);
                }
            }
            if recipient_emails.is_empty() {
                continue;
            }
            candidates.push(CreditEmailCandidate {
                organization_id,
                organization_name: row.get("name"),
                credit_type,
                remaining,
                limit,
                threshold_percent: threshold,
                recipient_emails,
            });
        }
        transaction.commit().await?;
        Ok(candidates)
    }

    pub async fn release_claim(
        &self,
        organization_id: Uuid,
        credit_type: &str,
        threshold: i16,
        recipient_email: &str,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let resource_id = format!("{organization_id}:{credit_type}");
        let event_key = threshold.to_string();
        client.execute("DELETE FROM email_notification_deliveries WHERE event_type = 'credit_balance' AND resource_id = $1 AND event_key = $2 AND recipient_email = $3", &[&resource_id, &event_key, &recipient_email]).await?;
        Ok(())
    }
}
