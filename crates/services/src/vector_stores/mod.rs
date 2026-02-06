pub mod ports;

pub use ports::{PaginationParams, VectorStoreRef, VectorStoreRefRepository};

use crate::common::RepositoryError;
use crate::files;
use crate::files::FileRepositoryTrait;
use crate::rag::{RagError, RagServiceTrait};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// ID prefix helpers
// ---------------------------------------------------------------------------

use crate::id_prefixes::{PREFIX_FILE, PREFIX_VS, PREFIX_VSFB};

/// Strip a known prefix from an ID string and return the raw UUID string.
fn strip_prefix(id: &str, prefix: &str) -> String {
    id.strip_prefix(prefix).unwrap_or(id).to_string()
}

/// Prefix a field in an object map if it exists and doesn't already have the prefix.
fn prefix_field(obj: &mut serde_json::Map<String, Value>, field: &str, prefix: &str) {
    if let Some(id_val) = obj.get(field).and_then(|v| v.as_str()).map(String::from) {
        if !id_val.starts_with(prefix) {
            obj.insert(
                field.to_string(),
                Value::String(format!("{prefix}{id_val}")),
            );
        }
    }
}

/// Add all known ID prefixes to a RAG response object.
/// Handles: id (vs_), vector_store_id (vs_), file_id (file-), batch_id (vsfb_)
/// Also prefixes first_id/last_id cursors in list responses.
fn add_id_prefixes(val: &mut Value) {
    if let Some(obj) = val.as_object_mut() {
        // Determine the object type to know which prefix to use for "id"
        let object_type = obj
            .get("object")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match object_type.as_str() {
            "vector_store" | "vector_store.deleted" => {
                prefix_field(obj, "id", PREFIX_VS);
            }
            "vector_store.file" | "vector_store.file.deleted" => {
                prefix_field(obj, "id", PREFIX_FILE);
                prefix_field(obj, "file_id", PREFIX_FILE);
                prefix_field(obj, "vector_store_id", PREFIX_VS);
            }
            "vector_store.file_batch" => {
                prefix_field(obj, "id", PREFIX_VSFB);
                prefix_field(obj, "vector_store_id", PREFIX_VS);
            }
            _ => {
                // For search results and other responses, try to prefix known fields
                prefix_field(obj, "file_id", PREFIX_FILE);
                prefix_field(obj, "vector_store_id", PREFIX_VS);
            }
        }

        // Recurse into "data" arrays (for list responses and search results)
        if let Some(data) = obj.get_mut("data") {
            if let Some(arr) = data.as_array_mut() {
                for item in arr.iter_mut() {
                    add_id_prefixes(item);
                }
            }
        }

        // Prefix list cursors (first_id, last_id) based on list content type
        // Determine prefix for cursor fields based on object type or data contents
        let cursor_prefix = match object_type.as_str() {
            "list" => {
                // Inspect first data item to determine cursor prefix
                obj.get("data")
                    .and_then(|d| d.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|item| item.get("object"))
                    .and_then(|o| o.as_str())
                    .and_then(|obj_type| match obj_type {
                        "vector_store" | "vector_store.deleted" => Some(PREFIX_VS),
                        "vector_store.file" | "vector_store.file.deleted" => Some(PREFIX_FILE),
                        "vector_store.file_batch" => Some(PREFIX_VSFB),
                        _ => None,
                    })
            }
            _ => None,
        };

        if let Some(prefix) = cursor_prefix {
            prefix_field(obj, "first_id", prefix);
            prefix_field(obj, "last_id", prefix);
        }
    }
}

