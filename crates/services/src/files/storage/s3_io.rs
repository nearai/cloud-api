use crate::files::FileServiceError;
use aws_sdk_s3::{
    error::{ProvideErrorMetadata, SdkError},
    operation::get_object::GetObjectOutput,
};
use std::fmt::Display;

pub(super) fn storage_error(operation: &str, error: impl Display) -> FileServiceError {
    FileServiceError::StorageError(format!("Object {operation} failed: {error}"))
}

pub(super) fn object_error<E: ProvideErrorMetadata>(
    operation: &str,
    error: SdkError<E>,
) -> FileServiceError {
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    if matches!(code, Some("NoSuchKey" | "NotFound" | "404"))
        || (code.is_none() && status == Some(404))
    {
        return FileServiceError::NotFound;
    }
    if code == Some("PreconditionFailed") || status == Some(412) {
        return FileServiceError::PreconditionFailed;
    }
    if matches!(status, Some(429 | 500..=599))
        || matches!(
            &error,
            SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_)
        )
    {
        return FileServiceError::TransientStorageError(format!(
            "Object {operation} failed: {error}"
        ));
    }
    storage_error(operation, error)
}

pub(super) fn content_length(length: Option<i64>) -> Result<u64, FileServiceError> {
    u64::try_from(length.unwrap_or_default())
        .map_err(|error| storage_error("content length", error))
}

pub(super) async fn body(response: GetObjectOutput) -> Result<Vec<u8>, FileServiceError> {
    Ok(response
        .body
        .collect()
        .await
        .map_err(|error| {
            FileServiceError::TransientStorageError(format!("Object body read failed: {error}"))
        })?
        .into_bytes()
        .to_vec())
}

pub(super) async fn bounded_body(
    mut response: GetObjectOutput,
    max_bytes: u64,
    is_current: bool,
    unknown_key_id: bool,
) -> Result<Vec<u8>, FileServiceError> {
    if content_length(response.content_length())? > max_bytes {
        return Err(FileServiceError::ObjectTooLarge {
            is_current,
            unknown_key_id,
        });
    }
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut data = Vec::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(|error| {
            FileServiceError::TransientStorageError(format!("Object body read failed: {error}"))
        })?;
        if chunk.len() > limit.saturating_sub(data.len()) {
            return Err(FileServiceError::ObjectTooLarge {
                is_current,
                unknown_key_id,
            });
        }
        if data.capacity() - data.len() < chunk.len() {
            let capacity = data
                .capacity()
                .saturating_mul(2)
                .max(data.len() + chunk.len())
                .min(limit);
            data.try_reserve_exact(capacity - data.len())
                .map_err(|error| storage_error("body allocation", error))?;
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[cfg(test)]
#[path = "s3_io_tests.rs"]
mod tests;
