use crate::{
    models::{CreateWorkspaceRequest, UpdateWorkspaceRequest, Workspace},
    pool::DbPool,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use tracing::debug;
use uuid::Uuid;

pub struct WorkspaceRepository {
    pool: DbPool,
}

impl WorkspaceRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Create a new workspace
    pub async fn create(
        &self,
        request: CreateWorkspaceRequest,
        organization_id: Uuid,
        created_by_user_id: Uuid,
    ) -> Result<Workspace> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let id = Uuid::new_v4();
        let now = Utc::now();

        let row = client
            .query_one(
                r#"
                INSERT INTO workspaces (
                    id, name, display_name, description, organization_id, 
                    created_by_user_id, created_at, updated_at, is_active
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, true)
                RETURNING *
                "#,
                &[
                    &id,
                    &request.name,
                    &request.display_name,
                    &request.description,
                    &organization_id,
                    &created_by_user_id,
                    &now,
                    &now,
                ],
            )
            .await
            .context("Failed to create workspace")?;

        debug!(
            "Created workspace: {} for org: {} by user: {}",
            id, organization_id, created_by_user_id
        );

        self.row_to_workspace(row)
    }

    /// Get a workspace by ID
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM workspaces WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .context("Failed to query workspace")?;

        match row {
            Some(row) => Ok(Some(self.row_to_workspace(row)?)),
            None => Ok(None),
        }
    }

    /// Get a workspace by name within an organization
    pub async fn get_by_name(
        &self,
        organization_id: Uuid,
        name: &str,
    ) -> Result<Option<Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT * FROM workspaces WHERE organization_id = $1 AND name = $2 AND is_active = true",
                &[&organization_id, &name],
            )
            .await
            .context("Failed to query workspace by name")?;

        match row {
            Some(row) => Ok(Some(self.row_to_workspace(row)?)),
            None => Ok(None),
        }
    }

    /// List workspaces for an organization
    pub async fn list_by_organization(&self, organization_id: Uuid) -> Result<Vec<Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                "SELECT * FROM workspaces WHERE organization_id = $1 AND is_active = true ORDER BY created_at DESC",
                &[&organization_id],
            )
            .await
            .context("Failed to list workspaces")?;

        rows.into_iter()
            .map(|row| self.row_to_workspace(row))
            .collect()
    }

    /// List workspaces created by a user
    pub async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows = client
            .query(
                "SELECT * FROM workspaces WHERE created_by_user_id = $1 AND is_active = true ORDER BY created_at DESC",
                &[&user_id],
            )
            .await
            .context("Failed to list user's workspaces")?;

        rows.into_iter()
            .map(|row| self.row_to_workspace(row))
            .collect()
    }

    /// Update a workspace
    pub async fn update(
        &self,
        id: Uuid,
        request: UpdateWorkspaceRequest,
    ) -> Result<Option<Workspace>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        // Build dynamic update query
        let mut query = String::from("UPDATE workspaces SET updated_at = NOW()");
        let mut param_index = 2; // Start from $2, $1 is the ID
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&id];

        if let Some(ref display_name) = request.display_name {
            query.push_str(&format!(", display_name = ${}", param_index));
            params.push(display_name);
            param_index += 1;
        }

        if let Some(ref description) = request.description {
            query.push_str(&format!(", description = ${}", param_index));
            params.push(description);
            param_index += 1;
        }

        if let Some(ref settings) = request.settings {
            query.push_str(&format!(", settings = ${}", param_index));
            params.push(settings);
        }

        query.push_str(" WHERE id = $1 AND is_active = true RETURNING *");

        let row = client
            .query_opt(&query, &params)
            .await
            .context("Failed to update workspace")?;

        match row {
            Some(row) => Ok(Some(self.row_to_workspace(row)?)),
            None => Ok(None),
        }
    }

    /// Delete (deactivate) a workspace
    pub async fn delete(&self, id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client
            .execute(
                "UPDATE workspaces SET is_active = false, updated_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await
            .context("Failed to delete workspace")?;

        Ok(rows_affected > 0)
    }

    /// Helper function to convert database row to Workspace
    fn row_to_workspace(&self, row: tokio_postgres::Row) -> Result<Workspace> {
        Ok(Workspace {
            id: row.get("id"),
            name: row.get("name"),
            display_name: row.get("display_name"),
            description: row.get("description"),
            organization_id: row.get("organization_id"),
            created_by_user_id: row.get("created_by_user_id"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            is_active: row.get("is_active"),
            settings: row.get("settings"),
        })
    }

    /// Get workspace with organization info for API key validation
    /// This returns both workspace and organization data needed for auth
    pub async fn get_workspace_with_organization(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<(Workspace, crate::models::Organization)>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                r#"
                SELECT 
                    w.*,
                    o.id as org_id, o.name as org_name, o.display_name as org_display_name,
                    o.description as org_description, o.created_at as org_created_at,
                    o.updated_at as org_updated_at, o.is_active as org_is_active,
                    o.rate_limit as org_rate_limit, o.settings as org_settings
                FROM workspaces w
                JOIN organizations o ON w.organization_id = o.id
                WHERE w.id = $1 AND w.is_active = true AND o.is_active = true
                "#,
                &[&workspace_id],
            )
            .await
            .context("Failed to query workspace with organization")?;

        match row {
            Some(row) => {
                let workspace = Workspace {
                    id: row.get("id"),
                    name: row.get("name"),
                    display_name: row.get("display_name"),
                    description: row.get("description"),
                    organization_id: row.get("organization_id"),
                    created_by_user_id: row.get("created_by_user_id"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                    is_active: row.get("is_active"),
                    settings: row.get("settings"),
                };

                let organization = crate::models::Organization {
                    id: row.get("org_id"),
                    name: row.get("org_name"),
                    display_name: row.get("org_display_name"),
                    description: row.get("org_description"),
                    created_at: row.get("org_created_at"),
                    updated_at: row.get("org_updated_at"),
                    is_active: row.get("org_is_active"),
                    rate_limit: row.get("org_rate_limit"),
                    settings: row.get("org_settings"),
                };

                Ok(Some((workspace, organization)))
            }
            None => Ok(None),
        }
    }
}

// Helper functions to convert database models to service domain models
fn db_workspace_to_service_workspace(
    db_workspace: crate::models::Workspace,
) -> services::auth::ports::Workspace {
    services::auth::ports::Workspace {
        id: services::auth::ports::WorkspaceId(db_workspace.id),
        name: db_workspace.name,
        display_name: db_workspace.display_name,
        description: db_workspace.description,
        organization_id: services::organization::ports::OrganizationId(
            db_workspace.organization_id,
        ),
        created_by_user_id: services::auth::ports::UserId(db_workspace.created_by_user_id),
        created_at: db_workspace.created_at,
        updated_at: db_workspace.updated_at,
        is_active: db_workspace.is_active,
        settings: db_workspace.settings,
    }
}

fn db_organization_to_service_organization(
    db_organization: crate::models::Organization,
) -> services::organization::Organization {
    services::organization::Organization {
        id: services::organization::ports::OrganizationId(db_organization.id),
        name: db_organization.name,
        description: db_organization.description,
        // NOTE: For workspace auth context, we use a placeholder owner_id
        // since the actual organization permissions are checked via membership
        // rather than direct owner checks. The real owner lookup requires
        // a database query which would make this function async.
        owner_id: services::auth::ports::UserId(uuid::Uuid::nil()),
        settings: db_organization.settings.unwrap_or_default(),
        is_active: db_organization.is_active,
        created_at: db_organization.created_at,
        updated_at: db_organization.updated_at,
    }
}

// Implement the service layer trait
#[async_trait]
impl services::auth::ports::WorkspaceRepository for WorkspaceRepository {
    async fn get_workspace_with_organization(
        &self,
        workspace_id: services::auth::ports::WorkspaceId,
    ) -> anyhow::Result<
        Option<(
            services::auth::ports::Workspace,
            services::organization::Organization,
        )>,
    > {
        match self.get_workspace_with_organization(workspace_id.0).await? {
            Some((db_workspace, db_organization)) => Ok(Some((
                db_workspace_to_service_workspace(db_workspace),
                db_organization_to_service_organization(db_organization),
            ))),
            None => Ok(None),
        }
    }

    async fn get_by_id(
        &self,
        workspace_id: services::auth::ports::WorkspaceId,
    ) -> anyhow::Result<Option<services::auth::ports::Workspace>> {
        match self.get_by_id(workspace_id.0).await? {
            Some(db_workspace) => Ok(Some(db_workspace_to_service_workspace(db_workspace))),
            None => Ok(None),
        }
    }
}
