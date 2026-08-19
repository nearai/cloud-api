//! Request-scoped, in-memory repositories for stateless Responses requests.
//!
//! These implementations deliberately satisfy the existing response repository
//! interfaces without touching the database.  A fresh store is created for each
//! request so response and response-item data are discarded when the request
//! finishes.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{
    conversations::models::ConversationId,
    responses::{models, ports},
    workspace::WorkspaceId,
};

#[derive(Default)]
struct TransientResponseStore {
    responses: Mutex<HashMap<Uuid, models::ResponseObject>>,
    response_items: Mutex<Vec<TransientResponseItem>>,
}

struct TransientResponseItem {
    id: Uuid,
    response_id: Uuid,
    api_key_id: Uuid,
    item: models::ResponseOutputItem,
}

struct TransientResponseRepository {
    store: Arc<TransientResponseStore>,
}

struct TransientResponseItemsRepository {
    store: Arc<TransientResponseStore>,
}

/// Build repository implementations that live only for one Responses request.
pub(super) fn repositories() -> (
    Arc<dyn ports::ResponseRepositoryTrait>,
    Arc<dyn ports::ResponseItemRepositoryTrait>,
) {
    let store = Arc::new(TransientResponseStore::default());

    (
        Arc::new(TransientResponseRepository {
            store: store.clone(),
        }),
        Arc::new(TransientResponseItemsRepository { store }),
    )
}

fn response_id_string(response_id: Uuid) -> String {
    format!("resp_{}", response_id.simple())
}

fn default_tools() -> Vec<models::ResponseTool> {
    vec![models::ResponseTool::WebSearch {
        filters: None,
        search_context_size: Some("medium".to_string()),
        user_location: Some(models::UserLocation {
            type_: "approximate".to_string(),
            city: None,
            country: Some("US".to_string()),
            region: None,
            timezone: None,
        }),
    }]
}

fn initial_response(request: models::CreateResponseRequest) -> (Uuid, models::ResponseObject) {
    let response_id = Uuid::new_v4();
    let now = chrono::Utc::now().timestamp();

    (
        response_id,
        models::ResponseObject {
            id: response_id_string(response_id),
            object: "response".to_string(),
            created_at: now,
            status: models::ResponseStatus::InProgress,
            // Stateless requests never retain a response in the background or
            // attach it to a persisted conversation.
            background: false,
            conversation: None,
            error: None,
            incomplete_details: None,
            instructions: request.instructions,
            max_output_tokens: request.max_output_tokens,
            max_tool_calls: request.max_tool_calls,
            model: request.model,
            output: vec![],
            parallel_tool_calls: request.parallel_tool_calls.unwrap_or(false),
            previous_response_id: None,
            next_response_ids: vec![],
            prompt_cache_key: request.prompt_cache_key,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: request.safety_identifier,
            service_tier: "default".to_string(),
            store: false,
            temperature: request.temperature.unwrap_or(1.0),
            tool_choice: models::ResponseToolChoiceOutput::Auto("auto".to_string()),
            tools: request.tools.unwrap_or_else(default_tools),
            top_logprobs: 0,
            top_p: request.top_p.unwrap_or(1.0),
            truncation: "disabled".to_string(),
            usage: models::Usage::new(0, 0),
            user: None,
            metadata: Some(request.metadata.unwrap_or_else(|| serde_json::json!({}))),
        },
    )
}

fn response_item_id(item: &models::ResponseOutputItem) -> Uuid {
    item.id()
        .rsplit('_')
        .next()
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(Uuid::new_v4)
}

