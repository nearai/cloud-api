use super::{
    CreateOrganizationReportingTokenRequest, CreatedOrganizationReportingToken,
    OrganizationReportingToken, OrganizationReportingTokenRepository,
    OrganizationReportingTokenService as _, OrganizationReportingTokenServiceImpl,
    ValidatedOrganizationReportingToken, REPORTING_TOKEN_SCOPE_USAGE_READ,
};
use crate::common::RepositoryError;
use async_trait::async_trait;
use chrono::{TimeZone as _, Utc};
use std::sync::Arc;
use uuid::Uuid;

const RAW_TOKEN: &str = "rpt-0123456789abcdef0123456789abcdef";

struct StubReportingTokenRepository;

fn token_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("valid token UUID")
}

fn organization_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("valid organization UUID")
}

fn user_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000003").expect("valid user UUID")
}

fn service_token(id: Uuid, organization_id: Uuid) -> OrganizationReportingToken {
    OrganizationReportingToken {
        id,
        organization_id,
        name: "warehouse sync".to_string(),
        token_prefix: "rpt-test".to_string(),
        created_by_user_id: user_id(),
        created_at: Utc
            .with_ymd_and_hms(2026, 7, 10, 12, 0, 0)
            .single()
            .expect("valid fixture timestamp"),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        revoked_by_user_id: None,
        scope: REPORTING_TOKEN_SCOPE_USAGE_READ,
    }
}

#[async_trait]
impl OrganizationReportingTokenRepository for StubReportingTokenRepository {
    async fn create(
        &self,
        request: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        Ok(CreatedOrganizationReportingToken {
            token: OrganizationReportingToken {
                name: request.name,
                created_by_user_id: request.created_by_user_id,
                expires_at: request.expires_at,
                ..service_token(token_id(), request.organization_id)
            },
            raw_token: RAW_TOKEN.to_string(),
        })
    }

    async fn validate(
        &self,
        raw_token: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        Ok(
            (raw_token == RAW_TOKEN).then(|| ValidatedOrganizationReportingToken {
                id: token_id(),
                organization_id: organization_id(),
                token_prefix: "rpt-test".to_string(),
                last_used_at: None,
                scope: REPORTING_TOKEN_SCOPE_USAGE_READ,
            }),
        )
    }

    async fn get_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        Ok((id == token_id()).then(|| service_token(id, organization_id())))
    }

    async fn list_active_by_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        Ok(vec![service_token(token_id(), organization_id)])
    }

    async fn revoke(&self, id: Uuid, revoked_by_user_id: Uuid) -> Result<bool, RepositoryError> {
        Ok(id == token_id() && revoked_by_user_id == user_id())
    }
}

fn service() -> OrganizationReportingTokenServiceImpl {
    OrganizationReportingTokenServiceImpl::new(Arc::new(StubReportingTokenRepository))
}

#[tokio::test]
async fn create_forwards_request_to_repository() {
    // Given: a reporting-token service backed by a repository adapter.
    let service = service();
    let expires_at = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("valid fixture timestamp");

    // When: a reporting token is created.
    let created = service
        .create(CreateOrganizationReportingTokenRequest {
            organization_id: organization_id(),
            name: "  finance export  ".to_string(),
            created_by_user_id: user_id(),
            expires_at: Some(expires_at),
        })
        .await
        .expect("service call should succeed");

    // Then: the repository receives every request field.
    assert_eq!(created.token.organization_id, organization_id());
    assert_eq!(created.token.name, "finance export");
    assert_eq!(created.token.created_by_user_id, user_id());
    assert_eq!(created.token.expires_at, Some(expires_at));
}

#[tokio::test]
async fn create_rejects_invalid_domain_values_before_persistence() {
    let service = service();
    let expired = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid fixture timestamp");

    for request in [
        CreateOrganizationReportingTokenRequest {
            organization_id: organization_id(),
            name: "   ".to_string(),
            created_by_user_id: user_id(),
            expires_at: None,
        },
        CreateOrganizationReportingTokenRequest {
            organization_id: organization_id(),
            name: "finance export".to_string(),
            created_by_user_id: user_id(),
            expires_at: Some(expired),
        },
    ] {
        assert!(matches!(
            service.create(request).await,
            Err(RepositoryError::ValidationFailed(_))
        ));
    }
}

#[tokio::test]
async fn validate_rejects_malformed_token_without_hash_lookup() {
    assert!(service()
        .validate("rpt-not-a-token")
        .await
        .expect("invalid format is not a repository failure")
        .is_none());
}

#[tokio::test]
async fn validate_forwards_raw_token_to_repository() {
    // Given: a reporting-token service backed by a repository adapter.
    let service = service();

    // When: valid raw token material is validated.
    let validated = service
        .validate(RAW_TOKEN)
        .await
        .expect("service call should succeed");

    // Then: the repository result is returned unchanged.
    assert_eq!(validated.map(|token| token.id), Some(token_id()));
}

#[tokio::test]
async fn get_forwards_token_id_to_repository() {
    // Given: a reporting-token service backed by a repository adapter.
    let service = service();

    // When: a token is loaded by ID.
    let token = service
        .get_by_id(token_id())
        .await
        .expect("service call should succeed");

    // Then: the matching repository token is returned.
    assert_eq!(token.map(|token| token.id), Some(token_id()));
}

#[tokio::test]
async fn list_forwards_organization_id_to_repository() {
    // Given: a reporting-token service backed by a repository adapter.
    let service = service();

    // When: active organization tokens are listed.
    let tokens = service
        .list_active_by_organization(organization_id())
        .await
        .expect("service call should succeed");

    // Then: the repository result is scoped to that organization.
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].organization_id, organization_id());
}

#[tokio::test]
async fn revoke_forwards_token_and_actor_ids_to_repository() {
    // Given: a reporting-token service backed by a repository adapter.
    let service = service();

    // When: a token is revoked by its actor.
    let revoked = service
        .revoke(token_id(), user_id())
        .await
        .expect("service call should succeed");

    // Then: the repository confirms the matching IDs were received.
    assert!(revoked);
}
