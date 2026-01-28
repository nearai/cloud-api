//! Anthropic format converter
//!
//! Converts Anthropic's Messages API format to OpenAI-compatible format.
//! This module handles:
//! - Request conversion (OpenAI → Anthropic)
//! - Response/event parsing (Anthropic → OpenAI)
//! - Streaming state management for tool calls

use crate::{
    chunk_builder::ChunkContext, ChatMessage, CompletionError, FunctionCall, MessageRole,
    SSEEventParser, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// Anthropic Request Types
// =============================================================================

/// Anthropic message format for requests
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicMessageContent,
}

/// Message content - can be a string or array of content blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentPart>),
}

/// Content part in a message (for multi-part messages like tool results)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Anthropic tool definition
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

/// Anthropic tool choice
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

/// Anthropic request format
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    pub stream: bool,
}

// =============================================================================
// Anthropic Response Types (Streaming)
// =============================================================================

/// Streaming event types from Anthropic
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageInfo },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: i64,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: i64, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: i64 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: AnthropicUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicError },
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessageInfo {
    pub id: String,
    pub usage: AnthropicUsage,
}

/// Content block in streaming responses (uses struct for forward compatibility)
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicContentBlock {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

impl AnthropicContentBlock {
    pub fn is_tool_use(&self) -> bool {
        self.type_ == "tool_use"
    }

    pub fn is_text(&self) -> bool {
        self.type_ == "text"
    }
}

/// Delta in streaming responses
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicDelta {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub partial_json: Option<String>,
}

impl AnthropicDelta {
    pub fn is_text_delta(&self) -> bool {
        self.type_ == "text_delta"
    }

    pub fn is_input_json_delta(&self) -> bool {
        self.type_ == "input_json_delta"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessageDelta {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: i32,
    #[serde(default)]
    pub output_tokens: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicError {
    #[serde(rename = "type")]
    pub type_: String,
    pub message: String,
}

// =============================================================================
// Anthropic Response Types (Non-streaming)
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    pub content: Vec<AnthropicContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

// =============================================================================
// Conversion Functions
// =============================================================================

/// Convert OpenAI messages to Anthropic format
pub fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_message = None;
    let mut anthropic_messages = Vec::new();

    let extract_content = |value: &serde_json::Value| -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        }
    };

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                if let Some(content) = &msg.content {
                    system_message = Some(extract_content(content));
                }
            }
            MessageRole::User => {
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();
                anthropic_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicMessageContent::Text(content),
                });
            }
            MessageRole::Assistant => {
                // Check if the assistant message contains tool calls
                if let Some(tool_calls) = &msg.tool_calls {
                    if !tool_calls.is_empty() {
                        // Build content blocks: optional text + tool_use blocks
                        let mut blocks = Vec::new();

                        // Add text content if present
                        if let Some(text) = msg.content.as_ref().map(&extract_content) {
                            if !text.is_empty() {
                                blocks.push(AnthropicContentPart::Text { text });
                            }
                        }

                        // Add tool_use blocks for each tool call
                        for tc in tool_calls {
                            let id = tc.id.clone().unwrap_or_default();
                            let name = tc.function.name.clone().unwrap_or_default();
                            let input = tc
                                .function
                                .arguments
                                .as_ref()
                                .and_then(|args| serde_json::from_str(args).ok())
                                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                            blocks.push(AnthropicContentPart::ToolUse { id, name, input });
                        }

                        anthropic_messages.push(AnthropicMessage {
                            role: "assistant".to_string(),
                            content: AnthropicMessageContent::Blocks(blocks),
                        });
                        continue;
                    }
                }

                // No tool calls - just text content
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();
                anthropic_messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicMessageContent::Text(content),
                });
            }
            MessageRole::Tool => {
                // Tool results need special formatting for Anthropic
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                anthropic_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentPart::ToolResult {
                            tool_use_id,
                            content,
                        },
                    ]),
                });
            }
        }
    }

    (system_message, anthropic_messages)
}

/// Convert OpenAI tools to Anthropic format
pub fn convert_tools(tools: &[ToolDefinition]) -> Vec<AnthropicTool> {
    tools
        .iter()
        .map(|tool| AnthropicTool {
            name: tool.function.name.clone(),
            description: tool.function.description.clone(),
            input_schema: tool.function.parameters.clone(),
        })
        .collect()
}

