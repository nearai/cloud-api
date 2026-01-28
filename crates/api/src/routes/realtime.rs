//! Realtime WebSocket API for voice-to-voice conversations
//!
//! This module implements the WebSocket handler for bidirectional audio streaming,
//! handling the STT -> LLM -> TTS pipeline in real-time.

use crate::middleware::auth::AuthenticatedApiKey;
use crate::models::ErrorResponse;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Extension, State,
};
use axum::response::IntoResponse;
use futures::stream::StreamExt;
use futures::SinkExt;
use services::realtime::ports::{
    ClientEvent, ErrorInfo, RealtimeServiceTrait, ServerEvent, SessionConfig, WorkspaceContext,
};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Maximum size for a single WebSocket message (1MB)
const MAX_WEBSOCKET_MESSAGE_SIZE: usize = 1024 * 1024;

/// State for realtime routes
#[derive(Clone)]
pub struct RealtimeRouteState {
    pub realtime_service: Arc<dyn RealtimeServiceTrait>,
}

/// WebSocket upgrade handler for realtime API
///
/// GET /v1/realtime
/// Upgrades to WebSocket for bidirectional audio streaming.
#[utoipa::path(
    get,
    path = "/v1/realtime",
    tag = "Realtime",
    responses(
        (status = 101, description = "WebSocket upgrade successful"),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn realtime_handler(
    ws: WebSocketUpgrade,
    State(state): State<RealtimeRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
) -> impl IntoResponse {
    info!(
        api_key_id = %api_key.api_key.id.0,
        "WebSocket realtime connection requested"
    );

    // Build workspace context from authenticated API key
    let api_key_id = uuid::Uuid::parse_str(&api_key.api_key.id.0).unwrap_or_else(|_| {
        error!("Invalid API key ID format");
        uuid::Uuid::nil()
    });

    // If API key ID is invalid (nil), reject the request
    if api_key_id.is_nil() {
        error!("Rejecting WebSocket connection due to invalid API key ID");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorResponse::new(
                "Invalid API key configuration".to_string(),
                "server_error".to_string(),
            )),
        )
            .into_response();
    }

    let workspace_ctx = WorkspaceContext {
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id,
        user_id: uuid::Uuid::nil(), // API key auth doesn't have user context
    };

    ws.on_upgrade(move |socket| handle_realtime_socket(socket, state, workspace_ctx))
}

/// Handle the WebSocket connection for realtime audio
async fn handle_realtime_socket(
    socket: WebSocket,
    state: RealtimeRouteState,
    ctx: WorkspaceContext,
) {
    let (mut sender, mut receiver) = socket.split();

    // Create session with default config
    let session_result = state
        .realtime_service
        .create_session(SessionConfig::default(), &ctx)
        .await;

    let mut session = match session_result {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "Failed to create realtime session");
            let error_event = ServerEvent::Error {
                error: ErrorInfo {
                    error_type: "server_error".to_string(),
                    code: "session_creation_failed".to_string(),
                    message: "Failed to create session".to_string(),
                },
            };
            let _ = send_event(&mut sender, &error_event).await;
            return;
        }
    };

    info!(
        session_id = %session.session_id,
        "Realtime session created"
    );

    // Send session.created event
    let created_event = ServerEvent::SessionCreated {
        session: services::realtime::ports::SessionInfo {
            id: session.session_id.clone(),
            model: session.config.llm_model.clone(),
            voice: session.config.voice.clone(),
            instructions: session.config.instructions.clone(),
        },
    };

    if let Err(e) = send_event(&mut sender, &created_event).await {
        error!(error = %e, "Failed to send session.created event");
        return;
    }

    // Main event loop
    while let Some(msg_result) = receiver.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, session_id = %session.session_id, "WebSocket receive error");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                // Parse client event
                let event: Result<ClientEvent, _> = serde_json::from_str(&text);

                match event {
                    Ok(client_event) => {
                        if let Err(e) = handle_client_event(
                            &mut session,
                            &state,
                            &ctx,
                            client_event,
                            &mut sender,
                        )
                        .await
                        {
                            error!(
                                error = %e,
                                session_id = %session.session_id,
                                "Error handling client event"
                            );
                            let error_event = ServerEvent::Error {
                                error: ErrorInfo {
                                    error_type: "server_error".to_string(),
                                    code: "event_handling_failed".to_string(),
                                    message: "Failed to process event".to_string(),
                                },
                            };
                            let _ = send_event(&mut sender, &error_event).await;
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            session_id = %session.session_id,
                            "Invalid client event"
                        );
                        let error_event = ServerEvent::Error {
                            error: ErrorInfo {
                                error_type: "invalid_request_error".to_string(),
                                code: "invalid_event".to_string(),
                                message: "Invalid event format".to_string(),
                            },
                        };
                        let _ = send_event(&mut sender, &error_event).await;
                    }
                }
            }
            Message::Binary(audio) => {
                // Validate message size
                if audio.len() > MAX_WEBSOCKET_MESSAGE_SIZE {
                    let error_event = ServerEvent::Error {
                        error: ErrorInfo {
                            error_type: "invalid_request_error".to_string(),
                            code: "message_too_large".to_string(),
                            message: "Audio chunk exceeds size limit".to_string(),
                        },
                    };
                    let _ = send_event(&mut sender, &error_event).await;
                    continue;
                }

                // Direct binary audio input (alternative to base64)
                let audio_base64 =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &audio);
                if let Err(e) = state
                    .realtime_service
                    .handle_audio_chunk(&mut session, &audio_base64)
                    .await
                {
                    error!(
                        error = %e,
                        session_id = %session.session_id,
                        "Error handling binary audio"
                    );
                    let error_event = ServerEvent::Error {
                        error: ErrorInfo {
                            error_type: "server_error".to_string(),
                            code: "audio_processing_failed".to_string(),
                            message: "Failed to process audio chunk".to_string(),
                        },
                    };
                    let _ = send_event(&mut sender, &error_event).await;
                }
            }
            Message::Close(_) => {
                info!(session_id = %session.session_id, "WebSocket closed by client");
                break;
            }
            Message::Ping(data) => {
                // Respond with pong
                if let Err(e) = sender.send(Message::Pong(data)).await {
                    debug!(error = %e, "Failed to send pong");
                }
            }
            Message::Pong(_) => {
                // Ignore pongs
            }
        }
    }

    info!(session_id = %session.session_id, "Realtime session ended");
}

