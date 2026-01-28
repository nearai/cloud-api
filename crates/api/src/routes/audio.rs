//! Audio API routes for speech-to-text and text-to-speech

use crate::{
    middleware::{auth::AuthenticatedApiKey, RequestBodyHash},
    models::{
        AudioSpeechRequest, AudioTranscriptionRequest, AudioTranscriptionResponse,
        AudioTranscriptionSegment, AudioTranscriptionWord, ErrorResponse,
    },
};
use axum::{
    body::Body,
    extract::{Extension, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use futures::stream::StreamExt;
use services::audio::ports::{AudioServiceTrait, SpeechRequest, TranscribeRequest};
use services::models::ports::ModelsServiceTrait;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

/// State for audio routes
#[derive(Clone)]
pub struct AudioRouteState {
    pub audio_service: Arc<dyn AudioServiceTrait>,
    pub models_service: Arc<dyn ModelsServiceTrait>,
}

/// Transcribe audio to text
///
/// POST /v1/audio/transcriptions
///
/// Accepts multipart form data with audio file and model parameters.
///
/// **Form Fields:**
/// - `file` (required): Binary audio file data
/// - `model` (required): Model ID (e.g., "whisper-1")
/// - `language` (optional): ISO-639-1 language code (e.g., "en")
/// - `response_format` (optional): json, text, srt, verbose_json, or vtt
/// - `prompt` (optional): Optional text to guide transcription style
/// - `temperature` (optional): Sampling temperature 0-1 (default: 0)
/// - `timestamp_granularities[]` (optional): "word" or "segment" for detailed timestamps
///
/// **Example Usage:**
/// ```bash
/// curl -X POST http://localhost:3000/v1/audio/transcriptions \
///   -H "Authorization: Bearer sk-live-xxx" \
///   -F "file=@audio.wav" \
///   -F "model=whisper-1" \
///   -F "language=en"
/// ```
#[utoipa::path(
    post,
    path = "/v1/audio/transcriptions",
    tag = "Audio",
    request_body(content = AudioTranscriptionRequest, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Transcription successful", body = AudioTranscriptionResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn transcribe_audio(
    State(state): State<AudioRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    mut multipart: Multipart,
) -> Result<ResponseJson<AudioTranscriptionResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    debug!(
        "Audio transcription request from api key: {:?}",
        api_key.api_key.id
    );

    // Parse multipart form data
    let mut audio_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut model: Option<String> = None;
    let mut language: Option<String> = None;
    let mut response_format: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        let error_str = e.to_string();
        let error_message = if error_str.contains("boundary") {
            "Invalid multipart/form-data: missing or malformed boundary. Ensure Content-Type header includes boundary parameter (e.g., 'multipart/form-data; boundary=----...')".to_string()
        } else {
            "Invalid multipart/form-data format".to_string()
        };
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(error_message, "invalid_request_error".to_string())),
        )
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                audio_data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| {
                            (
                                StatusCode::BAD_REQUEST,
                                ResponseJson(ErrorResponse::new(
                                    format!("Failed to read audio file: {e}"),
                                    "invalid_request_error".to_string(),
                                )),
                            )
                        })?
                        .to_vec(),
                );
            }
            "model" => {
                model = Some(field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!("Failed to read model field: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?);
            }
            "language" => {
                language = Some(field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!("Failed to read language field: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?);
            }
            "response_format" => {
                response_format = Some(field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!("Failed to read response_format field: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?);
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // Validate required fields
    let audio_data = audio_data.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "file is required".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Validate audio file size (25MB limit)
    const MAX_AUDIO_FILE_SIZE: usize = 25 * 1024 * 1024;
    if audio_data.len() > MAX_AUDIO_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            ResponseJson(ErrorResponse::new(
                "Audio file exceeds 25MB limit".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let model = model.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "model is required".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    let filename = filename.unwrap_or_else(|| "audio.wav".to_string());

    // Resolve model to get UUID for usage tracking
    let model_record = state
        .models_service
        .get_model_by_name(&model)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, model = %model, "Failed to resolve model");
            (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", model),
                    "invalid_request_error".to_string(),
                )),
            )
        })?;

    // Build service request
    let request = TranscribeRequest {
        model: model.clone(),
        audio_data,
        filename,
        language,
        response_format,
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id: Uuid::parse_str(&api_key.api_key.id.0).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Invalid API key ID".to_string(),
                    "server_error".to_string(),
                )),
            )
        })?,
        model_id: model_record.id,
        request_hash: body_hash.hash.clone(),
    };

    // Call the service
    let response = state.audio_service.transcribe(request).await.map_err(|e| {
        tracing::error!(error = %e, "Audio transcription failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ResponseJson(ErrorResponse::new(
                "Transcription failed".to_string(),
                "server_error".to_string(),
            )),
        )
    })?;

    debug!(
        model = %model,
        "Audio transcription completed"
    );

    Ok(ResponseJson(AudioTranscriptionResponse {
        text: response.text,
        task: Some("transcribe".to_string()),
        language: response.language,
        duration: response.duration,
        words: response.words.map(|words| {
            words
                .into_iter()
                .map(|w| AudioTranscriptionWord {
                    word: w.word,
                    start: w.start,
                    end: w.end,
                })
                .collect()
        }),
        segments: response.segments.map(|segments| {
            segments
                .into_iter()
                .map(|s| AudioTranscriptionSegment {
                    id: s.id,
                    seek: s.seek,
                    start: s.start,
                    end: s.end,
                    text: s.text,
                    tokens: s.tokens,
                    avg_logprob: s.avg_logprob,
                    compression_ratio: s.compression_ratio,
                    no_speech_prob: s.no_speech_prob,
                    temperature: s.temperature,
                })
                .collect()
        }),
    }))
}

