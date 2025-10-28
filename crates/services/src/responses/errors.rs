#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Internal error: {0}")]
    InternalError(String),
}
