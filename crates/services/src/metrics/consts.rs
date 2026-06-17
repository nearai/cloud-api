// Latency metrics
pub const METRIC_LATENCY_TTFT: &str = "cloud_api.latency.time_to_first_token";
pub const METRIC_LATENCY_TTFT_TOTAL: &str = "cloud_api.latency.time_to_first_token_total";
pub const METRIC_LATENCY_TOTAL: &str = "cloud_api.latency.total";
pub const METRIC_LATENCY_QUEUE_TIME: &str = "cloud_api.latency.queue_time";
pub const METRIC_LATENCY_DECODING_TIME: &str = "cloud_api.latency.decoding_time";
pub const METRIC_TOKENS_PER_SECOND: &str = "cloud_api.tokens_per_second";

// Verification metrics (optional - for signature verification)
pub const METRIC_VERIFICATION_SUCCESS: &str = "cloud_api.verification.success";
pub const METRIC_VERIFICATION_FAILURE: &str = "cloud_api.verification.failure";
pub const METRIC_VERIFICATION_DURATION: &str = "cloud_api.verification.duration";

// Gateway signature creation metrics
pub const METRIC_SIGNATURE_CREATION_SUCCESS: &str = "cloud_api.signature.creation.success";
pub const METRIC_SIGNATURE_CREATION_DURATION: &str = "cloud_api.signature.creation.duration";

// Usage/engagement metrics
pub const METRIC_REQUEST_COUNT: &str = "cloud_api.request.count";
pub const METRIC_TOKENS_INPUT: &str = "cloud_api.tokens.input";
pub const METRIC_TOKENS_OUTPUT: &str = "cloud_api.tokens.output";

// Tiered-routing visibility: one increment per served request, tagged with
// `model`, `provider_tier` (near|attested_3p|non_attested), `fallback`
// (true when the primary tier failed and a fallback served), and `operation`.
// Lets dashboards show Chutes-served traffic and the NEAR->fallback rate.
pub const METRIC_PROVIDER_REQUESTS: &str = "cloud_api.provider.requests";

// Error metrics
pub const METRIC_REQUEST_ERRORS: &str = "cloud_api.request.errors";

// Cost metrics
pub const METRIC_COST_USD: &str = "cloud_api.cost.usd";

// Billed-usage metrics (emitted once per newly-recorded usage row, covering every
// inference type, dimensioned by model + inference_type). These are the authoritative
// "what we billed" counters; tokens.input/output above are throughput-oriented and only
// cover the chat-completions path.
pub const METRIC_BILLED_REQUESTS: &str = "cloud_api.billed.requests";
pub const METRIC_BILLED_INPUT_TOKENS: &str = "cloud_api.billed.input_tokens";
pub const METRIC_BILLED_OUTPUT_TOKENS: &str = "cloud_api.billed.output_tokens";
pub const METRIC_BILLED_CACHE_READ_TOKENS: &str = "cloud_api.billed.cache_read_tokens";
pub const METRIC_BILLED_INPUT_COST_USD: &str = "cloud_api.billed.input_cost_usd";
pub const METRIC_BILLED_OUTPUT_COST_USD: &str = "cloud_api.billed.output_cost_usd";

// Provider data quality metrics
pub const METRIC_PROVIDER_TOKEN_ANOMALIES: &str = "cloud_api.provider.token_anomalies";
pub const METRIC_PROVIDER_ZERO_TOKENS: &str = "cloud_api.provider.zero_tokens";

// HTTP metrics
pub const METRIC_HTTP_REQUESTS: &str = "cloud_api.http.requests";
pub const METRIC_HTTP_DURATION: &str = "cloud_api.http.duration";

// Low-cardinality tags only (NO org/workspace/api_key - those go to database analytics)
pub const TAG_MODEL: &str = "model";
pub const TAG_ENVIRONMENT: &str = "environment";
pub const TAG_ERROR_TYPE: &str = "error_type";
pub const TAG_STATUS_CODE: &str = "status_code";
pub const TAG_ENDPOINT: &str = "endpoint";
pub const TAG_METHOD: &str = "method";
pub const TAG_REASON: &str = "reason";
pub const TAG_INPUT_BUCKET: &str = "input_bucket";
pub const TAG_INFERENCE_TYPE: &str = "inference_type";

// Error types for TAG_ERROR_TYPE
pub const ERROR_TYPE_INVALID_MODEL: &str = "invalid_model";
pub const ERROR_TYPE_INVALID_PARAMS: &str = "invalid_params";
pub const ERROR_TYPE_RATE_LIMIT: &str = "rate_limit";
pub const ERROR_TYPE_INFERENCE_ERROR: &str = "inference_error";
pub const ERROR_TYPE_SERVICE_OVERLOADED: &str = "service_overloaded";
pub const ERROR_TYPE_INTERNAL_ERROR: &str = "internal_error";

// Failure reasons (for verification)
pub const REASON_INFERENCE_ERROR: &str = "inference_error";
pub const REASON_REPOSITORY_ERROR: &str = "repository_error";

// Provider token anomaly reasons
pub const REASON_TOKEN_OVERFLOW: &str = "overflow";
pub const REASON_MISSING_USAGE: &str = "missing_usage";

/// Get the current environment from the ENVIRONMENT env var, defaulting to "local"
pub fn get_environment() -> String {
    std::env::var("ENVIRONMENT").unwrap_or_else(|_| "local".to_string())
}