/// Convert OpenAI tool_choice to Anthropic format
pub fn convert_tool_choice(tool_choice: &crate::ToolChoice) -> Option<AnthropicToolChoice> {
    match tool_choice {
        crate::ToolChoice::String(s) => match s.as_str() {
            "none" => None,
            "auto" => Some(AnthropicToolChoice::Auto),
            "required" => Some(AnthropicToolChoice::Any),
            _ => Some(AnthropicToolChoice::Auto),
        },
        crate::ToolChoice::Function { function, .. } => Some(AnthropicToolChoice::Tool {
            name: function.name.clone(),
        }),
    }
}

/// Map Anthropic's stop_reason to OpenAI-compatible finish_reason
pub fn map_finish_reason(stop_reason: Option<String>) -> Option<crate::FinishReason> {
    stop_reason.map(|r| match r.as_str() {
        "end_turn" | "stop_sequence" => crate::FinishReason::Stop,
        "max_tokens" => crate::FinishReason::Length,
        "tool_use" => crate::FinishReason::ToolCalls,
        _ => crate::FinishReason::Stop,
    })
}

/// Map Anthropic's stop_reason to string (for non-streaming)
pub fn map_finish_reason_string(stop_reason: Option<String>) -> Option<String> {
    stop_reason.map(|r| match r.as_str() {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        _ => "stop".to_string(),
    })
}

/// Extract text and tool calls from non-streaming response
pub fn extract_response_content(
    content: &[AnthropicContentBlock],
) -> (Option<String>, Option<Vec<ToolCall>>) {
    let text: String = content
        .iter()
        .filter_map(|c| if c.is_text() { c.text.as_deref() } else { None })
        .collect::<Vec<_>>()
        .join("");

    let tool_calls: Vec<ToolCall> = content
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if c.is_tool_use() {
                Some(ToolCall {
                    id: c.id.clone(),
                    type_: Some("function".to_string()),
                    function: FunctionCall {
                        name: c.name.clone(),
                        arguments: c
                            .input
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default()),
                    },
                    index: Some(i as i64),
                    thought_signature: None, // Anthropic doesn't use thought_signature
                })
            } else {
                None
            }
        })
        .collect();

    let text_option = if text.is_empty() { None } else { Some(text) };
    let tool_calls_option = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    (text_option, tool_calls_option)
}

// =============================================================================
// Streaming Parser State & Implementation
// =============================================================================

/// Active tool call being accumulated during streaming
#[derive(Debug, Clone)]
struct ActiveToolCall {
    /// Accumulated JSON arguments
    json_buffer: String,
    /// Index in the tool_calls array (for OpenAI format)
    index: i64,
}

/// Parser state for Anthropic streaming
pub struct AnthropicParserState {
    pub message_id: Option<String>,
    pub model: String,
    pub created: i64,
    pub input_tokens: i32,
    pub output_tokens: i32,
    tool_calls: HashMap<i64, ActiveToolCall>,
    tool_call_counter: i64,
}

impl AnthropicParserState {
    pub fn new(model: String) -> Self {
        Self {
            message_id: None,
            model,
            created: chrono::Utc::now().timestamp(),
            input_tokens: 0,
            output_tokens: 0,
            tool_calls: HashMap::new(),
            tool_call_counter: 0,
        }
    }

    fn chunk_context(&self) -> ChunkContext {
        ChunkContext::new(
            self.message_id.clone().unwrap_or_default(),
            self.model.clone(),
            self.created,
        )
    }
}

/// Anthropic event parser
pub struct AnthropicEventParser;

