#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
}
