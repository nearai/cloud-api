pub mod ports;

use crate::auth::{AuthServiceTrait, UserId};
use crate::organization::{OrganizationId, OrganizationServiceTrait};
use crate::workspace::{CreateApiKeyRequest, WorkspaceId, WorkspaceServiceTrait};
use chrono::Utc;
use ports::*;
use std::sync::Arc;
use uuid::Uuid;

fn generate_promo_code() -> String {
    use rand_core::OsRng;
    use rand_core::RngCore;
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let group = |slice: &[u8]| {
        slice
            .iter()
            .map(|b| CHARSET[*b as usize % CHARSET.len()] as char)
            .collect::<String>()
    };
    format!(
        "NEAR-{}-{}-{}-{}",
        group(&buf[0..4]),
        group(&buf[4..8]),
        group(&buf[8..12]),
        group(&buf[12..16])
    )
}

fn event_to_info(event: &CreditEventData) -> CreditEventInfo {
    CreditEventInfo {
        id: event.id,
        name: event.name.clone(),
        description: event.description.clone(),
        credit_amount: event.credit_amount,
        currency: event.currency.clone(),
        max_claims: event.max_claims,
        claim_count: event.claim_count,
        starts_at: event.starts_at,
        claim_deadline: event.claim_deadline,
        credit_expires_at: event.credit_expires_at,
        is_active: event.is_active,
        created_at: event.created_at,
    }
}

fn code_to_info(code: &CreditEventCodeData) -> CreditEventCodeInfo {
    CreditEventCodeInfo {
        id: code.id,
        code: code.code.clone(),
        is_claimed: code.is_claimed,
        claimed_by_user_id: code.claimed_by_user_id,
        claimed_by_near_account_id: code.claimed_by_near_account_id.clone(),
        claimed_at: code.claimed_at,
    }
}

pub struct CreditEventServiceImpl {
    credit_event_repo: Arc<dyn CreditEventRepositoryTrait>,
    auth_service: Arc<dyn AuthServiceTrait>,
    organization_service: Arc<dyn OrganizationServiceTrait + Send + Sync>,
    workspace_service: Arc<dyn WorkspaceServiceTrait + Send + Sync>,
}

impl CreditEventServiceImpl {
    pub fn new(
        credit_event_repo: Arc<dyn CreditEventRepositoryTrait>,
        auth_service: Arc<dyn AuthServiceTrait>,
        organization_service: Arc<dyn OrganizationServiceTrait + Send + Sync>,
        workspace_service: Arc<dyn WorkspaceServiceTrait + Send + Sync>,
    ) -> Self {
        Self {
            credit_event_repo,
            auth_service,
            organization_service,
            workspace_service,
        }
    }
}

#[async_trait::async_trait]
impl CreditEventServiceTrait for CreditEventServiceImpl {
    async fn create_event(
        &self,
        request: CreateEventRequest,
    ) -> Result<CreditEventInfo, CreditEventError> {
        if request.credit_amount <= 0 {
            return Err(CreditEventError::ValidationError(
                "Credit amount must be positive".to_string(),
            ));
        }

        if let Some(max) = request.max_claims {
            if max <= 0 {
                return Err(CreditEventError::ValidationError(
                    "Max claims must be positive if specified".to_string(),
                ));
            }
        }

        let starts_at = request.starts_at.unwrap_or_else(Utc::now);
        let event = self
            .credit_event_repo
            .create_event(
                request.name,
                request.description,
                request.credit_amount,
                request.currency.unwrap_or_else(|| "USD".to_string()),
                request.max_claims,
                starts_at,
                request.claim_deadline,
                request.credit_expires_at,
                request.created_by_user_id,
            )
            .await?;

        Ok(event_to_info(&event))
    }

    async fn get_event(&self, event_id: Uuid) -> Result<CreditEventInfo, CreditEventError> {
        let event = self
            .credit_event_repo
            .get_event(event_id)
            .await?
            .ok_or_else(|| CreditEventError::NotFound(format!("Event not found: {event_id}")))?;

        Ok(event_to_info(&event))
    }

    async fn list_events(&self) -> Result<Vec<CreditEventInfo>, CreditEventError> {
        let events = self.credit_event_repo.list_active_events().await?;
        Ok(events.iter().map(event_to_info).collect())
    }

    async fn deactivate_event(&self, event_id: Uuid) -> Result<CreditEventInfo, CreditEventError> {
        let event = self
            .credit_event_repo
            .deactivate_event(event_id)
            .await?
            .ok_or_else(|| {
                CreditEventError::NotFound(format!(
                    "Event not found or already inactive: {event_id}"
                ))
            })?;

        Ok(event_to_info(&event))
    }

    async fn generate_codes(
        &self,
        request: GenerateCodesRequest,
    ) -> Result<Vec<String>, CreditEventError> {
        if request.count <= 0 || request.count > 10000 {
            return Err(CreditEventError::ValidationError(
                "Count must be between 1 and 10000".to_string(),
            ));
        }

        let _event = self
            .credit_event_repo
            .get_event(request.event_id)
            .await?
            .ok_or_else(|| {
                CreditEventError::NotFound(format!("Event not found: {}", request.event_id))
            })?;

        let codes: Vec<String> = (0..request.count).map(|_| generate_promo_code()).collect();
        let codes_clone = codes.clone();

        self.credit_event_repo
            .generate_codes(request.event_id, codes_clone)
            .await?;

        Ok(codes)
    }

