//! Observe one accepted upstream stream; never initiate retries or change bytes.
use super::entities::{AttemptRepository, InferenceAttempt};
use futures::{Stream, StreamExt};
use inference_providers::{CompletionError, SSEEvent, StreamChunk, StreamingResult};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Covers cancellation while waiting for upstream response headers. The stream
/// observer takes over after dispatch; neither observer initiates another call.
pub(crate) struct PendingAttempt {
    attempt: Option<InferenceAttempt>,
    repository: Arc<dyn AttemptRepository>,
    started: std::time::Instant,
}

impl PendingAttempt {
    pub(crate) fn new(attempt: InferenceAttempt, repository: Arc<dyn AttemptRepository>) -> Self {
        Self {
            attempt: Some(attempt),
            repository,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.attempt = None;
    }
}

impl Drop for PendingAttempt {
    fn drop(&mut self) {
        let Some(mut attempt) = self.attempt.take() else {
            return;
        };
        attempt.outcome = "cancelled".into();
        attempt.latency_ms = Some(self.started.elapsed().as_millis().min(i64::MAX as u128) as i64);
        let repository = self.repository.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if repository.finish(&attempt).await.is_err() {
                    tracing::error!(request_id=%attempt.request_id,"Cancelled provider attempt persistence failed");
                }
            });
        }
    }
}

struct AttemptStream {
    inner: StreamingResult,
    attempt: Option<InferenceAttempt>,
    repository: Arc<dyn AttemptRepository>,
    stream_started: std::time::Instant,
    deadline: Option<(Pin<Box<tokio::time::Sleep>>, u64)>,
    terminated: bool,
}

impl AttemptStream {
    fn finish(&mut self, outcome: &str) {
        if let Some(mut attempt) = self.attempt.take() {
            attempt.outcome = outcome.into();
            if outcome == "success"
                && attempt
                    .route
                    .cost_rates
                    .as_ref()
                    .is_some_and(|rates| rates.billing_unit == "request")
            {
                attempt.billable_quantity = super::entities::BillableQuantity::whole("request", 1);
            }
            attempt.latency_ms = Some(
                attempt.latency_ms.unwrap_or(0).saturating_add(
                    self.stream_started
                        .elapsed()
                        .as_millis()
                        .min(i64::MAX as u128) as i64,
                ),
            );
            let repository = self.repository.clone();
            // If shutdown prevents persistence, the durable pre-dispatch record
            // remains unknown. No cancellation is misrepresented as zero cost.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if repository.finish(&attempt).await.is_err() {
                        tracing::error!(request_id=%attempt.request_id,"Provider stream outcome persistence failed");
                    }
                });
            }
        }
    }
}

impl Drop for AttemptStream {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

impl Stream for AttemptStream {
    type Item = Result<SSEEvent, CompletionError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        if let Some((timer, seconds)) = &mut self.deadline {
            if timer.as_mut().poll(cx).is_ready() {
                let timeout_seconds = *seconds;
                self.inner = Box::pin(futures::stream::empty());
                self.terminated = true;
                self.finish("timeout");
                return Poll::Ready(Some(Err(CompletionError::Timeout {
                    operation: "chat_completion_stream".into(),
                    timeout_seconds,
                })));
            }
        }
        let result = self.inner.poll_next_unpin(cx);
        match &result {
            Poll::Ready(Some(Ok(event))) => {
                if let Some(StreamChunk::Chat(chunk)) = &event.chunk {
                    if let (Some(attempt), Some(usage)) = (&mut self.attempt, &chunk.usage) {
                        attempt.usage = Some(usage.clone());
                    }
                }
            }
            Poll::Ready(Some(Err(_))) => {
                self.inner = Box::pin(futures::stream::empty());
                self.terminated = true;
                self.finish("stream_failed");
            }
            Poll::Ready(None) => {
                self.terminated = true;
                self.finish("success");
            }
            _ => {}
        }
        result
    }
}

pub(crate) fn track(
    inner: StreamingResult,
    attempt: InferenceAttempt,
    repository: Arc<dyn AttemptRepository>,
    deadline: Option<(tokio::time::Instant, u64)>,
) -> StreamingResult {
    Box::pin(AttemptStream {
        inner,
        attempt: Some(attempt),
        repository,
        stream_started: std::time::Instant::now(),
        deadline: deadline
            .map(|(deadline, seconds)| (Box::pin(tokio::time::sleep_until(deadline)), seconds)),
        terminated: false,
    })
}

