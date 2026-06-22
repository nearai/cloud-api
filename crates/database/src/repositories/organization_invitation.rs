use crate::models::{
    InvitationEmailStatus, InvitationStatus, OrganizationInvitation, OrganizationRole,
};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use services::common::RepositoryError;
use services::organization::ports::{
    CreateInvitationRequest, InvitationEmailDeliveryFilters,
    InvitationEmailStatus as ServicesInvitationEmailStatus,
    InvitationStatus as ServicesInvitationStatus, OrganizationInvitation as ServicesInvitation,
    OrganizationInvitationEmailDelivery, OrganizationInvitationRepository,
    OrganizationInvitationWithDetails as ServicesInvitationWithDetails,
};
use tracing::debug;
use uuid::Uuid;

pub struct PgOrganizationInvitationRepository {
    pool: DbPool,
}

impl PgOrganizationInvitationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Convert database invitation to domain invitation
    fn db_to_domain(&self, db_inv: OrganizationInvitation) -> Result<ServicesInvitation> {
        let role = match db_inv.role {
            OrganizationRole::Owner => services::organization::MemberRole::Owner,
            OrganizationRole::Admin => services::organization::MemberRole::Admin,
            OrganizationRole::Member => services::organization::MemberRole::Member,
        };

        let status = self.db_to_domain_status(db_inv.status);
        let email_status = self.db_to_domain_email_status(db_inv.email_status);

        Ok(ServicesInvitation {
            id: db_inv.id,
            organization_id: services::organization::OrganizationId(db_inv.organization_id),
            email: db_inv.email,
            role,
            invited_by_user_id: services::auth::UserId(db_inv.invited_by_user_id),
            status,
            token: db_inv.token,
            created_at: db_inv.created_at,
            expires_at: db_inv.expires_at,
            responded_at: db_inv.responded_at,
            email_status,
            email_sent_at: db_inv.email_sent_at,
            email_last_error: db_inv.email_last_error,
            email_message_id: db_inv.email_message_id,
        })
    }

    fn row_to_db_invitation(&self, row: &tokio_postgres::Row) -> Result<OrganizationInvitation> {
        Ok(OrganizationInvitation {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            email: row.get("email"),
            role: serde_json::from_value(serde_json::json!(row.get::<_, String>("role")))?,
            invited_by_user_id: row.get("invited_by_user_id"),
            status: serde_json::from_value(serde_json::json!(row.get::<_, String>("status")))?,
            token: row.get("token"),
            created_at: row.get("created_at"),
            expires_at: row.get("expires_at"),
            responded_at: row.get("responded_at"),
            email_status: serde_json::from_value(serde_json::json!(
                row.get::<_, String>("email_status")
            ))?,
            email_sent_at: row.get("email_sent_at"),
            email_last_error: row.get("email_last_error"),
            email_message_id: row.get("email_message_id"),
        })
    }

    fn row_to_domain_invitation_with_details(
        &self,
        row: &tokio_postgres::Row,
    ) -> Result<ServicesInvitationWithDetails> {
        let db_inv = self.row_to_db_invitation(row)?;

        Ok(ServicesInvitationWithDetails {
            invitation: self.db_to_domain(db_inv)?,
            organization_name: row.get("organization_name"),
            invited_by_display_name: row.get("invited_by_display_name"),
        })
    }

    fn row_to_domain_email_delivery(
        &self,
        row: &tokio_postgres::Row,
    ) -> Result<OrganizationInvitationEmailDelivery> {
        let db_inv = self.row_to_db_invitation(row)?;

        Ok(OrganizationInvitationEmailDelivery {
            invitation: self.db_to_domain(db_inv)?,
            organization_name: row.get("organization_name"),
            invited_by_email: row.get("invited_by_email"),
            invited_by_display_name: row.get("invited_by_display_name"),
        })
    }

    fn db_to_domain_status(&self, status: InvitationStatus) -> ServicesInvitationStatus {
        match status {
            InvitationStatus::Pending => ServicesInvitationStatus::Pending,
            InvitationStatus::Accepted => ServicesInvitationStatus::Accepted,
            InvitationStatus::Declined => ServicesInvitationStatus::Declined,
            InvitationStatus::Expired => ServicesInvitationStatus::Expired,
        }
    }

    fn db_to_domain_email_status(
        &self,
        status: InvitationEmailStatus,
    ) -> ServicesInvitationEmailStatus {
        match status {
            InvitationEmailStatus::NotAttempted => ServicesInvitationEmailStatus::NotAttempted,
            InvitationEmailStatus::Sent => ServicesInvitationEmailStatus::Sent,
            InvitationEmailStatus::Failed => ServicesInvitationEmailStatus::Failed,
            InvitationEmailStatus::Skipped => ServicesInvitationEmailStatus::Skipped,
        }
    }

    fn domain_to_db_email_status(
        &self,
        status: ServicesInvitationEmailStatus,
    ) -> InvitationEmailStatus {
        match status {
            ServicesInvitationEmailStatus::NotAttempted => InvitationEmailStatus::NotAttempted,
            ServicesInvitationEmailStatus::Sent => InvitationEmailStatus::Sent,
            ServicesInvitationEmailStatus::Failed => InvitationEmailStatus::Failed,
            ServicesInvitationEmailStatus::Skipped => InvitationEmailStatus::Skipped,
        }
    }

    /// Convert domain role to database role
    fn domain_to_db_role(&self, role: services::organization::MemberRole) -> OrganizationRole {
        match role {
            services::organization::MemberRole::Owner => OrganizationRole::Owner,
            services::organization::MemberRole::Admin => OrganizationRole::Admin,
            services::organization::MemberRole::Member => OrganizationRole::Member,
        }
    }

    /// Convert domain status to database status
    fn domain_to_db_status(&self, status: ServicesInvitationStatus) -> InvitationStatus {
        match status {
            ServicesInvitationStatus::Pending => InvitationStatus::Pending,
            ServicesInvitationStatus::Accepted => InvitationStatus::Accepted,
            ServicesInvitationStatus::Declined => InvitationStatus::Declined,
            ServicesInvitationStatus::Expired => InvitationStatus::Expired,
        }
    }

    /// Generate a secure random token
    fn generate_token() -> String {
        use rand::RngExt;
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut rng = rand::rng();
        let token: String = (0..64)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect();
        token
    }
}

