use async_trait::async_trait;

/// Model object (matches OpenAI API)
/// Describes an OpenAI model offering that can be used with the API.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// The Unix timestamp (in seconds) when the model was created
    pub created: u64,
    /// The model identifier, which can be referenced in the API endpoints
    pub id: String,
    /// The object type, which is always "model"
    pub object: String,
    /// The organization that owns the model
    pub owned_by: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsError {
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
}

#[async_trait]
pub trait ModelsService: Send + Sync {
    async fn get_models(&self) -> Result<Vec<ModelInfo>, ModelsError>;
}
