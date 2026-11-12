use super::*;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use http_body_util::StreamBody;
use hyper::body::Frame;

#[tokio::test]
async fn bounded_body_rejects_announced_growth_before_polling_the_stream() {
    // Given a response larger than the HEAD limit and a body that cannot complete.
    let pending = futures::stream::pending::<Result<Frame<Bytes>, std::io::Error>>();
    let response = GetObjectOutput::builder()
        .content_length(36)
        .body(ByteStream::from_body_1_x(StreamBody::new(pending)))
        .build();
    // When a bounded body read starts.
    let read = std::pin::pin!(bounded_body(response, 35, ObjectKeyState::Current));
    let result = futures::poll!(read);
    // Then Content-Length alone rejects it without polling that body.
    assert!(matches!(
        result,
        std::task::Poll::Ready(Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::Current
        }))
    ));
}

#[tokio::test]
async fn bounded_body_rejects_streamed_growth_even_when_length_is_small() {
    // Given two chunks that exceed the declared size and a trailing body error.
    let frames = futures::stream::iter([
        Ok(Frame::data(Bytes::from_static(b"abc"))),
        Ok(Frame::data(Bytes::from_static(b"def"))),
        Err(std::io::Error::other("must not consume beyond limit")),
    ]);
    let response = GetObjectOutput::builder()
        .content_length(3)
        .body(ByteStream::from_body_1_x(StreamBody::new(frames)))
        .build();
    // When the body crosses the configured byte limit.
    let result = bounded_body(response, 5, ObjectKeyState::UnknownKeyId).await;
    // Then size classification wins before the trailing body is consumed.
    assert!(matches!(
        result,
        Err(FileServiceError::ObjectTooLarge {
            key_state: ObjectKeyState::UnknownKeyId
        })
    ));
}

#[tokio::test]
async fn bounded_body_accepts_exact_limit_without_a_content_length() {
    // Given an unadvertised body of exactly five bytes.
    let response = GetObjectOutput::builder()
        .body(ByteStream::from_static(b"12345"))
        .build();
    // When it is read with that exact byte limit.
    let data = bounded_body(response, 5, ObjectKeyState::StaleOrAbsent)
        .await
        .unwrap();
    // Then all bytes are retained.
    assert_eq!(data, b"12345");
}

#[tokio::test]
async fn interrupted_body_is_retryable() {
    // Given a body stream interrupted during download.
    let frames = futures::stream::iter([Err::<Frame<Bytes>, _>(std::io::Error::other(
        "connection lost",
    ))]);
    let response = GetObjectOutput::builder()
        .body(ByteStream::from_body_1_x(StreamBody::new(frames)))
        .build();
    // When the bounded reader polls the failed body.
    let result = bounded_body(response, 5, ObjectKeyState::StaleOrAbsent).await;
    // Then the caller can retry the object.
    assert!(matches!(
        result,
        Err(FileServiceError::TransientStorageError(_))
    ));
}
