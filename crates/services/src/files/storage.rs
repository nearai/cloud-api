use super::{encryption, FileServiceError};
use async_trait::async_trait;
use aws_sdk_s3::{primitives::ByteStream, Client as S3Client};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, error};

/// Trait for file storage operations
#[async_trait]
pub trait StorageTrait: Send + Sync {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FileServiceError>;

    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError>;

    async fn delete(&self, key: &str) -> Result<(), FileServiceError>;

    async fn exists(&self, key: &str) -> Result<bool, FileServiceError>;
}

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
}

#[async_trait]
impl StorageTrait for S3Storage {
    /// Upload a file to S3 (with encryption)
    async fn upload(
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
    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
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
    async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
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

/// Mock storage for testing (stores files in memory)
#[derive(Clone)]
pub struct MockStorage {
    files: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    encryption_key: String,
}

impl MockStorage {
    pub fn new(encryption_key: String) -> Self {
        Self {
            files: Arc::new(RwLock::new(HashMap::new())),
            encryption_key,
        }
    }
}

#[async_trait]
impl StorageTrait for MockStorage {
    async fn upload(
        &self,
        key: &str,
        data: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), FileServiceError> {
        debug!("Mock storage: uploading file with key: {}", key);

        // Encrypt the data (same as S3)
        let encrypted_data = encryption::encrypt(&data, &self.encryption_key)?;

        let mut files = self.files.write().unwrap();
        files.insert(key.to_string(), encrypted_data);

        debug!("Mock storage: file uploaded successfully");
        Ok(())
    }

    async fn download(&self, key: &str) -> Result<Vec<u8>, FileServiceError> {
        debug!("Mock storage: downloading file with key: {}", key);

        let files = self.files.read().unwrap();
        let encrypted_data = files
            .get(key)
            .ok_or(FileServiceError::NotFound)?
            .clone();

        // Decrypt the data (same as S3)
        let decrypted_data = encryption::decrypt(&encrypted_data, &self.encryption_key)?;

        debug!("Mock storage: file downloaded successfully");
        Ok(decrypted_data)
    }

    async fn delete(&self, key: &str) -> Result<(), FileServiceError> {
        debug!("Mock storage: deleting file with key: {}", key);

        let mut files = self.files.write().unwrap();
        files.remove(key);

        debug!("Mock storage: file deleted successfully");
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, FileServiceError> {
        let files = self.files.read().unwrap();
        Ok(files.contains_key(key))
    }
}
