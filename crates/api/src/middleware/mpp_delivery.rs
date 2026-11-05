//! Track bytes returned by the HTTP body, including cancellation before its first poll.

use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::response::Response;
use futures::{Stream, StreamExt};
use std::future::Future;
use tokio::sync::oneshot;

struct Delivery<S> {
    inner: Option<S>,
    started: Arc<AtomicBool>,
    finished: Option<oneshot::Sender<()>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    line: Vec<u8>,
    comment: bool,
}

impl<S> Delivery<S> {
    fn observe(&mut self, bytes: &[u8]) -> Result<(), axum::Error> {
        for byte in bytes {
            if *byte == b'\n' {
                if !self.comment && line_has_content(&self.line) {
                    self.line.clear();
                    self.started.store(true, Ordering::Release);
                    return Ok(());
                }
                self.line.clear();
                self.comment = false;
            } else if !self.comment {
                if self.line.is_empty() && *byte == b':' {
                    self.comment = true;
                    continue;
                }
                // Upstream content is untrusted. Bound the inspection buffer;
                // reject an oversized data line rather than inventing delivery.
                if self.line.len() >= 1024 * 1024 {
                    return Err(axum::Error::new(std::io::Error::other(
                        "MPP SSE line limit",
                    )));
                }
                self.line.push(*byte);
            }
        }
        // A whole JSON payload may arrive without its optional line separator.
        if !self.comment && line_has_complete_content(&self.line) {
            self.started.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn finish(&mut self) {
        // Drop the inner stream first so its final usage is available to the waiter.
        self.inner.take();
        if let Some(sender) = self.finished.take() {
            let _ = sender.send(());
        }
    }
}

impl<S: Stream<Item = Result<Bytes, axum::Error>> + Unpin> Stream for Delivery<S> {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // After finalization times out, no later poll may start free output.
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.finish();
            return Poll::Ready(None);
        }
        let result = match this.inner.as_mut() {
            Some(stream) => Pin::new(stream).poll_next(cx),
            None => return Poll::Ready(None),
        };
        match &result {
            Poll::Ready(Some(Ok(bytes))) if !this.started.load(Ordering::Acquire) => {
                if let Err(error) = this.observe(bytes) {
                    this.finish();
                    return Poll::Ready(Some(Err(error)));
                }
            }
            Poll::Ready(None | Some(Err(_))) => this.finish(),
            _ => {}
        }
        result
    }
}

impl<S> Drop for Delivery<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(super) fn track(body: Body, started: Arc<AtomicBool>) -> (Body, oneshot::Receiver<()>) {
    let (sender, receiver) = oneshot::channel();
    let stream = Delivery {
        inner: Some(body.into_data_stream()),
        started,
        finished: Some(sender),
        deadline: Box::pin(tokio::time::sleep(super::STREAM_WAIT)),
        line: Vec::new(),
        comment: false,
    };
    (Body::from_stream(stream), receiver)
}

fn line_has_complete_content(line: &[u8]) -> bool {
    let Some(data) = line.strip_prefix(b"data:") else {
        return false;
    };
    serde_json::from_slice(data)
        .ok()
        .is_some_and(|value| services::completions::usage_progress::payload_has_content(&value))
}

fn line_has_content(line: &[u8]) -> bool {
    let Some(data) = line.strip_prefix(b"data:") else {
        return false;
    };
    let data = data.trim_ascii();
    if data.is_empty() || data == b"[DONE]" {
        return false;
    }
    match serde_json::from_slice(data) {
        Ok(value) => services::completions::usage_progress::payload_has_content(&value),
        Err(_) => true, // A non-JSON data frame still delivers a non-empty body.
    }
}

/// Inspect through the first non-empty data frame, retaining frames and trailers.
pub(super) async fn non_streaming(response: Response, started: &AtomicBool) -> Response {
    let (parts, body) = response.into_parts();
    let mut frames = http_body_util::BodyStream::new(body);
    let mut prefix = Vec::new();
    while let Some(frame) = frames.next().await {
        if frame
            .as_ref()
            .is_ok_and(|frame| frame.data_ref().is_some_and(Bytes::is_empty))
        {
            continue;
        }
        let delivered = frame
            .as_ref()
            .is_ok_and(|frame| frame.data_ref().is_some_and(|data| !data.is_empty()));
        prefix.push(frame);
        started.store(delivered, Ordering::Release);
        break;
    }
    let body = Body::new(http_body_util::StreamBody::new(
        futures::stream::iter(prefix).chain(frames),
    ));
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn content_before_an_oversized_following_frame_is_forwarded_without_reclassification() {
        let started = Arc::new(AtomicBool::new(false));
        let mut bytes =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\ndata: ".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 1024 * 1024 + 1));
        let expected = Bytes::from(bytes);
        let original = expected.clone();
        let (body, finished) = track(
            Body::from_stream(futures::stream::once(async move {
                Ok::<_, std::convert::Infallible>(original)
            })),
            started.clone(),
        );
        let mut stream = body.into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), expected);
        assert!(started.load(Ordering::Acquire));
        drop(stream);
        finished.await.unwrap();
    }

    #[tokio::test]
    async fn fragmented_comments_and_role_delta_do_not_start_delivery() {
        let started = Arc::new(AtomicBool::new(false));
        let frames = [
            b": keep-".as_slice(),
            b"alive\n\n",
            b"data: {\"choices\":[{\"delta\":{\"role\":",
            b"\"assistant\",\"content\":\"\"}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
        ];
        let body =
            Body::from_stream(futures::stream::iter(frames.into_iter().map(|frame| {
                Ok::<_, std::convert::Infallible>(Bytes::from_static(frame))
            })));
        let (body, finished) = track(body, started.clone());
        let mut stream = body.into_data_stream();
        for _ in 0..4 {
            stream.next().await.unwrap().unwrap();
            assert!(!started.load(Ordering::Acquire));
        }
        stream.next().await.unwrap().unwrap();
        assert!(started.load(Ordering::Acquire));
        drop(stream);
        finished.await.unwrap();
    }

    #[tokio::test]
    async fn empty_unknown_length_non_streaming_body_does_not_start_delivery() {
        let started = AtomicBool::new(false);
        let body = Body::from_stream(futures::stream::iter([Ok::<_, std::convert::Infallible>(
            Bytes::new(),
        )]));
        let response = non_streaming(Response::new(body), &started).await;
        assert!(!started.load(Ordering::Acquire));
        assert!(axum::body::to_bytes(response.into_body(), 1)
            .await
            .unwrap()
            .is_empty());
    }
}
