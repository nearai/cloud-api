#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    /// The conversation does not exist in the caller's workspace.
    ///
    /// Deliberately carries no detail: unknown IDs and IDs owned by another
    /// workspace must be indistinguishable to the caller (non-enumerating 404).
    #[error("Conversation not found")]
    NotFound,
}
