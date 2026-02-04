//! Tool Configuration Helpers
//!
//! This module provides functions for converting request tool configurations
//! to the inference provider format used by the LLM, and parsing tool calls
//! from LLM responses.

use crate::responses::models::{CreateResponseRequest, ResponseTool, ResponseToolChoice};
use crate::responses::service_helpers::ToolCallInfo;

/// Special tool type for error tool calls (malformed calls from LLM)
pub const ERROR_TOOL_TYPE: &str = "__error__";

/// Tool name for code interpreter
pub const CODE_INTERPRETER_TOOL_NAME: &str = "code_interpreter";

/// Tool name for computer use
pub const COMPUTER_TOOL_NAME: &str = "computer";

/// Extract tool names from request tools configuration
///
/// Returns a list of tool names that can be used to infer tool names when
/// the model doesn't provide one (e.g., GLM-4.6 intermittently omits function.name).
pub fn get_tool_names(request: &CreateResponseRequest) -> Vec<String> {
    let mut names = Vec::new();

    if let Some(tools) = &request.tools {
        for tool in tools {
            match tool {
                ResponseTool::WebSearch { .. } => {
                    names.push(super::WEB_SEARCH_TOOL_NAME.to_string());
                }
                ResponseTool::FileSearch {} => {
                    names.push(super::FILE_SEARCH_TOOL_NAME.to_string());
                }
                ResponseTool::Function { name, .. } => {
                    names.push(name.clone());
                }
                ResponseTool::CodeInterpreter {} => {
                    names.push(CODE_INTERPRETER_TOOL_NAME.to_string());
                }
                ResponseTool::Computer {} => {
                    names.push(COMPUTER_TOOL_NAME.to_string());
                }
                // MCP tools are dynamically discovered - we don't know actual tool names
                // until the MCP server is queried asynchronously later
                ResponseTool::Mcp { .. } => {}
            }
        }
    }

    names
}

