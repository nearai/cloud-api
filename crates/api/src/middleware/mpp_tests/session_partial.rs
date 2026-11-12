// INTEGRATION: child of mpp_session_tests.rs; HTTP delivery and cancellation regressions.
use super::*;
use futures::StreamExt;
use std::collections::VecDeque;
use tokio::sync::oneshot;

const CONTENT: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n";
const ROLE: &[u8] =
    b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n";
const COMMENT: &[u8] = b": keep-alive\n\n";

struct PartialBody {
    observer: Arc<dyn UsageObserver>,
    frames: VecDeque<bytes::Bytes>,
    content_chunks: i32,
}

impl Stream for PartialBody {
    type Item = Result<bytes::Bytes, std::convert::Infallible>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.frames.pop_front() {
            Some(frame) => {
                if frame.as_ref() == CONTENT {
                    this.content_chunks += 1;
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            None => std::task::Poll::Pending,
        }
    }
}

impl Drop for PartialBody {
    fn drop(&mut self) {
        self.observer.on_finished(UsageReport {
            request_id: Uuid::new_v4(),
            prompt_tokens: Some(7),
            completion_tokens: Some(self.content_chunks),
            completed: false,
            partial: false,
        });
    }
}

fn partial_body(observer: Arc<dyn UsageObserver>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(PartialBody {
            observer,
            frames: [COMMENT, ROLE, CONTENT, CONTENT, CONTENT]
                .into_iter()
                .map(bytes::Bytes::from_static)
                .collect(),
            content_chunks: 0,
        }))
        .unwrap()
}

#[tokio::test]
async fn disconnect_after_n_content_chunks_charges_n_observed_tokens() {
    for chunks in [1, 2, 3] {
        let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
        seed(&app).await;
        *app.state.mode.lock().unwrap() = SessionHandlerMode::Custom(partial_body);
        let response = post_session(
            &app,
            Some(&session_credential(&app, voucher(INITIAL)).await),
        )
        .await;
        let mut stream = response.into_body().into_data_stream();
        for _ in 0..(2 + chunks) {
            stream.next().await.unwrap().unwrap();
        }
        drop(stream);
        super::finalization::await_finalized(&app).await;
        let units = 7 + chunks;
        assert_eq!(
            (row(&app).await.spent, row(&app).await.units),
            (2 * u128::from(units), units)
        );
    }
}

#[tokio::test]
async fn comment_and_role_only_frames_before_disconnect_release_reservation() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Custom(partial_body);
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), COMMENT);
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), ROLE);
    drop(stream);
    super::finalization::await_finalized(&app).await;
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
}

#[tokio::test]
async fn non_streaming_empty_success_releases_reservation() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    *app.state.mode.lock().unwrap() =
        SessionHandlerMode::Custom(|_| StatusCode::OK.into_response());
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!((row(&app).await.spent, row(&app).await.reserved), (0, 0));
}

static PENDING_REPORT: Mutex<Option<oneshot::Sender<Arc<dyn UsageObserver>>>> = Mutex::new(None);

fn deferred_report(observer: Arc<dyn UsageObserver>) -> Response {
    PENDING_REPORT
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(observer)
        .unwrap();
    axum::Json(json!({"status":"ok"})).into_response()
}

#[tokio::test]
async fn cancellation_during_usage_wait_charges_the_pending_report() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    let (sender, receiver) = oneshot::channel();
    *PENDING_REPORT.lock().unwrap() = Some(sender);
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Custom(deferred_report);
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let mut request = Box::pin(post_session(&app, Some(&credential)));
    let observer = tokio::select! {
        observer = receiver => observer.unwrap(),
        _ = &mut request => panic!("the handler must wait for the deferred report"),
    };
    assert_eq!(app.ledger.reservation_count(), 1);
    drop(request);
    tokio::task::yield_now().await;
    assert_eq!(
        app.ledger.reservation_count(),
        1,
        "the finalizer waits for pending usage"
    );
    observer.on_finished(UsageReport {
        request_id: Uuid::new_v4(),
        prompt_tokens: Some(7),
        completion_tokens: Some(5),
        completed: true,
        partial: false,
    });
    super::finalization::await_finalized(&app).await;
    assert_eq!((row(&app).await.spent, row(&app).await.units), (24, 12));
}

#[test]
fn serving_claim_reclaim_outlives_stream_wait() {
    assert!(
        services::mpp::retention::SERVING_TIMEOUT > crate::middleware::mpp_session::STREAM_WAIT
    );
}

#[tokio::test]
async fn unknown_prompt_uses_body_bound_and_partial_cost_stays_within_reservation() {
    for (handler, expected_units) in [
        (
            (|observer: Arc<dyn UsageObserver>| {
                observer.on_finished(UsageReport {
                    request_id: Uuid::new_v4(),
                    prompt_tokens: None,
                    completion_tokens: Some(2),
                    completed: false,
                    partial: true,
                });
                axum::Json(json!({"status":"ok"})).into_response()
            }) as fn(Arc<dyn UsageObserver>) -> Response,
            u64::try_from(body().len()).unwrap() + 2,
        ),
        (
            (|observer: Arc<dyn UsageObserver>| {
                observer.on_finished(UsageReport {
                    request_id: Uuid::new_v4(),
                    prompt_tokens: Some(7),
                    completion_tokens: Some(1000),
                    completed: false,
                    partial: true,
                });
                axum::Json(json!({"status":"ok"})).into_response()
            }) as fn(Arc<dyn UsageObserver>) -> Response,
            1007,
        ),
    ] {
        let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
        seed(&app).await;
        *app.state.mode.lock().unwrap() = SessionHandlerMode::Custom(handler);
        let response = post_session(
            &app,
            Some(&session_credential(&app, voucher(INITIAL)).await),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cap = 2 * (body().len() as u128 + 8);
        assert_eq!(
            (row(&app).await.spent, row(&app).await.units),
            ((2 * u128::from(expected_units)).min(cap), expected_units)
        );
    }
}

#[tokio::test]
async fn known_zero_counts_are_distinct_from_missing_counts() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Custom(|observer| {
        observer.on_finished(UsageReport {
            request_id: Uuid::new_v4(),
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
            completed: false,
            partial: false,
        });
        axum::Json(json!({"status":"ok"})).into_response()
    });
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
}
