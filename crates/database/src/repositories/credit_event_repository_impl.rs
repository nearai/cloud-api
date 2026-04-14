use crate::models::{CreditClaim, CreditEvent, CreditEventCode};
use crate::repositories::CreditEventRepository;
use services::common::RepositoryError;
use services::credit_events::ports::*;

fn map_error(e: anyhow::Error) -> CreditEventError {
    match e.downcast_ref::<RepositoryError>() {
        Some(RepositoryError::NotFound(msg)) => CreditEventError::NotFound(msg.clone()),
        Some(RepositoryError::AlreadyExists) => CreditEventError::CodeAlreadyClaimed,
        Some(RepositoryError::ValidationFailed(msg)) => {
            if msg.contains("max claims") || msg.contains("max_claims") {
                CreditEventError::MaxClaimsReached
            } else {
                CreditEventError::ValidationError(msg.clone())
            }
        }
        _ => {
            let msg = e.to_string();
            if msg.contains("credit_claims_event_user") || msg.contains("credit_event_id_user_id") {
                CreditEventError::UserAlreadyClaimed
            } else {
                CreditEventError::InternalError(msg)
            }
        }
    }
}

fn db_event_to_data(event: &CreditEvent) -> CreditEventData {
    CreditEventData {
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
        created_by_user_id: event.created_by_user_id,
        created_at: event.created_at,
        updated_at: event.updated_at,
    }
}

fn db_code_to_data(code: &CreditEventCode) -> CreditEventCodeData {
    CreditEventCodeData {
        id: code.id,
        credit_event_id: code.credit_event_id,
        code: code.code.clone(),
        is_claimed: code.is_claimed,
        claimed_by_user_id: code.claimed_by_user_id,
        claimed_by_near_account_id: code.claimed_by_near_account_id.clone(),
        claimed_at: code.claimed_at,
        created_at: code.created_at,
    }
}

fn db_claim_to_data(claim: CreditClaim) -> CreditClaimData {
    CreditClaimData {
        id: claim.id,
        credit_event_id: claim.credit_event_id,
        code_id: claim.code_id,
        near_account_id: claim.near_account_id,
        user_id: claim.user_id,
        organization_id: claim.organization_id,
        organization_limit_id: claim.organization_limit_id,
        claimed_at: claim.claimed_at,
    }
}

#[async_trait::async_trait]
impl CreditEventRepositoryTrait for CreditEventRepository {
    async fn create_event(
        &self,
        name: String,
        description: Option<String>,
        credit_amount: i64,
        currency: String,
        max_claims: Option<i32>,
        starts_at: chrono::DateTime<chrono::Utc>,
        claim_deadline: Option<chrono::DateTime<chrono::Utc>>,
        credit_expires_at: chrono::DateTime<chrono::Utc>,
        created_by_user_id: Option<uuid::Uuid>,
    ) -> Result<CreditEventData, CreditEventError> {
        let event = self
            .create_event(
                name,
                description,
                credit_amount,
                currency,
                max_claims,
                starts_at,
                claim_deadline,
                credit_expires_at,
                created_by_user_id,
            )
            .await
            .map_err(map_error)?;
        Ok(db_event_to_data(&event))
    }

    async fn get_event(
        &self,
        event_id: uuid::Uuid,
    ) -> Result<Option<CreditEventData>, CreditEventError> {
        self.get_event(event_id)
            .await
            .map_err(map_error)
            .map(|o| o.as_ref().map(db_event_to_data))
    }

    async fn list_active_events(&self) -> Result<Vec<CreditEventData>, CreditEventError> {
        self.list_active_events()
            .await
            .map_err(map_error)
            .map(|v| v.iter().map(db_event_to_data).collect())
    }

    async fn deactivate_event(
        &self,
        event_id: uuid::Uuid,
    ) -> Result<Option<CreditEventData>, CreditEventError> {
        self.deactivate_event(event_id)
            .await
            .map_err(map_error)
            .map(|o| o.as_ref().map(db_event_to_data))
    }

    async fn generate_codes(
        &self,
        event_id: uuid::Uuid,
        count: i32,
        codes: Vec<String>,
    ) -> Result<Vec<CreditEventCodeData>, CreditEventError> {
        self.generate_codes(event_id, count, codes)
            .await
            .map_err(map_error)
            .map(|v| v.iter().map(db_code_to_data).collect())
    }

    async fn get_codes_for_event(
        &self,
        event_id: uuid::Uuid,
    ) -> Result<Vec<CreditEventCodeData>, CreditEventError> {
        self.get_codes_for_event(event_id)
            .await
            .map_err(map_error)
            .map(|v| v.iter().map(db_code_to_data).collect())
    }

    async fn find_unclaimed_code(
        &self,
        event_id: uuid::Uuid,
        code: &str,
    ) -> Result<Option<CreditEventCodeData>, CreditEventError> {
        self.find_unclaimed_code(event_id, code)
            .await
            .map_err(map_error)
            .map(|o| o.as_ref().map(db_code_to_data))
    }

    async fn claim_code(
        &self,
        code_id: uuid::Uuid,
        event_id: uuid::Uuid,
        user_id: uuid::Uuid,
        near_account_id: &str,
        organization_id: uuid::Uuid,
        organization_limit_id: Option<uuid::Uuid>,
    ) -> Result<CreditClaimData, CreditEventError> {
        self.claim_code(
            code_id,
            event_id,
            user_id,
            near_account_id,
            organization_id,
            organization_limit_id,
        )
        .await
        .map_err(map_error)
        .map(db_claim_to_data)
    }
}
