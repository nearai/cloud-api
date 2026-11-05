//! Track bytes returned by the HTTP body, including cancellation before its first poll.

use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use futures::Stream;
use std::future::Future;
use tokio::sync::oneshot;

struct Delivery<S> {
    inner: Option<S>,
    started: Arc<AtomicBool>,
    finished: Option<oneshot::Sender<()>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl<S> Delivery<S> {
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
            Poll::Ready(Some(Ok(bytes))) if !bytes.is_empty() => {
                this.started.store(true, Ordering::Release)
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
    };
    (Body::from_stream(stream), receiver)
}
