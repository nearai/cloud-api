use super::{progress::Progress, types::Mode, ObjectEncryptionState};
use services::files::{
    storage::{ObjectMetadata, RewrapOutcome},
    FileServiceError,
};
use std::{future::Future, time::Duration};

const STORAGE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn storage_call<T>(
    operation: impl Future<Output = Result<T, FileServiceError>>,
) -> anyhow::Result<T> {
    Ok(tokio::time::timeout(STORAGE_TIMEOUT, operation).await??)
}

async fn retry<T, F, Fut>(state: &ObjectEncryptionState, mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, FileServiceError>>,
{
    let mut attempt = 0;
    loop {
        let result = tokio::time::timeout(state.storage_timeout, operation())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result.map_err(anyhow::Error::from));
        match result {
            Err(error) if attempt < 3 && transient(&error) => {
                attempt += 1;
                tokio::time::sleep(state.storage_retry_interval).await;
            }
            outcome => return outcome,
        }
    }
}

fn transient(error: &anyhow::Error) -> bool {
    error.is::<tokio::time::error::Elapsed>()
        || matches!(
            error.downcast_ref::<FileServiceError>(),
            Some(FileServiceError::TransientStorageError(_))
        )
}

pub(super) async fn process_key(
    state: &ObjectEncryptionState,
    mode: Mode,
    key: &str,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    anyhow::ensure!(state.max_bytes > 0, "invalid object size limit");
    anyhow::ensure!(
        !matches!(mode, Mode::Execute) || state.write_enabled,
        "object rewrap write gate is disabled"
    );
    let unknown_before = progress.unknown_key_ids;
    let undecryptable_before = progress.undecryptable;
    let result = process_object(state, mode, key, progress).await;
    match result {
        Ok(()) => {}
        Err(error) => match error.downcast_ref::<FileServiceError>() {
            Some(FileServiceError::NotFound) => progress.record_missing(key),
            Some(FileServiceError::PreconditionFailed) => progress.record_changed(key),
            Some(FileServiceError::ObjectTooLarge {
                is_current,
                unknown_key_id,
            }) => {
                if *unknown_key_id {
                    if progress.unknown_key_ids == unknown_before {
                        progress.unknown_key_ids += 1;
                    }
                    if !matches!(mode, Mode::Execute)
                        && progress.undecryptable == undecryptable_before
                    {
                        progress.record_undecryptable(key, "unknown_key_id");
                    }
                }
                progress.record_oversized(key, matches!(mode, Mode::Verify) && !is_current)
            }
            _ => progress.record_error(key, error_class(&error)),
        },
    }
    progress.processed += 1;
    progress.total += 1;
    Ok(())
}

async fn process_object(
    state: &ObjectEncryptionState,
    mode: Mode,
    key: &str,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    let metadata: ObjectMetadata = retry(state, || state.storage.object_metadata(key)).await?;
    let is_current = metadata.key_id.as_deref() == Some(state.ring.write_id());
    let unknown = metadata
        .key_id
        .as_ref()
        .is_some_and(|id| state.ring.lookup(id).is_none());
    if unknown {
        progress.unknown_key_ids += 1;
        if !matches!(mode, Mode::Execute) {
            progress.record_undecryptable(key, "unknown_key_id");
        }
    }
    if matches!(mode, Mode::DryRun) {
        progress.record_key_id(metadata.key_id, &state.ring);
    }
    if metadata.content_length > state.max_bytes {
        progress.record_oversized(key, matches!(mode, Mode::Verify) && !is_current);
        return Ok(());
    }
    match mode {
        Mode::DryRun => {}
        Mode::Execute => {
            let outcome =
                retry(state, || state.storage.rewrap_object(key, state.max_bytes)).await?;
            if !unknown
                && matches!(
                    &outcome,
                    RewrapOutcome::RewrappedUnknownKeyId | RewrapOutcome::UndecryptableUnknownKeyId
                )
            {
                progress.unknown_key_ids += 1;
            }
            progress.record_rewrap(key, outcome);
        }
        Mode::Verify if unknown => {
            if !is_current {
                progress.stale += 1;
            }
        }
        Mode::Verify => {
            let outcome =
                retry(state, || state.storage.verify_object(key, state.max_bytes)).await?;
            if !unknown && outcome.unknown_key_id {
                progress.unknown_key_ids += 1;
            }
            progress.record_verification(key, outcome);
        }
    }
    Ok(())
}

fn error_class(error: &anyhow::Error) -> &'static str {
    if error.is::<tokio::time::error::Elapsed>() {
        return "timeout";
    }
    match error.downcast_ref::<FileServiceError>() {
        Some(FileServiceError::TransientStorageError(_)) => "transient_storage_error",
        Some(FileServiceError::StorageError(_)) => "storage_error",
        Some(FileServiceError::EncryptionError(_)) => "encryption_error",
        Some(FileServiceError::RepositoryError(_)) => "repository_error",
        Some(FileServiceError::NotFound) => "missing",
        Some(FileServiceError::PreconditionFailed) => "changed",
        Some(FileServiceError::ObjectTooLarge { .. } | FileServiceError::FileTooLarge(_, _)) => {
            "oversized"
        }
        Some(
            FileServiceError::InvalidFileType(_)
            | FileServiceError::InvalidPurpose(_)
            | FileServiceError::InvalidEncoding
            | FileServiceError::MissingField(_)
            | FileServiceError::InvalidExpiresAfter(_),
        ) => "invalid_object",
        None => "storage_error",
    }
}