#[async_trait]
impl OrganizationInvitationRepository for PgOrganizationInvitationRepository {
    async fn create(
        &self,
        org_id: Uuid,
        request: CreateInvitationRequest,
        invited_by: Uuid,
    ) -> Result<ServicesInvitation> {
        let token = Self::generate_token();
        let role = self.domain_to_db_role(request.role);
        let expires_at = Utc::now() + Duration::hours(request.expires_in_hours);

        debug!(
            "Creating invitation for organization {} with role {}",
            org_id, role
        );

        let row = retry_db!("create_organization_invitation", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            // First, cancel any existing pending invitations for this email+org
            client
                .execute(
                    "UPDATE organization_invitations
                     SET status = 'expired'
                     WHERE organization_id = $1 AND email = $2 AND status = 'pending'",
                    &[&org_id, &request.email],
                )
                .await
                .map_err(map_db_error)?;

            // Create new invitation
            client
                .query_one(
                    "INSERT INTO organization_invitations
                     (organization_id, email, role, invited_by_user_id, token, expires_at)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     RETURNING id, organization_id, email, role, invited_by_user_id, status, token,
                               created_at, expires_at, responded_at, email_status, email_sent_at,
                               email_last_error, email_message_id",
                    &[
                        &org_id,
                        &request.email,
                        &role.to_string(),
                        &invited_by,
                        &token,
                        &expires_at,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = self.row_to_db_invitation(&row)?;

        self.db_to_domain(db_inv)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<ServicesInvitation>> {
        let row = retry_db!("get_organization_invitation_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                     FROM organization_invitations
                     WHERE id = $1",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(r) => {
                let db_inv = self.row_to_db_invitation(&r)?;
                Ok(Some(self.db_to_domain(db_inv)?))
            }
            None => Ok(None),
        }
    }

    async fn get_by_token(&self, token: &str) -> Result<Option<ServicesInvitation>> {
        let row = retry_db!("get_organization_invitation_by_token", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                     FROM organization_invitations
                     WHERE token = $1",
                    &[&token],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(r) => {
                let db_inv = self.row_to_db_invitation(&r)?;
                Ok(Some(self.db_to_domain(db_inv)?))
            }
            None => Ok(None),
        }
    }

    async fn list_by_organization(
        &self,
        org_id: Uuid,
        status: Option<ServicesInvitationStatus>,
    ) -> Result<Vec<ServicesInvitation>> {
        let db_status = status.map(|s| self.domain_to_db_status(s));

        let rows = retry_db!("list_organization_invitations_by_org", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            if let Some(ref db_status) = db_status {
                client
                    .query(
                        "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                         FROM organization_invitations
                         WHERE organization_id = $1 AND status = $2
                         ORDER BY created_at DESC",
                        &[&org_id, &db_status.to_string()],
                    )
                    .await
                    .map_err(map_db_error)
            } else {
                client
                    .query(
                        "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                         FROM organization_invitations
                         WHERE organization_id = $1
                         ORDER BY created_at DESC",
                        &[&org_id],
                    )
                    .await
                    .map_err(map_db_error)
            }
        })?;

        let mut invitations = Vec::new();
        for r in rows {
            let db_inv = self.row_to_db_invitation(&r)?;
            invitations.push(self.db_to_domain(db_inv)?);
        }

        Ok(invitations)
    }

    async fn list_by_email(
        &self,
        email: &str,
        status: Option<ServicesInvitationStatus>,
    ) -> Result<Vec<ServicesInvitation>> {
        let db_status = status.map(|s| self.domain_to_db_status(s));

        let rows = retry_db!("list_organization_invitations_by_email", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            if let Some(ref db_status) = db_status {
                client
                    .query(
                        "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                         FROM organization_invitations
                         WHERE email = $1 AND status = $2
                         ORDER BY created_at DESC",
                        &[&email, &db_status.to_string()],
                    )
                    .await
                    .map_err(map_db_error)
            } else {
                client
                    .query(
                        "SELECT id, organization_id, email, role, invited_by_user_id, status, token,
                            created_at, expires_at, responded_at, email_status, email_sent_at,
                            email_last_error, email_message_id
                         FROM organization_invitations
                         WHERE email = $1
                         ORDER BY created_at DESC",
                        &[&email],
                    )
                    .await
                    .map_err(map_db_error)
            }
        })?;

        let mut invitations = Vec::new();
        for r in rows {
            let db_inv = self.row_to_db_invitation(&r)?;
            invitations.push(self.db_to_domain(db_inv)?);
        }

        Ok(invitations)
    }

    async fn list_by_email_with_details(
        &self,
        email: &str,
        status: Option<ServicesInvitationStatus>,
    ) -> Result<Vec<ServicesInvitationWithDetails>> {
        let db_status = status.map(|s| self.domain_to_db_status(s));

        let rows = retry_db!("list_organization_invitations_by_email_with_details", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            if let Some(ref db_status) = db_status {
                client
                    .query(
                        "SELECT i.id, i.organization_id, i.email, i.role, i.invited_by_user_id,
                            i.status, i.token, i.created_at, i.expires_at, i.responded_at,
                            i.email_status, i.email_sent_at, i.email_last_error,
                            i.email_message_id, o.name AS organization_name,
                            u.display_name AS invited_by_display_name
                         FROM organization_invitations i
                         JOIN organizations o ON o.id = i.organization_id
                         LEFT JOIN users u ON u.id = i.invited_by_user_id
                         WHERE i.email = $1 AND i.status = $2
                         ORDER BY i.created_at DESC",
                        &[&email, &db_status.to_string()],
                    )
                    .await
                    .map_err(map_db_error)
            } else {
                client
                    .query(
                        "SELECT i.id, i.organization_id, i.email, i.role, i.invited_by_user_id,
                            i.status, i.token, i.created_at, i.expires_at, i.responded_at,
                            i.email_status, i.email_sent_at, i.email_last_error,
                            i.email_message_id, o.name AS organization_name,
                            u.display_name AS invited_by_display_name
                         FROM organization_invitations i
                         JOIN organizations o ON o.id = i.organization_id
                         LEFT JOIN users u ON u.id = i.invited_by_user_id
                         WHERE i.email = $1
                         ORDER BY i.created_at DESC",
                        &[&email],
                    )
                    .await
                    .map_err(map_db_error)
            }
        })?;

        let mut invitations = Vec::new();
        for r in rows {
            invitations.push(self.row_to_domain_invitation_with_details(&r)?);
        }

        Ok(invitations)
    }