    async fn get_codes(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<CreditEventCodeInfo>, CreditEventError> {
        let codes = self.credit_event_repo.get_codes_for_event(event_id).await?;

        Ok(codes.iter().map(code_to_info).collect())
    }

    async fn claim_credits(
        &self,
        request: ClaimCreditsRequest,
    ) -> Result<ClaimResult, CreditEventError> {
        let now = Utc::now();

        // 1. Validate event
        let event = self
            .credit_event_repo
            .get_event(request.event_id)
            .await?
            .ok_or_else(|| {
                CreditEventError::NotFound(format!("Event not found: {}", request.event_id))
            })?;

        if !event.is_active {
            return Err(CreditEventError::EventInactive);
        }

        if now < event.starts_at {
            return Err(CreditEventError::ClaimPeriodNotStarted);
        }

        if let Some(deadline) = event.claim_deadline {
            if now > deadline {
                return Err(CreditEventError::ClaimPeriodEnded);
            }
        }

        // 2. Find and validate the code (DB enforces is_claimed = false)
        let code = self
            .credit_event_repo
            .find_unclaimed_code(request.event_id, &request.code)
            .await?
            .ok_or(CreditEventError::InvalidCode)?;

        // 3. Get or create user's organization
        let user = self
            .auth_service
            .get_user_by_id(UserId(request.user_id))
            .await
            .map_err(|e| CreditEventError::InternalError(format!("Failed to get user: {e}")))?;

        let orgs = self
            .organization_service
            .list_organizations_for_user(UserId(request.user_id), 100, 0, None, None)
            .await
            .map_err(|e| CreditEventError::InternalError(format!("Failed to list orgs: {e}")))?;

        let (org_id, created_new) = if let Some(requested_org_id) = request.organization_id {
            let org = orgs
                .into_iter()
                .find(|o| o.id.0 == requested_org_id)
                .ok_or_else(|| {
                    CreditEventError::ValidationError(
                        "User is not a member of the specified organization".to_string(),
                    )
                })?;
            (org.id.0, false)
        } else if let Some(org) = orgs.into_iter().min_by_key(|o| o.created_at) {
            (org.id.0, false)
        } else {
            let org_name = format!(
                "{}-org-{}",
                request.near_account_id,
                &request.user_id.to_string()[..8]
            );
            let org = self
                .organization_service
                .create_organization(org_name, None, UserId(request.user_id))
                .await
                .map_err(|e| {
                    CreditEventError::InternalError(format!("Failed to create organization: {e}"))
                })?;
            (org.id.0, true)
        };

        // 4. Create default workspace if new org
        if created_new {
            let org_id_typed = OrganizationId(org_id);
            self.workspace_service
                .create_workspace(
                    "default".to_string(),
                    Some("Default workspace".to_string()),
                    org_id_typed.clone(),
                    UserId(request.user_id),
                )
                .await
                .map_err(|e| {
                    CreditEventError::InternalError(format!("Failed to create workspace: {e}"))
                })?;
        }

        // 5. Atomically claim code + add credits + increment claim count (single transaction)
        let credits = CreditAdditionParams {
            spend_limit: event.credit_amount,
            credit_type: format!("event:{}:user:{}", request.event_id, request.user_id),
            source: Some("event_claim".to_string()),
            currency: event.currency.clone(),
            credit_expires_at: Some(event.credit_expires_at),
            changed_by: Some("credit_event_claim".to_string()),
            change_reason: Some(format!("Credits from event: {}", event.name)),
            changed_by_user_id: Some(request.user_id),
            changed_by_user_email: Some(user.email.clone()),
        };

        let claim = self
            .credit_event_repo
            .claim_code(
                code.id,
                request.event_id,
                request.user_id,
                &request.near_account_id,
                org_id,
                credits,
            )
            .await
            .map_err(|e| match e {
                CreditEventError::CodeAlreadyClaimed => CreditEventError::CodeAlreadyClaimed,
                CreditEventError::MaxClaimsReached => CreditEventError::MaxClaimsReached,
                CreditEventError::NotFound(msg) => {
                    CreditEventError::InternalError(format!("Claim failed: {msg}"))
                }
                other => other,
            })?;

        // 6. Create API key if user doesn't have one yet
        let org_id_typed = OrganizationId(org_id);
        let workspaces = self
            .workspace_service
            .list_workspaces_for_organization(org_id_typed.clone(), UserId(request.user_id))
            .await
            .map_err(|e| {
                CreditEventError::InternalError(format!("Failed to list workspaces: {e}"))
            })?;

        let api_key_str = if let Some(ws) = workspaces.first() {
            let ws_id = WorkspaceId(ws.id.0);
            let keys = self
                .workspace_service
                .list_api_keys_paginated(ws_id.clone(), UserId(request.user_id), 100, 0)
                .await
                .map_err(|e| {
                    CreditEventError::InternalError(format!("Failed to list API keys: {e}"))
                })?;
            if keys.iter().any(|k| k.is_active) {
                None
            } else {
                let new_key = self
                    .workspace_service
                    .create_api_key(CreateApiKeyRequest {
                        name: "event-credits".to_string(),
                        workspace_id: ws_id,
                        created_by_user_id: UserId(request.user_id),
                        expires_at: None,
                        spend_limit: None,
                    })
                    .await
                    .map_err(|e| {
                        CreditEventError::InternalError(format!("Failed to create API key: {e}"))
                    })?;
                new_key.key
            }
        } else {
            None
        };

        Ok(ClaimResult {
            claim_id: claim.id,
            event_id: request.event_id,
            near_account_id: request.near_account_id,
            organization_id: org_id,
            credit_amount: event.credit_amount,
            api_key: api_key_str,
            credit_expires_at: event.credit_expires_at,
        })
    }
}
