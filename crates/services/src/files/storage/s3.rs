use super::{
    page_size, FileServiceError, ObjectVerification, RewrapOutcome, StorageTrait, KEY_ID_METADATA,
};
use crate::files::encryption::{self, EncryptionError, ObjectKeyRing};
use async_trait::async_trait;
use aws_sdk_s3::{
    error::{ProvideErrorMetadata, SdkError},
    operation::get_object::GetObjectOutput,
    primitives::ByteStream,
    Client as S3Client,
};
use std::{fmt::Display, sync::Arc};

#[derive(Clone)]
pub struct S3Storage {
    client: S3Client,
    bucket: String,
    key_ring: Arc<ObjectKeyRing>,
}

impl S3Storage {
    pub fn new(
        client: S3Client,
        bucket: String,
        encryption_key: String,
    ) -> Result<Self, EncryptionError> {
        Ok(Self::new_with_ring(
            client,
            bucket,
            Arc::new(ObjectKeyRing::from_hex(&encryption_key)?),
        ))
    }

    pub const fn new_with_ring(
        client: S3Client,
        bucket: String,
        key_ring: Arc<ObjectKeyRing>,
    ) -> Self {
        Self {
            client,
            bucket,
            key_ring,
        }
    }

    async fn get_object(&self, key: &str) -> Result<GetObjectOutput, FileServiceError> {
        self.client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| object_error("download", error))
    }
}

fn storage_error(operation: &str, error: impl Display) -> FileServiceError {
    FileServiceError::StorageError(format!("Object {operation} failed: {error}"))
}

fn object_error<E: ProvideErrorMetadata>(operation: &str, error: SdkError<E>) -> FileServiceError {
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    let missing = matches!(code, Some("NoSuchKey" | "NotFound" | "404"))
        || (code.is_none()
            && error
                .raw_response()
                .is_some_and(|response| response.status().as_u16() == 404));
    if missing {
        FileServiceError::NotFound
    } else {
        storage_error(operation, error)
    }
}

async fn body(response: GetObjectOutput) -> Result<Vec<u8>, FileServiceError> {
    Ok(response
        .body
        .collect()
        .await
        .map_err(|error| storage_error("body read", error))?
        .into_bytes()
        .to_vec())
}

#[async_trait]
impl StorageTrait for S3Storage {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError> {
        let encrypted_data = encryption::encrypt_with_key(&data, self.key_ring.write_key())?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(encrypted_data))
            .content_type(content_type)
            .metadata(KEY_ID_METADATA, self.key_ring.write_id())
            .send()
            .await
            .map_err(|error| storage_error("upload", error))?;
        Ok(())
    }

    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
        let response = self.get_object(key).await?;
        let hint = response
            .metadata()
            .and_then(|metadata| metadata.get(KEY_ID_METADATA))
            .cloned();
        Ok(
            encryption::decrypt_with_ring(&body(response).await?, &self.key_ring, hint.as_deref())?
                .0,
        )
    }

    async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| storage_error("delete", error))?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|service| service.is_not_found()) =>
            {
                Ok(false)
            }
            Err(error) => Err(storage_error("existence check", error)),
        }
    }

    async fn list_keys(
        &self,
        continuation: Option<String>,
        max: i32,
    ) -> Result<(Vec<String>, Option<String>), FileServiceError> {
        page_size(max)?;
        let response = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .set_continuation_token(continuation)
            .max_keys(max)
            .send()
            .await
            .map_err(|error| match error.as_service_error() {
                Some(service) => FileServiceError::StorageError(format!(
                    "Object listing failed: {}: {}",
                    service.code().unwrap_or("Unknown"),
                    service.message().unwrap_or("No message provided")
                )),
                None => storage_error("listing", error),
            })?;
        let keys = response
            .contents()
            .iter()
            .filter_map(|object| object.key().map(str::to_owned))
            .collect();
        Ok((keys, response.next_continuation_token().map(str::to_owned)))
    }

    async fn object_key_id(&self, key: &str) -> Result<Option<String>, FileServiceError> {
        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| object_error("metadata read", error))?;
        Ok(response
            .metadata()
            .and_then(|metadata| metadata.get(KEY_ID_METADATA))
            .cloned())
    }

    async fn rewrap_object(&self, key: &str) -> Result<RewrapOutcome, FileServiceError> {
        if self.object_key_id(key).await?.as_deref() == Some(self.key_ring.write_id()) {
            return Ok(RewrapOutcome::AlreadyCurrent);
        }
        let response = self.get_object(key).await?;
        let mut metadata = response.metadata().cloned().unwrap_or_default();
        let content_type = response.content_type().map(str::to_owned);
        let etag = response.e_tag().map(str::to_owned);
        let (plaintext, from) = match encryption::decrypt_with_ring(
            &body(response).await?,
            &self.key_ring,
            metadata.get(KEY_ID_METADATA).map(String::as_str),
        ) {
            Ok(value) => value,
            Err(_) => return Ok(RewrapOutcome::Undecryptable),
        };
        let encrypted_data = encryption::encrypt_with_key(&plaintext, self.key_ring.write_key())?;
        metadata.insert(
            KEY_ID_METADATA.to_owned(),
            self.key_ring.write_id().to_owned(),
        );
        // If-Match prevents recreation after deletion when GET supplies an ETag.
        // Without an ETag the PUT is unconditional: deletion during rewrap may
        // recreate an orphan after its file row is gone; listing-based cleanup
        // can remove it. Keep the existing conditional write when available.
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(encrypted_data))
            .set_content_type(content_type)
            .set_metadata(Some(metadata))
            .set_if_match(etag)
            .send()
            .await
            .map_err(|error| object_error("rewrap", error))?;
        Ok(RewrapOutcome::Rewrapped { from })
    }

    async fn verify_object(&self, key: &str) -> Result<ObjectVerification, FileServiceError> {
        let response = self.get_object(key).await?;
        let is_current = response
            .metadata()
            .and_then(|metadata| metadata.get(KEY_ID_METADATA))
            .map(String::as_str)
            == Some(self.key_ring.write_id());
        let encrypted_data = body(response).await?;
        Ok(ObjectVerification {
            is_current,
            is_decryptable: encryption::decrypt_with_key(
                &encrypted_data,
                self.key_ring.write_key(),
            )
            .is_ok(),
        })
    }
}

#[cfg(test)]
#[path = "s3_missing_tests.rs"]
mod missing_tests;
