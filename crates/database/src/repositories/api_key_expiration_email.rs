use crate::pool::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ApiKeyExpirationEmailCandidate {
    pub api_key_id: Uuid,
    pub key_name: String,
    pub key_prefix: String,
    pub workspace_name: String,
    pub organization_id: Uuid,
    pub organization_name: String,
    pub expires_at: DateTime<Utc>,
    pub interval_days: i16,
    pub recipient_emails: Vec<String>,
}

#[derive(Clone)]
pub struct ApiKeyExpirationEmailRepository {
    pool: DbPool,
    reminder_days: Vec<i16>,
}

impl ApiKeyExpirationEmailRepository {
    pub fn new(pool: DbPool, mut reminder_days: Vec<i16>) -> Self {
        reminder_days.retain(|days| *days > 0 && *days <= 365);
        reminder_days.sort_unstable();
        reminder_days.dedup();
        Self {
            pool,
            reminder_days,
        }
    }

    pub async fn claim_due(&self) -> Result<Vec<ApiKeyExpirationEmailCandidate>> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let max_days = i32::from(self.reminder_days.iter().copied().max().unwrap_or(7));
        let rows = tx.query(r#"
            SELECT k.id, k.name, k.key_prefix, k.expires_at, w.name AS workspace_name,
                   o.id AS organization_id, o.name AS organization_name
            FROM api_keys k JOIN workspaces w ON w.id = k.workspace_id
            JOIN organizations o ON o.id = w.organization_id
            WHERE k.expires_at IS NOT NULL AND k.expires_at <= NOW() + make_interval(days => $1)
              AND k.is_active = true AND k.deleted_at IS NULL AND w.is_active = true AND o.is_active = true
            ORDER BY k.expires_at
        "#, &[&max_days]).await?;
        let mut candidates = Vec::new();
        for row in rows {
            let key_id: Uuid = row.get("id");
            let expires_at: DateTime<Utc> = row.get("expires_at");
            let interval_days = if expires_at <= Utc::now() {
                0
            } else {
                let Some(days) = self.reminder_days.iter().copied().find(|days| {
                    expires_at <= Utc::now() + chrono::Duration::days(i64::from(*days))
                }) else {
                    continue;
                };
                days
            };
            let resource_id = key_id.to_string();
            let event_key = format!("{}:{interval_days}", expires_at.to_rfc3339());
            let organization_id: Uuid = row.get("organization_id");
            let recipients = tx.query(r#"
                SELECT DISTINCT CASE WHEN u.contact_email_verified_at IS NOT NULL THEN u.contact_email ELSE u.email END AS email
                FROM users u
                LEFT JOIN organization_members om ON om.user_id = u.id AND om.organization_id = $1
                WHERE u.is_active = true AND (u.id = (SELECT created_by_user_id FROM api_keys WHERE id = $2) OR om.role IN ('owner', 'admin'))
                  AND (u.contact_email_verified_at IS NOT NULL OR u.email NOT LIKE '%@near')
                ORDER BY email
            "#, &[&organization_id, &key_id]).await?;
            let mut recipient_emails = Vec::new();
            for recipient in recipients {
                let recipient_email: String = recipient.get("email");
                let claimed = tx.query_opt("INSERT INTO email_notification_deliveries (event_type, resource_id, event_key, recipient_email) VALUES ('api_key_expiration', $1, $2, $3) ON CONFLICT DO NOTHING RETURNING resource_id", &[&resource_id, &event_key, &recipient_email]).await?;
                if claimed.is_some() {
                    recipient_emails.push(recipient_email);
                }
            }
            if recipient_emails.is_empty() {
                continue;
            }
            candidates.push(ApiKeyExpirationEmailCandidate {
                api_key_id: key_id,
                key_name: row.get("name"),
                key_prefix: row.get("key_prefix"),
                workspace_name: row.get("workspace_name"),
                organization_id,
                organization_name: row.get("organization_name"),
                expires_at,
                interval_days,
                recipient_emails,
            });
        }
        tx.commit().await?;
        Ok(candidates)
    }

    pub async fn release_claim(
        &self,
        key_id: Uuid,
        expires_at: DateTime<Utc>,
        interval_days: i16,
        recipient_email: &str,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let resource_id = key_id.to_string();
        let event_key = format!("{}:{interval_days}", expires_at.to_rfc3339());
        client.execute("DELETE FROM email_notification_deliveries WHERE event_type = 'api_key_expiration' AND resource_id = $1 AND event_key = $2 AND recipient_email = $3", &[&resource_id, &event_key, &recipient_email]).await?;
        Ok(())
    }
}
