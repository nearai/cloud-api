//! Shared, privacy-safe diagnostics for the workspace API-key list flow.
use super::request_context::current_request_id;
use std::{future::Future, time::Instant};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub enum Operation {
    Count,
    List,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count_api_keys",
            Self::List => "list_api_keys",
        }
    }
}

pub enum Phase {
    Permission,
    Pool,
    Query,
}

/// Time one attempt without inspecting its result or changing retry behavior.
/// Events deliberately omit error text, query arguments, and customer content.
pub async fn measure<T, E>(
    operation: Operation,
    phase: Phase,
    workspace_id: Uuid,
    work: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let started = Instant::now();
    let outcome = work.await;
    let (event, phase) = match phase {
        Phase::Permission => ("workspace_api_key_permission_finished", "permission"),
        Phase::Pool => ("workspace_api_key_db_phase_finished", "pool"),
        Phase::Query => ("workspace_api_key_db_phase_finished", "query"),
    };
    tracing::debug!(
        target: "workspace_api_key_timing",
        event,
        request_id = current_request_id().as_deref(),
        operation = operation.as_str(),
        phase,
        %workspace_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        success = outcome.is_ok(),
    );
    outcome
}
