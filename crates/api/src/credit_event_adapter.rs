use services::common::RepositoryError;
use services::credit_events::ports::*;

fn map_limits_error(e: anyhow::Error) -> CreditEventError {
    match e.downcast_ref::<RepositoryError>() {
        Some(RepositoryError::NotFound(msg)) => CreditEventError::NotFound(msg.clone()),
        Some(RepositoryError::AlreadyExists) => CreditEventError::CodeAlreadyClaimed,
        _ => CreditEventError::InternalError(e.to_string()),
    }
}

pub struct LimitsRepositoryAdapter {
    repo: database::repositories::OrganizationLimitsRepository,
}

impl LimitsRepositoryAdapter {
    pub fn new(repo: database::repositories::OrganizationLimitsRepository) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl CreditsLimitsRepository for LimitsRepositoryAdapter {
    async fn add_credits(
        &self,
        request: AddCreditsRequest,
    ) -> Result<uuid::Uuid, CreditEventError> {
        let db_request = database::models::UpdateOrganizationLimitsDbRequest {
            spend_limit: request.spend_limit,
            credit_type: request.credit_type,
            source: request.source,
            currency: request.currency,
            credit_expires_at: request.credit_expires_at,
            changed_by: request.changed_by,
            change_reason: request.change_reason,
            changed_by_user_id: request.changed_by_user_id,
            changed_by_user_email: request.changed_by_user_email,
        };

        let result = self
            .repo
            .update_limits(request.organization_id, &db_request)
            .await
            .map_err(map_limits_error)?;
        Ok(result.id)
    }
}
