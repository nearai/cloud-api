use moka::future::Cache;
use std::time::Duration;
use std::sync::Arc;

/// Cache for authenticated API key results to reduce database lookups
/// TTL is 60 seconds to balance performance and freshness
pub type ApiKeyCache = Arc<Cache<String, Arc<super::auth::AuthenticatedApiKey>>>;

/// Cache for model resolution results
/// TTL is 5 minutes since model configurations change infrequently
pub type ModelCache = Arc<Cache<String, Arc<database::models::Model>>>;

/// Create a new API key cache with appropriate settings
pub fn create_api_key_cache() -> ApiKeyCache {
    Arc::new(
        Cache::builder()
            // Max 10,000 entries (reasonable for most deployments)
            .max_capacity(10_000)
            // Entries expire after 60 seconds
            .time_to_live(Duration::from_secs(60))
            // Evict entries based on access time as well
            .time_to_idle(Duration::from_secs(30))
            .build()
    )
}

/// Create a new model cache with appropriate settings
pub fn create_model_cache() -> ModelCache {
    Arc::new(
        Cache::builder()
            // Max 1,000 model entries (models change rarely)
            .max_capacity(1_000)
            // Entries expire after 5 minutes
            .time_to_live(Duration::from_secs(300))
            // Keep accessed entries longer
            .time_to_idle(Duration::from_secs(180))
            .build()
    )
}