    async fn list_email_deliveries(
        &self,
        filters: InvitationEmailDeliveryFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationInvitationEmailDelivery>, i64)> {
        let organization_id = filters.organization_id.map(|id| id.0);
        let recipient_email = filters.recipient_email;
        let email_status = filters
            .email_status
            .map(|status| self.domain_to_db_email_status(status).to_string());
        let invitation_status = filters
            .invitation_status
            .map(|status| self.domain_to_db_status(status).to_string());
        let created_after = filters.created_after;
        let created_before = filters.created_before;

        let rows = retry_db!("list_organization_invitation_email_deliveries", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query(
                    "SELECT i.id, i.organization_id, i.email, i.role, i.invited_by_user_id,
                            i.status, i.token, i.created_at, i.expires_at, i.responded_at,
                            i.email_status, i.email_sent_at, i.email_last_error,
                            i.email_message_id, o.name AS organization_name,
                            u.email AS invited_by_email,
                            u.display_name AS invited_by_display_name
                     FROM organization_invitations i
                     JOIN organizations o ON o.id = i.organization_id
                     LEFT JOIN users u ON u.id = i.invited_by_user_id
                     WHERE ($1::uuid IS NULL OR i.organization_id = $1)
                       AND ($2::text IS NULL OR i.email ILIKE '%' || $2 || '%')
                       AND ($3::text IS NULL OR i.email_status::text = $3)
                       AND ($4::text IS NULL OR i.status::text = $4)
                       AND ($5::timestamptz IS NULL OR i.created_at >= $5)
                       AND ($6::timestamptz IS NULL OR i.created_at <= $6)
                     ORDER BY i.created_at DESC, i.id DESC
                     LIMIT $7 OFFSET $8",
                    &[
                        &organization_id,
                        &recipient_email,
                        &email_status,
                        &invitation_status,
                        &created_after,
                        &created_before,
                        &limit,
                        &offset,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        let count_row = retry_db!("count_organization_invitation_email_deliveries", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "SELECT COUNT(*) AS total
                     FROM organization_invitations i
                     WHERE ($1::uuid IS NULL OR i.organization_id = $1)
                       AND ($2::text IS NULL OR i.email ILIKE '%' || $2 || '%')
                       AND ($3::text IS NULL OR i.email_status::text = $3)
                       AND ($4::text IS NULL OR i.status::text = $4)
                       AND ($5::timestamptz IS NULL OR i.created_at >= $5)
                       AND ($6::timestamptz IS NULL OR i.created_at <= $6)",
                    &[
                        &organization_id,
                        &recipient_email,
                        &email_status,
                        &invitation_status,
                        &created_after,
                        &created_before,
                    ],
                )
                .await
                .map_err(map_db_error)
        })?;

        let mut deliveries = Vec::new();
        for row in rows {
            deliveries.push(self.row_to_domain_email_delivery(&row)?);
        }

        Ok((deliveries, count_row.get("total")))
    }

    async fn update_status(
        &self,
        id: Uuid,
        status: ServicesInvitationStatus,
    ) -> Result<ServicesInvitation> {
        let db_status = self.domain_to_db_status(status);

        let row = retry_db!("update_organization_invitation_status", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "UPDATE organization_invitations
                     SET status = $1, responded_at = NOW()
                     WHERE id = $2
                     RETURNING id, organization_id, email, role, invited_by_user_id, status, token,
                               created_at, expires_at, responded_at, email_status, email_sent_at,
                               email_last_error, email_message_id",
                    &[&db_status.to_string(), &id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = self.row_to_db_invitation(&row)?;

        self.db_to_domain(db_inv)
    }

    async fn record_email_sent(
        &self,
        id: Uuid,
        message_id: Option<String>,
    ) -> Result<ServicesInvitation> {
        let row = retry_db!("record_organization_invitation_email_sent", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "UPDATE organization_invitations
                     SET email_status = 'sent',
                         email_sent_at = NOW(),
                         email_last_error = NULL,
                         email_message_id = $2
                     WHERE id = $1
                     RETURNING id, organization_id, email, role, invited_by_user_id, status, token,
                               created_at, expires_at, responded_at, email_status, email_sent_at,
                               email_last_error, email_message_id",
                    &[&id, &message_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = self.row_to_db_invitation(&row)?;
        self.db_to_domain(db_inv)
    }

    async fn record_email_failed(&self, id: Uuid, error: String) -> Result<ServicesInvitation> {
        let row = retry_db!("record_organization_invitation_email_failed", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "UPDATE organization_invitations
                     SET email_status = 'failed',
                         email_sent_at = NULL,
                         email_last_error = $2,
                         email_message_id = NULL
                     WHERE id = $1
                     RETURNING id, organization_id, email, role, invited_by_user_id, status, token,
                               created_at, expires_at, responded_at, email_status, email_sent_at,
                               email_last_error, email_message_id",
                    &[&id, &error],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = self.row_to_db_invitation(&row)?;
        self.db_to_domain(db_inv)
    }

    async fn record_email_skipped(&self, id: Uuid) -> Result<ServicesInvitation> {
        let row = retry_db!("record_organization_invitation_email_skipped", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "UPDATE organization_invitations
                     SET email_status = 'skipped',
                         email_sent_at = NULL,
                         email_last_error = NULL,
                         email_message_id = NULL
                     WHERE id = $1
                     RETURNING id, organization_id, email, role, invited_by_user_id, status, token,
                               created_at, expires_at, responded_at, email_status, email_sent_at,
                               email_last_error, email_message_id",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = self.row_to_db_invitation(&row)?;
        self.db_to_domain(db_inv)
    }

    async fn delete(&self, id: Uuid) -> Result<bool> {
        let rows_affected = retry_db!("delete_organization_invitation", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute("DELETE FROM organization_invitations WHERE id = $1", &[&id])
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    async fn delete_pending(&self, id: Uuid) -> Result<bool> {
        let rows_affected = retry_db!("delete_pending_organization_invitation", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "DELETE FROM organization_invitations WHERE id = $1 AND status = 'pending'",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    async fn mark_expired(&self) -> Result<usize> {
        let rows_affected = retry_db!("mark_expired_organization_invitations", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "UPDATE organization_invitations
                     SET status = 'expired'
                     WHERE status = 'pending' AND expires_at < NOW()",
                    &[],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected as usize)
    }
}
