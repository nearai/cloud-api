use axum::{
    extract::{Path, Query, State, Extension, Json},
    http::StatusCode,
    response::{Json as ResponseJson, IntoResponse, sse::{Event, Sse}},
};
use crate::{models::*, middleware::AuthenticatedUser, routes::common::map_domain_error_to_status};
use domain::{ResponseService, ResponseRequest, ResponseInput as DomainResponseInput, ResponseMessage, ResponseStatus as DomainResponseStatus};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, info};
use futures::stream::StreamExt;
use std::convert::Infallible;

/// Create a new response
pub async fn create_response(
    State(service): State<Arc<ResponseService>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateResponseRequest>,
) -> axum::response::Response {
    debug!("Create response request from user: {}", user.0.id);
    
    // Validate the request
    if let Err(error) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error, "invalid_request_error".to_string())),
        ).into_response();
    }

    // Convert HTTP request to domain request
    let domain_input = request.input.clone().map(|input| match input {
        ResponseInput::Text(text) => DomainResponseInput::Text(text),
        ResponseInput::Items(items) => {
            let messages = items.into_iter().filter_map(|item| match item {
                ResponseInputItem::Message { role, content } => {
                    let text = match content {
                        ResponseContent::Text(t) => t,
                        ResponseContent::Parts(parts) => {
                            // Extract text from parts
                            parts.into_iter()
                                .filter_map(|part| match part {
                                    ResponseContentPart::InputText { text } => Some(text),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        }
                    };
                    Some(ResponseMessage { role, content: text })
                }
            }).collect();
            DomainResponseInput::Messages(messages)
        }
    });

    let domain_request = ResponseRequest {
        model: request.model.clone(),
        input: domain_input,
        instructions: request.instructions.clone(),
        conversation_id: request.conversation.clone().and_then(|c| match c {
            ConversationReference::Id(id) => Some(id),
            ConversationReference::Object { id, .. } => Some(id),
        }),
        previous_response_id: request.previous_response_id.clone(),
        max_output_tokens: request.max_output_tokens,
        temperature: request.temperature,
        top_p: request.top_p,
        user_id: user.0.id.to_string(),
        metadata: request.metadata.clone(),
    };

    // Check if streaming is requested
    if request.stream.unwrap_or(false) {
        // Create streaming response
        match service.create_response_stream(domain_request).await {
            Ok(stream) => {
                let sse_stream = stream
                    .map(|event| {
                        Ok::<_, Infallible>(Event::default()
                            .event(event.event_name)
                            .data(serde_json::to_string(&event.data).unwrap_or_default()))
                    });
                
                // Return SSE response
                Sse::new(sse_stream)
                    .keep_alive(axum::response::sse::KeepAlive::default())
                    .into_response()
            }
            Err(error) => {
                let status_code = map_domain_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    } else {
        // Create non-streaming response
        match service.create_response(domain_request).await {
            Ok(domain_response) => {
                // Convert domain response to HTTP response
                let http_response = convert_domain_response_to_http_with_request(domain_response, &request);
                info!("Created response {} for user {}", http_response.id, user.0.id);
                (StatusCode::OK, ResponseJson(http_response)).into_response()
            }
            Err(error) => {
                let status_code = map_domain_error_to_status(&error);
                (status_code, ResponseJson::<ErrorResponse>(error.into())).into_response()
            }
        }
    }
}

/// Get a response by ID
pub async fn get_response(
    Path(response_id): Path<String>,
    Query(_params): Query<GetResponseQuery>,
    State(service): State<Arc<ResponseService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Get response {} for user {}", response_id, user.0.id);

    match service.get_response(&response_id, &user.0.id.to_string()).await {
        Ok(Some(domain_response)) => {
            let http_response = convert_domain_response_to_http_simple(domain_response);
            Ok(ResponseJson(http_response))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Response not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Delete a response
pub async fn delete_response(
    Path(response_id): Path<String>,
    State(service): State<Arc<ResponseService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ResponseDeleteResult>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Delete response {} for user {}", response_id, user.0.id);

    match service.delete_response(&response_id, &user.0.id.to_string()).await {
        Ok(true) => {
            info!("Deleted response {} for user {}", response_id, user.0.id);
            Ok(ResponseJson(ResponseDeleteResult {
                id: response_id,
                object: "response".to_string(),
                deleted: true,
            }))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Response not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// Cancel a response (for background responses)
pub async fn cancel_response(
    Path(response_id): Path<String>,
    State(service): State<Arc<ResponseService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ResponseObject>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("Cancel response {} for user {}", response_id, user.0.id);

    match service.cancel_response(&response_id, &user.0.id.to_string()).await {
        Ok(Some(domain_response)) => {
            let http_response = convert_domain_response_to_http_simple(domain_response);
            info!("Cancelled response {} for user {}", response_id, user.0.id);
            Ok(ResponseJson(http_response))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Response not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

/// List input items for a response (simplified implementation)
pub async fn list_input_items(
    Path(response_id): Path<String>,
    Query(params): Query<ListInputItemsQuery>,
    State(service): State<Arc<ResponseService>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<ResponseJson<ResponseInputItemList>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!("List input items for response {} for user {}", response_id, user.0.id);

    match service.get_response(&response_id, &user.0.id.to_string()).await {
        Ok(Some(domain_response)) => {
            // Convert input messages to HTTP format
            let items: Vec<ResponseInputItem> = domain_response.input_messages
                .into_iter()
                .map(|msg| ResponseInputItem::Message {
                    role: msg.role,
                    content: ResponseContent::Text(msg.content),
                })
                .collect();

            let limit = params.limit.unwrap_or(20).min(100) as usize;
            let page_items: Vec<_> = items.into_iter().take(limit).collect();

            let first_id = page_items.first().map(|_| "msg_first".to_string()).unwrap_or_default();
            let last_id = page_items.last().map(|_| "msg_last".to_string()).unwrap_or_default();

            Ok(ResponseJson(ResponseInputItemList {
                object: "list".to_string(),
                data: page_items,
                first_id,
                last_id,
                has_more: false,
            }))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Response not found".to_string(),
                "not_found_error".to_string(),
            )),
        )),
        Err(error) => Err((
            map_domain_error_to_status(&error),
            ResponseJson(error.into()),
        )),
    }
}

// Helper functions

fn convert_domain_response_to_http_with_request(domain_response: domain::Response, request: &CreateResponseRequest) -> ResponseObject {
    let status = match domain_response.status {
        DomainResponseStatus::InProgress => ResponseStatus::InProgress,
        DomainResponseStatus::Completed => ResponseStatus::Completed,
        DomainResponseStatus::Failed => ResponseStatus::Failed,
        DomainResponseStatus::Cancelled => ResponseStatus::Cancelled,
    };

    let output = if let Some(output_text) = domain_response.output_message {
        vec![ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            status: ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![ResponseOutputContent::OutputText {
                text: output_text,
                annotations: vec![],
            }],
        }]
    } else {
        vec![]
    };

    ResponseObject {
        id: domain_response.id,
        object: "response".to_string(),
        created_at: domain_response.created_at.timestamp() as u64,
        status,
        error: None,
        incomplete_details: None,
        instructions: domain_response.instructions,
        max_output_tokens: request.max_output_tokens,
        max_tool_calls: request.max_tool_calls,
        model: domain_response.model,
        output,
        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
        previous_response_id: domain_response.previous_response_id,
        reasoning: Some(ResponseReasoningOutput {
            effort: None,
            summary: None,
        }),
        store: request.store.unwrap_or(true),
        temperature: request.temperature.unwrap_or(1.0),
        text: request.text.clone().or_else(|| Some(ResponseTextConfig {
            format: ResponseTextFormat::Text,
        })),
        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
        tools: request.tools.clone().unwrap_or_default(),
        top_p: request.top_p.unwrap_or(1.0),
        truncation: "disabled".to_string(),
        usage: domain_response.usage.map(|u| Usage {
            input_tokens: u.prompt_tokens,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 0,
            }),
            output_tokens: u.completion_tokens,
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: 0,
            }),
            total_tokens: u.total_tokens,
        }).unwrap_or(Usage::new(10, 20)),
        user: None,
        metadata: domain_response.metadata.or_else(|| Some(serde_json::json!({}))),
    }
}

// Simple conversion function for endpoints that don't have request context
fn convert_domain_response_to_http_simple(domain_response: domain::Response) -> ResponseObject {
    let status = match domain_response.status {
        DomainResponseStatus::InProgress => ResponseStatus::InProgress,
        DomainResponseStatus::Completed => ResponseStatus::Completed,
        DomainResponseStatus::Failed => ResponseStatus::Failed,
        DomainResponseStatus::Cancelled => ResponseStatus::Cancelled,
    };

    let output = if let Some(output_text) = domain_response.output_message {
        vec![ResponseOutputItem::Message {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            status: ResponseItemStatus::Completed,
            role: "assistant".to_string(),
            content: vec![ResponseOutputContent::OutputText {
                text: output_text,
                annotations: vec![],
            }],
        }]
    } else {
        vec![]
    };

    ResponseObject {
        id: domain_response.id,
        object: "response".to_string(),
        created_at: domain_response.created_at.timestamp() as u64,
        status,
        error: None,
        incomplete_details: None,
        instructions: domain_response.instructions,
        max_output_tokens: None,
        max_tool_calls: None,
        model: domain_response.model,
        output,
        parallel_tool_calls: true,
        previous_response_id: domain_response.previous_response_id,
        reasoning: Some(ResponseReasoningOutput {
            effort: None,
            summary: None,
        }),
        store: true,
        temperature: 1.0,
        text: Some(ResponseTextConfig {
            format: ResponseTextFormat::Text,
        }),
        tool_choice: ResponseToolChoiceOutput::Auto("auto".to_string()),
        tools: vec![],
        top_p: 1.0,
        truncation: "disabled".to_string(),
        usage: domain_response.usage.map(|u| Usage {
            input_tokens: u.prompt_tokens,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 0,
            }),
            output_tokens: u.completion_tokens,
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: 0,
            }),
            total_tokens: u.total_tokens,
        }).unwrap_or(Usage::new(10, 20)),
        user: None,
        metadata: domain_response.metadata.or_else(|| Some(serde_json::json!({}))),
    }
}


