pub mod ports;
use super::auth::ports::UserId;
use super::common::RepositoryError;
use anyhow::Result;
use async_trait::async_trait;
pub use ports::*;
use std::sync::Arc;

pub struct OrganizationServiceImpl {
    repository: Arc<dyn OrganizationRepository>,
}

impl OrganizationServiceImpl {
    pub fn new(repository: Arc<dyn OrganizationRepository>) -> Self {
        Self { repository }
    }

    /// Convert RepositoryError to OrganizationError
    fn map_repository_error(err: crate::common::RepositoryError) -> OrganizationError {
        match err {
            RepositoryError::AlreadyExists => OrganizationError::AlreadyExists,
            RepositoryError::NotFound(msg) => {
                OrganizationError::InternalError(format!("Resource not found: {msg}"))
            }
            RepositoryError::RequiredFieldMissing(field) => {
                OrganizationError::InvalidParams(format!("Required field is missing: {field}"))
            }
            RepositoryError::ForeignKeyViolation(msg) => {
                OrganizationError::InvalidParams(format!("Referenced entity does not exist: {msg}"))
            }
            RepositoryError::ValidationFailed(msg) => {
                OrganizationError::InvalidParams(format!("Validation failed: {msg}"))
            }
            RepositoryError::DependencyExists(msg) => OrganizationError::InvalidParams(format!(
                "Cannot delete due to dependencies: {msg}"
            )),
            RepositoryError::TransactionConflict => {
                OrganizationError::InternalError("Transaction conflict, please retry".to_string())
            }
            RepositoryError::ConnectionFailed(msg) => {
                OrganizationError::InternalError(format!("Database connection failed: {msg}"))
            }
            RepositoryError::AuthenticationFailed => {
                OrganizationError::InternalError("Database authentication failed".to_string())
            }
            RepositoryError::QueryTimeout => {
                OrganizationError::InternalError("Database query timed out".to_string())
            }
            RepositoryError::PoolError(err) => {
                OrganizationError::InternalError(format!("Database connection pool error: {err}"))
            }
            RepositoryError::DatabaseError(err) => {
                OrganizationError::InternalError(format!("Database operation failed: {err}"))
            }
            RepositoryError::DataConversionError(err) => {
                OrganizationError::InternalError(format!("Data conversion error: {err}"))
            }
        }
    }

    async fn get_organization(
        &self,
        id: OrganizationId,
    ) -> Result<Organization, OrganizationError> {
        self.repository
            .get_by_id(id.0)
            .await
            .map_err(Self::map_repository_error)?
            .ok_or(OrganizationError::NotFound)
    }

    fn settings_from_organization(org: &Organization) -> ports::OrganizationSettings {
        ports::OrganizationSettings {
            system_prompt: org
                .settings
                .get("system_prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            fallback_enabled: org.fallback_enabled(),
            allowed_models: org.allowed_models().map(|names| {
                let mut names: Vec<String> = names.into_iter().collect();
                names.sort();
                names
            }),
        }
    }
}

// Implement the trait for the service
#[async_trait]
impl OrganizationServiceTrait for OrganizationServiceImpl {
    /// Create a new organization (private helper)
    async fn create_organization(
        &self,
        name: String,
        description: Option<String>,
        owner_id: UserId,
    ) -> Result<Organization, OrganizationError> {
        // Validate input
        if name.trim().is_empty() {
            return Err(OrganizationError::InvalidParams(
                "Organization name cannot be empty".to_string(),
            ));
        }

        let request = CreateOrganizationRequest { name, description };

        self.repository
            .create(request, owner_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Check if a user is a member of an organization (private helper)
    async fn is_member(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization(organization_id.clone()).await?;
        if org.owner_id == user_id {
            return Ok(true);
        }

        // Check membership
        let member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?;

        Ok(member.is_some())
    }

    /// Get a user's role in an organization (private helper)
    async fn get_user_role(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<MemberRole>, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization(organization_id.clone()).await?;
        if org.owner_id == user_id {
            return Ok(Some(MemberRole::Owner));
        }

        // Check membership
        let member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?;

        Ok(member.map(|m| m.role))
    }

    async fn get_system_prompt(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<String>, OrganizationError> {
        let is_member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?
            .is_some();

        if !is_member {
            return Err(OrganizationError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        let org = self.get_organization(organization_id).await?;
        Ok(Self::settings_from_organization(&org).system_prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn organization(owner: Uuid, settings: serde_json::Value) -> Organization {
        Organization {
            request_priority: Default::default(),
            id: OrganizationId(Uuid::new_v4()),
            name: "org".to_string(),
            description: None,
            owner_id: UserId(owner),
            settings,
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn member(org: &Organization, user: Uuid, role: MemberRole) -> OrganizationMember {
        OrganizationMember {
            organization_id: org.id.clone(),
            user_id: UserId(user),
            role,
            joined_at: Utc::now(),
        }
    }

    fn service(repository: MockOrganizationRepository) -> OrganizationServiceImpl {
        OrganizationServiceImpl::new(Arc::new(repository))
    }

    #[tokio::test]
    async fn create_organization_rejects_a_blank_name() {
        let result = service(MockOrganizationRepository::new())
            .create_organization("  ".to_string(), None, UserId(Uuid::new_v4()))
            .await;
        assert!(matches!(result, Err(OrganizationError::InvalidParams(_))));
    }

    #[tokio::test]
    async fn membership_and_role_follow_the_owner_and_the_member_rows() {
        let owner = Uuid::new_v4();
        let admin = Uuid::new_v4();
        let outsider = Uuid::new_v4();
        let org = organization(owner, serde_json::json!({}));
        let admin_row = member(&org, admin, MemberRole::Admin);

        let mut repository = MockOrganizationRepository::new();
        let found = org.clone();
        repository
            .expect_get_by_id()
            .returning(move |_| Ok(Some(found.clone())));
        repository
            .expect_get_member()
            .returning(move |_, user| Ok((user == admin).then(|| admin_row.clone())));
        let service = service(repository);

        for (user, is_member, role) in [
            (owner, true, Some(MemberRole::Owner)),
            (admin, true, Some(MemberRole::Admin)),
            (outsider, false, None),
        ] {
            assert_eq!(
                service
                    .is_member(org.id.clone(), UserId(user))
                    .await
                    .unwrap(),
                is_member
            );
            assert_eq!(
                service
                    .get_user_role(org.id.clone(), UserId(user))
                    .await
                    .unwrap(),
                role
            );
        }
    }

    #[tokio::test]
    async fn system_prompt_is_read_for_members_only() {
        let owner = Uuid::new_v4();
        let outsider = Uuid::new_v4();
        let org = organization(owner, serde_json::json!({ "system_prompt": "Be brief." }));
        let owner_row = member(&org, owner, MemberRole::Owner);

        let mut repository = MockOrganizationRepository::new();
        let found = org.clone();
        repository
            .expect_get_by_id()
            .returning(move |_| Ok(Some(found.clone())));
        repository
            .expect_get_member()
            .returning(move |_, user| Ok((user == owner).then(|| owner_row.clone())));
        let service = service(repository);

        assert_eq!(
            service
                .get_system_prompt(org.id.clone(), UserId(owner))
                .await
                .unwrap(),
            Some("Be brief.".to_string())
        );
        assert!(matches!(
            service
                .get_system_prompt(org.id.clone(), UserId(outsider))
                .await,
            Err(OrganizationError::Unauthorized(_))
        ));
    }
}
