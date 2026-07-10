// API Middleware
//
// This module contains custom middleware for the API layer,
// including authentication, authorization, and request processing.

pub mod auth;
pub mod body_hash;
pub mod metrics;
pub mod rate_limit;
pub mod request_correlation;
pub mod usage;

// Re-export commonly used items
pub use auth::{
    admin_middleware, auth_middleware, AdminUser, AuthState, AuthenticatedReportingToken,
    AuthenticatedUser,
};
pub use body_hash::{body_hash_middleware, RequestBodyHash};
pub use metrics::{http_metrics_middleware, MetricsState};
pub use rate_limit::{api_key_rate_limit_middleware, RateLimitState};
pub use request_correlation::{request_correlation_middleware, RequestCorrelation};
pub use usage::{usage_check_middleware, UsageState};
