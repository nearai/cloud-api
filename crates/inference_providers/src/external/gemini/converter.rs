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
///
/// Both `role` and `parts` are marked `#[serde(default)]` because Google's
/// `generateContent` response may omit either field when no usable output
/// was produced (typically when `finishReason` is `MAX_TOKENS`, `SAFETY`, or
/// `RECITATION`). Defaulting to empty values lets the parser succeed and the
/// downstream code treat it as an empty completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub parts: Vec<GeminiPart>,
}

/// Gemini system instruction
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

/// Gemini generation config
#[derive(Debug, Clone, Serialize, Default)]
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
    /// Deterministic-sampling seed. Gemini's native API supports this via
    /// `generationConfig.seed`; OpenAI clients send the OpenAI-standard `seed`
    /// (nearai/cloud-api #669).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Structured-output MIME type. `"application/json"` enables Gemini's JSON
    /// mode (raw JSON, no markdown fences) for `response_format: json_object`
    /// and `json_schema` (nearai/cloud-api #668).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    /// Structured-output schema, translated from OpenAI `json_schema` and
    /// sanitized to Gemini's OpenAPI subset (nearai/cloud-api #668).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
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
///
/// `content` is `Option` because Google omits the field entirely on some
/// terminal-only responses (e.g. safety-blocked candidates that return only
/// `finishReason`). Consumers must treat a missing `content` the same as a
/// content with empty `parts`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: Option<GeminiContent>,
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

/// JSON Schema keywords that Gemini's OpenAPI subset does not accept.
///
/// Gemini rejects requests containing these with 400 "Unknown name 'X'". The
/// list is conservative — only keywords known to produce hard rejects when
/// present in `function_declarations[*].parameters`. See:
/// https://ai.google.dev/api/caching#Schema
///
/// Notably `additionalProperties` is rejected even though it is valid OpenAPI;
/// Gemini's subset omits it entirely. Schema generators (TypeBox, zod,
/// pydantic) emit it by default, so OpenAI-compatible clients routing through
/// us would otherwise fail every tool-using request.
// Note: `not` is intentionally omitted. Gemini's schema reference suggests it
// may be accepted in some forms, and stripping it would silently change the
// caller's intended semantics (negation vs no constraint). If a request with
// `not` is rejected by Gemini in practice, add it here.
//
// TODO(follow-up): stripping `$ref` and `$defs` silently degrades
// pydantic-style schemas with nested models to untyped objects (the model loses
// all argument type signal for referenced fields). A future enhancement could
// inline `$defs`/`$ref` before stripping to preserve fidelity.
//
// TODO(follow-up): `{"const": X}` could be rewritten to `{"enum": [X]}` rather
// than stripped — Gemini accepts `enum`, preserving the original constraint.
// Same pattern would apply to `{"type": [..., "null"]}` unions which OpenAI
// clients sometimes emit but Gemini's OpenAPI subset doesn't accept.
// Both are out of scope for this PR (which just stops the 400s).
const GEMINI_UNSUPPORTED_SCHEMA_KEYS: &[&str] = &[
    // OpenAPI / JSON Schema validation keywords Gemini does not implement
    // (alphabetized within group)
    "additionalProperties",
    "const",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "dependencies",
    "dependentRequired",
    "dependentSchemas",
    "else",
    "examples",
    "if",
    "patternProperties",
    "propertyNames",
    "readOnly",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
    "writeOnly",
    // JSON Schema meta-keywords (alphabetized within group)
    "$anchor",
    "$comment",
    "$defs",
    "$dynamicAnchor",
    "$dynamicRef",
    "$id",
    "$ref",
    "$schema",
    "$vocabulary",
    "definitions",
];