/// Handle a client event and send appropriate server events
async fn handle_client_event(
    session: &mut services::realtime::ports::RealtimeSession,
    state: &RealtimeRouteState,
    ctx: &WorkspaceContext,
    event: ClientEvent,
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
) -> Result<(), String> {
    match event {
        ClientEvent::SessionUpdate { session: config } => {
            debug!(session_id = %session.session_id, "Updating session configuration");

            state
                .realtime_service
                .update_session(session, config)
                .await
                .map_err(|e| e.to_string())?;

            let updated_event = ServerEvent::SessionUpdated {
                session: services::realtime::ports::SessionInfo {
                    id: session.session_id.clone(),
                    model: session.config.llm_model.clone(),
                    voice: session.config.voice.clone(),
                    instructions: session.config.instructions.clone(),
                },
            };
            send_event(sender, &updated_event).await?;
        }

        ClientEvent::InputAudioBufferAppend { audio } => {
            debug!(
                session_id = %session.session_id,
                audio_len = audio.len(),
                "Appending audio to buffer"
            );

            state
                .realtime_service
                .handle_audio_chunk(session, &audio)
                .await
                .map_err(|e| e.to_string())?;
        }

        ClientEvent::InputAudioBufferCommit => {
            debug!(session_id = %session.session_id, "Committing audio buffer");

            match state
                .realtime_service
                .commit_audio_buffer(session, ctx)
                .await
            {
                Ok(result) => {
                    // Send committed event
                    let committed_event = ServerEvent::InputAudioBufferCommitted {
                        item_id: result.item_id.clone(),
                    };
                    send_event(sender, &committed_event).await?;

                    // Send transcription completed event
                    let transcription_event =
                        ServerEvent::ConversationItemInputAudioTranscriptionCompleted {
                            item_id: result.item_id,
                            transcript: result.text,
                        };
                    send_event(sender, &transcription_event).await?;
                }
                Err(e) => {
                    error!(error = %e, "Transcription failed");
                    let error_event = ServerEvent::Error {
                        error: ErrorInfo {
                            error_type: "server_error".to_string(),
                            code: "transcription_failed".to_string(),
                            message: "Failed to transcribe audio".to_string(),
                        },
                    };
                    send_event(sender, &error_event).await?;
                }
            }
        }

        ClientEvent::InputAudioBufferClear => {
            debug!(session_id = %session.session_id, "Clearing audio buffer");

            state
                .realtime_service
                .clear_audio_buffer(session)
                .await
                .map_err(|e| e.to_string())?;

            let cleared_event = ServerEvent::InputAudioBufferCleared;
            send_event(sender, &cleared_event).await?;
        }

        ClientEvent::ConversationItemCreate { item } => {
            debug!(
                session_id = %session.session_id,
                item_id = %item.id,
                "Creating conversation item"
            );

            // Add item to conversation context through service layer
            state
                .realtime_service
                .add_conversation_item(session, item.clone())
                .await
                .map_err(|e| e.to_string())?;

            let created_event = ServerEvent::ConversationItemCreated { item };
            send_event(sender, &created_event).await?;
        }

        ClientEvent::ResponseCreate { response: _config } => {
            debug!(session_id = %session.session_id, "Generating response");

            match state.realtime_service.generate_response(session, ctx).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        if let Err(e) = send_event(sender, &event).await {
                            error!(error = %e, "Failed to send response event");
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Response generation failed");
                    let error_event = ServerEvent::Error {
                        error: ErrorInfo {
                            error_type: "server_error".to_string(),
                            code: "response_generation_failed".to_string(),
                            message: "Failed to generate response".to_string(),
                        },
                    };
                    send_event(sender, &error_event).await?;
                }
            }
        }

        ClientEvent::ResponseCancel => {
            debug!(session_id = %session.session_id, "Response cancellation requested");
            // Currently we don't support cancellation mid-stream
            // The response will complete naturally
        }
    }

    Ok(())
}

/// Send a server event over the WebSocket
async fn send_event(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    event: &ServerEvent,
) -> Result<(), String> {
    use futures::SinkExt;

    let json = serde_json::to_string(event).map_err(|e| e.to_string())?;

    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| e.to_string())
}
