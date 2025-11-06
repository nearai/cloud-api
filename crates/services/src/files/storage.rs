use super::{encryption, FileServiceError};
use aws_sdk_s3::{primitives::ByteStream, Client as S3Client};
use tracing::{debug, error};

#[derive(Clone)]
pub struct S3Storage {
    client: S3Client,
    bucket: String,
    encryption_key: String,
}

impl S3Storage {
    pub fn new(client: S3Client, bucket: String, encryption_key: String) -> Self {
        Self {
            client,
            bucket,
            encryption_key,
        }
    }

    /// Upload a file to S3 (with encryption)
    pub async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError> {
        debug!(
            "Encrypting and uploading file to S3: bucket={}, key={}",
            self.bucket, key
        );

        // Encrypt the data before uploading
        let encrypted_data = encryption::encrypt(&data, &self.encryption_key)?;
        debug!(
            "File encrypted: original_size={}, encrypted_size={}",
            data.len(),
            encrypted_data.len()
        );

        let byte_stream = ByteStream::from(encrypted_data);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(byte_stream)
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to upload file to S3: {}", e);
                FileServiceError::StorageError(format!("Failed to upload file: {}", e))
            })?;

        debug!("Successfully uploaded encrypted file to S3: {}", key);
        Ok(())
    }

    /// Download a file from S3 (with decryption)
    pub async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
        debug!(
            "Downloading file from S3: bucket={}, key={}",
            self.bucket, key
        );

        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to download file from S3: {}", e);
                FileServiceError::StorageError(format!("Failed to download file: {}", e))
            })?;

        let encrypted_data = response
            .body
            .collect()
            .await
            .map_err(|e| {
                error!("Failed to read file data from S3: {}", e);
                FileServiceError::StorageError(format!("Failed to read file data: {}", e))
            })?
            .into_bytes()
            .to_vec();

        debug!(
            "Downloaded encrypted file from S3: {} bytes",
            encrypted_data.len()
        );

        // Decrypt the data after downloading
        let decrypted_data = encryption::decrypt(&encrypted_data, &self.encryption_key)?;
        debug!(
            "File decrypted: encrypted_size={}, decrypted_size={}",
            encrypted_data.len(),
            decrypted_data.len()
        );

        Ok(decrypted_data)
    }

    /// Delete a file from S3
    pub async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
        debug!("Deleting file from S3: bucket={}, key={}", self.bucket, key);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to delete file from S3: {}", e);
                FileServiceError::StorageError(format!("Failed to delete file: {}", e))
            })?;

        debug!("Successfully deleted file from S3: {}", key);
        Ok(())
    }

    /// Check if a file exists in S3
    pub async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let error_str = e.to_string();
                if error_str.contains("Not Found") || error_str.contains("404") {
                    Ok(false)
                } else {
                    error!("Failed to check file existence in S3: {}", e);
                    Err(FileServiceError::StorageError(format!(
                        "Failed to check file existence: {}",
                        e
                    )))
                }
            }
        }
    }
}
