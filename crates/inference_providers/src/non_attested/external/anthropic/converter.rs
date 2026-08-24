//! Anthropic format converter
//!
//! Converts Anthropic's Messages API format to OpenAI-compatible format.
//! This module handles:
//! - Request conversion (OpenAI → Anthropic)
//! - Response/event parsing (Anthropic → OpenAI)
//! - Streaming state management for tool calls

#[cfg(test)]
use crate::TokenUsage;
use crate::{
    ChatMessage, CompletionError, FunctionCall, MessageRole, SSEEventParser, StreamChunk, ToolCall,
    ToolDefinition,
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
        /// Anthropic prompt-caching breakpoint. Anthropic documents `tool_use`
        /// as cacheable, and a flattened assistant turn carries the turn's
        /// breakpoint on its final block.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Anthropic prompt-caching breakpoint on a tool-result block. Anthropic
        /// documents tool_result as cacheable, and a tool loop's advancing anchor
        /// normally rides the trailing tool result — without this field the caller's
        /// breakpoint is silently dropped and the cache never advances past the
        /// system prefix. Same verbatim forwarding as `Text::cache_control`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
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

impl AnthropicContentPart {
    /// Mutable handle on this block's prompt-caching breakpoint.
    fn cache_control_mut(&mut self) -> &mut Option<serde_json::Value> {
        match self {
            Self::Text { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. }
            | Self::Image { cache_control, .. } => cache_control,
        }
    }
}

/// Anthropic rejects a request carrying more than four `cache_control` blocks
/// with `400 invalid_request_error` ("A maximum of 4 blocks with cache_control
/// may be provided"). Callers control the markers, so we clamp rather than let
/// an over-marking client turn into an upstream error.
const MAX_CACHE_BREAKPOINTS: usize = 4;

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
/// before, and use the block array for every message once caching is enabled.
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
    // #632: Anthropic returns the upstream dated canonical name here (e.g.
    // `claude-haiku-4-5-20251001`). We deserialize it for completeness/parity
    // with the wire payload, but intentionally do NOT surface it on our
    // response: the non-streaming path echoes the requested/sent model name to
    // stay consistent with the streaming path (which never sees this field).
    // Hence it is read only in tests; allow dead_code in non-test builds.
    #[allow(dead_code)]
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

// =============================================================================
// Conversion Functions
// =============================================================================

/// Pull the raw `cache_control` object (if any) out of a single OpenAI content
/// part. The shared `parse_content` discards unknown fields, so we read this
/// directly from the raw JSON here instead of widening the shared
/// `ContentPart` enum. Explicit unsupported TTLs are removed so the provider
/// uses its five-minute default, the cache-write tier Cloud currently bills.
fn cache_control_of(item: &serde_json::Value) -> Option<serde_json::Value> {
    let mut cache_control = item
        .as_object()
        .and_then(|obj| obj.get("cache_control"))
        .filter(|v| !v.is_null())
        .cloned()?;

    // Cloud currently bills only Anthropic's five-minute cache-write tier.
    // Downgrade unsupported explicit TTLs to Anthropic's default five-minute
    // tier instead of forwarding a one-hour write that would be underbilled.
    if let Some(object) = cache_control.as_object_mut() {
        if object.get("ttl").and_then(serde_json::Value::as_str) != Some("5m") {
            object.remove("ttl");
        }
    }

    Some(cache_control)
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
/// carries a breakpoint.
fn flattened_text_cache_control(content: &serde_json::Value) -> Option<serde_json::Value> {
    per_part_cache_controls(content)
        .into_iter()
        .flatten()
        .next_back()
}

/// Clamp the number of forwarded `cache_control` breakpoints to Anthropic's
/// limit of four.
///
/// Order is Anthropic's own prefix order — `system` blocks first, then messages
/// in order, then blocks within a message. When the caller sends more than four
/// we keep the FIRST to preserve the request's earliest, most-stable cache
/// segment — the tools/system prefix when a system breakpoint exists, otherwise
/// simply the oldest surviving prefix — and the LAST THREE as the advancing
/// anchor. This is a heuristic: dropping a breakpoint costs a cache write,
/// whereas exceeding the limit costs the whole request.
fn enforce_breakpoint_limit(
    system: &mut Option<AnthropicSystem>,
    messages: &mut [AnthropicMessage],
) {
    enum BreakpointPosition {
        System(usize),
        Message {
            message_index: usize,
            part_index: usize,
        },
    }

    let mut positions = Vec::new();
    if let Some(AnthropicSystem::Blocks(blocks)) = system.as_mut() {
        positions.extend(
            blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| block.cache_control.is_some())
                .map(|(index, _)| BreakpointPosition::System(index)),
        );
    }
    for (message_index, message) in messages.iter_mut().enumerate() {
        let AnthropicMessageContent::Blocks(parts) = &mut message.content else {
            continue;
        };
        for (part_index, part) in parts.iter_mut().enumerate() {
            if part.cache_control_mut().is_some() {
                positions.push(BreakpointPosition::Message {
                    message_index,
                    part_index,
                });
            }
        }
    }

    let count = positions.len();
    if count <= MAX_CACHE_BREAKPOINTS {
        return;
    }

    tracing::debug!(
        breakpoints_sent = count,
        breakpoints_kept = MAX_CACHE_BREAKPOINTS,
        "Clamped Anthropic prompt-caching breakpoints to the provider limit"
    );

    for position in positions
        .into_iter()
        .skip(1)
        .take(count - MAX_CACHE_BREAKPOINTS)
    {
        match position {
            BreakpointPosition::System(index) => {
                let block = match system.as_mut() {
                    Some(AnthropicSystem::Blocks(blocks)) => blocks.get_mut(index),
                    Some(AnthropicSystem::Text(_)) | None => None,
                };
                if let Some(block) = block {
                    block.cache_control = None;
                }
            }
            BreakpointPosition::Message {
                message_index,
                part_index,
            } => {
                let cache_control = messages.get_mut(message_index).and_then(|message| {
                    match &mut message.content {
                        AnthropicMessageContent::Blocks(parts) => parts
                            .get_mut(part_index)
                            .map(AnthropicContentPart::cache_control_mut),
                        AnthropicMessageContent::Text(_) => None,
                    }
                });
                if let Some(cache_control) = cache_control {
                    *cache_control = None;
                }
            }
        }
    }
}

/// Build the Anthropic `system` value from a raw OpenAI system message content.
///
/// Uses a bare string when the request has no cache breakpoint so uncached
/// requests stay byte-identical to the pre-#666 behaviour. Once caching is
/// enabled anywhere in the request, emits block form for the system prompt so
/// moving a breakpoint between turns cannot change the prompt's representation.
fn build_system(content: &serde_json::Value, caching_enabled: bool) -> AnthropicSystem {
    use crate::non_attested::external::content::text_from_content as extract_content;

    if !caching_enabled {
        return AnthropicSystem::Text(extract_content(content));
    }

    // Anthropic rejects empty text blocks. Keep an empty system prompt in the
    // bare-string form even when a later message enables caching.
    let flattened = extract_content(content);
    if flattened.is_empty() {
        return AnthropicSystem::Text(flattened);
    }

    // Array form while caching is enabled: emit one text block per `text` part,
    // attaching its breakpoint. Image parts in a system prompt are dropped
    // (matching the text-only flattening this path already did).
    let serde_json::Value::Array(items) = content else {
        return AnthropicSystem::Blocks(vec![AnthropicSystemBlock {
            type_: "text",
            text: extract_content(content),
            cache_control: None,
        }]);
    };

    let mut blocks = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if obj.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let Some(text) = obj
            .get("text")
            .and_then(|t| t.as_str())
            .filter(|text| !text.is_empty())
        else {
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

    // #920: a caching client moves its breakpoint forward each turn, so the message
    // that was marked last turn arrives unmarked this turn. If the emitted shape
    // depended on that marking, the same message would change representation
    // underneath the previous turn's cache anchor. Once ANY breakpoint is present we
    // therefore emit the block form for every message, marked or not. Requests with
    // no caching at all keep the bare-string form and stay byte-identical to before.
    let caching_enabled = messages
        .iter()
        .filter_map(|msg| msg.content.as_ref())
        .any(content_has_cache_control);

    let mut system_message = None;
    let mut anthropic_messages = Vec::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                if let Some(content) = &msg.content {
                    system_message = Some(build_system(content, caching_enabled));
                }
            }
            MessageRole::User => {
                // User messages may be multimodal (text + image parts). Build
                // native Anthropic content blocks so images are transmitted as
                // images, not flattened into a JSON text blob (issue #640).
                let parts = msg.content.as_ref().map(parse_content).unwrap_or_default();

                let has_image = parts.iter().any(|p| !matches!(p, ContentPart::Text(_)));
                // Per-part cache_control breakpoints, forwarded verbatim (#666).
                // Keep them aligned even when another message enabled caching,
                // so only the originally marked parts carry a breakpoint.
                let cache_controls = msg
                    .content
                    .as_ref()
                    .map(per_part_cache_controls)
                    .unwrap_or_default();

                if has_image || caching_enabled {
                    // Keep text parts separate so a caller's per-part cache
                    // breakpoint remains attached to the exact prefix boundary it
                    // selected. This intentionally differs from the uncached
                    // newline-joined string form; flattening here would make the
                    // representation stable only by coarsening breakpoint precision.
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
                    let content = if blocks.is_empty() {
                        AnthropicMessageContent::Text(
                            msg.content
                                .as_ref()
                                .map(&extract_content)
                                .unwrap_or_default(),
                        )
                    } else {
                        AnthropicMessageContent::Blocks(blocks)
                    };
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content,
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
                // the last marker remains the turn's prefix boundary.
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
                                    cache_control: None,
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

                            blocks.push(AnthropicContentPart::ToolUse {
                                id,
                                name,
                                input,
                                cache_control: None,
                            });
                        }

                        // This branch requires non-empty tool_calls, so a final
                        // tool_use block always exists and carries the whole
                        // flattened turn's breakpoint.
                        if let Some(last_block) = blocks.last_mut() {
                            *last_block.cache_control_mut() = text_cache_control;
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
                // Once caching is enabled, every assistant turn uses block form so
                // moving the breakpoint cannot change its representation. The text
                // remains flattened into one block. Uncached requests keep the bare
                // string form and remain byte-identical to before.
                if caching_enabled && !content.is_empty() {
                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicMessageContent::Blocks(vec![
                            AnthropicContentPart::Text {
                                text: content,
                                cache_control: text_cache_control,
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
                // Tool results need special formatting for Anthropic, including
                // the flattened content's final prompt-caching breakpoint.
                let content = msg
                    .content
                    .as_ref()
                    .map(&extract_content)
                    .unwrap_or_default();
                let cache_control = msg.content.as_ref().and_then(flattened_text_cache_control);
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                anthropic_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentPart::ToolResult {
                            tool_use_id,
                            content,
                            cache_control,
                        },
                    ]),
                });
            }
        }
    }

    enforce_breakpoint_limit(&mut system_message, &mut anthropic_messages);
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

/// Thin inference-provider wrapper around the pure compatibility state machine.
pub struct AnthropicParserState {
    inner: anthropic_compat::StreamState,
}

impl AnthropicParserState {
    pub fn new(model: String) -> Self {
        Self {
            inner: anthropic_compat::StreamState::new(model, chrono::Utc::now().timestamp()),
        }
    }

    #[cfg(test)]
    fn input_tokens(&self) -> i32 {
        self.inner.input_tokens()
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
        let event: serde_json::Value = serde_json::from_str(data)
            .map_err(|_| CompletionError::InvalidResponse("Failed to parse event".to_string()))?;
        let converted = state
            .inner
            .convert_event(&event)
            .map_err(|error| match error.kind {
                anthropic_compat::CompatErrorKind::Upstream => {
                    CompletionError::CompletionError(error.message)
                }
                anthropic_compat::CompatErrorKind::Conversion => {
                    tracing::warn!(
                        parameter = error.parameter.as_deref().unwrap_or("request"),
                        "failed to convert Anthropic stream event"
                    );
                    CompletionError::InvalidResponse("Failed to convert event".to_string())
                }
            })?;
        converted
            .map(|chunk| {
                serde_json::from_value(chunk)
                    .map(StreamChunk::Chat)
                    .map_err(|_| {
                        CompletionError::InvalidResponse("Failed to convert event".to_string())
                    })
            })
            .transpose()
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
        assert_eq!(state.input_tokens(), 42);
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
    fn test_serialization_stable_when_anchor_moves() {
        let turn_n = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "System A"},
                    {
                        "type": "text",
                        "text": "System B",
                        "cache_control": {"type": "ephemeral"}
                    }
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "User A1"},
                    {
                        "type": "text",
                        "text": "User A2",
                        "cache_control": {"type": "ephemeral"}
                    }
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(serde_json::Value::String("Assistant B".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::Value::String("User C".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let turn_n_plus_one = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "System A"},
                    {
                        "type": "text",
                        "text": "System B",
                        "cache_control": {"type": "ephemeral"}
                    }
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "User A1"},
                    {"type": "text", "text": "User A2"}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(serde_json::Value::String("Assistant B".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "User C",
                    "cache_control": {"type": "ephemeral"}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(serde_json::Value::String("Assistant D".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system_n, messages_n) = convert_messages(&turn_n);
        let (system_n_plus_one, messages_n_plus_one) = convert_messages(&turn_n_plus_one);
        let mut system_n_json = serde_json::to_value(system_n.unwrap()).unwrap();
        let mut system_n_plus_one_json = serde_json::to_value(system_n_plus_one.unwrap()).unwrap();
        system_n_json[1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        system_n_plus_one_json[1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(system_n_json, system_n_plus_one_json);

        let mut user_a_n = serde_json::to_value(&messages_n[0]).unwrap();
        let mut user_a_n_plus_one = serde_json::to_value(&messages_n_plus_one[0]).unwrap();
        assert!(user_a_n["content"].is_array());
        assert!(user_a_n_plus_one["content"].is_array());
        assert_eq!(user_a_n["content"].as_array().unwrap().len(), 2);
        assert_eq!(user_a_n_plus_one["content"].as_array().unwrap().len(), 2);
        user_a_n["content"][1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        user_a_n_plus_one["content"][1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(user_a_n, user_a_n_plus_one);

        // The headline regression: an unmarked single-part message must have
        // the same block shape when the moving anchor lands on it next turn.
        let assistant_b_n = serde_json::to_value(&messages_n[1]).unwrap();
        let assistant_b_n_plus_one = serde_json::to_value(&messages_n_plus_one[1]).unwrap();
        assert_eq!(assistant_b_n, assistant_b_n_plus_one);

        let user_c_n = serde_json::to_value(&messages_n[2]).unwrap();
        let mut user_c_n_plus_one = serde_json::to_value(&messages_n_plus_one[2]).unwrap();
        user_c_n_plus_one["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(user_c_n, user_c_n_plus_one);
    }

    #[test]
    fn test_empty_system_stays_bare_string_when_caching_is_enabled() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String(String::new())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "anchor",
                    "cache_control": {"type": "ephemeral"}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, _messages) = convert_messages(&messages);
        assert!(matches!(system, Some(AnthropicSystem::Text(text)) if text.is_empty()));
    }

    #[test]
    fn test_empty_assistant_stays_bare_string_when_caching_is_enabled() {
        for content in [None, Some(serde_json::Value::String(String::new()))] {
            let messages = vec![
                ChatMessage {
                    role: MessageRole::User,
                    content: Some(serde_json::json!([{
                        "type": "text",
                        "text": "anchor",
                        "cache_control": {"type": "ephemeral"}
                    }])),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                ChatMessage {
                    role: MessageRole::Assistant,
                    content,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(Vec::new()),
                },
            ];

            let (_system, anthropic_messages) = convert_messages(&messages);
            assert!(matches!(
                &anthropic_messages[1].content,
                AnthropicMessageContent::Text(text) if text.is_empty()
            ));
        }
    }

    #[test]
    fn test_multipart_text_not_newline_joined_when_caching() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "A"},
                    {"type": "text", "text": "B"}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "anchor",
                    "cache_control": {"type": "ephemeral"}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (_system, anthropic_messages) = convert_messages(&messages);
        let AnthropicMessageContent::Blocks(blocks) = &anthropic_messages[0].content else {
            panic!("expected multipart block form while caching is enabled");
        };
        assert_eq!(blocks.len(), 2);
        let json = serde_json::to_value(&anthropic_messages[0]).unwrap();
        assert_eq!(json["content"][0]["text"], "A");
        assert_eq!(json["content"][1]["text"], "B");
    }

    #[test]
    fn test_non_caching_request_bytes_unchanged() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String("System".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "User A"},
                    {"type": "text", "text": "User B"}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some(serde_json::json!([
                    {"type": "text", "text": "Assistant A"},
                    {"type": "text", "text": "Assistant B"}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, anthropic_messages) = convert_messages(&messages);
        assert_eq!(system, Some(AnthropicSystem::Text("System".to_string())));
        assert!(matches!(
            &anthropic_messages[0].content,
            AnthropicMessageContent::Text(text) if text == "User A\nUser B"
        ));
        assert!(matches!(
            &anthropic_messages[1].content,
            AnthropicMessageContent::Text(text) if text == "Assistant A\nAssistant B"
        ));
        let json = serde_json::to_string(&(system, anthropic_messages)).unwrap();
        assert!(!json.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_system_bare_string_becomes_block_when_caching_enabled() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: Some(serde_json::Value::String("System".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "anchor",
                    "cache_control": {"type": "ephemeral"}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, _anthropic_messages) = convert_messages(&messages);
        let Some(AnthropicSystem::Blocks(blocks)) = system else {
            panic!("expected block system while caching is enabled");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "System");
        assert!(blocks[0].cache_control.is_none());
        assert_eq!(
            serde_json::to_value(&blocks).unwrap(),
            serde_json::json!([{"type": "text", "text": "System"}])
        );
    }

    #[test]
    fn test_empty_user_content_falls_back_to_string_form() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": "",
                "cache_control": {"type": "ephemeral"}
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        assert!(matches!(
            &anthropic_messages[0].content,
            AnthropicMessageContent::Text(text) if text.is_empty()
        ));
        assert_eq!(
            serde_json::to_value(&anthropic_messages[0]).unwrap()["content"],
            ""
        );
    }

    #[test]
    fn test_assistant_flattening_unchanged() {
        let assistant = ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::json!([
                {"type": "text", "text": "A"},
                {"type": "text", "text": "B"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let (_system, uncached_messages) = convert_messages(std::slice::from_ref(&assistant));
        assert!(matches!(
            &uncached_messages[0].content,
            AnthropicMessageContent::Text(text) if text == "A\nB"
        ));

        let cached_request = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "anchor",
                    "cache_control": {"type": "ephemeral"}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            assistant,
        ];
        let (_system, cached_messages) = convert_messages(&cached_request);
        let AnthropicMessageContent::Blocks(blocks) = &cached_messages[1].content else {
            panic!("expected assistant block form while caching is enabled");
        };
        let [AnthropicContentPart::Text {
            text,
            cache_control,
        }] = blocks.as_slice()
        else {
            panic!("expected one flattened assistant text block");
        };
        assert_eq!(text, "A\nB");
        assert!(cache_control.is_none());
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
    fn test_one_hour_cache_control_is_downgraded_to_five_minutes() {
        // A user text part with cache_control forces the block form (even with
        // no image). Unsupported one-hour TTLs are removed so Anthropic uses
        // its default five-minute tier, which Cloud can bill correctly.
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
                    Some(serde_json::json!({"type": "ephemeral"}))
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

        // The serialized outgoing request carries the breakpoint without 1h.
        let json = serde_json::to_string(&anthropic_messages[0]).unwrap();
        assert!(json.contains("\"cache_control\""));
        assert!(!json.contains("\"ttl\":\"1h\""));
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
        // the breakpoint moved to the final tool_use block because Anthropic
        // caches up to and including the marked block, so the whole turn must
        // be covered rather than only its leading text.
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
                assert!(
                    cache_control.is_none(),
                    "leading text must not end the cached prefix"
                );
            }
            other => panic!("expected text block first, got {other:?}"),
        }
        let serialized_blocks = serde_json::to_value(blocks).unwrap();
        assert_eq!(serialized_blocks[1]["type"], "tool_use");
        assert_eq!(
            serialized_blocks[1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn test_cache_control_on_empty_assistant_text_with_tool_calls_reaches_final_tool_use() {
        let messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": "",
                "cache_control": {"type": "ephemeral"}
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: Some("call_empty".to_string()),
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
        let AnthropicMessageContent::Blocks(blocks) = &anthropic_messages[0].content else {
            panic!("expected block form");
        };
        assert_eq!(blocks.len(), 1, "empty text must not emit a text block");
        let serialized_blocks = serde_json::to_value(blocks).unwrap();
        assert_eq!(serialized_blocks[0]["type"], "tool_use");
        assert_eq!(
            serialized_blocks[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
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
    fn test_cache_control_on_tool_message_reaches_tool_result() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": "record 101: logged.",
                "cache_control": {"type": "ephemeral"}
            }])),
            name: None,
            tool_call_id: Some("toolu_101".to_string()),
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        assert_eq!(anthropic_messages[0].role, "user");
        let AnthropicMessageContent::Blocks(blocks) = &anthropic_messages[0].content else {
            panic!("expected tool result blocks");
        };
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            AnthropicContentPart::ToolResult {
                tool_use_id,
                content,
                cache_control,
            } => {
                assert_eq!(tool_use_id, "toolu_101");
                assert_eq!(content, "record 101: logged.");
                assert_eq!(
                    *cache_control,
                    Some(serde_json::json!({"type": "ephemeral"}))
                );
            }
            other => panic!("expected tool_result block, got {other:?}"),
        }

        let json = serde_json::to_value(&anthropic_messages[0]).unwrap();
        assert_eq!(
            json["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn test_tool_message_without_cache_control_is_unchanged() {
        let messages = [
            ChatMessage {
                role: MessageRole::Tool,
                content: Some(serde_json::Value::String("bare result".to_string())),
                name: None,
                tool_call_id: Some("toolu_bare".to_string()),
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::Tool,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": "parts result"
                }])),
                name: None,
                tool_call_id: Some("toolu_parts".to_string()),
                tool_calls: None,
            },
        ];

        let (_system, anthropic_messages) = convert_messages(&messages);
        for message in &anthropic_messages {
            let AnthropicMessageContent::Blocks(blocks) = &message.content else {
                panic!("expected tool result blocks");
            };
            let [AnthropicContentPart::ToolResult { cache_control, .. }] = blocks.as_slice() else {
                panic!("expected one tool result block");
            };
            assert!(cache_control.is_none());
            let json = serde_json::to_value(message).unwrap();
            assert!(json["content"][0].get("cache_control").is_none());
        }
    }

    #[test]
    fn test_tool_message_last_breakpoint_wins() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: Some(serde_json::json!([
                {
                    "type": "text",
                    "text": "first",
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "type": "text",
                    "text": "second",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ])),
            name: None,
            tool_call_id: Some("toolu_last".to_string()),
            tool_calls: None,
        }];

        let (_system, anthropic_messages) = convert_messages(&messages);
        let AnthropicMessageContent::Blocks(blocks) = &anthropic_messages[0].content else {
            panic!("expected tool result blocks");
        };
        let [AnthropicContentPart::ToolResult { cache_control, .. }] = blocks.as_slice() else {
            panic!("expected one tool result block");
        };
        assert_eq!(
            *cache_control,
            Some(serde_json::json!({"type": "ephemeral"}))
        );
        let json = serde_json::to_value(&anthropic_messages[0]).unwrap();
        assert_eq!(
            json["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn test_breakpoint_cap_keeps_first_and_last_three() {
        let mut messages = vec![ChatMessage {
            role: MessageRole::System,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": "system",
                "cache_control": {"type": "ephemeral", "label": "system"}
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        messages.extend((1..=5).map(|index| ChatMessage {
            role: MessageRole::User,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": format!("user {index}"),
                "cache_control": {"type": "ephemeral", "label": format!("user-{index}")}
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }));

        let (system, anthropic_messages) = convert_messages(&messages);
        let system = system.expect("system should be present");
        let AnthropicSystem::Blocks(system_blocks) = &system else {
            panic!("expected system blocks");
        };
        assert_eq!(
            system_blocks[0].cache_control.as_ref().unwrap()["label"],
            "system"
        );

        let serialized_messages = serde_json::to_value(&anthropic_messages).unwrap();
        let labels = serialized_messages
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"][0].get("cache_control"))
            .map(|cache_control| cache_control["label"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["user-3", "user-4", "user-5"]);
        assert_eq!(
            serde_json::to_value(&system).unwrap()[0]["cache_control"]["label"],
            "system"
        );
    }

    #[test]
    fn test_breakpoint_cap_no_op_at_or_below_limit() {
        let messages = (1..=4)
            .map(|index| ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": format!("user {index}"),
                    "cache_control": {"type": "ephemeral", "label": format!("user-{index}")}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect::<Vec<_>>();

        let (_system, anthropic_messages) = convert_messages(&messages);
        for (index, message) in anthropic_messages.iter().enumerate() {
            let AnthropicMessageContent::Blocks(blocks) = &message.content else {
                panic!("expected user blocks");
            };
            let AnthropicContentPart::Text { cache_control, .. } = &blocks[0] else {
                panic!("expected text block");
            };
            assert_eq!(
                cache_control.as_ref().unwrap()["label"],
                format!("user-{}", index + 1)
            );
        }
        let serialized = serde_json::to_string(&anthropic_messages).unwrap();
        assert_eq!(serialized.matches("cache_control").count(), 4);
    }

    #[test]
    fn test_breakpoint_cap_counts_tool_results() {
        let mut messages = (1..=4)
            .map(|index| ChatMessage {
                role: MessageRole::User,
                content: Some(serde_json::json!([{
                    "type": "text",
                    "text": format!("user {index}"),
                    "cache_control": {"type": "ephemeral", "label": format!("user-{index}")}
                }])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect::<Vec<_>>();
        messages.push(ChatMessage {
            role: MessageRole::Tool,
            content: Some(serde_json::json!([{
                "type": "text",
                "text": "tool result",
                "cache_control": {"type": "ephemeral", "label": "tool"}
            }])),
            name: None,
            tool_call_id: Some("toolu_cap".to_string()),
            tool_calls: None,
        });

        let (_system, anthropic_messages) = convert_messages(&messages);
        let AnthropicMessageContent::Blocks(tool_blocks) = &anthropic_messages[4].content else {
            panic!("expected tool result blocks");
        };
        let [AnthropicContentPart::ToolResult { cache_control, .. }] = tool_blocks.as_slice()
        else {
            panic!("expected one tool result block");
        };
        assert_eq!(
            cache_control.as_ref().unwrap()["label"],
            serde_json::json!("tool")
        );
        let serialized = serde_json::to_value(&anthropic_messages).unwrap();
        let labels = serialized
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"][0].get("cache_control"))
            .map(|cache_control| cache_control["label"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["user-1", "user-3", "user-4", "tool"]);
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
        assert_eq!(usage.cache_creation_tokens(), 5);
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
        assert_eq!(usage.cache_creation_tokens(), 5);
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