/// Native protocols retain their byte stream. Reuse the attempt lifecycle but
/// leave usage unknown until protocol-specific accounting can reconcile it.
struct RawAttemptStream {
    inner: inference_providers::responses_raw::ResponsesRawBody,
    lifecycle: AttemptStream,
    usage: Option<super::native_usage::NativeUsage>,
}

impl Stream for RawAttemptStream {
    type Item = Result<bytes::Bytes, CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.lifecycle.terminated {
            return Poll::Ready(None);
        }
        if let Some((timer, seconds)) = &mut self.lifecycle.deadline {
            if timer.as_mut().poll(cx).is_ready() {
                let timeout_seconds = *seconds;
                self.inner = Box::pin(futures::stream::empty());
                self.lifecycle.terminated = true;
                self.lifecycle.finish("timeout");
                return Poll::Ready(Some(Err(CompletionError::Timeout {
                    operation: "native_response_body".into(),
                    timeout_seconds,
                })));
            }
        }
        let result = self.inner.poll_next_unpin(cx);
        if let Some(usage) = &mut self.usage {
            match &result {
                Poll::Ready(Some(Ok(bytes))) => usage.push(bytes),
                Poll::Ready(None) => usage.eof(),
                _ => {}
            }
            let tokens = usage.usage.clone();
            if let Some(attempt) = &mut self.lifecycle.attempt {
                attempt.usage = tokens;
            }
        }
        let outcome = match &result {
            Poll::Ready(Some(Err(_))) => Some("stream_failed"),
            Poll::Ready(None) => Some(
                self.usage
                    .as_ref()
                    .map_or("success", |usage| usage.outcome.unwrap_or("incomplete")),
            ),
            _ => None,
        };
        if let Some(outcome) = outcome {
            self.inner = Box::pin(futures::stream::empty());
            self.lifecycle.terminated = true;
            self.lifecycle.finish(outcome);
        }
        result
    }
}

#[cfg(test)]
pub(crate) fn track_raw(
    inner: inference_providers::responses_raw::ResponsesRawBody,
    attempt: InferenceAttempt,
    repository: Arc<dyn AttemptRepository>,
    deadline: Option<(tokio::time::Instant, u64)>,
) -> inference_providers::responses_raw::ResponsesRawBody {
    track_native(inner, attempt, repository, deadline, None)
}

