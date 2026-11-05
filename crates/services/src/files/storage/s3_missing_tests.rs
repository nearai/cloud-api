use super::*;
use aws_sdk_s3::{
    config::http::HttpResponse,
    error::ErrorMetadata,
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
            if !missing {
                assert!(matches!(error, FileServiceError::StorageError(_)));
            }
        }
    }
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
