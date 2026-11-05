use super::*;
use aws_sdk_s3::{
    config::http::HttpResponse,
    error::{ErrorMetadata, SdkError},
    operation::{
        get_object::GetObjectError, head_object::HeadObjectError, put_object::PutObjectError,
    },
};

fn response(status: u16) -> HttpResponse {
    HttpResponse::new(
        status.try_into().unwrap(),
        ByteStream::from_static(b"").into_inner(),
    )
}

#[test]
fn object_operations_classify_missing_responses_precisely() {
    // Given SDK HEAD, GET, and PUT errors, including an unmodeled 404.
    for (status, code, missing) in [
        (404, Some("NoSuchKey"), true),
        (404, Some("NotFound"), true),
        (404, Some("404"), true),
        (404, None, true),
        (404, Some("NoSuchBucket"), false),
        (403, Some("AccessDenied"), false),
        (412, Some("PreconditionFailed"), false),
        (409, Some("ConditionalRequestConflict"), false),
        (500, None, false),
    ] {
        let metadata = match code {
            Some(code) => ErrorMetadata::builder().code(code).build(),
            None => ErrorMetadata::builder().build(),
        };
        let errors = [
            object_error(
                "metadata read",
                SdkError::service_error(
                    HeadObjectError::generic(metadata.clone()),
                    response(status),
                ),
            ),
            object_error(
                "download",
                SdkError::service_error(
                    GetObjectError::generic(metadata.clone()),
                    response(status),
                ),
            ),
            object_error(
                "rewrap",
                SdkError::service_error(PutObjectError::generic(metadata), response(status)),
            ),
        ];
        // When each error crosses the object-storage boundary.
        for error in errors {
            // Then only absent objects are skippable, never permissions, bucket, or write conflicts.
            assert_eq!(
                matches!(error, FileServiceError::NotFound),
                missing,
                "status={status} code={code:?}: {error}"
            );
            if status == 412 {
                assert!(matches!(error, FileServiceError::PreconditionFailed));
            } else if status == 500 {
                assert!(matches!(error, FileServiceError::TransientStorageError(_)));
            } else if !missing {
                assert!(matches!(error, FileServiceError::StorageError(_)));
            }
        }
    }
}

#[test]
fn throttling_and_timeout_errors_are_retryable() {
    // Given throttled, unavailable, and timed-out storage operations.
    let errors = [
        SdkError::service_error(
            GetObjectError::generic(ErrorMetadata::builder().build()),
            response(429),
        ),
        SdkError::service_error(
            GetObjectError::generic(ErrorMetadata::builder().code("SlowDown").build()),
            response(503),
        ),
        SdkError::timeout_error(std::io::Error::other("deadline exceeded")),
        SdkError::service_error(
            GetObjectError::generic(ErrorMetadata::builder().code("PreconditionFailed").build()),
            response(400),
        ),
    ];
    // When each SDK error is classified.
    let mut results = errors
        .into_iter()
        .map(|error| object_error("download", error));
    // Then transient outcomes and a conditional-write conflict stay distinct.
    for error in results.by_ref().take(3) {
        assert!(matches!(error, FileServiceError::TransientStorageError(_)));
    }
    assert!(matches!(
        results.next().unwrap(),
        FileServiceError::PreconditionFailed
    ));
}

#[test]
fn transport_failures_are_not_missing_objects() {
    // Given a transport failure without an HTTP response.
    let error = SdkError::<GetObjectError>::construction_failure(std::io::Error::other(
        "transport unavailable",
    ));
    // When classifying the failed GET.
    let error = object_error("download", error);
    // Then the storage failure is retained.
    assert!(matches!(error, FileServiceError::StorageError(_)));
}