/// Recursively strip schema keywords that Gemini does not accept.
///
/// Walks `value` in-place. At each object, removes keys listed in
/// `GEMINI_UNSUPPORTED_SCHEMA_KEYS`, then recurses into the surviving values
/// and any array elements. This handles nested schemas (e.g. inside
/// `properties`, `items`, `anyOf`) at arbitrary depth.
///
/// Special case: keys inside `properties` (and equivalent maps like `$defs`,
/// `definitions`, `patternProperties`, `dependentSchemas`) are **user-defined
/// parameter names**, not schema keywords. A tool can legitimately have a
/// parameter named `const` or `examples`. We must NOT strip those keys — we
/// only descend into their *values* (which are sub-schemas). Without this,
/// `{"properties": {"const": {"type": "string"}}}` would lose the `const`
/// parameter entirely. Reported by gemini-code-assist on PR #610.
fn sanitize_schema_for_gemini(value: &mut serde_json::Value) {
    // Maps whose keys are user-defined names (not schema keywords). For these,
    // we only sanitize the values, never the key set.
    //
    // Note: only `properties` is currently functionally active here. The other
    // four are themselves in `GEMINI_UNSUPPORTED_SCHEMA_KEYS` and get stripped
    // before the iter_mut() loop below ever sees them. They are listed here
    // defensively so that if a future Gemini revision starts accepting them
    // and they are removed from the unsupported list, the user-name protection
    // automatically applies without a separate code change.
    const USER_NAMED_KEY_MAPS: &[&str] = &[
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ];

    match value {
        serde_json::Value::Object(map) => {
            for key in GEMINI_UNSUPPORTED_SCHEMA_KEYS {
                map.remove(*key);
            }
            for (k, v) in map.iter_mut() {
                if USER_NAMED_KEY_MAPS.contains(&k.as_str()) {
                    // The map's keys are parameter names; only recurse into values.
                    if let Some(inner) = v.as_object_mut() {
                        for inner_v in inner.values_mut() {
                            sanitize_schema_for_gemini(inner_v);
                        }
                        continue;
                    }
                    // Fall through to regular recursion if the value isn't an
                    // object (malformed schema); the general path handles
                    // non-objects safely via the `_ => {}` arm.
                }
                sanitize_schema_for_gemini(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                sanitize_schema_for_gemini(v);
            }
        }
        _ => {}
    }
}

/// Translate an OpenAI `response_format` value into Gemini's structured-output
/// `generationConfig` fields (nearai/cloud-api #668).
///
/// Returns `(response_mime_type, response_schema)`:
/// - `{"type": "json_object"}` → `("application/json", None)` so Gemini emits
///   raw JSON instead of markdown-fenced text.
/// - `{"type": "json_schema", "json_schema": {"schema": {...}}}` →
///   `("application/json", Some(sanitized schema))` so Gemini enforces the
///   schema natively (it was previously ignored entirely).
/// - anything else (including `{"type": "text"}`) → `(None, None)`.
///
/// The schema is sanitized through `sanitize_schema_for_gemini` so JSON-Schema
/// keywords Gemini's OpenAPI subset rejects (e.g. `additionalProperties`,
/// `$schema`) do not 400 the request.
pub fn response_format_to_gemini(
    response_format: &serde_json::Value,
) -> (Option<String>, Option<serde_json::Value>) {
    let Some(type_) = response_format.get("type").and_then(|v| v.as_str()) else {
        return (None, None);
    };

    match type_ {
        "json_object" => (Some("application/json".to_string()), None),
        "json_schema" => {
            let schema = response_format
                .get("json_schema")
                .and_then(|js| js.get("schema"))
                .cloned()
                .map(|mut s| {
                    sanitize_schema_for_gemini(&mut s);
                    s
                });
            (Some("application/json".to_string()), schema)
        }
        _ => (None, None),
    }
}

/// Convert OpenAI tools to Gemini format.
///
/// Sanitizes each tool's `parameters` JSON Schema by stripping keywords that
/// Gemini's OpenAPI subset does not support (see `sanitize_schema_for_gemini`).
/// Without this, schemas containing `additionalProperties` and similar
/// keywords cause Gemini to reject the request with 400 "Unknown name".
pub fn convert_tools(tools: &[ToolDefinition]) -> Vec<GeminiTools> {
    let declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .map(|tool| {
            let mut parameters = tool.function.parameters.clone();
            sanitize_schema_for_gemini(&mut parameters);
            GeminiFunctionDeclaration {
                name: tool.function.name.clone(),
                description: tool.function.description.clone(),
                parameters,
            }
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

        // Extract text and function calls from parts. `content` may be absent
        // entirely (e.g. safety-blocked candidates); treat it as empty parts.
        let parts: &[GeminiPart] = candidate
            .content
            .as_ref()
            .map_or(&[], |c| c.parts.as_slice());
        let (text, tool_calls) = extract_response_content(parts);

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
    fn test_sanitize_schema_strips_top_level_unsupported_keys() {
        // Schema generators like TypeBox/zod emit `additionalProperties: false`
        // at the root by default. Gemini rejects this. Reproduces the original
        // bug report from pi-coding-agent's `edit` tool.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#"
        });
        sanitize_schema_for_gemini(&mut schema);
        let obj = schema.as_object().unwrap();
        assert!(!obj.contains_key("additionalProperties"));
        assert!(!obj.contains_key("$schema"));
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("properties"));
        assert!(obj.contains_key("required"));
    }

    #[test]
    fn test_sanitize_schema_strips_nested_unsupported_keys() {
        // Reproduces the exact path Gemini complained about:
        //   tools[0].function_declarations[2].parameters.properties[1].value.items
        // `additionalProperties` lives both at the root AND inside an array's
        // `items` schema. Both must be removed.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string"},
                            "newText": {"type": "string"}
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["edits"],
            "additionalProperties": false
        });
        sanitize_schema_for_gemini(&mut schema);
        // Recursively assert no `additionalProperties` survives anywhere
        fn assert_no_unsupported(v: &serde_json::Value) {
            match v {
                serde_json::Value::Object(map) => {
                    for key in GEMINI_UNSUPPORTED_SCHEMA_KEYS {
                        assert!(
                            !map.contains_key(*key),
                            "unsupported key '{}' survived sanitization",
                            key
                        );
                    }
                    for v in map.values() {
                        assert_no_unsupported(v);
                    }
                }
                serde_json::Value::Array(arr) => {
                    for v in arr {
                        assert_no_unsupported(v);
                    }
                }
                _ => {}
            }
        }
        assert_no_unsupported(&schema);

        // And verify the structural keys are still there
        let edits = &schema["properties"]["edits"];
        assert_eq!(edits["type"], "array");
        let items = &edits["items"];
        assert_eq!(items["type"], "object");
        assert!(items["properties"]["oldText"].is_object());
    }

    #[test]
    fn test_sanitize_schema_handles_anyof_oneof_arrays() {
        // Schema generators sometimes wrap fields in anyOf/oneOf; ensure we
        // recurse into array siblings of `properties` correctly.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [
                        {"type": "string", "const": "a"},
                        {"type": "object", "additionalProperties": false}
                    ]
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let any_of = &schema["properties"]["value"]["anyOf"];
        assert!(any_of.is_array());
        // `const` removed from first variant
        assert!(!any_of[0].as_object().unwrap().contains_key("const"));
        // `additionalProperties` removed from second variant
        assert!(!any_of[1]
            .as_object()
            .unwrap()
            .contains_key("additionalProperties"));
    }

    #[test]
    fn test_sanitize_schema_does_not_strip_parameter_names_matching_keywords() {
        // Regression: if a tool's `properties` map has a key that happens to
        // match an unsupported schema keyword (e.g. a parameter literally named
        // `const` or `examples`), the sanitizer must NOT remove it — those are
        // parameter names, not schema keywords. Reported by gemini-code-assist
        // on PR #610.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "const": { "type": "string", "description": "A parameter literally named const" },
                "examples": { "type": "array", "items": {"type": "string"} },
                "if": { "type": "boolean" },
                "normal_param": { "type": "string" }
            },
            "required": ["const", "examples", "if", "normal_param"]
        });
        sanitize_schema_for_gemini(&mut schema);
        let props = schema["properties"].as_object().unwrap();
        assert!(
            props.contains_key("const"),
            "parameter named 'const' was incorrectly stripped from properties"
        );
        assert!(
            props.contains_key("examples"),
            "parameter named 'examples' was incorrectly stripped from properties"
        );
        assert!(
            props.contains_key("if"),
            "parameter named 'if' was incorrectly stripped from properties"
        );
        assert!(props.contains_key("normal_param"));
        // required[] entries are strings, never recursed into — always preserved
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 4);
    }

    #[test]
    fn test_sanitize_schema_preserves_parameter_names_nested_deeply() {
        // The user-named-key protection must apply at every level of nesting,
        // not just at the outermost `properties`. Verifies that a parameter
        // named `if` nested two levels deep inside another object parameter
        // survives sanitization.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "outer_param": {
                    "type": "object",
                    "properties": {
                        "if": { "type": "string" },
                        "const": { "type": "boolean" }
                    },
                    "required": ["if"]
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let inner_props = schema["properties"]["outer_param"]["properties"]
            .as_object()
            .unwrap();
        assert!(
            inner_props.contains_key("if"),
            "deeply-nested parameter 'if' was incorrectly stripped"
        );
        assert!(
            inner_props.contains_key("const"),
            "deeply-nested parameter 'const' was incorrectly stripped"
        );
    }

    #[test]
    fn test_sanitize_schema_still_strips_unsupported_keywords_inside_property_schemas() {
        // Companion to the test above: the FIX must not over-correct. Inside
        // each property's own schema, unsupported keywords (e.g. const at the
        // value level) MUST still be stripped.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "const": "delete"  // schema keyword inside a property's value
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let action = &schema["properties"]["action"];
        assert_eq!(action["type"], "string");
        assert!(
            !action.as_object().unwrap().contains_key("const"),
            "`const` inside a property's schema should still be stripped"
        );
    }

    #[test]
    fn test_sanitize_schema_preserves_supported_keys() {
        // Sanity check: keys Gemini accepts must survive verbatim.
        let mut schema = serde_json::json!({
            "type": "object",
            "description": "A point",
            "properties": {
                "x": {"type": "number", "minimum": 0, "maximum": 100},
                "label": {"type": "string", "enum": ["a", "b"]}
            },
            "required": ["x"]
        });
        let before = schema.clone();
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(before, schema, "clean schema should pass through unchanged");
    }

    #[test]
    fn test_convert_tools_sanitizes_parameters() {
        // End-to-end: convert_tools must produce Gemini-clean schemas. This is
        // the regression test for the pi-coding-agent bug —
        // `additionalProperties` would survive into the request body and
        // Gemini would 400.
        let tools = vec![ToolDefinition {
            type_: "function".to_string(),
            function: crate::FunctionDefinition {
                name: "edit".to_string(),
                description: Some("Edit a file".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {"type": "string"},
                                    "newText": {"type": "string"}
                                },
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["path", "edits"],
                    "additionalProperties": false
                }),
            },
        }];
        let gemini_tools = convert_tools(&tools);
        let params = &gemini_tools[0].function_declarations[0].parameters;
        let serialized = serde_json::to_string(params).unwrap();
        assert!(
            !serialized.contains("additionalProperties"),
            "serialized parameters must not contain `additionalProperties`; got: {}",
            serialized
        );
    }

    // ── #668: response_format → Gemini structured output ────────────────────

    #[test]
    fn test_response_format_json_object_sets_mime_type_only() {
        let rf = serde_json::json!({"type": "json_object"});
        let (mime, schema) = response_format_to_gemini(&rf);
        assert_eq!(mime.as_deref(), Some("application/json"));
        assert!(schema.is_none());
    }

    #[test]
    fn test_response_format_json_schema_sets_mime_and_sanitized_schema() {
        let rf = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "weather",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["city", "temp_c"],
                    "properties": {
                        "city": {"type": "string"},
                        "temp_c": {"type": "number"}
                    }
                }
            }
        });
        let (mime, schema) = response_format_to_gemini(&rf);
        assert_eq!(mime.as_deref(), Some("application/json"));
        let schema = schema.expect("schema translated");
        // `additionalProperties` (rejected by Gemini) must be stripped.
        assert!(schema
            .as_object()
            .unwrap()
            .get("additionalProperties")
            .is_none());
        // Structural keys survive.
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["city"].is_object());
        assert_eq!(schema["required"], serde_json::json!(["city", "temp_c"]));
    }

    #[test]
    fn test_response_format_text_is_noop() {
        let (mime, schema) = response_format_to_gemini(&serde_json::json!({"type": "text"}));
        assert!(mime.is_none());
        assert!(schema.is_none());
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
        let content = response.candidates[0].content.as_ref().unwrap();
        let (text, tool_calls) = extract_response_content(&content.parts);

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

    // Regression: Google may omit response fields when no usable output is
    // produced (typically MAX_TOKENS / SAFETY / RECITATION). The strict schema
    // previously rejected these payloads and surfaced as 502s. The four tests
    // below cover every shape we have observed or that the reviewer flagged.

    /// Payload captured from `gemini-3-flash-preview`: `content` is `{}`.
    #[test]
    fn test_parse_response_with_empty_content_on_max_tokens() {
        let json = r#"{
            "candidates": [{
                "content": {},
                "finishReason": "MAX_TOKENS",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "totalTokenCount": 5
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let content = response.candidates[0].content.as_ref().unwrap();
        assert_eq!(content.role, "");
        assert!(content.parts.is_empty());
        let (text, tool_calls) = extract_response_content(&content.parts);
        assert!(text.is_none());
        assert!(tool_calls.is_none());
    }

    /// Payload captured from `gemini-2.5-flash`: `parts` omitted, `role` present.
    #[test]
    fn test_parse_response_with_role_only_content_on_max_tokens() {
        let json = r#"{
            "candidates": [{
                "content": {"role": "model"},
                "finishReason": "MAX_TOKENS",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "totalTokenCount": 5
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        let content = response.candidates[0].content.as_ref().unwrap();
        assert_eq!(content.role, "model");
        assert!(content.parts.is_empty());
        let (text, tool_calls) = extract_response_content(&content.parts);
        assert!(text.is_none());
        assert!(tool_calls.is_none());
    }

    /// Reviewer-flagged variant: `content` field absent entirely on the
    /// candidate (observed in safety-blocked responses).
    #[test]
    fn test_parse_response_with_missing_content_field() {
        let json = r#"{
            "candidates": [{
                "finishReason": "SAFETY",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "totalTokenCount": 8
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        assert!(response.candidates[0].content.is_none());
        assert_eq!(
            response.candidates[0].finish_reason.as_deref(),
            Some("SAFETY")
        );
    }

    /// `content: null` — the explicit-null form.
    #[test]
    fn test_parse_response_with_null_content() {
        let json = r#"{
            "candidates": [{
                "content": null,
                "finishReason": "SAFETY",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "totalTokenCount": 8
            }
        }"#;

        let response: GeminiResponse = serde_json::from_str(json).unwrap();
        assert!(response.candidates[0].content.is_none());
    }
}