impl SSEEventParser for AnthropicEventParser {
    type State = AnthropicParserState;

    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        let event: AnthropicStreamEvent = serde_json::from_str(data)
            .map_err(|_| CompletionError::InvalidResponse("Failed to parse event".to_string()))?;

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                state.message_id = Some(message.id);
                state.input_tokens = message.usage.input_tokens;
                let ctx = state.chunk_context();
                Ok(Some(StreamChunk::Chat(ctx.role_chunk())))
            }

            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if content_block.is_tool_use() {
                    if let (Some(id), Some(name)) = (content_block.id, content_block.name) {
                        let tool_index = state.tool_call_counter;
                        state.tool_call_counter += 1;

                        state.tool_calls.insert(
                            index,
                            ActiveToolCall {
                                json_buffer: String::new(),
                                index: tool_index,
                            },
                        );

                        let ctx = state.chunk_context();
                        return Ok(Some(StreamChunk::Chat(
                            ctx.tool_call_start_chunk(tool_index, id, name),
                        )));
                    }
                }
                Ok(None)
            }

            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                let ctx = state.chunk_context();

                if delta.is_text_delta() {
                    if let Some(text) = delta.text {
                        return Ok(Some(StreamChunk::Chat(ctx.text_chunk(text))));
                    }
                } else if delta.is_input_json_delta() {
                    if let Some(partial_json) = delta.partial_json {
                        if let Some(tool_call) = state.tool_calls.get_mut(&index) {
                            tool_call.json_buffer.push_str(&partial_json);
                            return Ok(Some(StreamChunk::Chat(
                                ctx.tool_call_args_chunk(tool_call.index, partial_json),
                            )));
                        }
                    }
                }
                Ok(None)
            }

            AnthropicStreamEvent::ContentBlockStop { index } => {
                state.tool_calls.remove(&index);
                Ok(None)
            }

            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                state.output_tokens = usage.output_tokens;
                let ctx = state.chunk_context();
                let finish_reason = map_finish_reason(delta.stop_reason);
                let token_usage = TokenUsage {
                    prompt_tokens: state.input_tokens,
                    completion_tokens: state.output_tokens,
                    total_tokens: state.input_tokens + state.output_tokens,
                    prompt_tokens_details: None,
                };
                Ok(Some(StreamChunk::Chat(
                    ctx.finish_chunk(finish_reason, token_usage),
                )))
            }

            AnthropicStreamEvent::Error { error } => {
                tracing::warn!(backend = "anthropic", error_type = %error.type_, "Stream error received");
                Err(CompletionError::CompletionError(format!(
                    "Anthropic error: {} - {}",
                    error.type_, error.message
                )))
            }

            // Ignore Ping, MessageStop
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_extracts_system() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String("You are helpful.".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String("Hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, anthropic_messages) = convert_messages(&messages);

        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(anthropic_messages.len(), 1);
    }

    #[test]
    fn test_map_finish_reason() {
        assert_eq!(
            map_finish_reason(Some("end_turn".to_string())),
            Some(crate::FinishReason::Stop)
        );
        assert_eq!(
            map_finish_reason(Some("tool_use".to_string())),
            Some(crate::FinishReason::ToolCalls)
        );
        assert_eq!(
            map_finish_reason(Some("max_tokens".to_string())),
            Some(crate::FinishReason::Length)
        );
        assert_eq!(map_finish_reason(None), None);
    }

    #[test]
    fn test_parse_tool_use_content_block() {
        let json = r#"{"type":"tool_use","id":"toolu_123","name":"web_search","input":{}}"#;
        let block: AnthropicContentBlock = serde_json::from_str(json).unwrap();

        assert!(block.is_tool_use());
        assert_eq!(block.id, Some("toolu_123".to_string()));
        assert_eq!(block.name, Some("web_search".to_string()));
    }

    #[test]
    fn test_parse_text_delta() {
        let json = r#"{"type":"text_delta","text":"Hello"}"#;
        let delta: AnthropicDelta = serde_json::from_str(json).unwrap();

        assert!(delta.is_text_delta());
        assert_eq!(delta.text, Some("Hello".to_string()));
    }

    #[test]
    fn test_parse_input_json_delta() {
        let json = r#"{"type":"input_json_delta","partial_json":"{\"query\":"}"#;
        let delta: AnthropicDelta = serde_json::from_str(json).unwrap();

        assert!(delta.is_input_json_delta());
        assert_eq!(delta.partial_json, Some("{\"query\":".to_string()));
    }

    #[test]
    fn test_extract_response_content_text_only() {
        let content = vec![AnthropicContentBlock {
            type_: "text".to_string(),
            text: Some("Hello world".to_string()),
            id: None,
            name: None,
            input: None,
        }];

        let (text, tool_calls) = extract_response_content(&content);

        assert_eq!(text, Some("Hello world".to_string()));
        assert!(tool_calls.is_none());
    }

    #[test]
    fn test_extract_response_content_with_tool_calls() {
        let content = vec![
            AnthropicContentBlock {
                type_: "text".to_string(),
                text: Some("Let me search.".to_string()),
                id: None,
                name: None,
                input: None,
            },
            AnthropicContentBlock {
                type_: "tool_use".to_string(),
                text: None,
                id: Some("toolu_123".to_string()),
                name: Some("web_search".to_string()),
                input: Some(serde_json::json!({"query": "weather"})),
            },
        ];

        let (text, tool_calls) = extract_response_content(&content);

        assert_eq!(text, Some("Let me search.".to_string()));
        assert!(tool_calls.is_some());
        let calls = tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, Some("toolu_123".to_string()));
    }
}