// Query parameter structs
#[derive(Debug, Deserialize)]
pub struct GetResponseQuery {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub include_obfuscation: Option<bool>,
    pub starting_after: Option<i32>,
    pub stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListInputItemsQuery {
    pub after: Option<String>,
    pub include: Option<Vec<String>>,
    pub limit: Option<i32>,
    pub order: Option<String>, // "asc" or "desc"
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
    use domain::{Domain, MockCompletionHandler};
    use futures::stream::StreamExt;

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
        let conversation_service = Arc::new(domain::ConversationService::new(domain.database.clone()));
        let response_service = Arc::new(ResponseService::new(
            Arc::new(MockCompletionHandler),
            domain.database,
            conversation_service,
        ));
        
        Router::new()
            .route("/responses", post(create_response))
            .route("/responses/{response_id}", get(get_response))
            .route("/responses/{response_id}", axum::routing::delete(delete_response))
            .route("/responses/{response_id}/cancel", post(cancel_response))
            .route("/responses/{response_id}/input_items", get(list_input_items))
            .with_state(response_service)
    }

    #[tokio::test]
    async fn test_create_response_success() {
        let app = create_test_app();
        
        let request_body = json!({
            "model": "gpt-4o",
            "input": "Tell me a joke",
            "temperature": 0.7,
            "max_output_tokens": 100
        });

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .extension(create_test_user())
            .body(Body::from(serde_json::to_string(&request_body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        assert_eq!(response.status(), StatusCode::OK);
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_obj: ResponseObject = serde_json::from_slice(&body).unwrap();
        
        assert_eq!(response_obj.model, "gpt-4o");
        assert_eq!(response_obj.object, "response");
        assert!(matches!(response_obj.status, ResponseStatus::Completed));
        assert!(!response_obj.output.is_empty());
    }

    #[tokio::test]
    async fn test_create_response_validation_error() {
        let app = create_test_app();
        
        let request_body = json!({
            "model": "", // Invalid empty model
            "input": "Hello"
        });

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .extension(create_test_user())
            .body(Body::from(serde_json::to_string(&request_body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error_response: ErrorResponse = serde_json::from_slice(&body).unwrap();
        
        assert_eq!(error_response.error.r#type, "invalid_request_error");
        assert!(error_response.error.message.contains("Model cannot be empty"));
    }

    #[test]
    fn test_create_response_request_validation() {
        let valid_request = CreateResponseRequest {
            model: "gpt-4o".to_string(),
            input: Some(ResponseInput::Text("Hello".to_string())),
            instructions: None,
            conversation: None,
            previous_response_id: None,
            max_output_tokens: Some(100),
            max_tool_calls: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            stream: None,
            store: None,
            background: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            text: None,
            reasoning: None,
            include: None,
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        assert!(valid_request.validate().is_ok());

        let invalid_request = CreateResponseRequest {
            model: "".to_string(), // Empty model
            temperature: Some(3.0), // Invalid temperature
            top_p: Some(2.0), // Invalid top_p
            max_output_tokens: Some(0), // Invalid max_tokens
            conversation: Some(ConversationReference::Id("conv_123".to_string())),
            previous_response_id: Some("resp_456".to_string()), // Conflicting fields
            ..valid_request
        };

        assert!(invalid_request.validate().is_err());
    }

    #[test]
    fn test_helper_functions() {
        // Test error mapping
        let error = domain::CompletionError::InvalidModel("test".to_string());
        let status = map_domain_error_to_status(&error);
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let error = domain::CompletionError::RateLimited;
        let status = map_domain_error_to_status(&error);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        let error = domain::CompletionError::InternalError("test".to_string());
        let status = map_domain_error_to_status(&error);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_create_response_streaming() {
        let app = create_test_app();
        
        let request_body = json!({
            "model": "gpt-4o",
            "input": "Tell me a joke",
            "temperature": 0.7,
            "max_output_tokens": 100,
            "stream": true
        });

        let request = Request::builder()
            .method("POST")
            .uri("/responses")
            .header("content-type", "application/json")
            .extension(create_test_user())
            .body(Body::from(serde_json::to_string(&request_body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        
        // For streaming responses, we should get OK status with SSE content type
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false));
        
        // Read the streaming body
        let body = response.into_body();
        let mut stream = body.into_data_stream();
        let mut events_received = 0;
        let mut has_usage = false;
        let mut has_completed = false;
        
        while let Some(Ok(chunk)) = stream.next().await {
            let text = String::from_utf8_lossy(&chunk);
            
            // Check for completion event with usage
            if text.contains("event: response.completed") {
                has_completed = true;
                // The next data line should contain usage information
                if text.contains("\"usage\"") {
                    has_usage = true;
                }
            }
            
            if text.contains("event:") {
                events_received += 1;
            }
            
            // Don't stop early - consume the entire stream to ensure we get the final event
        }
        
        // We should have received at least some streaming events
        assert!(events_received > 0, "Should receive streaming events");
        assert!(has_completed, "Should receive response.completed event");
        assert!(has_usage, "Completed event should contain usage information");
    }
}