//! Gemini format converter
//!
//! Converts Google Gemini API format to OpenAI-compatible format.
//! This module handles:
//! - Request conversion (OpenAI → Gemini)
//! - Response/event parsing (Gemini → OpenAI)
//! - Tool call support

use crate::{
    chunk_builder::ChunkContext, ChatMessage, CompletionError, FunctionCall, MessageRole,
    SSEEventParser, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// =============================================================================
// Gemini Request Types
// =============================================================================

/// Gemini part format - can contain text or function call
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
    /// Thought signature for Gemini 3 models - required for tool calls to work correctly
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl GeminiPart {
    pub fn text(s: String) -> Self {
        Self {
            text: Some(s),
            function_call: None,
            function_response: None,
            thought_signature: None,
        }
    }

    pub fn function_response(name: String, response: serde_json::Value) -> Self {
        Self {
            text: None,
            function_call: None,
            function_response: Some(GeminiFunctionResponse { name, response }),
            thought_signature: None,
        }
    }

    pub fn function_call_with_signature(
        name: String,
        args: serde_json::Value,
        thought_signature: Option<String>,
    ) -> Self {
        Self {
            text: None,
            function_call: Some(GeminiFunctionCall { name, args }),
            function_response: None,
            thought_signature,
        }
    }
}

/// Function call in Gemini response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

/// Function response for tool results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

/// Gemini content format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

/// Gemini system instruction
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

/// Gemini generation config
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

/// Gemini function declaration (tool definition)
#[derive(Debug, Clone, Serialize)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Gemini tools wrapper
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiTools {
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// Gemini request format
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTools>>,
}

// =============================================================================
// Gemini Response Types
// =============================================================================

/// Gemini response candidate
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    pub content: GeminiContent,
    pub finish_reason: Option<String>,
}

/// Gemini usage metadata
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: i32,
    #[serde(default)]
    pub candidates_token_count: i32,
    #[serde(default)]
    pub total_token_count: i32,
}

/// Gemini response format
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiResponse {
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    pub usage_metadata: GeminiUsageMetadata,
}

// =============================================================================
// Conversion Functions
// =============================================================================

/// Convert OpenAI messages to Gemini format
pub fn convert_messages(
    messages: &[ChatMessage],
) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
    let extract_content = |value: &serde_json::Value| -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        }
    };

    let mut system_instruction = None;
    let mut contents = Vec::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                if let Some(content) = &msg.content {
                    system_instruction = Some(GeminiSystemInstruction {
                        parts: vec![GeminiPart::text(extract_content(content))],
                    });
                }
            }
            MessageRole::User => {
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: vec![GeminiPart::text(
                        msg.content
                            .as_ref()
                            .map(&extract_content)
                            .unwrap_or_default(),
                    )],
                });
            }
            MessageRole::Assistant => {
                // Check if this message has tool calls
                if let Some(tool_calls) = &msg.tool_calls {
                    let mut parts = Vec::new();

                    // Add text content if present
                    if let Some(content) = &msg.content {
                        let text = extract_content(content);
                        if !text.is_empty() {
                            parts.push(GeminiPart::text(text));
                        }
                    }

                    // Add function calls with thought_signature for Gemini 3 models
                    for tc in tool_calls {
                        if let Some(name) = &tc.function.name {
                            let args: serde_json::Value = tc
                                .function
                                .arguments
                                .as_ref()
                                .and_then(|a| serde_json::from_str(a).ok())
                                .unwrap_or(serde_json::Value::Object(Default::default()));

                            parts.push(GeminiPart::function_call_with_signature(
                                name.clone(),
                                args,
                                tc.thought_signature.clone(),
                            ));
                        }
                    }

                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts,
                    });
                } else {
                    // Regular text message
                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![GeminiPart::text(
                            msg.content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        )],
                    });
                }
            }
            MessageRole::Tool => {
                // Tool results go as function responses
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();

                // Try to parse as JSON object, otherwise wrap as {"result": content}
                // Gemini requires functionResponse.response to be a JSON object (Struct),
                // not a string or other primitive value
                let response: serde_json::Value = serde_json::from_str(&content)
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
                    .unwrap_or_else(|| serde_json::json!({"result": content}));

                // Get function name from the message's name field
                let name = msg.name.clone().unwrap_or_else(|| "function".to_string());

                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: vec![GeminiPart::function_response(name, response)],
                });
            }
        }
    }

    (system_instruction, contents)
}

