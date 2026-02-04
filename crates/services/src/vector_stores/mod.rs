pub mod ports;

pub use ports::{PaginationParams, VectorStoreRef, VectorStoreRefRepository};

use crate::common::RepositoryError;
use crate::files::FileRepositoryTrait;
use crate::rag::{RagError, RagServiceTrait};
use async_trait::async_trait;
use serde_json::Value;
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

    /// Verify file belongs to workspace.
    async fn verify_file(
        &self,
        file_uuid: Uuid,
        workspace_id: Uuid,
    ) -> Result<(), VectorStoreServiceError> {
        self.file_repo
            .get_by_id_and_workspace(file_uuid, workspace_id)
            .await?
            .ok_or(VectorStoreServiceError::FileNotFound)?;
        Ok(())
    }

    /// Verify multiple files belong to workspace.
    async fn verify_files(
        &self,
        file_uuids: &[Uuid],
        workspace_id: Uuid,
    ) -> Result<(), VectorStoreServiceError> {
        if file_uuids.is_empty() {
            return Ok(());
        }
        let all_owned = self
            .file_repo
            .verify_workspace_ownership(file_uuids, workspace_id)
            .await?;
        if !all_owned {
            return Err(VectorStoreServiceError::FileNotFound);
        }
        Ok(())
    }
}

#[async_trait]
impl VectorStoreServiceTrait for VectorStoreServiceImpl {
    async fn create_vector_store(
        &self,
        workspace_id: Uuid,
        mut body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        // If file_ids present in body, verify ALL belong to workspace
        if let Some(file_ids) = body.get("file_ids").and_then(|v| v.as_array()) {
            let uuids: Vec<Uuid> = file_ids
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| strip_prefix(s, PREFIX_FILE))
                .filter_map(|s| Uuid::parse_str(&s).ok())
                .collect();
            self.verify_files(&uuids, workspace_id).await?;
        }

        // Strip file ID prefixes before sending to RAG
        strip_file_ids_in_body(&mut body);

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

        let mut response = self
            .rag
            .update_vector_store(&vs_uuid.to_string(), body)
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

        let mut response = self
            .rag
            .search_vector_store(&vs_uuid.to_string(), body)
            .await?;
        add_id_prefixes(&mut response);
        Ok(response)
    }

    async fn attach_file(
        &self,
        vs_uuid: Uuid,
        file_uuid: Uuid,
        workspace_id: Uuid,
        mut body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        self.verify_file(file_uuid, workspace_id).await?;

        // Strip file ID prefixes
        strip_file_ids_in_body(&mut body);
        // Ensure file_id is the raw UUID
        if let Some(obj) = body.as_object_mut() {
            obj.insert("file_id".to_string(), Value::String(file_uuid.to_string()));
        }

        let mut response = self.rag.attach_file(&vs_uuid.to_string(), body).await?;
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

        let mut response = self
            .rag
            .update_vs_file(&vs_uuid.to_string(), &file_uuid.to_string(), body)
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
        mut body: Value,
    ) -> Result<Value, VectorStoreServiceError> {
        self.verify_vs(vs_uuid, workspace_id).await?;
        self.verify_files(file_uuids, workspace_id).await?;

        // Replace file_ids in body with raw UUIDs
        if let Some(obj) = body.as_object_mut() {
            let uuid_strings: Vec<Value> = file_uuids
                .iter()
                .map(|u| Value::String(u.to_string()))
                .collect();
            obj.insert("file_ids".to_string(), Value::Array(uuid_strings));
        }

        let mut response = self
            .rag
            .create_file_batch(&vs_uuid.to_string(), body)
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