/// Generate speech from text
///
/// POST /v1/audio/speech
/// Returns audio as MP3.
#[utoipa::path(
    post,
    path = "/v1/audio/speech",
    tag = "Audio",
    request_body = AudioSpeechRequest,
    responses(
        (status = 200, description = "Speech generated successfully", content_type = "audio/mpeg"),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn generate_speech(
    State(state): State<AudioRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Extension(body_hash): Extension<RequestBodyHash>,
    ResponseJson(request): ResponseJson<AudioSpeechRequest>,
) -> Response {
    debug!(
        "Text-to-speech request from api key: {:?}",
        api_key.api_key.id
    );

    // Validate request
    if let Err(e) = request.validate() {
        return (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(e, "invalid_request_error".to_string())),
        )
            .into_response();
    }

    // Resolve model to get UUID for usage tracking
    let model_record = match state.models_service.get_model_by_name(&request.model).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, model = %request.model, "Failed to resolve model");
            return (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    format!("Model '{}' not found", request.model),
                    "invalid_request_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    // Parse API key ID
    let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Invalid API key ID".to_string(),
                    "server_error".to_string(),
                )),
            )
                .into_response();
        }
    };

    let content_type = "audio/mpeg";

    // Build service request
    let service_request = SpeechRequest {
        model: request.model.clone(),
        input: request.input.clone(),
        voice: request.voice.clone(),
        response_format: None,
        speed: None,
        organization_id: api_key.organization.id.0,
        workspace_id: api_key.workspace.id.0,
        api_key_id,
        model_id: model_record.id,
        request_hash: body_hash.hash.clone(),
    };

    // Check if streaming is requested
    if request.stream == Some(true) {
        // Streaming response with proper error handling
        match state.audio_service.synthesize_stream(service_request).await {
            Ok(audio_stream) => {
                // Map stream items with String error type that can be propagated to client
                let byte_stream = audio_stream.map(|result| match result {
                    Ok(bytes) => Ok::<_, String>(axum::body::Bytes::from(bytes)),
                    Err(e) => {
                        let error_msg = format!("Streaming TTS error: {}", e);
                        tracing::error!("{}", error_msg);
                        // Return error to interrupt stream and inform client
                        Err(error_msg)
                    }
                });

                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::TRANSFER_ENCODING, "chunked")
                    .body(Body::from_stream(byte_stream))
                    .unwrap()
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to initialize streaming TTS");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Speech synthesis failed".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response()
            }
        }
    } else {
        // Non-streaming response
        match state.audio_service.synthesize(service_request).await {
            Ok(response) => {
                debug!(
                    model = %request.model,
                    voice = %request.voice,
                    "Text-to-speech completed"
                );

                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(response.audio_data))
                    .unwrap()
            }
            Err(e) => {
                tracing::error!(error = %e, "Text-to-speech failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Speech synthesis failed".to_string(),
                        "server_error".to_string(),
                    )),
                )
                    .into_response()
            }
        }
    }
}