/// Convert OpenAI tools to Gemini format
pub fn convert_tools(tools: &[ToolDefinition]) -> Vec<GeminiTools> {
    let declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .map(|tool| GeminiFunctionDeclaration {
            name: tool.function.name.clone(),
            description: tool.function.description.clone(),
            parameters: tool.function.parameters.clone(),
        })
        .collect();

    if declarations.is_empty() {
        vec![]
    } else {
        vec![GeminiTools {
            function_declarations: declarations,
        }]
    }
}

/// Map Gemini's finishReason to OpenAI-compatible finish_reason
pub fn map_finish_reason(finish_reason: Option<&String>) -> Option<crate::FinishReason> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => crate::FinishReason::Stop,
        "MAX_TOKENS" => crate::FinishReason::Length,
        "SAFETY" => crate::FinishReason::ContentFilter,
        _ => crate::FinishReason::Stop,
    })
}

/// Map Gemini's finishReason to string (for non-streaming)
pub fn map_finish_reason_string(finish_reason: Option<&String>) -> Option<String> {
    finish_reason.map(|r| match r.as_str() {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" => "content_filter".to_string(),
        _ => "stop".to_string(),
    })
}

/// Extract text and tool calls from Gemini response parts
pub fn extract_response_content(parts: &[GeminiPart]) -> (Option<String>, Option<Vec<ToolCall>>) {
    let text: String = parts.iter().filter_map(|p| p.text.as_deref()).collect();

    let tool_calls: Vec<ToolCall> = parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            p.function_call.as_ref().map(|fc| ToolCall {
                id: Some(format!("call_{}", Uuid::new_v4())),
                type_: Some("function".to_string()),
                function: FunctionCall {
                    name: Some(fc.name.clone()),
                    arguments: Some(serde_json::to_string(&fc.args).unwrap_or_default()),
                },
                index: Some(i as i64),
                // Capture thought signature from Gemini 3 models (required for tool calls)
                thought_signature: p.thought_signature.clone(),
            })
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
// Streaming Parser
// =============================================================================

/// Parser state for Gemini streaming
pub struct GeminiParserState {
    pub model: String,
    pub request_id: String,
    pub created: i64,
    pub chunk_index: i64,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
}

impl GeminiParserState {
    pub fn new(model: String) -> Self {
        Self {
            model,
            request_id: format!("gemini-{}", Uuid::new_v4()),
            created: chrono::Utc::now().timestamp(),
            chunk_index: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
        }
    }

    fn chunk_context(&self) -> ChunkContext {
        ChunkContext::new(self.request_id.clone(), self.model.clone(), self.created)
    }
}

/// Gemini event parser
pub struct GeminiEventParser;

impl SSEEventParser for GeminiEventParser {
    type State = GeminiParserState;

    fn parse_event(
        state: &mut Self::State,
        data: &str,
    ) -> Result<Option<StreamChunk>, CompletionError> {
        let response: GeminiResponse = serde_json::from_str(data)
            .map_err(|_| CompletionError::InvalidResponse("Failed to parse event".to_string()))?;

        if response.candidates.is_empty() {
            return Ok(None);
        }

        let candidate = &response.candidates[0];
        let ctx = state.chunk_context();

        // Update token counts
        state.prompt_tokens = response.usage_metadata.prompt_token_count;
        state.completion_tokens = response.usage_metadata.candidates_token_count;

        let is_first = state.chunk_index == 0;
        state.chunk_index += 1;

        // Extract text and function calls from parts
        let (text, tool_calls) = extract_response_content(&candidate.content.parts);

        // Determine finish reason
        let has_function_call = tool_calls.is_some();
        let finish_reason = if has_function_call {
            Some(crate::FinishReason::ToolCalls)
        } else {
            map_finish_reason(candidate.finish_reason.as_ref())
        };

        // Build the chunk using the context
        // For Gemini, we get complete function calls in one response, not streamed
        let chunk = if has_function_call {
            // Emit tool calls
            ctx.tool_calls_chunk(
                tool_calls.unwrap(),
                finish_reason,
                Some(TokenUsage {
                    prompt_tokens: state.prompt_tokens,
                    completion_tokens: state.completion_tokens,
                    total_tokens: state.prompt_tokens + state.completion_tokens,
                    prompt_tokens_details: None,
                }),
            )
        } else if is_first {
            // First chunk with role and possibly text
            let mut chunk = ctx.role_chunk();
            if let Some(ref t) = text {
                if let Some(delta) = chunk.choices.get_mut(0).and_then(|c| c.delta.as_mut()) {
                    delta.content = Some(t.clone());
                }
            }
            chunk.choices[0].finish_reason = finish_reason;
            chunk.usage = Some(TokenUsage {
                prompt_tokens: state.prompt_tokens,
                completion_tokens: state.completion_tokens,
                total_tokens: state.prompt_tokens + state.completion_tokens,
                prompt_tokens_details: None,
            });
            chunk
        } else if let Some(t) = text {
            // Subsequent text chunk
            let mut chunk = ctx.text_chunk(t);
            chunk.choices[0].finish_reason = finish_reason;
            chunk.usage = Some(TokenUsage {
                prompt_tokens: state.prompt_tokens,
                completion_tokens: state.completion_tokens,
                total_tokens: state.prompt_tokens + state.completion_tokens,
                prompt_tokens_details: None,
            });
            chunk
        } else {
            // Empty chunk with just finish reason
            ctx.finish_chunk(
                finish_reason,
                TokenUsage {
                    prompt_tokens: state.prompt_tokens,
                    completion_tokens: state.completion_tokens,
                    total_tokens: state.prompt_tokens + state.completion_tokens,
                    prompt_tokens_details: None,
                },
            )
        };

        Ok(Some(StreamChunk::Chat(chunk)))
    }

    fn handles_raw_json() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_with_system() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String("Be helpful".to_string())),
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

        let (system, contents) = convert_messages(&messages);

        assert!(system.is_some());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![ToolDefinition {
            type_: "function".to_string(),
            function: crate::FunctionDefinition {
                name: "web_search".to_string(),
                description: Some("Search the web".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    }
                }),
            },
        }];

        let gemini_tools = convert_tools(&tools);

        assert_eq!(gemini_tools.len(), 1);
        assert_eq!(gemini_tools[0].function_declarations.len(), 1);
        assert_eq!(gemini_tools[0].function_declarations[0].name, "web_search");
    }

    #[test]
    fn test_parse_function_call_response() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "web_search",
                            "args": {"query": "weather in SF"}
                        }
                    }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let (text, tool_calls) = extract_response_content(&response.candidates[0].content.parts);

        assert!(text.is_none());
        assert!(tool_calls.is_some());
        let calls = tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, Some("web_search".to_string()));
    }

    #[test]
    fn test_map_finish_reason() {
        assert_eq!(
            map_finish_reason(Some(&"STOP".to_string())),
            Some(crate::FinishReason::Stop)
        );
        assert_eq!(
            map_finish_reason(Some(&"MAX_TOKENS".to_string())),
            Some(crate::FinishReason::Length)
        );
        assert_eq!(map_finish_reason(None), None);
    }
}