/// Prepare tools configuration for LLM in OpenAI function calling format
pub fn prepare_tools(request: &CreateResponseRequest) -> Vec<inference_providers::ToolDefinition> {
    let mut tool_definitions = Vec::new();

    if let Some(tools) = &request.tools {
        for tool in tools {
            match tool {
                ResponseTool::WebSearch { .. } => {
                    tool_definitions.push(inference_providers::ToolDefinition {
                        type_: "function".to_string(),
                        function: inference_providers::FunctionDefinition {
                            name: super::WEB_SEARCH_TOOL_NAME.to_string(),
                            description: Some(
                                "Search the web for current information. Use this when you need up-to-date information or facts that you don't have. \
                                \n\nIMPORTANT PARAMETERS TO CONSIDER:\
                                \n- Use 'freshness' for time-sensitive queries (news, recent events, current trends)\
                                \n- Use 'country' for location-specific information\
                                \n- Use 'count' to limit results when user asks for specific number\
                                \n- Use 'safesearch' when dealing with sensitive topics".to_string()
                            ),
                            parameters: serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "query": {
                                        "type": "string",
                                        "description": "The search query to look up (required, max 400 characters)"
                                    },
                                    "country": {
                                        "type": "string",
                                        "description": "2-character country code where results come from (e.g., 'US', 'GB', 'DE'). Use when user asks about location-specific information or mentions a country."
                                    },
                                    "search_lang": {
                                        "type": "string",
                                        "description": "2+ character language code for search results (e.g., 'en', 'es', 'de'). Use when user's query or language preference suggests non-English results."
                                    },
                                    "ui_lang": {
                                        "type": "string",
                                        "description": "User interface language (e.g., 'en-US', 'es-ES')"
                                    },
                                    "count": {
                                        "type": "integer",
                                        "description": "Number of search results to return (1-20, default: 20). Use lower values (5-10) for focused queries, higher values (15-20) for comprehensive research.",
                                        "minimum": 1,
                                        "maximum": 20
                                    },
                                    "offset": {
                                        "type": "integer",
                                        "description": "Zero-based offset for pagination (0-9)",
                                        "minimum": 0,
                                        "maximum": 9
                                    },
                                    "safesearch": {
                                        "type": "string",
                                        "description": "Safe search filter: 'strict' for educational/family content, 'moderate' (default) for general use, 'off' only when explicitly needed",
                                        "enum": ["off", "moderate", "strict"]
                                    },
                                    "freshness": {
                                        "type": "string",
                                        "description": "Filter by freshness: 'pd' (24h) for breaking news, 'pw' (7d) for recent events, 'pm' (31d) for current trends, 'py' (365d) for recent developments. Always use for: news, current events, latest updates, recent changes, today's info."
                                    },
                                    "text_decorations": {
                                        "type": "boolean",
                                        "description": "Include text highlighting markers (default: true)"
                                    },
                                    "spellcheck": {
                                        "type": "boolean",
                                        "description": "Enable spellcheck on query (default: true)"
                                    },
                                    "units": {
                                        "type": "string",
                                        "description": "Measurement units: 'metric' or 'imperial'",
                                        "enum": ["metric", "imperial"]
                                    },
                                    "extra_snippets": {
                                        "type": "boolean",
                                        "description": "Get up to 5 additional alternative excerpts (requires AI/Data plan)"
                                    },
                                    "summary": {
                                        "type": "boolean",
                                        "description": "Enable summary key generation (requires AI/Data plan)"
                                    }
                                },
                                "required": ["query"]
                            }),
                        },
                    });
                }
                ResponseTool::FileSearch {} => {
                    tool_definitions.push(inference_providers::ToolDefinition {
                        type_: "function".to_string(),
                        function: inference_providers::FunctionDefinition {
                            name: "file_search".to_string(),
                            description: Some(
                                "Search through files in the current conversation. Use this to find information from uploaded documents or previous file content.".to_string()
                            ),
                            parameters: serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "query": {
                                        "type": "string",
                                        "description": "The search query to look up in files"
                                    }
                                },
                                "required": ["query"]
                            }),
                        },
                    });
                }
                ResponseTool::Function {
                    name,
                    description,
                    parameters,
                } => {
                    tool_definitions.push(inference_providers::ToolDefinition {
                        type_: "function".to_string(),
                        function: inference_providers::FunctionDefinition {
                            name: name.clone(),
                            description: description.clone(),
                            parameters: parameters.clone().unwrap_or_else(|| {
                                serde_json::json!({
                                    "type": "object",
                                    "properties": {}
                                })
                            }),
                        },
                    });
                }
                ResponseTool::CodeInterpreter {} => {
                    tool_definitions.push(inference_providers::ToolDefinition {
                        type_: "function".to_string(),
                        function: inference_providers::FunctionDefinition {
                            name: CODE_INTERPRETER_TOOL_NAME.to_string(),
                            description: Some(
                                "Execute Python code in a sandboxed environment.".to_string(),
                            ),
                            parameters: serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "code": {
                                        "type": "string",
                                        "description": "Python code to execute"
                                    }
                                },
                                "required": ["code"]
                            }),
                        },
                    });
                }
                ResponseTool::Computer {} => {
                    tool_definitions.push(inference_providers::ToolDefinition {
                        type_: "function".to_string(),
                        function: inference_providers::FunctionDefinition {
                            name: COMPUTER_TOOL_NAME.to_string(),
                            description: Some(
                                "Control computer actions like mouse clicks and keyboard input."
                                    .to_string(),
                            ),
                            parameters: serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "action": {
                                        "type": "string",
                                        "description": "The action to perform"
                                    }
                                },
                                "required": ["action"]
                            }),
                        },
                    });
                }
                ResponseTool::Mcp { .. } => {
                    // MCP tools are handled separately via McpToolExecutor
                    // Tool definitions are dynamically discovered from the MCP server
                    // and added to the request in process_response_stream
                }
            }
        }
    }

    tool_definitions
}

