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
    Text {
        text: String,
        /// Anthropic prompt-caching breakpoint, forwarded verbatim from the
        /// caller's OpenAI content part (`{"type":"ephemeral"}` shape, #666).
        /// Omitted when absent so the common (uncached) request is unchanged.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
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
    #[serde(rename = "image")]
    Image {
        source: AnthropicImageSource,
        /// Prompt-caching breakpoint on an image block (#666). Same verbatim
        /// forwarding as `Text::cache_control`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
}

/// Anthropic image source. Anthropic accepts either inline base64 bytes
/// (`type: "base64"`) or a remote URL (`type: "url"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicImageSource {
    #[serde(rename = "base64")]
    Base64 {
        media_type: String,
        /// Exact base64 payload, forwarded verbatim (no re-encoding).
        data: String,
    },
    #[serde(rename = "url")]
    Url { url: String },
}

/// Anthropic `system` prompt. Anthropic accepts either a bare string or an
/// array of text blocks; the array form is required to attach a
/// `cache_control` breakpoint to the system prompt (#666). We keep the bare
/// string for the common (uncached) case so that request is byte-identical to
/// before, and only switch to the block array when a cache_control is present.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum AnthropicSystem {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

/// A `text` block inside the `system` array, optionally carrying a
/// prompt-caching breakpoint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
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
    pub system: Option<AnthropicSystem>,
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
    /// Allowlisted reasoning-control fields forwarded from the caller's request
    /// (`thinking`, `reasoning_effort` — see `ANTHROPIC_PASSTHROUGH_KEYS`).
    /// Flattened to top-level JSON. Populated by `build_request`, which filters
    /// the request's `extra` map so internal E2EE keys and OpenAI-only fields
    /// never reach Anthropic. The allowlist guarantees no collision with the
    /// named fields above, so `flatten` cannot emit duplicate keys.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
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
    /// Tokens served from the prompt cache (cache hit). Reported by Anthropic
    /// SEPARATELY from `input_tokens` (#666). Defaults to 0 when the field is
    /// absent (no caching, or older API versions).
    #[serde(default)]
    pub cache_read_input_tokens: i32,
    /// Tokens written to the prompt cache on this request (cache miss/creation).
    /// Also reported separately from `input_tokens`.
    #[serde(default)]
    pub cache_creation_input_tokens: i32,
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

/// Pull the raw `cache_control` object (if any) out of a single OpenAI content
/// part. Anthropic accepts the breakpoint object verbatim
/// (`{"type":"ephemeral"}`), so we forward it untouched rather than inventing
/// our own shape (#666). The shared `parse_content` discards unknown fields, so
/// we read this directly from the raw JSON here in the Anthropic converter
/// instead of widening the shared `ContentPart` enum.
fn cache_control_of(item: &serde_json::Value) -> Option<serde_json::Value> {
    item.as_object()
        .and_then(|obj| obj.get("cache_control"))
        .filter(|v| !v.is_null())
        .cloned()
}

/// Whether a string content value carries any per-part `cache_control`. A bare
/// JSON string never does; an array might on one of its parts.
fn content_has_cache_control(content: &serde_json::Value) -> bool {
    matches!(content, serde_json::Value::Array(items)
        if items.iter().any(|item| cache_control_of(item).is_some()))
}

/// Extract `cache_control` for each content part, aligned 1:1 with the parts
/// `parse_content` produces. We reuse `parse_content_part` (the exact per-part
/// recogniser `parse_content` is built from) as the filter, so a part counts
/// here iff it produces a `ContentPart` there. This keeps the indices in lockstep
/// even for malformed parts (e.g. a `text` part whose `text` is missing/non-string,
/// which `parse_content_part` drops), so the caller always attaches each breakpoint
/// to the correct block (#666). A bare string yields a single `None` (matching the
/// single `ContentPart::Text`).
fn per_part_cache_controls(content: &serde_json::Value) -> Vec<Option<serde_json::Value>> {
    use crate::non_attested::external::content::parse_content_part;

    match content {
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|item| parse_content_part(item).is_some())
            .map(cache_control_of)
            .collect(),
        // A plain string (or any non-array) parses to exactly one text part.
        _ => vec![None],
    }
}

