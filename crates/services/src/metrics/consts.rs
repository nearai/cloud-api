// Latency metrics
pub const METRIC_LATENCY_TTFT: &str = "cloud_api.latency.time_to_first_token";
pub const METRIC_LATENCY_TOTAL: &str = "cloud_api.latency.total";
pub const METRIC_LATENCY_DECODING_TIME: &str = "cloud_api.latency.decoding_time";
pub const METRIC_TOKENS_PER_SECOND: &str = "cloud_api.tokens_per_second";

// Verification metrics
pub const METRIC_VERIFICATION_SUCCESS: &str = "cloud_api.verification.success";
pub const METRIC_VERIFICATION_FAILURE: &str = "cloud_api.verification.failure";
pub const METRIC_VERIFICATION_DURATION: &str = "cloud_api.verification.duration";

// Usage/engagement metrics
pub const METRIC_REQUEST_COUNT: &str = "cloud_api.request.count";
pub const METRIC_TOKENS_INPUT: &str = "cloud_api.tokens.input";
pub const METRIC_TOKENS_OUTPUT: &str = "cloud_api.tokens.output";

// Tags
pub const TAG_MODEL: &str = "model";
pub const TAG_ORG: &str = "org";
pub const TAG_ORG_NAME: &str = "org_name";
pub const TAG_WORKSPACE: &str = "workspace";
pub const TAG_WORKSPACE_NAME: &str = "workspace_name";
pub const TAG_API_KEY: &str = "api_key";
pub const TAG_API_KEY_NAME: &str = "api_key_name";
pub const TAG_REASON: &str = "reason";

// Failure reasons
pub const REASON_PROVIDER_ERROR: &str = "provider_error";
pub const REASON_REPOSITORY_ERROR: &str = "repository_error";
