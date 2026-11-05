use super::{
    page_size, FileServiceError, ObjectKeyState, ObjectMetadata, ObjectVerification, RewrapOutcome,
    StorageTrait, KEY_ID_METADATA,
};
use crate::files::encryption::{self, ObjectKeyRing};
use async_trait::async_trait;
use aws_sdk_s3::{
    operation::get_object::GetObjectOutput, primitives::ByteStream, Client as S3Client,
};
use std::sync::Arc;

#[path = "s3_io.rs"]
mod io;
use io::{body, bounded_body, content_length, exists_result, object_error};

#[derive(Clone)]
pub struct S3Storage {
    client: S3Client,
    bucket: String,
    key_ring: Arc<ObjectKeyRing>,
}

impl S3Storage {
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
            .map_err(|error| object_error("upload", error))?;
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
            .map_err(|error| object_error("delete", error))?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        exists_result(
            self.client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await,
        )
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
            .map_err(|error| object_error("listing", error))?;
        let keys = response
            .contents()
            .iter()
            .filter_map(|object| object.key().map(str::to_owned))
            .collect();
        Ok((keys, response.next_continuation_token().map(str::to_owned)))
    }

    async fn object_metadata(&self, key: &str) -> Result<ObjectMetadata, FileServiceError> {
        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| object_error("metadata read", error))?;
        Ok(ObjectMetadata {
            key_id: response
                .metadata()
                .and_then(|metadata| metadata.get(KEY_ID_METADATA))
                .cloned(),
            content_length: content_length(response.content_length())?,
        })
    }

    async fn rewrap_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<RewrapOutcome, FileServiceError> {
        let head = self.object_metadata(key).await?;
        head.enforce_limit(max_bytes, &self.key_ring)?;
        if head.key_id.as_deref() == Some(self.key_ring.write_id()) {
            return Ok(RewrapOutcome::AlreadyCurrent);
        }
        let response = self.get_object(key).await?;
        let mut metadata = response.metadata().cloned().unwrap_or_default();
        let content_type = response.content_type().map(str::to_owned);
        let etag = response.e_tag().map(str::to_owned);
        let unknown_key_id = metadata
            .get(KEY_ID_METADATA)
            .is_some_and(|id| self.key_ring.lookup(id).is_none());
        let (plaintext, from) = match encryption::decrypt_with_ring(
            &bounded_body(
                response,
                max_bytes,
                ObjectKeyState::for_key_id(
                    metadata.get(KEY_ID_METADATA).map(String::as_str),
                    &self.key_ring,
                ),
            )
            .await?,
            &self.key_ring,
            metadata.get(KEY_ID_METADATA).map(String::as_str),
        ) {
            Ok(value) => value,
            Err(_) if unknown_key_id => return Ok(RewrapOutcome::UndecryptableUnknownKeyId),
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
        Ok(if unknown_key_id {
            RewrapOutcome::RewrappedUnknownKeyId
        } else {
            RewrapOutcome::Rewrapped { from }
        })
    }

    async fn verify_object(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<ObjectVerification, FileServiceError> {
        let head = self.object_metadata(key).await?;
        head.enforce_limit(max_bytes, &self.key_ring)?;
        if ObjectKeyState::for_key_id(head.key_id.as_deref(), &self.key_ring)
            == ObjectKeyState::UnknownKeyId
        {
            return Ok(ObjectVerification::authenticate(
                &[],
                &self.key_ring,
                head.key_id.as_deref(),
            ));
        }
        let response = self.get_object(key).await?;
        let key_id = response
            .metadata()
            .and_then(|metadata| metadata.get(KEY_ID_METADATA))
            .cloned();
        let key_state = ObjectKeyState::for_key_id(key_id.as_deref(), &self.key_ring);
        if key_state == ObjectKeyState::UnknownKeyId {
            return Ok(ObjectVerification::authenticate(
                &[],
                &self.key_ring,
                key_id.as_deref(),
            ));
        }
        let encrypted_data = bounded_body(response, max_bytes, key_state).await?;
        Ok(ObjectVerification::authenticate(
            &encrypted_data,
            &self.key_ring,
            key_id.as_deref(),
        ))
    }
}

#[cfg(test)]
#[path = "s3_missing_tests.rs"]
mod missing_tests;