/// The `cache_control` breakpoint to attach to a single, flattened text block
/// (the assistant turn rebuilds all text parts into one block via
/// `text_from_content`). Anthropic allows a cache breakpoint on an assistant
/// content block, so a cached prefix that ends at an assistant turn must keep
/// its breakpoint (#666). When several text parts each carry a breakpoint, the
/// LAST one is the prefix boundary, so we surface that — attaching it to the one
/// block that represents the concatenated text. Returns `None` when no text part
/// carries a breakpoint (the common case stays the bare-string form).
fn flattened_text_cache_control(content: &serde_json::Value) -> Option<serde_json::Value> {
    per_part_cache_controls(content)
        .into_iter()
        .flatten()
        .next_back()
}

/// Build the Anthropic `system` value from a raw OpenAI system message content.
///
/// Uses a bare string in the common case (no cache_control) so the request is
/// byte-identical to the pre-#666 behaviour. If any part carries a
/// `cache_control`, emit the block-array form Anthropic requires for caching,
/// carrying the breakpoint on the matching text block.
fn build_system(content: &serde_json::Value) -> AnthropicSystem {
    use crate::non_attested::external::content::text_from_content as extract_content;

    if !content_has_cache_control(content) {
        return AnthropicSystem::Text(extract_content(content));
    }

    // Array form with at least one cache_control: emit one text block per
    // `text` part, attaching its breakpoint. Image parts in a system prompt are
    // dropped (matching the text-only flattening this path already did).
    let serde_json::Value::Array(items) = content else {
        // Not an array but flagged as having cache_control is impossible
        // (content_has_cache_control only returns true for arrays), but be safe.
        return AnthropicSystem::Text(extract_content(content));
    };

    let mut blocks = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if obj.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let Some(text) = obj.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        blocks.push(AnthropicSystemBlock {
            type_: "text",
            text: text.to_string(),
            cache_control: cache_control_of(item),
        });
    }

    if blocks.is_empty() {
        AnthropicSystem::Text(extract_content(content))
    } else {
        AnthropicSystem::Blocks(blocks)
    }
}

