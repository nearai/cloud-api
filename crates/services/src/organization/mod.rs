pub mod ports;
use super::auth::ports::{UserId, UserRepository};
use super::common::RepositoryError;
use anyhow::Result;
use async_trait::async_trait;
pub use ports::*;
use std::sync::Arc;

pub struct OrganizationServiceImpl {
    repository: Arc<dyn OrganizationRepository>,
    user_repository: Arc<dyn UserRepository>,
}

impl OrganizationServiceImpl {
    pub fn new(
        repository: Arc<dyn OrganizationRepository>,
        user_repository: Arc<dyn UserRepository>,
    ) -> Self {
        Self {
            repository,
            user_repository,
        }
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

    /// Get an organization by ID (private helper)
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

    /// List organizations accessible to a user (where they are a member, private helper)
    async fn list_organizations_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, OrganizationError> {
        self.repository
            .list_organizations_by_user(user_id.0, limit, offset, order_by, order_direction)
            .await
            .map_err(Self::map_repository_error)
    }

    async fn list_organizations_with_roles_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<OrganizationWithRole>, OrganizationError> {
        self.repository
            .list_organizations_with_roles_by_user(
                user_id.0,
                limit,
                offset,
                order_by,
                order_direction,
            )
            .await
            .map_err(Self::map_repository_error)
    }

    async fn count_organizations_for_user(
        &self,
        user_id: UserId,
    ) -> Result<i64, OrganizationError> {
        self.repository
            .count_organizations_by_user(user_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Add a member to an organization (private helper)
    async fn add_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            // Check if user is admin
            if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                if member.role != MemberRole::Owner && member.role != MemberRole::Admin {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can add members".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        // Can't add someone as owner through this method
        if role == MemberRole::Owner {
            return Err(OrganizationError::InvalidParams(
                "Cannot add a member as owner. Use transfer ownership instead.".to_string(),
            ));
        }

        let request = AddOrganizationMemberRequest {
            user_id: new_member_id.0,
            role,
        };

        self.repository
            .add_member(organization_id.0, request, requester_id.0)
            .await
            .map_err(|e| match e {
                RepositoryError::AlreadyExists => OrganizationError::AlreadyMember,
                RepositoryError::NotFound(_) => OrganizationError::NotFound,
                _ => Self::map_repository_error(e),
            })
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

    /// Get the number of members in an organization (private helper)
    async fn get_member_count(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            if let Ok(Some(_)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                // User is a member, can view member count
            } else {
                return Err(OrganizationError::Unauthorized(
                    "Only members can view the member count".to_string(),
                ));
            }
        }

        self.repository
            .get_member_count(organization_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Get organization by name (private helper)
    async fn get_organization_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError> {
        self.repository
            .get_by_name(name)
            .await
            .map_err(Self::map_repository_error)
    }

    /// List organization members with full user information (paginated, private helper)
    async fn get_members_with_users_paginated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMemberWithUser>, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            if let Ok(Some(_)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                // User is a member, can view member list
            } else {
                return Err(OrganizationError::Unauthorized(
                    "Only members can view the member list".to_string(),
                ));
            }
        }

        // Get members with pagination
        let members = self
            .repository
            .list_members_paginated(organization_id.0, limit, offset)
            .await
            .map_err(Self::map_repository_error)?;

        // Fetch user info for each member
        let mut members_with_users = Vec::new();
        for member in members {
            if let Ok(Some(user)) = self.user_repository.get_by_id(member.user_id.clone()).await {
                members_with_users.push(OrganizationMemberWithUser {
                    organization_id: member.organization_id,
                    user_id: member.user_id,
                    role: member.role,
                    joined_at: member.joined_at,
                    user,
                });
            }
        }

        Ok(members_with_users)
    }

    /// Add a member by user ID (validates user exists, private helper)
    async fn get_organization_settings(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<ports::OrganizationSettings, OrganizationError> {
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
        Ok(Self::settings_from_organization(&org))
    }

    async fn add_member_validated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Verify user exists
        let _user = self
            .user_repository
            .get_by_id(new_member_id.clone())
            .await
            .map_err(|e| OrganizationError::InternalError(format!("Failed to verify user: {e}")))?
            .ok_or(OrganizationError::UserNotFound)?;

        // Use existing add_member logic
        self.add_member(organization_id, requester_id, new_member_id, role)
            .await
    }

    async fn get_fallback_enabled_for_admin(
        &self,
        organization_id: OrganizationId,
    ) -> Result<bool, OrganizationError> {
        Ok(self
            .get_organization(organization_id)
            .await?
            .fallback_enabled())
    }

    async fn update_fallback_enabled_for_admin(
        &self,
        organization_id: OrganizationId,
        fallback_enabled: bool,
    ) -> Result<bool, OrganizationError> {
        let updated = self
            .repository
            .patch_settings(
                organization_id.0,
                ports::PatchOrganizationSettings {
                    system_prompt: None,
                    fallback_enabled: Some(Some(fallback_enabled)),
                    allowed_models: None,
                },
                None,
            )
            .await
            .map_err(Self::map_repository_error)?;

        Ok(updated.fallback_enabled())
    }

    async fn get_system_prompt(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<String>, OrganizationError> {
        Ok(self
            .get_organization_settings(organization_id, user_id)
            .await?
            .system_prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ports::{User, UserRole};
    use std::sync::Mutex;
    use uuid::Uuid;

    struct StubOrgRepo {
        org: Mutex<Organization>,
        member: Option<OrganizationMember>,
    }

    #[async_trait]
    impl OrganizationRepository for StubOrgRepo {
        async fn create(
            &self,
            _: CreateOrganizationRequest,
            _: Uuid,
        ) -> Result<Organization, RepositoryError> {
            unimplemented!()
        }

        async fn get_by_id(&self, id: Uuid) -> Result<Option<Organization>, RepositoryError> {
            let org = self.org.lock().unwrap();
            // Legacy tests use nil as a wildcard against this stub. Non-nil ids
            // still exercise real not-found behavior for the settings API.
            Ok((id.is_nil() || org.id.0 == id).then(|| org.clone()))
        }

        async fn get_by_name(&self, _: &str) -> Result<Option<Organization>, RepositoryError> {
            unimplemented!()
        }

        async fn get_member(
            &self,
            organization_id: Uuid,
            user_id: Uuid,
        ) -> Result<Option<OrganizationMember>, RepositoryError> {
            Ok(self
                .member
                .as_ref()
                .filter(|member| {
                    member.organization_id.0 == organization_id && member.user_id.0 == user_id
                })
                .cloned())
        }

        async fn patch_settings(
            &self,
            id: Uuid,
            patch: ports::PatchOrganizationSettings,
            _: Option<Uuid>,
        ) -> Result<Organization, RepositoryError> {
            let mut org = self.org.lock().unwrap();
            if org.id.0 != id {
                return Err(RepositoryError::NotFound(id.to_string()));
            }
            if !org.settings.is_object() {
                org.settings = serde_json::json!({});
            }
            let settings = org
                .settings
                .as_object_mut()
                .expect("settings initialized as object");
            if let Some(system_prompt) = patch.system_prompt {
                match system_prompt {
                    Some(value) => {
                        settings.insert("system_prompt".to_string(), serde_json::json!(value));
                    }
                    None => {
                        settings.remove("system_prompt");
                    }
                }
            }
            if let Some(fallback_enabled) = patch.fallback_enabled {
                match fallback_enabled {
                    Some(value) => {
                        settings.insert("fallback_enabled".to_string(), serde_json::json!(value));
                    }
                    None => {
                        settings.remove("fallback_enabled");
                    }
                }
            }
            org.updated_at = chrono::Utc::now();
            Ok(org.clone())
        }

        async fn add_member(
            &self,
            _: Uuid,
            _: AddOrganizationMemberRequest,
            _: Uuid,
        ) -> Result<OrganizationMember, RepositoryError> {
            unimplemented!()
        }

        async fn remove_member(
            &self,
            _: Uuid,
            _: Uuid,
            _: Uuid,
        ) -> Result<RemoveOrganizationMemberResult, RepositoryError> {
            unimplemented!()
        }

        async fn list_members_paginated(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
        ) -> Result<Vec<OrganizationMember>, RepositoryError> {
            unimplemented!()
        }

        async fn get_member_count(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }

        async fn count_organizations_by_user(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }

        async fn list_organizations_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<Organization>, RepositoryError> {
            unimplemented!()
        }

        async fn list_organizations_with_roles_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<OrganizationWithRole>, RepositoryError> {
            unimplemented!()
        }
    }

    struct StubUserRepo {
        inviter: User,
        get_by_id_calls: Mutex<usize>,
    }

    #[async_trait]
    impl UserRepository for StubUserRepo {
        async fn create(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }

        async fn create_from_oauth(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
            _: String,
            _: String,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }

        async fn get_by_id(&self, _: UserId) -> anyhow::Result<Option<User>> {
            *self.get_by_id_calls.lock().unwrap() += 1;
            Ok(Some(self.inviter.clone()))
        }

        async fn get_by_email(&self, _: &str) -> anyhow::Result<Option<User>> {
            Ok(None)
        }

        async fn get_by_provider(&self, _: &str, _: &str) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }

        async fn update_email(&self, _: UserId, _: String) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn update(
            &self,
            _: UserId,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }

        async fn update_last_login(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn update_tokens_revoked_at(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn delete(&self, _: UserId) -> anyhow::Result<bool> {
            unimplemented!()
        }

        async fn list(&self, _: i64, _: i64) -> anyhow::Result<Vec<User>> {
            unimplemented!()
        }
    }

    fn make_settings_service(
        role: MemberRole,
        settings: serde_json::Value,
    ) -> (
        OrganizationServiceImpl,
        Arc<StubOrgRepo>,
        OrganizationId,
        UserId,
    ) {
        let organization_id = OrganizationId(Uuid::new_v4());
        let user_id = UserId(Uuid::new_v4());
        let now = chrono::Utc::now();
        let org_repo = Arc::new(StubOrgRepo {
            org: Mutex::new(Organization {
                request_priority: 0,
                id: organization_id.clone(),
                name: "Settings Org".to_string(),
                description: None,
                owner_id: if role == MemberRole::Owner {
                    user_id.clone()
                } else {
                    UserId(Uuid::new_v4())
                },
                settings,
                is_active: true,
                created_at: now,
                updated_at: now,
            }),
            member: Some(OrganizationMember {
                organization_id: organization_id.clone(),
                user_id: user_id.clone(),
                role,
                joined_at: now,
            }),
        });
        let user_repo = Arc::new(StubUserRepo {
            inviter: User {
                id: user_id.clone(),
                email: "settings@example.com".to_string(),
                username: "settings".to_string(),
                display_name: Some("Settings User".to_string()),
                avatar_url: None,
                auth_provider: "test".to_string(),
                provider_user_id: "settings-user".to_string(),
                role: UserRole::User,
                is_active: true,
                last_login: None,
                created_at: now,
                updated_at: now,
                tokens_revoked_at: None,
                contact_email: None,
                contact_email_verified_at: None,
                email_opt_in_at: None,
            },
            get_by_id_calls: Mutex::new(0),
        });
        let service = OrganizationServiceImpl::new(
            org_repo.clone() as Arc<dyn OrganizationRepository>,
            user_repo as Arc<dyn UserRepository>,
        );

        (service, org_repo, organization_id, user_id)
    }

    #[tokio::test]
    async fn platform_admin_helpers_update_effective_fallback_and_reject_unknown_orgs() {
        let (service, repo, organization_id, _user_id) =
            make_settings_service(MemberRole::Member, serde_json::json!({ "other": 1 }));

        assert!(service
            .get_fallback_enabled_for_admin(organization_id.clone())
            .await
            .expect("missing override defaults to enabled"));
        assert!(!service
            .update_fallback_enabled_for_admin(organization_id.clone(), false)
            .await
            .expect("platform admin can disable fallback"));
        assert_eq!(repo.org.lock().unwrap().settings["other"], 1);

        let error = service
            .get_fallback_enabled_for_admin(OrganizationId(Uuid::new_v4()))
            .await
            .expect_err("unknown organization should be rejected");
        assert!(matches!(error, OrganizationError::NotFound));
    }

    #[test]
    fn allowed_models_parse_and_fail_closed() {
        let org = |settings: serde_json::Value| Organization {
            request_priority: 0,
            id: OrganizationId(Uuid::new_v4()),
            name: "Example Org".to_string(),
            description: None,
            owner_id: UserId(Uuid::new_v4()),
            settings,
            is_active: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let set = |names: &[&str]| -> std::collections::HashSet<String> {
            names.iter().map(|name| name.to_string()).collect()
        };
        assert_eq!(org(serde_json::json!({})).allowed_models(), None);
        assert_eq!(
            org(serde_json::json!({ "allowed_models": null })).allowed_models(),
            None
        );
        assert_eq!(
            org(serde_json::json!({ "allowed_models": ["a/b", "c/d"] })).allowed_models(),
            Some(set(&["a/b", "c/d"]))
        );
        // Anything else allows no model.
        for value in [
            serde_json::json!("a/b"),
            serde_json::json!(["a/b", 7]),
            serde_json::json!({ "a/b": true }),
        ] {
            assert_eq!(
                org(serde_json::json!({ "allowed_models": value })).allowed_models(),
                Some(set(&[]))
            );
        }
    }
}