/// Prepare tool choice configuration for LLM
pub fn prepare_tool_choice(
    request: &CreateResponseRequest,
) -> Option<inference_providers::ToolChoice> {
    request.tool_choice.as_ref().map(|choice| match choice {
        ResponseToolChoice::Auto(s) => inference_providers::ToolChoice::String(s.clone()),
        ResponseToolChoice::Specific { type_, function } => {
            inference_providers::ToolChoice::Function {
                type_: type_.clone(),
                function: inference_providers::FunctionChoice {
                    name: function.name.clone(),
                },
            }
        }
    })
}

/// Maximum recursion depth for JSON repair to prevent stack overflow
const MAX_REPAIR_DEPTH: u8 = 2;

/// Attempt to repair malformed JSON from LLM tool calls
///
/// Handles common failure modes:
/// - Truncated JSON: `{"query": "test", "count` -> `{"query": "test"}`
/// - Missing opening brace: `": "value"...` -> attempt to extract query
/// - Duplicated/nested JSON: `{"query": "a", "x":{"query": "a"}` -> use first complete object
/// - Unclosed strings: `{"query": "test` -> `{"query": "test"}`
///
/// Returns the repaired JSON string if successful, or None if repair failed.
///
/// # Performance
///
/// The progressive truncation loop has O(N²) time complexity where N is the input length,
/// as each iteration may call try_close_json (O(N)) and serde_json::from_str (O(N)).
/// This is acceptable for typical tool call arguments which are small (< 1KB).
fn try_repair_json(json_str: &str) -> Option<String> {
    try_repair_json_inner(json_str, 0)
}

/// Inner implementation with depth tracking to prevent unbounded recursion
fn try_repair_json_inner(json_str: &str, depth: u8) -> Option<String> {
    if depth > MAX_REPAIR_DEPTH {
        return None;
    }

    let trimmed = json_str.trim();

    if trimmed.is_empty() {
        return None;
    }

    // Already valid JSON - no repair needed
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Handle missing opening brace (stream started mid-JSON)
    if !trimmed.starts_with('{') {
        // Try to find if this looks like a partial JSON value
        // Common pattern: `": "some value", "key": ...`
        if trimmed.starts_with("\":") || trimmed.starts_with("\": ") {
            let with_prefix = format!("{{\"query{}", trimmed);
            if let Some(repaired) = try_repair_json_inner(&with_prefix, depth + 1) {
                return Some(repaired);
            }
        }
        // Can't repair without opening brace
        return None;
    }

    // Handle duplicated/nested JSON objects
    // e.g., `{"query": "a", "search_lang":{"query": "a", ...}`
    // Find the first complete JSON object by tracking brace depth
    if let Some(first_obj_end) = find_first_complete_object(trimmed) {
        // Bounds check before slicing
        if first_obj_end < trimmed.len() {
            let first_obj = &trimmed[..=first_obj_end];
            if serde_json::from_str::<serde_json::Value>(first_obj).is_ok() {
                return Some(first_obj.to_string());
            }
        }
    }

    // Progressive truncation loop: O(N²) complexity - see Performance note above.
    // Try progressively removing characters from the end and closing the JSON.
    // This handles truncated JSON like: `{"query": "test", "freshness":`
    let mut working = trimmed.to_string();

    for _ in 0..working.len() {
        // Try closing the JSON at current position
        if let Some(closed) = try_close_json(&working) {
            if serde_json::from_str::<serde_json::Value>(&closed).is_ok() {
                return Some(closed);
            }
        }

        // Remove last character and try again
        working.pop();

        // Stop if we've removed too much
        if working.len() < 2 {
            break;
        }
    }

    None
}

/// Find the end index of the first complete JSON object in a string
fn find_first_complete_object(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in s.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }

    None
}

