use crate::models::{InvitationStatus, OrganizationInvitation, OrganizationRole};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use services::common::RepositoryError;
use services::organization::ports::{
    CreateInvitationRequest, InvitationStatus as ServicesInvitationStatus,
    OrganizationInvitation as ServicesInvitation, OrganizationInvitationRepository,
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

        let status = match db_inv.status {
            InvitationStatus::Pending => ServicesInvitationStatus::Pending,
            InvitationStatus::Accepted => ServicesInvitationStatus::Accepted,
            InvitationStatus::Declined => ServicesInvitationStatus::Declined,
            InvitationStatus::Expired => ServicesInvitationStatus::Expired,
        };

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
        })
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
            "Creating invitation for {} to organization {} with role {}",
            request.email, org_id, role
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
                               created_at, expires_at, responded_at",
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

        let db_inv = OrganizationInvitation {
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
        };

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
                            created_at, expires_at, responded_at
                     FROM organization_invitations
                     WHERE id = $1",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(r) => {
                let db_inv = OrganizationInvitation {
                    id: r.get("id"),
                    organization_id: r.get("organization_id"),
                    email: r.get("email"),
                    role: serde_json::from_value(serde_json::json!(r.get::<_, String>("role")))?,
                    invited_by_user_id: r.get("invited_by_user_id"),
                    status: serde_json::from_value(
                        serde_json::json!(r.get::<_, String>("status")),
                    )?,
                    token: r.get("token"),
                    created_at: r.get("created_at"),
                    expires_at: r.get("expires_at"),
                    responded_at: r.get("responded_at"),
                };
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
                            created_at, expires_at, responded_at
                     FROM organization_invitations
                     WHERE token = $1",
                    &[&token],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(r) => {
                let db_inv = OrganizationInvitation {
                    id: r.get("id"),
                    organization_id: r.get("organization_id"),
                    email: r.get("email"),
                    role: serde_json::from_value(serde_json::json!(r.get::<_, String>("role")))?,
                    invited_by_user_id: r.get("invited_by_user_id"),
                    status: serde_json::from_value(
                        serde_json::json!(r.get::<_, String>("status")),
                    )?,
                    token: r.get("token"),
                    created_at: r.get("created_at"),
                    expires_at: r.get("expires_at"),
                    responded_at: r.get("responded_at"),
                };
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
                                created_at, expires_at, responded_at
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
                                created_at, expires_at, responded_at
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
            let db_inv = OrganizationInvitation {
                id: r.get("id"),
                organization_id: r.get("organization_id"),
                email: r.get("email"),
                role: serde_json::from_value(serde_json::json!(r.get::<_, String>("role")))?,
                invited_by_user_id: r.get("invited_by_user_id"),
                status: serde_json::from_value(serde_json::json!(r.get::<_, String>("status")))?,
                token: r.get("token"),
                created_at: r.get("created_at"),
                expires_at: r.get("expires_at"),
                responded_at: r.get("responded_at"),
            };
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
                                created_at, expires_at, responded_at
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
                                created_at, expires_at, responded_at
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
            let db_inv = OrganizationInvitation {
                id: r.get("id"),
                organization_id: r.get("organization_id"),
                email: r.get("email"),
                role: serde_json::from_value(serde_json::json!(r.get::<_, String>("role")))?,
                invited_by_user_id: r.get("invited_by_user_id"),
                status: serde_json::from_value(serde_json::json!(r.get::<_, String>("status")))?,
                token: r.get("token"),
                created_at: r.get("created_at"),
                expires_at: r.get("expires_at"),
                responded_at: r.get("responded_at"),
            };
            invitations.push(self.db_to_domain(db_inv)?);
        }

        Ok(invitations)
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
                               created_at, expires_at, responded_at",
                    &[&db_status.to_string(), &id],
                )
                .await
                .map_err(map_db_error)
        })?;

        let db_inv = OrganizationInvitation {
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
        };

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