/// Strip prefixes from file_ids in a JSON body (for sending to RAG).
fn strip_file_ids_in_body(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        // Strip file_id field
        if let Some(fid) = obj
            .get("file_id")
            .and_then(|v| v.as_str())
            .map(String::from)
        {
            obj.insert(
                "file_id".to_string(),
                Value::String(strip_prefix(&fid, PREFIX_FILE)),
            );
        }

        // Strip file_ids array
        if let Some(file_ids) = obj.get_mut("file_ids") {
            if let Some(arr) = file_ids.as_array_mut() {
                for item in arr.iter_mut() {
                    if let Some(s) = item.as_str().map(|s| strip_prefix(s, PREFIX_FILE)) {
                        *item = Value::String(s);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Typed request structs (OpenAI API spec — no internal fields)
// ---------------------------------------------------------------------------

/// Client request body for POST /v1/vector_stores/{id}/files
#[derive(Debug, Serialize, Deserialize)]
pub struct AttachFileRequest {
    pub file_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunking_strategy: Option<Value>,
}

/// Per-file spec used in CreateFileBatchRequest.files[]
#[derive(Debug, Serialize, Deserialize)]
pub struct FileSpec {
    pub file_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunking_strategy: Option<Value>,
}

/// Client request body for POST /v1/vector_stores/{id}/file_batches
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateFileBatchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunking_strategy: Option<Value>,
}

/// Internal struct for RAG file_metadata map entries
#[derive(Debug, Serialize)]
pub struct RagFileMetadataEntry {
    pub file_id: String,
    pub filename: String,
}

// ---------------------------------------------------------------------------
// Typed filter structs for RAG passthrough (prevents mass assignment)
// ---------------------------------------------------------------------------

/// Allowed fields for POST /v1/vector_stores/{id} (modify).
#[derive(Debug, Serialize, Deserialize)]
struct ModifyVectorStoreFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<HashMap<String, Value>>,
}

/// Allowed fields for POST /v1/vector_stores/{id}/search.
#[derive(Debug, Serialize, Deserialize)]
struct SearchVectorStoreFilter {
    query: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_num_results: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ranking_options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rewrite_query: Option<bool>,
}

/// Allowed fields for POST /v1/vector_stores/{id}/files/{file_id} (update attributes).
#[derive(Debug, Serialize, Deserialize)]
struct UpdateFileAttributesFilter {
    attributes: HashMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Service Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum VectorStoreServiceError {
    #[error("Vector store not found")]
    NotFound,
    #[error("File not found or not accessible")]
    FileNotFound,
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("RAG service error: {0}")]
    RagError(#[from] RagError),
    #[error("Repository error: {0}")]
    RepositoryError(#[from] RepositoryError),
}

// ---------------------------------------------------------------------------
// Service Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VectorStoreServiceTrait: Send + Sync {
    async fn create_vector_store(
        &self,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn get_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn list_vector_stores(
        &self,
        workspace_id: Uuid,
        params: &PaginationParams,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn update_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn delete_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn search_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn attach_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn list_vs_files(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        query_string: &str,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn get_vs_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn update_vs_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn detach_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn create_file_batch(
        &self,
        vs_uuid: Uuid,
        file_uuids: &[Uuid],
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn get_file_batch(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn cancel_file_batch(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError>;

    async fn list_batch_files(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
        query_string: &str,
    ) -> Result<Value, VectorStoreServiceError>;
}

// ---------------------------------------------------------------------------
// Service Implementation — Thin Proxy
// ---------------------------------------------------------------------------

pub struct VectorStoreServiceImpl {
    ref_repo: Arc<dyn VectorStoreRefRepository>,
    file_repo: Arc<dyn FileRepositoryTrait>,
    rag: Arc<dyn RagServiceTrait>,
}

impl VectorStoreServiceImpl {
    pub fn new(
        ref_repo: Arc<dyn VectorStoreRefRepository>,
        file_repo: Arc<dyn FileRepositoryTrait>,
        rag: Arc<dyn RagServiceTrait>,
    ) -> Self {
        Self {
            ref_repo,
            file_repo,
            rag,
        }
    }

    /// Verify vector store belongs to workspace (local DB check).
    async fn verify_vs(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<(), VectorStoreServiceError> {
        self.ref_repo
            .get(vs_uuid, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::NotFound)?;
        Ok(())
    }

    /// Verify file belongs to workspace and return the file record.
    async fn verify_file(
        &self,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<files::File, VectorStoreServiceError> {
        self.file_repo
            .get_by_id_and_workspace(file_uuid, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::FileNotFound)
    }

    /// Verify multiple files belong to workspace and return the file records.
    async fn verify_files(
        &self,
        file_uuids: &[Uuid],
        workspace_id: Uuid,
    ) -> Result<Vec<files::File>, VectorStoreServiceError> {
        if file_uuids.is_empty() {
            return Ok(vec![]);
        }
        let files = self
            .file_repo
            .get_by_ids_and_workspace(file_uuids, workspace_id)
            .await?;
        if files.len() != file_uuids.len() {
            return Err(VectorStoreServiceError::FileNotFound);
        }
        Ok(files)
    }
}

#[async_trait]
impl VectorStoreServiceTrait for VectorStoreServiceImpl {
    async fn create_vector_store(
        &self,
        workspace_id: Uuid,
        mut body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        // If file_ids present in body, verify ALL belong to workspace and get metadata
        if let Some(file_ids) = body.get("file_ids").and_then(|v| v.as_array()) {
            let uuids: Vec<Uuid> = file_ids
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let s = v.as_str().ok_or_else(|| {
                        VectorStoreServiceError::InvalidParams(format!(
                            "file_ids[{i}] must be a string"
                        ))
                    })?;
                    let raw = strip_prefix(s, PREFIX_FILE);
                    Uuid::parse_str(&raw).map_err(|_| {
                        VectorStoreServiceError::InvalidParams(format!(
                            "file_ids[{i}]: invalid file ID format"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let files = self.verify_files(&uuids, workspace_id).await?;

            // Strip file ID prefixes before sending to RAG
            strip_file_ids_in_body(&mut body);

            if let Some(obj) = body.as_object_mut() {
                // Build file_metadata map keyed by file_id
                let metadata: serde_json::Map<String, Value> = files
                    .iter()
                    .map(|f| {
                        let id_str = f.id.to_string();
                        (
                            id_str.clone(),
                            serde_json::json!({ "file_id": id_str, "filename": f.filename, "storage_key": f.storage_key }),
                        )
                    })
                    .collect();
                obj.insert("file_metadata".to_string(), Value::Object(metadata));
            }
        }

        // RAG first — create the vector store
        let mut response = self.rag.create_vector_store(body).await?;

        // Extract the RAG-generated UUID from response
        let rag_id = response
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| {
                VectorStoreServiceError::InvalidParams(
                    "RAG service did not return a valid id".to_string(),
                )
            })?;

        // Local: create ref (RAG succeeded, safe to create local ref)
        self.ref_repo.create(rag_id, workspace_id).await?;

        // Add prefixes to response
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn get_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        let mut response = self.rag.get_vector_store(&vs_uuid.to_string()).await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn list_vector_stores(
        &self,
        workspace_id: Uuid,
        params: &PaginationParams,
    ) -> Result<Value, VectorStoreServiceError> {
        // Query local refs for pagination
        let (refs, has_more) = self.ref_repo.list(workspace_id, params).await?;

        if refs.is_empty() {
            return Ok(serde_json::json!({
                "object": "list",
                "data": [],
                "first_id": null,
                "last_id": null,
                "has_more": false
            }));
        }

        // Collect UUIDs and batch-fetch metadata from RAG
        let rag_ids: Vec<String> = refs.iter().map(|r| r.id.to_string()).collect();
        let mut rag_response = self.rag.list_vector_stores(&rag_ids).await?;

        // Build a lookup map from RAG response
        let rag_data = rag_response
            .get_mut("data")
            .and_then(|v| v.as_array_mut())
            .cloned()
            .unwrap_or_default();

        let mut rag_map: std::collections::HashMap<String, Value> = rag_data
            .into_iter()
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?.to_string();
                Some((id, v))
            })
            .collect();

        // Merge: use local ref ordering, enrich with RAG metadata
        let mut data = Vec::with_capacity(refs.len());
        for r in &refs {
            let id_str = r.id.to_string();
            if let Some(mut item) = rag_map.remove(&id_str) {
                add_id_prefixes(&mut item);
                data.push(item);
            }
        }

        let first_id = data
            .first()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let last_id = data
            .last()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(serde_json::json!({
            "object": "list",
            "data": data,
            "first_id": first_id,
            "last_id": last_id,
            "has_more": has_more
        }))
    }

    async fn update_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        // Filter to allowed fields only (prevents mass assignment)
        let typed: ModifyVectorStoreFilter = serde_json::from_value(body)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;
        let filtered_body = serde_json::to_value(&typed)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;

        let mut response = self
            .rag
            .update_vector_store(&vs_uuid.to_string(), filtered_body)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn delete_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        // RAG first — delete the vector store
        let mut response = self.rag.delete_vector_store(&vs_uuid.to_string()).await?;

        // Local: soft-delete ref
        self.ref_repo.soft_delete(vs_uuid, workspace_id).await?;

        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn search_vector_store(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        // Filter to allowed fields only (prevents mass assignment)
        let typed: SearchVectorStoreFilter = serde_json::from_value(body)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;
        let filtered_body = serde_json::to_value(&typed)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;

        let mut response = self
            .rag
            .search_vector_store(&vs_uuid.to_string(), filtered_body)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn attach_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        let file = self.verify_file(file_uuid, workspace_id).await?;

        // Type-safe parsing of client request
        let req: AttachFileRequest = serde_json::from_value(body)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;
        let mut rag_body = serde_json::to_value(&req)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;

        // Inject internal fields for RAG S3 ingestion
        if let Some(obj) = rag_body.as_object_mut() {
            obj.insert("file_id".to_string(), Value::String(file_uuid.to_string()));
            obj.insert("filename".to_string(), Value::String(file.filename));
            obj.insert("storage_key".to_string(), Value::String(file.storage_key));
        }

        let mut response = self.rag.attach_file(&vs_uuid.to_string(), rag_body).await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn list_vs_files(
        &self,
        vs_uuid: Uuid,
        workspace_id: Uuid,
        query_string: &str,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .list_vs_files(&vs_uuid.to_string(), query_string)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn get_vs_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        self.verify_file(file_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .get_vs_file(&vs_uuid.to_string(), &file_uuid.to_string())
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn update_vs_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        self.verify_file(file_uuid, workspace_id).await?;

        // Filter to allowed fields only (prevents mass assignment)
        let typed: UpdateFileAttributesFilter = serde_json::from_value(body)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;
        let filtered_body = serde_json::to_value(&typed)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;

        let mut response = self
            .rag
            .update_vs_file(&vs_uuid.to_string(), &file_uuid.to_string(), filtered_body)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn detach_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        self.verify_file(file_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .detach_file(&vs_uuid.to_string(), &file_uuid.to_string())
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn create_file_batch(
        &self,
        vs_uuid: Uuid,
        file_uuids: &[Uuid],
        workspace_id: Uuid,
        body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        let files = self.verify_files(file_uuids, workspace_id).await?;

        // Type-safe parsing of client request
        let req: CreateFileBatchRequest = serde_json::from_value(body)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;
        let mut rag_body = serde_json::to_value(&req)
            .map_err(|e| VectorStoreServiceError::InvalidParams(e.to_string()))?;

        if let Some(obj) = rag_body.as_object_mut() {
            // Remove unverified files field — only verified file_ids should reach RAG
            obj.remove("files");

            // Replace file_ids with raw UUIDs
            let uuid_strings: Vec<Value> = file_uuids
                .iter()
                .map(|u| Value::String(u.to_string()))
                .collect();
            obj.insert("file_ids".to_string(), Value::Array(uuid_strings));

            // Build file_metadata map keyed by file_id
            let metadata: serde_json::Map<String, Value> = files
                .iter()
                .map(|f| {
                    let id_str = f.id.to_string();
                    (
                        id_str.clone(),
                        serde_json::json!({ "file_id": id_str, "filename": f.filename, "storage_key": f.storage_key }),
                    )
                })
                .collect();
            obj.insert("file_metadata".to_string(), Value::Object(metadata));
        }

        let mut response = self
            .rag
            .create_file_batch(&vs_uuid.to_string(), rag_body)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn get_file_batch(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .get_file_batch(&vs_uuid.to_string(), &batch_uuid.to_string())
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn cancel_file_batch(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .cancel_file_batch(&vs_uuid.to_string(), &batch_uuid.to_string())
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn list_batch_files(
        &self,
        vs_uuid: Uuid,
        batch_uuid: Uuid,
        workspace_id: Uuid,
        query_string: &str,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;

        let mut response = self
            .rag
            .list_batch_files(&vs_uuid.to_string(), &batch_uuid.to_string(), query_string)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }
}