/// Attempt to close incomplete JSON by adding missing quotes, brackets, and braces
fn try_close_json(s: &str) -> Option<String> {
    let mut result = s.to_string();

    // Count unclosed structures
    let mut brace_count = 0i32;
    let mut bracket_count = 0i32;
    let mut in_string = false;
    let mut escape_next = false;

    for ch in s.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => brace_count += 1,
            '}' if !in_string => brace_count -= 1,
            '[' if !in_string => bracket_count += 1,
            ']' if !in_string => bracket_count -= 1,
            _ => {}
        }
    }

    // If we're inside a string, close it
    if in_string {
        result.push('"');
    }

    // Add missing closing brackets
    for _ in 0..bracket_count {
        result.push(']');
    }

    // Add missing closing braces
    for _ in 0..brace_count {
        result.push('}');
    }

    Some(result)
}

/// Result of parsing tool call arguments
enum ParseArgsResult {
    /// Successfully parsed (or repaired) JSON
    Ok(serde_json::Value),
    /// Successfully repaired malformed JSON
    Repaired(serde_json::Value),
    /// Failed to parse or repair
    Failed,
}

/// Parse tool call arguments JSON, with optional repair for web_search
///
/// For web_search tools, attempts to repair malformed JSON from truncated streams.
/// For other tools, returns Failed if JSON is invalid.
fn parse_tool_args(tool_name: &str, args_str: &str) -> ParseArgsResult {
    // Try direct parsing first
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
        return ParseArgsResult::Ok(args);
    }

    // Only attempt repair for web_search
    if tool_name != super::WEB_SEARCH_TOOL_NAME {
        return ParseArgsResult::Failed;
    }

    // Attempt to repair malformed JSON
    match try_repair_json(args_str) {
        Some(repaired) => match serde_json::from_str::<serde_json::Value>(&repaired) {
            Ok(args) => ParseArgsResult::Repaired(args),
            Err(_) => ParseArgsResult::Failed,
        },
        None => ParseArgsResult::Failed,
    }
}

/// Create an error ToolCallInfo for invalid JSON arguments
fn create_invalid_json_error(
    name: &str,
    idx: i64,
    id: Option<String>,
    thought_signature: Option<String>,
) -> ToolCallInfo {
    ToolCallInfo {
        id,
        tool_type: ERROR_TOOL_TYPE.to_string(),
        query: format!(
            "Tool call for '{name}' (index {idx}) has invalid JSON arguments. \
             Please ensure arguments are valid JSON."
        ),
        params: Some(serde_json::json!({
            "error_type": "invalid_json",
            "tool_name": name,
            "index": idx
        })),
        thought_signature,
    }
}

/// Create an error ToolCallInfo for missing query field
fn create_missing_query_error(
    name: &str,
    idx: i64,
    id: Option<String>,
    thought_signature: Option<String>,
) -> ToolCallInfo {
    ToolCallInfo {
        id,
        tool_type: ERROR_TOOL_TYPE.to_string(),
        query: format!(
            "Tool call for '{name}' (index {idx}) is missing the required 'query' field \
             in its arguments. Please include a 'query' parameter."
        ),
        params: Some(serde_json::json!({
            "error_type": "missing_query_field",
            "tool_name": name,
            "index": idx
        })),
        thought_signature,
    }
}

