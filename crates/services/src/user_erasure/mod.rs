//! Admin-driven GDPR erasure of a user. See the user erasure design spec.

pub mod ports;

use std::sync::Arc;

use uuid::Uuid;

pub use ports::*;

#[derive(Debug, thiserror::Error)]
pub enum UserErasureError {
    #[error("user not found")]
    NotFound,
    #[error("erasure blocked")]
    Blocked(Vec<ErasureBlocker>),
    #[error("confirm_email does not match")]
    ConfirmEmailMismatch,
    #[error("erasure failed")]
    Internal(#[source] anyhow::Error),
}

pub struct UserErasureService {
    repository: Arc<dyn UserErasureRepository>,
}

impl UserErasureService {
    pub fn new(repository: Arc<dyn UserErasureRepository>) -> Self {
        Self { repository }
    }

    pub async fn preview(&self, user_id: Uuid) -> Result<ErasurePlan, UserErasureError> {
        self.repository
            .plan(user_id)
            .await
            .map_err(UserErasureError::Internal)?
            .ok_or(UserErasureError::NotFound)
    }
}
