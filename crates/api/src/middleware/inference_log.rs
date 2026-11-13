//! One INFO line per authenticated inference request, written when the
//! response body ends or is dropped: request and tenant ids, path, the
//! resolved catalog model, status and duration, or a cancel line with the
//! stage it stopped at.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes, HttpBody},
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use http_body::{Frame, SizeHint};
use services::models::{ModelCaller, ModelsServiceTrait};
use uuid::Uuid;

use super::auth::AuthenticatedApiKey;
use super::model_deprecation::RequestedModel;
use super::request_correlation::{log_safe_path, RequestCorrelation};

/// Bound on the catalog lookup for the model id. It runs after the response
/// is done, off the request path, and is normally a cache hit.
const MODEL_LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct InferenceLogState {
    pub models_service: Arc<dyn ModelsServiceTrait>,
}

/// Wraps an inference route group, inside API-key authentication. Requests
/// without an authenticated key pass through unlogged.
pub async fn inference_log_middleware(
    State(state): State<InferenceLogState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(api_key) = request.extensions().get::<AuthenticatedApiKey>() else {
        return next.run(request).await;
    };
    // Shared with the handler, which records the requested model.
    let requested_model = request
        .extensions()
        .get::<RequestedModel>()
        .cloned()
        .unwrap_or_default();
    let mut log = RequestLog {
        request_id: request
            .extensions()
            .get::<RequestCorrelation>()
            .map(|correlation| correlation.request_id),
        org_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id: api_key.api_key.id.0.clone(),
        path: log_safe_path(request.uri().path()),
        requested_model: requested_model.clone(),
        models_service: state.models_service,
        // The deferred lookup runs outside the request's scope; an unlisted
        // model's access check needs the caller's key there too.
        caller_credential: services::caller_credential::current_caller_credential(),
        started: Instant::now(),
        status: None,
        outcome: Outcome::Pending,
    };
    request.extensions_mut().insert(requested_model);

    // Dropped here, before a response exists, the log records a cancel.
    let response = next.run(request).await;
    log.status = Some(response.status().as_u16());
    response.map(|inner| Body::new(LoggedBody { inner, log }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// No response yet, or the body has not ended.
    Pending,
    /// The body was sent to the end.
    Finished,
    /// The body stream returned an error.
    Failed,
}

struct RequestLog {
    request_id: Option<Uuid>,
    org_id: Uuid,
    workspace_id: Uuid,
    api_key_id: String,
    path: String,
    requested_model: RequestedModel,
    models_service: Arc<dyn ModelsServiceTrait>,
    /// For the model lookup only.
    caller_credential: Option<String>,
    started: Instant,
    status: Option<u16>,
    outcome: Outcome,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        let line = LogLine {
            request_id: self.request_id,
            org_id: self.org_id,
            workspace_id: self.workspace_id,
            api_key_id: std::mem::take(&mut self.api_key_id),
            path: std::mem::take(&mut self.path),
            status: self.status,
            outcome: self.outcome,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        // A 4xx usually means the model did not resolve; skip the lookup.
        let requested = self
            .requested_model
            .get()
            .filter(|_| !matches!(self.status, Some(400..=499)))
            .map(str::to_owned);
        let (Some(requested), Ok(runtime)) = (requested, tokio::runtime::Handle::try_current())
        else {
            line.emit(None);
            return;
        };
        let models_service = self.models_service.clone();
        let caller_credential = self.caller_credential.take();
        runtime.spawn(async move {
            let lookup = tokio::time::timeout(
                MODEL_LOOKUP_TIMEOUT,
                models_service
                    .resolve_and_get_model(&requested, ModelCaller::Organization(line.org_id)),
            );
            let resolved = match caller_credential {
                Some(token) => {
                    services::caller_credential::with_caller_credential(token, lookup).await
                }
                None => lookup.await,
            };
            let model = resolved
                .ok()
                .and_then(Result::ok)
                .map(|model| model.model_name);
            line.emit(model.as_deref());
        });
    }
}

struct LogLine {
    request_id: Option<Uuid>,
    org_id: Uuid,
    workspace_id: Uuid,
    api_key_id: String,
    path: String,
    status: Option<u16>,
    outcome: Outcome,
    duration_ms: u64,
}

impl LogLine {
    /// `model` is the resolved catalog id.
    fn emit(&self, model: Option<&str>) {
        let request_id = self.request_id.map(tracing::field::display);
        match (self.outcome, self.status) {
            (Outcome::Finished, Some(status)) => tracing::info!(
                request_id,
                org_id = %self.org_id,
                workspace_id = %self.workspace_id,
                api_key_id = %self.api_key_id,
                path = %self.path,
                model,
                status,
                duration_ms = self.duration_ms,
                "inference request finished"
            ),
            (outcome, status) => tracing::info!(
                request_id,
                org_id = %self.org_id,
                workspace_id = %self.workspace_id,
                api_key_id = %self.api_key_id,
                path = %self.path,
                model,
                status,
                duration_ms = self.duration_ms,
                stage = if status.is_none() {
                    "before_response"
                } else if outcome == Outcome::Failed {
                    "body_error"
                } else {
                    "during_body"
                },
                "inference request cancelled"
            ),
        }
    }
}

/// The response body, carrying the request's log until it ends or drops.
struct LoggedBody {
    inner: Body,
    log: RequestLog,
}

impl HttpBody for LoggedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(None) => self.log.outcome = Outcome::Finished,
            Poll::Ready(Some(Err(_))) => self.log.outcome = Outcome::Failed,
            // The server stops polling a body that reports its end, so the
            // last frame of a sized body is where it finishes.
            Poll::Ready(Some(Ok(_))) if self.inner.is_end_stream() => {
                self.log.outcome = Outcome::Finished
            }
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        // An empty body is complete without ever being polled.
        if self.log.outcome == Outcome::Pending && self.inner.is_end_stream() {
            self.log.outcome = Outcome::Finished;
        }
    }
}

#[cfg(test)]
#[path = "inference_log_tests.rs"]
mod tests;