fn decorate_response_item(
    item: &mut models::ResponseOutputItem,
    response: &models::ResponseObject,
) {
    let response_id = response.id.clone();
    let previous_response_id = response.previous_response_id.clone();
    let created_at = chrono::Utc::now().timestamp();

    match item {
        models::ResponseOutputItem::Message {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::ToolCall {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::WebSearchCall {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::Reasoning {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::McpCall {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::McpApprovalRequest {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        }
        | models::ResponseOutputItem::FunctionCall {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            model,
            ..
        } => {
            *item_response_id = response_id;
            *item_previous_response_id = previous_response_id;
            *next_response_ids = vec![];
            *item_created_at = created_at;
            if model.is_empty() {
                *model = response.model.clone();
            }
        }
        models::ResponseOutputItem::FunctionCallOutput {
            response_id: item_response_id,
            previous_response_id: item_previous_response_id,
            next_response_ids,
            created_at: item_created_at,
            ..
        } => {
            *item_response_id = response_id;
            *item_previous_response_id = previous_response_id;
            *next_response_ids = vec![];
            *item_created_at = created_at;
        }
        models::ResponseOutputItem::McpListTools { .. } => {}
    }
}

#[async_trait]
impl ports::ResponseRepositoryTrait for TransientResponseRepository {
    async fn create(
        &self,
        _workspace_id: WorkspaceId,
        _api_key_id: Uuid,
        request: models::CreateResponseRequest,
    ) -> anyhow::Result<models::ResponseObject> {
        let (response_id, response) = initial_response(request);
        self.store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?
            .insert(response_id, response.clone());
        Ok(response)
    }

    async fn get_by_id(
        &self,
        id: models::ResponseId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>> {
        Ok(self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?
            .get(&id.0)
            .cloned())
    }

    async fn update(
        &self,
        id: models::ResponseId,
        _workspace_id: WorkspaceId,
        _output_message: Option<String>,
        status: models::ResponseStatus,
        usage: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<models::ResponseObject>> {
        let mut responses = self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?;
        let Some(response) = responses.get_mut(&id.0) else {
            return Ok(None);
        };

        response.status = status;
        if let Some(usage) = usage {
            response.usage = serde_json::from_value(usage)
                .map_err(|error| anyhow::anyhow!("Invalid transient response usage: {error}"))?;
        }

        Ok(Some(response.clone()))
    }

    async fn delete(
        &self,
        id: models::ResponseId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<bool> {
        Ok(self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?
            .remove(&id.0)
            .is_some())
    }

    async fn cancel(
        &self,
        id: models::ResponseId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>> {
        let mut responses = self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?;
        let Some(response) = responses.get_mut(&id.0) else {
            return Ok(None);
        };

        response.status = models::ResponseStatus::Cancelled;
        Ok(Some(response.clone()))
    }

    async fn list_by_workspace(
        &self,
        _workspace_id: WorkspaceId,
        _limit: i64,
        _offset: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>> {
        Ok(self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?
            .values()
            .cloned()
            .collect())
    }

    async fn list_by_conversation(
        &self,
        _conversation_id: ConversationId,
        _workspace_id: WorkspaceId,
        _limit: i64,
    ) -> anyhow::Result<Vec<models::ResponseObject>> {
        Ok(vec![])
    }

    async fn get_previous(
        &self,
        _response_id: models::ResponseId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>> {
        Ok(None)
    }

    async fn get_latest_in_conversation(
        &self,
        _conversation_id: ConversationId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseObject>> {
        Ok(None)
    }

    async fn get_or_create_root_response(
        &self,
        _conversation_id: ConversationId,
        _workspace_id: WorkspaceId,
        _api_key_id: Uuid,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "Conversations are not supported by stateless Responses requests"
        ))
    }
}

#[async_trait]
impl ports::ResponseItemRepositoryTrait for TransientResponseItemsRepository {
    async fn create(
        &self,
        response_id: models::ResponseId,
        api_key_id: Uuid,
        _conversation_id: Option<ConversationId>,
        mut item: models::ResponseOutputItem,
    ) -> anyhow::Result<models::ResponseOutputItem> {
        let response = self
            .store
            .responses
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response store lock poisoned"))?
            .get(&response_id.0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Transient response not found"))?;
        decorate_response_item(&mut item, &response);

        self.store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?
            .push(TransientResponseItem {
                id: response_item_id(&item),
                response_id: response_id.0,
                api_key_id,
                item: item.clone(),
            });

        Ok(item)
    }

    async fn get_by_id(
        &self,
        id: models::ResponseItemId,
        _workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<models::ResponseOutputItem>> {
        Ok(self
            .store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?
            .iter()
            .find(|stored| stored.id == id.0)
            .map(|stored| stored.item.clone()))
    }

    async fn update(
        &self,
        id: models::ResponseItemId,
        item: models::ResponseOutputItem,
    ) -> anyhow::Result<models::ResponseOutputItem> {
        let mut items = self
            .store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?;
        let stored = items
            .iter_mut()
            .find(|stored| stored.id == id.0)
            .ok_or_else(|| anyhow::anyhow!("Transient response item not found"))?;
        stored.item = item.clone();
        Ok(item)
    }

    async fn delete(&self, id: models::ResponseItemId) -> anyhow::Result<bool> {
        let mut items = self
            .store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?;
        let original_len = items.len();
        items.retain(|stored| stored.id != id.0);
        Ok(items.len() != original_len)
    }

    async fn list_by_response(
        &self,
        response_id: models::ResponseId,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>> {
        Ok(self
            .store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?
            .iter()
            .filter(|stored| stored.response_id == response_id.0)
            .map(|stored| stored.item.clone())
            .collect())
    }

    async fn list_by_api_key(
        &self,
        api_key_id: Uuid,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>> {
        Ok(self
            .store
            .response_items
            .lock()
            .map_err(|_| anyhow::anyhow!("Transient response item store lock poisoned"))?
            .iter()
            .filter(|stored| stored.api_key_id == api_key_id)
            .map(|stored| stored.item.clone())
            .collect())
    }

    async fn list_by_conversation(
        &self,
        _conversation_id: ConversationId,
        _workspace_id: WorkspaceId,
        _after: Option<String>,
        _limit: i64,
    ) -> anyhow::Result<Vec<models::ResponseOutputItem>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> models::CreateResponseRequest {
        models::CreateResponseRequest {
            model: "test-model".to_string(),
            input: None,
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: None,
            max_tool_calls: None,
            temperature: None,
            top_p: None,
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    #[tokio::test]
    async fn repositories_keep_response_data_in_request_scoped_memory() {
        let (responses, response_items) = repositories();
        let workspace_id = WorkspaceId(Uuid::new_v4());
        let api_key_id = Uuid::new_v4();

        let response = responses
            .create(workspace_id.clone(), api_key_id, request())
            .await
            .unwrap();
        // Omitted `store` and `background` fields default to the stateless
        // values when the transient response is constructed.
        assert!(!response.store);
        assert!(!response.background);
        assert!(response.conversation.is_none());

        let response_id = models::ResponseId(
            Uuid::parse_str(response.id.strip_prefix("resp_").unwrap())
                .expect("transient response ID is a UUID"),
        );
        let message = models::ResponseOutputItem::Message {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            response_id: String::new(),
            previous_response_id: None,
            next_response_ids: vec![],
            created_at: 0,
            status: models::ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![models::ResponseContentItem::OutputText {
                text: "hello".to_string(),
                annotations: vec![],
                logprobs: vec![],
            }],
            model: String::new(),
            metadata: None,
        };

        response_items
            .create(response_id.clone(), api_key_id, None, message)
            .await
            .unwrap();

        let items = response_items.list_by_response(response_id).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].response_id(), Some(response.id.as_str()));
    }
}
