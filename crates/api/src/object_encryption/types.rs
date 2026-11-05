use axum::{http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub(super) type ApiResult<T> = Result<Json<T>, (StatusCode, Json<Value>)>;
pub(super) type AcceptedJob =
    Result<(StatusCode, Json<ObjectJobResponse>), (StatusCode, Json<Value>)>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = ObjectEncryptionMode)]
pub(super) enum Mode {
    DryRun,
    Execute,
    Verify,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Execute => "execute",
            Self::Verify => "verify",
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObjectScopeKind {
    #[default]
    Objects,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ObjectEncryptionScope)]
pub struct Scope {
    #[serde(default)]
    kind: ObjectScopeKind,
    #[serde(default)]
    pub(super) options: JobOptions,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = ObjectEncryptionOptions)]
pub(super) struct JobOptions {
    #[schema(minimum = 0, maximum = 10000)]
    pub batch_delay_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectScanRequest {
    /// Maximum objects in this call: defaults to 10000, at most 100000.
    #[schema(minimum = 1, maximum = 100000, default = 10000)]
    pub(super) limit: Option<i64>,
    /// Wall-clock budget for this call: defaults to 60 seconds, at most 60.
    #[schema(minimum = 1, maximum = 60, default = 60)]
    pub(super) deadline_seconds: Option<u64>,
    /// Pass the cursor returned by an incomplete scan unchanged.
    #[schema(inline)]
    pub(super) cursor: Option<ScanCursor>,
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ScanCursor {
    pub continuation: Option<String>,
    pub last_key: Option<String>,
    pub complete: bool,
    #[serde(skip_serializing_if = "std::collections::VecDeque::is_empty")]
    #[schema(value_type = Vec<String>)]
    pub pending_keys: std::collections::VecDeque<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_continuation: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectCreateJobRequest {
    pub(super) mode: Mode,
    #[serde(default)]
    pub(super) scope: Scope,
    #[serde(default = "batch_default")]
    pub(super) batch_size: i64,
    pub(super) max_rows: Option<i64>,
    #[serde(default = "rewrap_default")]
    pub(super) actions: Vec<String>,
    /// Throttle batches; specify here or in scope.options, never both.
    #[serde(default)]
    #[schema(minimum = 0, maximum = 10000)]
    pub(super) batch_delay_ms: Option<i64>,
}

pub(super) const fn batch_default() -> i64 {
    100
}
fn rewrap_default() -> Vec<String> {
    vec!["rewrap".into()]
}

impl ObjectCreateJobRequest {
    pub(super) fn validated(mut self) -> Result<Self, (StatusCode, Json<Value>)> {
        if !(1..=1000).contains(&self.batch_size) {
            return Err(bad("batch_size must be between 1 and 1000"));
        }
        let expected = match self.mode {
            Mode::DryRun | Mode::Execute => "rewrap",
            Mode::Verify => "verify",
        };
        if !matches!(self.actions.as_slice(), [action] if action == expected) {
            return Err(bad("actions do not match the requested job mode"));
        }
        if self.batch_delay_ms.is_some() && self.scope.options.batch_delay_ms.is_some() {
            return Err(bad(
                "specify batch_delay_ms at the top level or in scope.options, not both",
            ));
        }
        for delay in [self.batch_delay_ms, self.scope.options.batch_delay_ms]
            .into_iter()
            .flatten()
        {
            if !(0..=10_000).contains(&delay) {
                return Err(bad("batch_delay_ms must be between 0 and 10000"));
            }
        }
        if self.max_rows.is_some_and(|max| max <= 0) {
            return Err(bad("max_rows must be greater than zero"));
        }
        self.scope.options.batch_delay_ms = self
            .batch_delay_ms
            .take()
            .or(self.scope.options.batch_delay_ms);
        Ok(self)
    }
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectVerifyRequest {
    #[serde(default)]
    pub(super) scope: Scope,
    pub(super) batch_size: Option<i64>,
    /// Throttle batches; specify here or in scope.options, never both.
    #[serde(default)]
    #[schema(minimum = 0, maximum = 10000)]
    pub(super) batch_delay_ms: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ObjectJobResponse {
    pub(super) job_id: Uuid,
    pub(super) status: String,
    pub(super) mode: String,
    pub(super) scope: Value,
    pub(super) actions: Value,
    pub(super) progress: Value,
    pub(super) cursor: Value,
    pub(super) created_at: DateTime<Utc>,
    pub(super) completed_at: Option<DateTime<Utc>>,
    pub(super) last_error_class: Option<String>,
    pub(super) last_error_message: Option<String>,
}

pub(super) fn bad(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"code":"invalid_request","message":message}})),
    )
}

pub(super) fn not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":{"code":"job_not_found","message":"Object encryption job not found"}})),
    )
}

pub(super) fn internal(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!(
        error_class = "object_encryption",
        "Object encryption operation failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(
            json!({"error":{"code":"object_encryption_failed","message":"Object encryption operation failed"}}),
        ),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_modes_accept_only_the_corresponding_action() {
        // Given each supported mode and action.
        for mode in [Mode::DryRun, Mode::Execute, Mode::Verify] {
            for action in ["rewrap", "verify", "encrypt", "unknown"] {
                let request: ObjectCreateJobRequest = serde_json::from_value(json!({
                    "mode": mode, "actions": [action]
                }))
                .unwrap();
                // When validating the request.
                let valid = request.validated().is_ok();
                // Then only the exact action for that mode is accepted.
                assert_eq!(
                    valid,
                    match mode {
                        Mode::DryRun | Mode::Execute => action == "rewrap",
                        Mode::Verify => action == "verify",
                    }
                );
            }
        }
    }

    #[test]
    fn job_bounds_reject_values_outside_the_supported_range() {
        // Given invalid batch, delay, and object limits.
        for extra in [
            json!({"batch_size":0}),
            json!({"batch_size":1001}),
            json!({"batch_delay_ms":-1}),
            json!({"batch_delay_ms":10001}),
            json!({"scope":{"options":{"batch_delay_ms":10001}}}),
            json!({"max_rows":0}),
            json!({"batch_delay_ms":0,"scope":{"options":{"batch_delay_ms":10}}}),
            json!({"batch_delay_ms":10,"scope":{"options":{"batch_delay_ms":10}}}),
        ] {
            let mut value = json!({"mode":"execute"});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let request: ObjectCreateJobRequest = serde_json::from_value(value).unwrap();
            // When validating the request.
            let result = request.validated();
            // Then the invalid bound is rejected.
            assert!(result.is_err());
        }
    }

    #[test]
    fn object_scope_is_persisted_with_the_selected_delay() {
        // Given a delay specified in exactly one supported location.
        let request: ObjectCreateJobRequest = serde_json::from_value(json!({
            "mode":"execute", "batch_delay_ms":0
        }))
        .unwrap();
        // When normalizing the request.
        let validated = request.validated().unwrap();
        // Then only the persisted scope owns the selected delay.
        assert!(validated.batch_delay_ms.is_none());
        assert_eq!(
            serde_json::to_value(validated.scope).unwrap(),
            json!({"kind":"objects","options":{"batch_delay_ms":0}})
        );
    }

    #[test]
    fn database_scope_is_rejected_at_the_object_boundary() {
        // Given a scope describing database fields.
        let value = json!({"mode":"execute","scope":{"tables":["files"]}});
        // When parsing an object request.
        let result = serde_json::from_value::<ObjectCreateJobRequest>(value);
        // Then it cannot become an object job.
        assert!(result.is_err());
    }
}
