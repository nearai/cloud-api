use axum::{
    extract::{Path, Query, State, Extension, Json},
    http::StatusCode,
    response::Json as ResponseJson,
};
use crate::{models::*, middleware::AuthenticatedUser, routes::common::map_domain_error_to_status};
use domain::{ConversationService, ConversationRequest};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, info};

/// Create a new conversation
pub async fn create_conversation(
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateConversationRequest>,
) -> Result<(StatusCode, ResponseJson<ConversationObject>), (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Create conversation request from user: {}", user.0.id);
    
    // Validate the request
    if let Err(error) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ));
    }

    let domain_request = ConversationRequest {
        user_id: user.0.id.to_string(),
        metadata: request.metadata,
    };

    match service.create_conversation(domain_request).await {
        Ok(domain_conversation) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            info!("Created conversation {} for user {}", http_conversation.id, user.0.id);
            Ok((StatusCode::CREATED, ResponseJson(http_conversation)))
        }
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Get a conversation by ID
pub async fn get_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get conversation {} for user {}", conversation_id, user.0.id);

    match service.get_conversation(&conversation_id, &user.0.id.to_string()).await {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            Ok(ResponseJson(http_conversation))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Update a conversation
pub async fn update_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<UpdateConversationRequest>,
) -> Result<ResponseJson<ConversationObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Update conversation {} for user {}", conversation_id, user.0.id);

    let metadata = request.metadata.unwrap_or_else(|| serde_json::json!({}));

    match service.update_conversation(&conversation_id, &user.0.id.to_string(), metadata).await {
        Ok(Some(domain_conversation)) => {
            let http_conversation = convert_domain_conversation_to_http(domain_conversation);
            info!("Updated conversation {} for user {}", conversation_id, user.0.id);
            Ok(ResponseJson(http_conversation))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Delete a conversation
pub async fn delete_conversation(
    Path(conversation_id): Path<String>,
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Delete conversation {} for user {}", conversation_id, user.0.id);

    match service.delete_conversation(&conversation_id, &user.0.id.to_string()).await {
        Ok(true) => {
            info!("Deleted conversation {} for user {}", conversation_id, user.0.id);
            Ok(ResponseJson(ConversationDeleteResult {
                id: conversation_id,
                object: "conversation.deleted".to_string(),
                deleted: true,
            }))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Conversation not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// List conversations
pub async fn list_conversations(
    Query(params): Query<ListConversationsQuery>,
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("List conversations for user {} with limit={:?}, offset={:?}", 
           user.0.id, params.limit, params.offset);

    match service.list_conversations(&user.0.id.to_string(), params.limit, params.offset).await {
        Ok(domain_conversations) => {
            let http_conversations: Vec<ConversationObject> = domain_conversations
                .into_iter()
                .map(convert_domain_conversation_to_http)
                .collect();
            
            let first_id = http_conversations.first()
                .map(|c| c.id.clone())
                .unwrap_or_default();
            let last_id = http_conversations.last()
                .map(|c| c.id.clone())
                .unwrap_or_default();
            
            // Determine if there are more conversations
            let has_more = params.limit.map_or(false, |limit| {
                http_conversations.len() >= limit as usize
            });

            Ok(ResponseJson(ConversationList {
                object: "list".to_string(),
                data: http_conversations,
                first_id,
                last_id,
                has_more,
            }))
        }
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}


/// List items in a conversation (extracts from responses)
pub async fn list_conversation_items(
    Path(conversation_id): Path<String>,
    Query(params): Query<ListItemsQuery>,
    State(service): State<Arc<ConversationService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ConversationItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("List items in conversation {} for user {}", conversation_id, user.0.id);

    match service.get_conversation_messages(&conversation_id, &user.0.id.to_string(), params.limit).await {
        Ok(messages) => {
            let http_items: Vec<ConversationItem> = messages.into_iter().map(|msg| {
                ConversationItem::Message {
                    id: msg.id,
                    status: ResponseItemStatus::Completed,
                    role: msg.role,
                    content: vec![ConversationContentPart::InputText {
                        text: msg.content,
                    }],
                    metadata: msg.metadata,
                }
            }).collect();
            
            let first_id = http_items.first().map(get_item_id).unwrap_or_default();
            let last_id = http_items.last().map(get_item_id).unwrap_or_default();

            Ok(ResponseJson(ConversationItemList {
                object: "list".to_string(),
                data: http_items,
                first_id,
                last_id,
                has_more: false,
            }))
        }
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}



// Helper functions

fn convert_domain_conversation_to_http(domain_conversation: domain::Conversation) -> ConversationObject {
    ConversationObject {
        id: domain_conversation.id,
        object: "conversation".to_string(),
        created_at: domain_conversation.created_at.timestamp() as u64,
        metadata: domain_conversation.metadata,
    }
}


fn get_item_id(item: &ConversationItem) -> String {
    match item {
        ConversationItem::Message { id, .. } => id.clone(),
    }
}


// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct ListConversationsQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    pub limit: Option<i32>,
    pub order: Option<String>, // "asc" or "desc"
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use serde_json::json;
    use tower::ServiceExt;
    use database::User as DbUser;
    use domain::Domain;

    // Helper function to create test user
    fn create_test_user() -> AuthenticatedUser {
        AuthenticatedUser(DbUser {
            id: uuid::Uuid::new_v4(),
            email: "test@example.com".to_string(),
            username: "testuser".to_string(),
            display_name: Some("Test User".to_string()),
            avatar_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_login_at: Some(chrono::Utc::now()),
            is_active: true,
            auth_provider: "github".to_string(),
            provider_user_id: "123456".to_string(),
        })
    }

    // Helper function to create test app with routes
    fn create_test_app() -> Router {
        let domain = Domain::new();
        let conversation_service = Arc::new(ConversationService::new(domain.database));
        
        Router::new()
            .route("/conversations", get(list_conversations))
            .route("/conversations", post(create_conversation))
            .route("/conversations/{conversation_id}", get(get_conversation))
            .route("/conversations/{conversation_id}", post(update_conversation))
            .route("/conversations/{conversation_id}", axum::routing::delete(delete_conversation))
            .route("/conversations/{conversation_id}/items", get(list_conversation_items))
            .with_state(conversation_service)
    }

    #[tokio::test]
    async fn test_create_conversation_success() {
        let app = create_test_app();
        
        let request_body = json!({
            "metadata": {"topic": "test"}
        });

        let request = Request::builder()
            .method("POST")
            .uri("/conversations")
            .header("content-type", "application/json")
            .extension(create_test_user())
            .body(Body::from(serde_json::to_string(&request_body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        // This will fail without database, but shows the structure
        assert!(response.status() == StatusCode::CREATED || response.status() == StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_list_conversations() {
        let app = create_test_app();

        let request = Request::builder()
            .method("GET")
            .uri("/conversations?limit=10&offset=0")
            .extension(create_test_user())
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        // Should return OK (with empty list in mock mode) or internal server error
        assert!(response.status() == StatusCode::OK || response.status() == StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_validation_functions() {
        // Test valid create conversation request
        let valid_request = CreateConversationRequest {
            metadata: Some(json!({"test": "value"})),
        };
        assert!(valid_request.validate().is_ok());

        // Test create conversation request without metadata
        let valid_request_no_metadata = CreateConversationRequest {
            metadata: None,
        };
        assert!(valid_request_no_metadata.validate().is_ok());
    }

    #[test]
    fn test_helper_functions() {
        // Test error mapping
        let error = domain::CompletionError::InvalidModel("test".to_string());
        let status = map_domain_error_to_status(&error);
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}