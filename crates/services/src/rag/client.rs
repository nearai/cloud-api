use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::ports::{RagError, RagFile, RagServiceTrait, SearchResult, VectorStore};

/// HTTP client for communicating with the RAG service over VPC
pub struct RagServiceClient {
    client: reqwest::Client,
    base_url: String,
    auth_token: Option<String>,
}

impl RagServiceClient {
    /// Create a new RAG service client
    ///
    /// # Arguments
    /// * `base_url` - Base URL of the RAG service (e.g. "http://rag-service:8000")
    /// * `auth_token_file` - Optional path to file containing the bearer token
    /// * `timeout_seconds` - Request timeout in seconds
    pub fn new(
        base_url: String,
        auth_token_file: Option<&str>,
        timeout_seconds: u64,
    ) -> Result<Self, RagError> {
        let auth_token = if let Some(path) = auth_token_file {
            let token = std::fs::read_to_string(path)
                .map_err(|e| RagError::RequestFailed(format!("Failed to read auth token file: {e}")))?
                .trim()
                .to_string();
            if token.is_empty() {
                tracing::warn!("RAG service auth token file is empty");
                None
            } else {
                Some(token)
            }
        } else {
            None
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_seconds))
            .build()
            .map_err(|e| RagError::RequestFailed(format!("Failed to build HTTP client: {e}")))?;

        tracing::info!(
            base_url = %base_url,
            auth_configured = auth_token.is_some(),
            "RAG service client initialized"
        );

        Ok(Self {
            client,
            base_url,
            auth_token,
        })
    }

    /// Build a request with auth header if configured
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self.client.request(method, &url);
        if let Some(ref token) = self.auth_token {
            builder = builder.bearer_auth(token);
        }
        builder
    }

    /// Check response status and extract error body if needed
    async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, RagError> {
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            let status_code = status.as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error body".to_string());
            Err(RagError::ApiError {
                status: status_code,
                body,
            })
        }
    }
}

/// Request body for creating a vector store
#[derive(Serialize)]
struct CreateVectorStoreRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

/// Request body for attaching a file to a vector store
#[derive(Serialize)]
struct AttachFileRequest {
    file_id: String,
}

/// Request body for search
#[derive(Serialize)]
struct SearchRequest {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_num_results: Option<u32>,
}

/// Response wrapper for search results
#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<SearchResultItem>,
}

/// Individual search result item from RAG service
#[derive(Deserialize)]
struct SearchResultItem {
    file_id: String,
    filename: String,
    content: String,
    score: f32,
}

#[async_trait]
impl RagServiceTrait for RagServiceClient {
    async fn create_vector_store(
        &self,
        name: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<VectorStore, RagError> {
        let body = CreateVectorStoreRequest {
            name: name.to_string(),
            metadata,
        };

        let response = self
            .request(reqwest::Method::POST, "/v1/vector_stores")
            .json(&body)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<VectorStore>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }

    async fn get_vector_store(&self, id: &str) -> Result<VectorStore, RagError> {
        let response = self
            .request(reqwest::Method::GET, &format!("/v1/vector_stores/{id}"))
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<VectorStore>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }

    async fn delete_vector_store(&self, id: &str) -> Result<(), RagError> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v1/vector_stores/{id}"),
            )
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        Self::check_response(response).await?;
        Ok(())
    }

    async fn upload_file(
        &self,
        filename: &str,
        content: Vec<u8>,
        purpose: &str,
    ) -> Result<RagFile, RagError> {
        let part = reqwest::multipart::Part::bytes(content)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("purpose", purpose.to_string());

        let response = self
            .request(reqwest::Method::POST, "/v1/files")
            .multipart(form)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<RagFile>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }

    async fn attach_file_to_store(
        &self,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<(), RagError> {
        let body = AttachFileRequest {
            file_id: file_id.to_string(),
        };

        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/vector_stores/{vector_store_id}/files"),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        Self::check_response(response).await?;
        Ok(())
    }

    async fn delete_file(&self, file_id: &str) -> Result<(), RagError> {
        let response = self
            .request(reqwest::Method::DELETE, &format!("/v1/files/{file_id}"))
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        Self::check_response(response).await?;
        Ok(())
    }

    async fn search(
        &self,
        vector_store_id: &str,
        query: &str,
        max_results: Option<u32>,
    ) -> Result<Vec<SearchResult>, RagError> {
        let body = SearchRequest {
            query: query.to_string(),
            max_num_results: max_results,
        };

        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/vector_stores/{vector_store_id}/search"),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        let search_response = response
            .json::<SearchResponse>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))?;

        Ok(search_response
            .data
            .into_iter()
            .map(|item| SearchResult {
                file_id: item.file_id,
                file_name: item.filename,
                content: item.content,
                score: item.score,
            })
            .collect())
    }
}
