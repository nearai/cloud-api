use crate::models::{
    Organization, OrganizationMember, OrganizationRole,
    CreateOrganizationRequest, UpdateOrganizationRequest,
    AddOrganizationMemberRequest, UpdateOrganizationMemberRequest
};
use crate::pool::DbPool;
use anyhow::{Result, Context, bail};
use uuid::Uuid;
use chrono::Utc;
use tracing::debug;

pub struct OrganizationRepository {
    pool: DbPool,
}

impl OrganizationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new organization and add creator as owner
    pub async fn create(&self, request: CreateOrganizationRequest, creator_user_id: Uuid) -> Result<Organization> {
        let mut client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let transaction = client.transaction().await
            .context("Failed to start transaction")?;
        
        let id = Uuid::new_v4();
        let now = Utc::now();
        
        // Create the organization
        let row = transaction.query_one(
            r#"
            INSERT INTO organizations (
                id, name, display_name, description, 
                created_at, updated_at, is_active
            )
            VALUES ($1, $2, $3, $4, $5, $6, true)
            RETURNING *
            "#,
            &[
                &id,
                &request.name,
                &request.display_name,
                &request.description,
                &now,
                &now,
            ],
        ).await.context("Failed to create organization")?;
        
        // Add creator as owner
        transaction.execute(
            r#"
            INSERT INTO organization_members (organization_id, user_id, role, joined_at)
            VALUES ($1, $2, 'owner', $3)
            "#,
            &[&id, &creator_user_id, &now],
        ).await.context("Failed to add creator as owner")?;
        
        transaction.commit().await
            .context("Failed to commit transaction")?;
        
        debug!("Created organization: {} with owner: {}", id, creator_user_id);
        self.row_to_organization(row)
    }

    /// Get an organization by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Organization>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM organizations WHERE id = $1 AND is_active = true",
            &[&id],
        ).await.context("Failed to query organization")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_organization(row)?)),
            None => Ok(None),
        }
    }

    /// Get an organization by name
    pub async fn get_by_name(&self, name: &str) -> Result<Option<Organization>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM organizations WHERE name = $1 AND is_active = true",
            &[&name],
        ).await.context("Failed to query organization by name")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_organization(row)?)),
            None => Ok(None),
        }
    }

    /// Update an organization
    pub async fn update(&self, id: Uuid, request: UpdateOrganizationRequest) -> Result<Organization> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_one(
            r#"
            UPDATE organizations
            SET display_name = COALESCE($2, display_name),
                description = COALESCE($3, description),
                rate_limit = COALESCE($4, rate_limit),
                settings = COALESCE($5, settings),
                updated_at = NOW()
            WHERE id = $1 AND is_active = true
            RETURNING *
            "#,
            &[
                &id,
                &request.display_name,
                &request.description,
                &request.rate_limit,
                &request.settings,
            ],
        ).await.context("Failed to update organization")?;
        
        debug!("Updated organization: {}", id);
        self.row_to_organization(row)
    }

    /// Delete an organization (soft delete)
    pub async fn delete(&self, id: Uuid) -> Result<bool> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows_affected = client.execute(
            "UPDATE organizations SET is_active = false WHERE id = $1 AND is_active = true",
            &[&id],
        ).await.context("Failed to delete organization")?;
        
        Ok(rows_affected > 0)
    }

    /// Add a member to an organization
    pub async fn add_member(&self, org_id: Uuid, request: AddOrganizationMemberRequest, invited_by: Uuid) -> Result<OrganizationMember> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        // Check if user is already a member
        let existing = client.query_opt(
            "SELECT * FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&org_id, &request.user_id],
        ).await.context("Failed to check existing membership")?;
        
        if existing.is_some() {
            bail!("User is already a member of this organization");
        }
        
        let id = Uuid::new_v4();
        let now = Utc::now();
        
        let row = client.query_one(
            r#"
            INSERT INTO organization_members (id, organization_id, user_id, role, joined_at, invited_by)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *
            "#,
            &[
                &id,
                &org_id,
                &request.user_id,
                &request.role.to_string().to_lowercase(),
                &now,
                &invited_by,
            ],
        ).await.context("Failed to add organization member")?;
        
        debug!("Added member {} to organization {} with role {:?}", request.user_id, org_id, request.role);
        self.row_to_org_member(row)
    }

    /// Update a member's role in an organization
    pub async fn update_member(&self, org_id: Uuid, user_id: Uuid, request: UpdateOrganizationMemberRequest) -> Result<OrganizationMember> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_one(
            r#"
            UPDATE organization_members
            SET role = $3
            WHERE organization_id = $1 AND user_id = $2
            RETURNING *
            "#,
            &[
                &org_id,
                &user_id,
                &request.role.to_string().to_lowercase(),
            ],
        ).await.context("Failed to update organization member")?;
        
        debug!("Updated member {} in organization {} to role {:?}", user_id, org_id, request.role);
        self.row_to_org_member(row)
    }

    /// Remove a member from an organization
    pub async fn remove_member(&self, org_id: Uuid, user_id: Uuid) -> Result<bool> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows_affected = client.execute(
            "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&org_id, &user_id],
        ).await.context("Failed to remove organization member")?;
        
        Ok(rows_affected > 0)
    }

    /// List members of an organization
    pub async fn list_members(&self, org_id: Uuid) -> Result<Vec<OrganizationMember>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let rows = client.query(
            "SELECT * FROM organization_members WHERE organization_id = $1 ORDER BY joined_at DESC",
            &[&org_id],
        ).await.context("Failed to list organization members")?;
        
        rows.into_iter()
            .map(|row| self.row_to_org_member(row))
            .collect()
    }

    /// Get member count for an organization
    pub async fn get_member_count(&self, org_id: Uuid) -> Result<i64> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_one(
            "SELECT COUNT(*) as count FROM organization_members WHERE organization_id = $1",
            &[&org_id],
        ).await.context("Failed to count organization members")?;
        
        Ok(row.get("count"))
    }

    /// Get organization member by user ID
    pub async fn get_member(&self, org_id: Uuid, user_id: Uuid) -> Result<Option<OrganizationMember>> {
        let client = self.pool.get().await
            .context("Failed to get database connection")?;
        
        let row = client.query_opt(
            "SELECT * FROM organization_members WHERE organization_id = $1 AND user_id = $2",
            &[&org_id, &user_id],
        ).await.context("Failed to query organization member")?;
        
        match row {
            Some(row) => Ok(Some(self.row_to_org_member(row)?)),
            None => Ok(None),
        }
    }

    // Helper function to convert database row to Organization
    fn row_to_organization(&self, row: tokio_postgres::Row) -> Result<Organization> {
        Ok(Organization {
            id: row.get("id"),
            name: row.get("name"),
            display_name: row.get("display_name"),
            description: row.get("description"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            is_active: row.get("is_active"),
            rate_limit: row.get("rate_limit"),
            settings: row.get("settings"),
        })
    }

    // Helper function to convert database row to OrganizationMember
    fn row_to_org_member(&self, row: tokio_postgres::Row) -> Result<OrganizationMember> {
        let role_str: String = row.get("role");
        let role = match role_str.as_str() {
            "owner" => OrganizationRole::Owner,
            "admin" => OrganizationRole::Admin,
            "member" => OrganizationRole::Member,
            _ => bail!("Invalid role: {}", role_str),
        };
        
        Ok(OrganizationMember {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            user_id: row.get("user_id"),
            role,
            joined_at: row.get("joined_at"),
            invited_by: row.get("invited_by"),
        })
    }
}