/// Convert accumulated tool calls from LLM response to ToolCallInfo
///
/// Takes a map of tool call index -> (id, name, arguments_json) and converts
/// them to structured ToolCallInfo. Invalid or malformed tool calls are
/// converted to error tool calls that inform the LLM of the issue.
///
/// The `model` parameter is used for logging to help identify which models
/// produce malformed tool calls.
///
/// The `available_tool_names` parameter is used to infer the tool name when
/// the model doesn't provide one (e.g., GLM-4.6 intermittently omits function.name).
/// If only one tool is available and the name is missing, we default to that tool.
pub fn convert_tool_calls(
    tool_call_accumulator: crate::responses::service_helpers::ToolCallAccumulator,
    model: &str,
    available_tool_names: &[String],
) -> Vec<ToolCallInfo> {
    let mut tool_calls_detected = Vec::new();

    for (idx, entry) in tool_call_accumulator {
        let id_opt = entry.id;
        let args_str = entry.arguments;
        let thought_signature = entry.thought_signature;

        // Check if name is None or empty string
        let name = match entry.name {
            Some(n) if !n.trim().is_empty() => n,
            _ => {
                // Fallback: if only one tool is available, use that tool's name
                // This handles models like GLM-4.6 that intermittently omit function.name
                if available_tool_names.len() == 1 {
                    let inferred_name = available_tool_names[0].clone();
                    tracing::info!(
                        model = model,
                        index = idx,
                        inferred_tool = %inferred_name,
                        "Tool call missing name, inferring from single available tool"
                    );
                    inferred_name
                } else {
                    tracing::warn!(
                        model = model,
                        index = idx,
                        available_tools = ?available_tool_names,
                        "Tool call has no name and multiple tools available, cannot infer"
                    );
                    // Log args at debug level for staging troubleshooting (not in prod)
                    tracing::debug!(
                        model = model,
                        index = idx,
                        args = args_str,
                        "Tool call missing name - arguments for debugging"
                    );
                    tool_calls_detected.push(ToolCallInfo {
                        id: id_opt,
                        tool_type: ERROR_TOOL_TYPE.to_string(),
                        query: format!(
                            "Tool call at index {idx} is missing a tool name. \
                             Please ensure all tool calls include a valid 'name' field. \
                             Arguments provided: {args_str}"
                        ),
                        params: Some(serde_json::json!({
                            "error_type": "missing_tool_name",
                            "index": idx,
                            "arguments": args_str
                        })),
                        thought_signature,
                    });
                    continue;
                }
            }
        };

        // Parse arguments, with repair for web_search if needed
        let args = match parse_tool_args(&name, &args_str) {
            ParseArgsResult::Ok(args) => args,
            ParseArgsResult::Repaired(args) => {
                tracing::info!(
                    model = model,
                    tool_name = name,
                    index = idx,
                    "Repaired malformed web_search tool call JSON"
                );
                args
            }
            ParseArgsResult::Failed => {
                tracing::warn!(
                    model = model,
                    tool_name = name,
                    index = idx,
                    "Failed to parse tool call arguments"
                );
                // Log args at debug level for staging troubleshooting (not in prod)
                tracing::debug!(
                    model = model,
                    tool_name = name,
                    index = idx,
                    args = args_str,
                    "Failed to parse tool call - arguments for debugging"
                );
                tool_calls_detected.push(create_invalid_json_error(
                    &name,
                    idx,
                    id_opt,
                    thought_signature,
                ));
                continue;
            }
        };

        // Check if this is a tool that doesn't use the 'query' parameter:
        // - MCP tools (format: "server_label:tool_name")
        // - code_interpreter (uses "code" parameter)
        // - computer (uses "action" parameter)
        if name.contains(':') || name == CODE_INTERPRETER_TOOL_NAME || name == COMPUTER_TOOL_NAME {
            tracing::debug!("Tool call detected (no query required): {}", name);
            tool_calls_detected.push(ToolCallInfo {
                id: id_opt,
                tool_type: name,
                query: String::new(),
                params: Some(args),
                thought_signature,
            });
        } else if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
            tracing::debug!("Tool call detected: {}", name);
            tool_calls_detected.push(ToolCallInfo {
                id: id_opt,
                tool_type: name,
                query: query.to_string(),
                params: Some(args),
                thought_signature,
            });
        } else {
            tracing::warn!(
                model = model,
                tool_name = name,
                index = idx,
                "Tool call has no 'query' field in arguments"
            );
            tool_calls_detected.push(create_missing_query_error(
                &name,
                idx,
                id_opt,
                thought_signature,
            ));
        }
    }

    tool_calls_detected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::service_helpers::ToolCallAccumulatorEntry;
    use std::collections::HashMap;

    #[test]
    fn test_try_repair_json_valid_json() {
        let input = r#"{"query": "test", "count": 10}"#;
        let result = try_repair_json(input);
        assert_eq!(result, Some(input.to_string()));
    }

    #[test]
    fn test_try_repair_json_truncated_at_end() {
        // Truncated JSON missing closing brace after key
        let input = r#"{"query": "test search query", "freshness":"#;
        let result = try_repair_json(input);
        assert!(result.is_some());
        let repaired = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(
            parsed.get("query").and_then(|v| v.as_str()),
            Some("test search query")
        );
    }

    #[test]
    fn test_try_repair_json_truncated_string_value() {
        // Truncated in the middle of a key name
        let input =
            r#"{"query": "test query with multiple words", "freshness": "pm", "country": "US", ""#;
        let result = try_repair_json(input);
        assert!(result.is_some());
        let repaired = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert!(parsed.get("query").is_some());
        assert_eq!(parsed.get("country").and_then(|v| v.as_str()), Some("US"));
    }

    #[test]
    fn test_try_repair_json_truncated_key() {
        // Truncated after a key name
        let input = r#"{"query": "example search", "freshness": "pd", "count"#;
        let result = try_repair_json(input);
        assert!(result.is_some());
        let repaired = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert!(parsed.get("query").is_some());
        assert_eq!(parsed.get("freshness").and_then(|v| v.as_str()), Some("pd"));
    }

    #[test]
    fn test_try_repair_json_duplicated_nested() {
        // Duplicated/nested JSON where the LLM repeated itself
        let input = r#"{"query": "test query", "freshness": "pw", "country": "US", "count": 10, "search_lang":{"query": "test query", "freshness": "pw", "country": "US", "count": 10}"#;
        let result = try_repair_json(input);
        assert!(result.is_some());
        let repaired = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(
            parsed.get("query").and_then(|v| v.as_str()),
            Some("test query")
        );
    }

    #[test]
    fn test_try_repair_json_missing_opening_brace() {
        // Stream started mid-JSON (missing opening brace)
        let input = r#"": "test search value", "count": 10, "freshness": "pw", "safesearch"#;
        let result = try_repair_json(input);
        // This case prepends {"query and attempts repair
        assert!(result.is_some());
        let repaired = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert!(parsed.get("query").is_some());
    }

    #[test]
    fn test_try_repair_json_empty_string() {
        let result = try_repair_json("");
        assert!(result.is_none());
    }

    #[test]
    fn test_try_repair_json_whitespace_only() {
        let result = try_repair_json("   ");
        assert!(result.is_none());
    }

    #[test]
    fn test_try_repair_json_not_json() {
        // Completely invalid - not JSON at all
        let result = try_repair_json("this is not json at all");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_first_complete_object() {
        let input = r#"{"a": 1}{"b": 2}"#;
        let end = find_first_complete_object(input);
        assert_eq!(end, Some(7)); // Index of first closing brace
        assert_eq!(&input[..=7], r#"{"a": 1}"#);
    }

    #[test]
    fn test_find_first_complete_object_nested() {
        let input = r#"{"a": {"b": 1}, "c": 2}extra"#;
        let end = find_first_complete_object(input);
        assert_eq!(end, Some(22));
        assert_eq!(&input[..=22], r#"{"a": {"b": 1}, "c": 2}"#);
    }

    #[test]
    fn test_find_first_complete_object_with_escaped_quotes() {
        let input = r#"{"a": "test \"quoted\" value"}"#;
        let end = find_first_complete_object(input);
        assert_eq!(end, Some(29));
    }

    #[test]
    fn test_try_close_json_unclosed_string() {
        let input = r#"{"query": "test"#;
        let result = try_close_json(input);
        assert!(result.is_some());
        let closed = result.unwrap();
        assert!(closed.ends_with("\"}"));
    }

    #[test]
    fn test_try_close_json_unclosed_brace() {
        let input = r#"{"query": "test""#;
        let result = try_close_json(input);
        assert!(result.is_some());
        let closed = result.unwrap();
        assert!(closed.ends_with("}"));
    }

    #[test]
    fn test_convert_tool_calls_with_valid_json() {
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("call_123".to_string()),
                name: Some("web_search".to_string()),
                arguments: r#"{"query": "test search"}"#.to_string(),
                thought_signature: None,
            },
        );

        let result = convert_tool_calls(accumulator, "test-model", &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, Some("call_123".to_string()));
        assert_eq!(result[0].tool_type, "web_search");
        assert_eq!(result[0].query, "test search");
    }

    #[test]
    fn test_convert_tool_calls_repairs_truncated_json() {
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("call_456".to_string()),
                name: Some("web_search".to_string()),
                arguments: r#"{"query": "Bitcoin price", "freshness":"#.to_string(),
                thought_signature: None,
            },
        );

        let result = convert_tool_calls(accumulator, "test-model", &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, Some("call_456".to_string()));
        assert_eq!(result[0].tool_type, "web_search");
        assert_eq!(result[0].query, "Bitcoin price");
    }

    #[test]
    fn test_convert_tool_calls_missing_name_with_multiple_tools() {
        // When name is missing and multiple tools available, should return error
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("call_789".to_string()),
                name: None,
                arguments: r#"{"query": "test"}"#.to_string(),
                thought_signature: None,
            },
        );

        let available_tools = vec!["web_search".to_string(), "file_search".to_string()];
        let result = convert_tool_calls(accumulator, "test-model", &available_tools);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tool_type, ERROR_TOOL_TYPE);
        assert_eq!(result[0].id, Some("call_789".to_string())); // ID preserved even in error
    }

    #[test]
    fn test_convert_tool_calls_missing_name_infers_single_tool() {
        // When name is missing but only one tool available, should infer the tool name
        // This handles models like GLM-4.6 that intermittently omit function.name
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("call_abc".to_string()),
                name: None,
                arguments: r#"{"query": "Bitcoin price"}"#.to_string(),
                thought_signature: None,
            },
        );

        let available_tools = vec!["web_search".to_string()];
        let result = convert_tool_calls(accumulator, "test-model", &available_tools);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, Some("call_abc".to_string()));
        assert_eq!(result[0].tool_type, "web_search");
        assert_eq!(result[0].query, "Bitcoin price");
    }

    #[test]
    fn test_convert_tool_calls_missing_name_no_tools_available() {
        // When name is missing and no tools available, should return error
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: None,
                name: None,
                arguments: r#"{"query": "test"}"#.to_string(),
                thought_signature: None,
            },
        );

        let result = convert_tool_calls(accumulator, "test-model", &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tool_type, ERROR_TOOL_TYPE);
    }

    #[test]
    fn test_convert_tool_calls_mcp_tool() {
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("toolu_xyz".to_string()),
                name: Some("server:tool_name".to_string()),
                arguments: r#"{"param1": "value1"}"#.to_string(),
                thought_signature: None,
            },
        );

        let result = convert_tool_calls(accumulator, "test-model", &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, Some("toolu_xyz".to_string()));
        assert_eq!(result[0].tool_type, "server:tool_name");
        assert!(result[0].query.is_empty()); // MCP tools don't require query
    }

    #[test]
    fn test_convert_tool_calls_no_repair_for_non_web_search() {
        // Verify that JSON repair is NOT attempted for non-web_search tools
        let mut accumulator = HashMap::new();
        accumulator.insert(
            0,
            ToolCallAccumulatorEntry {
                id: Some("call_err".to_string()),
                name: Some("file_search".to_string()),
                arguments: r#"{"query": "test", "truncated":"#.to_string(),
                thought_signature: None,
            },
        );

        let result = convert_tool_calls(accumulator, "test-model", &[]);
        assert_eq!(result.len(), 1);
        // Should return an error, not attempt repair
        assert_eq!(result[0].tool_type, ERROR_TOOL_TYPE);
        assert_eq!(result[0].id, Some("call_err".to_string())); // ID preserved in error
    }
}