/// Convert OpenAI messages to Anthropic format
pub fn convert_messages(
    messages: &[ChatMessage],
) -> (Option<AnthropicSystem>, Vec<AnthropicMessage>) {
    use crate::non_attested::external::content::{
        parse_content, text_from_content as extract_content, ContentPart,
    };

    let mut system_message = None;
    let mut anthropic_messages = Vec::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                if let Some(content) = &msg.content {
                    system_message = Some(build_system(content));
                }
            }
            MessageRole::User => {
                // User messages may be multimodal (text + image parts). Build
                // native Anthropic content blocks so images are transmitted as
                // images, not flattened into a JSON text blob (issue #640).
                let parts = msg.content.as_ref().map(parse_content).unwrap_or_default();

                let has_image = parts.iter().any(|p| !matches!(p, ContentPart::Text(_)));
                // Per-part cache_control breakpoints, forwarded verbatim (#666).
                // When present we must emit content blocks (the bare-string form
                // can't carry a breakpoint), even with no image part.
                let cache_controls = msg
                    .content
                    .as_ref()
                    .map(per_part_cache_controls)
                    .unwrap_or_default();
                let has_cache_control = cache_controls.iter().any(Option::is_some);

                if has_image || has_cache_control {
                    let mut blocks = Vec::with_capacity(parts.len());
                    for (idx, part) in parts.into_iter().enumerate() {
                        let cc = cache_controls.get(idx).cloned().flatten();
                        match part {
                            ContentPart::Text(text) => {
                                if !text.is_empty() {
                                    blocks.push(AnthropicContentPart::Text {
                                        text,
                                        cache_control: cc,
                                    });
                                }
                            }
                            ContentPart::ImageBase64 { media_type, data } => {
                                blocks.push(AnthropicContentPart::Image {
                                    source: AnthropicImageSource::Base64 { media_type, data },
                                    cache_control: cc,
                                });
                            }
                            ContentPart::ImageUrl { url } => {
                                blocks.push(AnthropicContentPart::Image {
                                    source: AnthropicImageSource::Url { url },
                                    cache_control: cc,
                                });
                            }
                        }
                    }
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: AnthropicMessageContent::Blocks(blocks),
                    });
                } else {
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
            }
            MessageRole::Assistant => {
                // Per-part cache_control, aligned the same way the user branch
                // does — Anthropic allows a breakpoint on an assistant content
                // block, so a cached prefix ending at an assistant turn keeps it
                // (#666). The assistant text is flattened into a single block, so
                // we attach the last (prefix-boundary) breakpoint to that block.
                let text_cache_control =
                    msg.content.as_ref().and_then(flattened_text_cache_control);

                // Check if the assistant message contains tool calls
                if let Some(tool_calls) = &msg.tool_calls {
                    if !tool_calls.is_empty() {
                        // Build content blocks: optional text + tool_use blocks
                        let mut blocks = Vec::new();

                        // Add text content if present
                        if let Some(text) = msg.content.as_ref().map(&extract_content) {
                            if !text.is_empty() {
                                blocks.push(AnthropicContentPart::Text {
                                    text,
                                    cache_control: text_cache_control,
                                });
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

                // No tool calls - just text content.
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();
                // A bare string can't carry a breakpoint, so when this turn has a
                // cache_control we emit the block-array form (mirroring the user
                // branch). The common (uncached) case keeps the bare string so the
                // request is byte-identical to before.
                if let Some(cache_control) = text_cache_control {
                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicMessageContent::Blocks(vec![
                            AnthropicContentPart::Text {
                                text: content,
                                cache_control: Some(cache_control),
                            },
                        ]),
                    });
                } else {
                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicMessageContent::Text(content),
                    });
                }
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
    /// Prompt-cache tokens (read + creation) Anthropic charged us for. Reported
    /// separately from `input_tokens` by Anthropic; we fold them into
    /// `prompt_tokens` and surface the read portion as `cached_tokens` so the
    /// existing OpenAI-shaped billing path bills cache reads (#666).
    cache_read_tokens: i32,
    cache_creation_tokens: i32,
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
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls: HashMap::new(),
            tool_call_counter: 0,
        }
    }

    /// Build the prompt-token usage for a streaming chunk.
    ///
    /// CRITICAL accounting (#666): Anthropic reports cache reads/creation
    /// SEPARATELY from `input_tokens`, whereas OpenAI's `cached_tokens` is a
    /// SUBSET of `prompt_tokens` (and `TokenUsage::cached_tokens()` caps it to
    /// `[0, prompt_tokens]`). To both preserve that OpenAI invariant AND bill
    /// the cache-read cost, we ADD the cache tokens into `prompt_tokens` and
    /// report the read portion as `cached_tokens`.
    fn prompt_tokens(&self) -> i32 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    /// `prompt_tokens_details` for a chunk: `cached_tokens` when there was a
    /// cache read, else `None` (so an uncached stream is byte-identical to
    /// before).
    fn prompt_tokens_details(&self) -> Option<serde_json::Value> {
        if self.cache_read_tokens > 0 {
            Some(serde_json::json!({ "cached_tokens": self.cache_read_tokens }))
        } else {
            None
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
                // Cache-token counts are known up front (message_start), like
                // input_tokens (#666). Capture them so an interrupted stream is
                // still billed for the cache reads Anthropic charged us for.
                state.cache_read_tokens = message.usage.cache_read_input_tokens;
                state.cache_creation_tokens = message.usage.cache_creation_input_tokens;
                let ctx = state.chunk_context();
                // Anthropic reports input tokens up front in `message_start`, but
                // completion tokens only in the final `message_delta`. Carry the
                // known input tokens on the first chunk so an interrupted stream is
                // still billed for the prompt tokens Anthropic charged us for
                // (completion tokens stay 0 until the final chunk). On a clean
                // stream the final chunk's full usage overwrites this.
                let prompt_tokens = state.prompt_tokens();
                let early_usage = TokenUsage {
                    prompt_tokens,
                    completion_tokens: 0,
                    total_tokens: prompt_tokens,
                    prompt_tokens_details: state.prompt_tokens_details(),
                };
                Ok(Some(StreamChunk::Chat(
                    ctx.role_chunk_with_usage(Some(early_usage)),
                )))
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
                // The final message_delta usage may also restate the cache
                // counts; prefer non-zero values so the final chunk carries the
                // authoritative figures even if message_start was missed (#666).
                if usage.cache_read_input_tokens > 0 {
                    state.cache_read_tokens = usage.cache_read_input_tokens;
                }
                if usage.cache_creation_input_tokens > 0 {
                    state.cache_creation_tokens = usage.cache_creation_input_tokens;
                }
                let ctx = state.chunk_context();
                let finish_reason = map_finish_reason(delta.stop_reason);
                let prompt_tokens = state.prompt_tokens();
                let token_usage = TokenUsage {
                    prompt_tokens,
                    completion_tokens: state.output_tokens,
                    total_tokens: prompt_tokens + state.output_tokens,
                    prompt_tokens_details: state.prompt_tokens_details(),
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

        // No cache_control -> bare string form (unchanged from pre-#666).
        match system {
            Some(AnthropicSystem::Text(s)) => assert_eq!(s, "You are helpful."),
            other => panic!("expected bare-string system, got {other:?}"),
        }
        assert_eq!(anthropic_messages.len(), 1);
    }

    /// A real, minimal 1x1 solid-red PNG (constructed by hand, base64-encoded).
    /// Used to prove the converter forwards the EXACT bytes (issue #640).
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAD\
        UlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn test_convert_messages_image_preserves_base64_and_media_type() {
        // Strip the line-continuation whitespace so the constant is a clean
        // base64 string, exactly as a client would send it.
        let payload: String = RED_PNG_B64.split_whitespace().collect();
        let data_uri = format!("data:image/png;base64,{payload}");

        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([
                {"type": "text", "text": "Describe this image."},
                {"type": "image_url", "image_url": {"url": data_uri}}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        assert_eq!(anthropic_messages.len(), 1);

        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            AnthropicMessageContent::Text(t) => {
                panic!("image was flattened to text instead of an image block: {t}")
            }
        };
        assert_eq!(blocks.len(), 2, "expected text + image blocks");

        match &blocks[0] {
            AnthropicContentPart::Text { text, .. } => assert_eq!(text, "Describe this image."),
            other => panic!("expected text block first, got {other:?}"),
        }
        match &blocks[1] {
            AnthropicContentPart::Image {
                source: AnthropicImageSource::Base64 { media_type, data },
                ..
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, &payload, "base64 payload must be byte-identical");
            }
            other => panic!("expected base64 image block, got {other:?}"),
        }

        // Serialize the whole request shape Anthropic receives and assert the
        // exact bytes survive (no double-encoding, no JSON-blob flattening).
        let json = serde_json::to_string(&anthropic_messages[0]).unwrap();
        assert!(
            json.contains(&payload),
            "serialized request lost the base64 payload"
        );
        assert!(json.contains("\"type\":\"image\""));
        assert!(json.contains("\"media_type\":\"image/png\""));
    }

    #[test]
    fn test_convert_messages_image_url_uses_url_source() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            _ => panic!("expected blocks"),
        };
        match &blocks[0] {
            AnthropicContentPart::Image {
                source: AnthropicImageSource::Url { url },
                ..
            } => assert_eq!(url, "https://example.com/cat.jpg"),
            other => panic!("expected url image source, got {other:?}"),
        }
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

    #[test]
    fn test_message_start_chunk_carries_input_usage() {
        // Regression test for nearai/infra#98: an interrupted Anthropic stream
        // (client disconnect / provider error before the final message_delta)
        // must still be billable for the prompt tokens Anthropic charged us for.
        // The billing layer (`InterceptStream`) only sees usage attached to a
        // chunk, so `message_start` must surface the input tokens immediately.
        let mut state = AnthropicParserState::new("claude-test".to_string());
        let data =
            r#"{"type":"message_start","message":{"id":"msg_123","usage":{"input_tokens":42}}}"#;

        let chunk = AnthropicEventParser::parse_event(&mut state, data)
            .unwrap()
            .expect("message_start should produce a chunk");

        let StreamChunk::Chat(chat) = chunk else {
            panic!("expected a chat chunk");
        };
        let usage = chat.usage.expect("role chunk should carry early usage");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 42);
        assert_eq!(state.input_tokens, 42);
    }

    // ── #666: prompt-caching cache_control passthrough + cache-stat surfacing ──

    /// Read `prompt_tokens_details.cached_tokens` off a usage object, or 0.
    fn cached_tokens_of(usage: &TokenUsage) -> i64 {
        usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    #[test]
    fn test_cache_control_on_system_emits_block_array() {
        // A system message whose text part carries cache_control must serialize
        // `system` as an array of text blocks with the breakpoint, not a string.
        let messages = vec![ChatMessage {
            role: MessageRole::System,
            content: Some(serde_json::json!([
                {
                    "type": "text",
                    "text": "Large shared preamble",
                    "cache_control": {"type": "ephemeral"}
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (system, _msgs) = convert_messages(&messages);
        let system = system.expect("system should be present");
        match &system {
            AnthropicSystem::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, "Large shared preamble");
                assert_eq!(
                    blocks[0].cache_control,
                    Some(serde_json::json!({"type": "ephemeral"}))
                );
            }
            other => panic!("expected block-array system, got {other:?}"),
        }

        // Serialized request: `system` is an array with the verbatim breakpoint.
        let json = serde_json::to_value(&system).unwrap();
        assert!(json.is_array(), "system must serialize as an array");
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_cache_control_on_user_text_part_appears_in_request() {
        // A user text part with cache_control forces the block form (even with
        // no image) and forwards the breakpoint verbatim.
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([
                {
                    "type": "text",
                    "text": "Cached context",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                },
                {"type": "text", "text": "Volatile question"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        assert_eq!(anthropic_messages.len(), 1);
        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            AnthropicMessageContent::Text(t) => {
                panic!("cache_control should force the block form, got text: {t}")
            }
        };
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Cached context");
                assert_eq!(
                    *cache_control,
                    Some(serde_json::json!({"type": "ephemeral", "ttl": "1h"}))
                );
            }
            other => panic!("expected text block, got {other:?}"),
        }
        // The volatile part keeps no breakpoint.
        match &blocks[1] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Volatile question");
                assert!(cache_control.is_none());
            }
            other => panic!("expected text block, got {other:?}"),
        }

        // The serialized outgoing request carries the breakpoint verbatim.
        let json = serde_json::to_string(&anthropic_messages[0]).unwrap();
        assert!(json.contains("\"cache_control\""));
        assert!(json.contains("\"ttl\":\"1h\""));
    }

    #[test]
    fn test_cache_control_alignment_survives_malformed_part() {
        // Alignment guard (#666): a malformed `text` part (no `text` field) is
        // dropped by `parse_content`, so the cache_control list must drop it too —
        // otherwise the breakpoint would be misattached to the wrong block. The
        // breakpoint here belongs to "Cached", and "Volatile" must keep none.
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([
                {"type": "text"}, // malformed: dropped by parse_content
                {
                    "type": "text",
                    "text": "Cached",
                    "cache_control": {"type": "ephemeral"}
                },
                {"type": "text", "text": "Volatile"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            AnthropicMessageContent::Text(t) => panic!("expected block form, got text: {t}"),
        };
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Cached");
                assert_eq!(
                    *cache_control,
                    Some(serde_json::json!({"type": "ephemeral"}))
                );
            }
            other => panic!("expected text block, got {other:?}"),
        }
        match &blocks[1] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Volatile");
                assert!(
                    cache_control.is_none(),
                    "breakpoint must not bleed onto the volatile part"
                );
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn test_cache_control_on_assistant_text_part_forwards_breakpoint() {
        // #666: Anthropic allows a cache breakpoint on an assistant
        // content block, so a prefix ending at an assistant turn must keep it.
        // A plain-text assistant turn carrying cache_control must become the
        // block-array form with the breakpoint on the rebuilt text block.
        let messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::json!([
                {
                    "type": "text",
                    "text": "Assistant prefix to cache",
                    "cache_control": {"type": "ephemeral"}
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        assert_eq!(anthropic_messages.len(), 1);
        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            AnthropicMessageContent::Text(t) => {
                panic!("assistant cache_control should force the block form, got text: {t}")
            }
        };
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Assistant prefix to cache");
                assert_eq!(
                    *cache_control,
                    Some(serde_json::json!({"type": "ephemeral"}))
                );
            }
            other => panic!("expected text block, got {other:?}"),
        }

        // Serialized assistant message carries the verbatim breakpoint.
        let json = serde_json::to_string(&anthropic_messages[0]).unwrap();
        assert!(json.contains("\"cache_control\""));
        assert!(json.contains("\"role\":\"assistant\""));
    }

    #[test]
    fn test_cache_control_on_assistant_with_tool_calls_forwards_breakpoint() {
        // Assistant turn with BOTH text (carrying a breakpoint) and tool calls:
        // the breakpoint must land on the text block, the tool_use blocks follow.
        let messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::json!([
                {
                    "type": "text",
                    "text": "Let me look that up.",
                    "cache_control": {"type": "ephemeral"}
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: Some("call_1".to_string()),
                type_: Some("function".to_string()),
                function: FunctionCall {
                    name: Some("search".to_string()),
                    arguments: Some("{\"q\":\"x\"}".to_string()),
                },
                index: None,
                thought_signature: None,
            }]),
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        let blocks = match &anthropic_messages[0].content {
            AnthropicMessageContent::Blocks(b) => b,
            AnthropicMessageContent::Text(t) => panic!("expected block form, got text: {t}"),
        };
        assert_eq!(blocks.len(), 2, "text block + tool_use block");
        match &blocks[0] {
            AnthropicContentPart::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "Let me look that up.");
                assert_eq!(
                    *cache_control,
                    Some(serde_json::json!({"type": "ephemeral"}))
                );
            }
            other => panic!("expected text block first, got {other:?}"),
        }
        assert!(matches!(blocks[1], AnthropicContentPart::ToolUse { .. }));
    }

    #[test]
    fn test_assistant_without_cache_control_stays_bare_string() {
        // Regression guard: an assistant turn with no breakpoint keeps the
        // bare-string form (byte-identical to pre-#666).
        let messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::Value::String("Plain answer".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        match &anthropic_messages[0].content {
            AnthropicMessageContent::Text(t) => assert_eq!(t, "Plain answer"),
            other => panic!("expected bare-string assistant content, got {other:?}"),
        }
    }

    #[test]
    fn test_no_cache_control_keeps_bare_string_system() {
        // Regression guard: a request with NO cache_control must still serialize
        // `system` as a bare string (no #666 regression for the common case).
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
        let system = system.expect("system present");
        assert_eq!(
            system,
            AnthropicSystem::Text("You are helpful.".to_string())
        );
        let json = serde_json::to_value(&system).unwrap();
        assert!(
            json.is_string(),
            "system must stay a bare string when uncached"
        );

        // The user message stays the bare-string form too (no block array).
        match &anthropic_messages[0].content {
            AnthropicMessageContent::Text(t) => assert_eq!(t, "Hello"),
            other => panic!("expected bare-string user content, got {other:?}"),
        }
    }

    #[test]
    fn test_anthropic_usage_deserializes_cache_fields() {
        let json = r#"{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":80,"cache_creation_input_tokens":40}"#;
        let usage: AnthropicUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 80);
        assert_eq!(usage.cache_creation_input_tokens, 40);
    }

    #[test]
    fn test_anthropic_usage_cache_fields_default_to_zero() {
        // Absent cache fields default to 0 (older API versions / no caching).
        let json = r#"{"input_tokens":100,"output_tokens":20}"#;
        let usage: AnthropicUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
    }

    #[test]
    fn test_streaming_final_chunk_carries_cached_tokens() {
        // message_start surfaces cache reads; the final message_delta restates
        // them. Both fold the cache tokens into prompt_tokens and report the
        // read portion as cached_tokens, preserving cached <= prompt.
        let mut state = AnthropicParserState::new("claude-test".to_string());

        let start = r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":80,"cache_creation_input_tokens":5}}}"#;
        let chunk = AnthropicEventParser::parse_event(&mut state, start)
            .unwrap()
            .expect("message_start should produce a chunk");
        let StreamChunk::Chat(chat) = chunk else {
            panic!("expected a chat chunk");
        };
        let usage = chat.usage.expect("early usage");
        // prompt_tokens = input + cache_read + cache_creation = 10 + 80 + 5 = 95.
        assert_eq!(usage.prompt_tokens, 95);
        assert_eq!(cached_tokens_of(&usage), 80);
        assert!(
            cached_tokens_of(&usage) <= usage.prompt_tokens as i64,
            "cached_tokens must not exceed prompt_tokens"
        );

        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":30}}"#;
        let chunk = AnthropicEventParser::parse_event(&mut state, delta)
            .unwrap()
            .expect("message_delta should produce a chunk");
        let StreamChunk::Chat(chat) = chunk else {
            panic!("expected a chat chunk");
        };
        let usage = chat.usage.expect("final usage");
        assert_eq!(usage.prompt_tokens, 95);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.total_tokens, 125);
        assert_eq!(cached_tokens_of(&usage), 80);
        assert!(cached_tokens_of(&usage) <= usage.prompt_tokens as i64);
    }

    #[test]
    fn test_streaming_no_cache_omits_prompt_tokens_details() {
        // No cache reads -> prompt_tokens_details stays None (no regression).
        let mut state = AnthropicParserState::new("claude-test".to_string());
        let start =
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":42}}}"#;
        let chunk = AnthropicEventParser::parse_event(&mut state, start)
            .unwrap()
            .unwrap();
        let StreamChunk::Chat(chat) = chunk else {
            panic!("expected a chat chunk");
        };
        let usage = chat.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 42);
        assert!(usage.prompt_tokens_details.is_none());
    }
}