pub(crate) fn track_native(
    inner: inference_providers::responses_raw::ResponsesRawBody,
    attempt: InferenceAttempt,
    repository: Arc<dyn AttemptRepository>,
    deadline: Option<(tokio::time::Instant, u64)>,
    protocol: Option<(bool, bool)>,
) -> inference_providers::responses_raw::ResponsesRawBody {
    Box::pin(RawAttemptStream {
        inner,
        usage: protocol.map(|(anthropic, streaming)| {
            super::native_usage::NativeUsage::new(anthropic, streaming)
        }),
        lifecycle: AttemptStream {
            inner: Box::pin(futures::stream::empty()),
            attempt: Some(attempt),
            repository,
            stream_started: std::time::Instant::now(),
            deadline: deadline
                .map(|(at, seconds)| (Box::pin(tokio::time::sleep_until(at)), seconds)),
            terminated: false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_registry::entities::RouteRuntime;
    use uuid::Uuid;

    struct Ledger(tokio::sync::mpsc::UnboundedSender<InferenceAttempt>);

    #[tokio::test]
    async fn pending_dispatch_cancellation_records_unknown_cost_once() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ledger = Arc::new(Ledger(tx));
        drop(PendingAttempt::new(attempt(), ledger.clone()));
        let cancelled = rx.recv().await.unwrap();
        assert_eq!(cancelled.outcome, "cancelled");
        assert!(cancelled.usage.is_none());
        assert!(cancelled.billable_quantity.is_none());
        let mut completed = PendingAttempt::new(attempt(), ledger);
        completed.disarm();
        drop(completed);
        assert!(rx.try_recv().is_err());
    }
    #[async_trait::async_trait]
    impl AttemptRepository for Ledger {
        async fn claim_quota(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn record_health(
            &self,
            _: &str,
            _: chrono::DateTime<chrono::Utc>,
            _: Option<u64>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start(&self, _: &InferenceAttempt) -> anyhow::Result<()> {
            Ok(())
        }
        async fn finish(&self, attempt: &InferenceAttempt) -> anyhow::Result<()> {
            self.0.send(attempt.clone())?;
            Ok(())
        }
    }
    fn attempt() -> InferenceAttempt {
        InferenceAttempt {
            billable_quantity: None,
            request_id: Uuid::new_v4(),
            accepted_request_id: None,
            started_at: chrono::Utc::now(),
            usage: None,
            attempt_index: 1,
            route: RouteRuntime {
                account_id: None,
                deployment_revision: None,
                route_id: Uuid::new_v4(),
                deployment_id: Uuid::new_v4(),
                quota_group: "test".into(),
                revision: 1,
                context_limit: None,
                output_limit: None,
                role: "primary".into(),
                cost_profile_id: None,
                cost_rates: None,
            },
            effective_tee: crate::provider_registry::TeeRequirement::Any,
            reasoning_effort: None,
            outcome: "started".into(),
            fallback_reason: None,
            estimated_cost_microusd: Some(7),
            latency_ms: Some(2),
        }
    }

    #[tokio::test]
    async fn native_usage_reaches_the_attempt_without_rewriting_body() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let body = bytes::Bytes::from_static(br#"{"status":"completed","usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}"#);
        let mut stream = track_native(
            Box::pin(futures::stream::iter([Ok(body.clone())])),
            attempt(),
            Arc::new(Ledger(tx)),
            None,
            Some((false, false)),
        );
        assert_eq!(stream.next().await.unwrap().unwrap(), body);
        assert!(stream.next().await.is_none());
        drop(stream);
        let recorded = rx.recv().await.unwrap();
        assert_eq!(recorded.outcome, "success");
        let usage = recorded.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.cached_tokens()
            ),
            (10, 2, 4)
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn raw_bytes_are_unchanged_and_terminal_outcomes_are_recorded_once() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ledger = Arc::new(Ledger(tx));
        let bytes = bytes::Bytes::from_static(b"event: arbitrary\ndata: opaque\n\n");
        let mut stream = track_raw(
            Box::pin(futures::stream::iter([
                Ok(bytes.clone()),
                Err(CompletionError::InvalidResponse("fixture".into())),
            ])),
            attempt(),
            ledger.clone(),
            None,
        );
        assert_eq!(stream.next().await.unwrap().unwrap(), bytes);
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(rx.recv().await.unwrap().outcome, "stream_failed");
        assert!(rx.try_recv().is_err());

        let mut stream = track_raw(
            Box::pin(futures::stream::pending()),
            attempt(),
            ledger.clone(),
            Some((tokio::time::Instant::now(), 1)),
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(CompletionError::Timeout { .. }))
        ));
        drop(stream);
        assert_eq!(rx.recv().await.unwrap().outcome, "timeout");
        drop(track_raw(
            Box::pin(futures::stream::pending()),
            attempt(),
            ledger,
            None,
        ));
        assert_eq!(rx.recv().await.unwrap().outcome, "cancelled");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn deadline_and_cancellation_record_once_without_retrying() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ledger = Arc::new(Ledger(tx));
        let mut stream = track(
            Box::pin(futures::stream::pending()),
            attempt(),
            ledger.clone(),
            Some((
                tokio::time::Instant::now() + std::time::Duration::from_millis(1),
                1,
            )),
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(CompletionError::Timeout { .. }))
        ));
        assert!(stream.next().await.is_none());
        drop(stream);
        let recorded = rx.recv().await.unwrap();
        assert_eq!(recorded.outcome, "timeout");
        assert!(recorded.usage.is_none());
        assert!(rx.try_recv().is_err());
        drop(track(
            Box::pin(futures::stream::pending()),
            attempt(),
            ledger,
            None,
        ));
        assert_eq!(rx.recv().await.unwrap().outcome, "cancelled");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_stream_error_cannot_be_overwritten_as_success_at_eof() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = track(
            Box::pin(futures::stream::iter([Err(
                CompletionError::InvalidResponse("fixture".into()),
            )])),
            attempt(),
            Arc::new(Ledger(tx)),
            None,
        );
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(rx.recv().await.unwrap().outcome, "stream_failed");
        assert!(rx.try_recv().is_err());
    }
}
