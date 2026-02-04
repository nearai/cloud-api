use std::time::Duration;

use async_trait::async_trait;

use super::ports::{RagError, RagServiceTrait};

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
                .map_err(|e| {
                    RagError::RequestFailed(format!("Failed to read auth token file: {e}"))
                })?
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

    /// Send a JSON POST request and return the parsed response
    async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }

    /// Send a GET request and return the parsed response
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, RagError> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }

    /// Send a DELETE request and return the parsed response
    async fn delete_json(&self, path: &str) -> Result<serde_json::Value, RagError> {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .map_err(|e| RagError::RequestFailed(e.to_string()))?;

        let response = Self::check_response(response).await?;
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| RagError::ParseError(e.to_string()))
    }
}

#[async_trait]
impl RagServiceTrait for RagServiceClient {
    // -----------------------------------------------------------------------
    // Vector Stores
    // -----------------------------------------------------------------------

    async fn create_vector_store(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json("/v1/vector_stores", &body).await
    }

    async fn get_vector_store(&self, rag_id: &str) -> Result<serde_json::Value, RagError> {
        self.get_json(&format!("/v1/vector_stores/{rag_id}")).await
    }

    async fn list_vector_stores(&self, rag_ids: &[String]) -> Result<serde_json::Value, RagError> {
        // Build query string: ?ids=uuid1&ids=uuid2
        let query: String = rag_ids
            .iter()
            .map(|id| format!("ids={id}"))
            .collect::<Vec<_>>()
            .join("&");

        let path = if query.is_empty() {
            "/v1/vector_stores".to_string()
        } else {
            format!("/v1/vector_stores?{query}")
        };

        self.get_json(&path).await
    }

    async fn update_vector_store(
        &self,
        rag_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(&format!("/v1/vector_stores/{rag_id}"), &body)
            .await
    }

    async fn delete_vector_store(&self, rag_id: &str) -> Result<serde_json::Value, RagError> {
        self.delete_json(&format!("/v1/vector_stores/{rag_id}"))
            .await
    }

    async fn search_vector_store(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(&format!("/v1/vector_stores/{rag_vs_id}/search"), &body)
            .await
    }

    // -----------------------------------------------------------------------
    // Vector Store Files
    // -----------------------------------------------------------------------

    async fn attach_file(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(&format!("/v1/vector_stores/{rag_vs_id}/files"), &body)
            .await
    }

    async fn get_vs_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        self.get_json(&format!(
            "/v1/vector_stores/{rag_vs_id}/files/{rag_file_id}"
        ))
        .await
    }

    async fn list_vs_files(
        &self,
        rag_vs_id: &str,
        query_string: &str,
    ) -> Result<serde_json::Value, RagError> {
        let path = if query_string.is_empty() {
            format!("/v1/vector_stores/{rag_vs_id}/files")
        } else {
            format!("/v1/vector_stores/{rag_vs_id}/files?{query_string}")
        };
        self.get_json(&path).await
    }

    async fn update_vs_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(
            &format!("/v1/vector_stores/{rag_vs_id}/files/{rag_file_id}"),
            &body,
        )
        .await
    }

    async fn detach_file(
        &self,
        rag_vs_id: &str,
        rag_file_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        self.delete_json(&format!(
            "/v1/vector_stores/{rag_vs_id}/files/{rag_file_id}"
        ))
        .await
    }

    // -----------------------------------------------------------------------
    // File Batches
    // -----------------------------------------------------------------------

    async fn create_file_batch(
        &self,
        rag_vs_id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(
            &format!("/v1/vector_stores/{rag_vs_id}/file_batches"),
            &body,
        )
        .await
    }

    async fn get_file_batch(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        self.get_json(&format!(
            "/v1/vector_stores/{rag_vs_id}/file_batches/{rag_batch_id}"
        ))
        .await
    }

    async fn cancel_file_batch(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
    ) -> Result<serde_json::Value, RagError> {
        self.post_json(
            &format!("/v1/vector_stores/{rag_vs_id}/file_batches/{rag_batch_id}/cancel"),
            &serde_json::json!({}),
        )
        .await
    }

    async fn list_batch_files(
        &self,
        rag_vs_id: &str,
        rag_batch_id: &str,
        query_string: &str,
    ) -> Result<serde_json::Value, RagError> {
        let path = if query_string.is_empty() {
            format!("/v1/vector_stores/{rag_vs_id}/file_batches/{rag_batch_id}/files")
        } else {
            format!(
                "/v1/vector_stores/{rag_vs_id}/file_batches/{rag_batch_id}/files?{query_string}"
            )
        };
        self.get_json(&path).await
    }
}
