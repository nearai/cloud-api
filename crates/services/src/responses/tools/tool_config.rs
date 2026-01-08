//! Tool Configuration Helpers
//!
//! This module provides functions for converting request tool configurations
//! to the inference provider format used by the LLM, and parsing tool calls
//! from LLM responses.

use std::collections::HashMap;

use crate::responses::models::{CreateResponseRequest, ResponseTool, ResponseToolChoice};
use crate::responses::service_helpers::ToolCallInfo;

/// Special tool type for error tool calls (malformed calls from LLM)
pub const ERROR_TOOL_TYPE: &str = "__error__";

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
                            name: "code_interpreter".to_string(),
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
                            name: "computer".to_string(),
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

/// Convert accumulated tool calls from LLM response to ToolCallInfo
///
/// Takes a map of tool call index -> (name, arguments_json) and converts
/// them to structured ToolCallInfo. Invalid or malformed tool calls are
/// converted to error tool calls that inform the LLM of the issue.
pub fn convert_tool_calls(
    tool_call_accumulator: HashMap<i64, (Option<String>, String)>,
) -> Vec<ToolCallInfo> {
    let mut tool_calls_detected = Vec::new();

    for (idx, (name_opt, args_str)) in tool_call_accumulator {
        // Check if name is None or empty string
        let name = match name_opt {
            Some(n) if !n.trim().is_empty() => n,
            _ => {
                tracing::warn!(
                    "Tool call at index {} has no name or empty name. Args: {}",
                    idx,
                    args_str
                );
                // Create a special error tool call to inform the LLM
                tool_calls_detected.push(ToolCallInfo {
                    tool_type: ERROR_TOOL_TYPE.to_string(),
                    query: format!(
                        "Tool call at index {idx} is missing a tool name. Please ensure all tool calls include a valid 'name' field. Arguments provided: {args_str}"
                    ),
                    params: Some(serde_json::json!({
                        "error_type": "missing_tool_name",
                        "index": idx,
                        "arguments": args_str
                    })),
                });
                continue;
            }
        };

        // Try to parse the complete arguments for tools that need parameters
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_str) {
            // Check if this is an MCP tool (format: "server_label:tool_name")
            if name.contains(':') {
                // MCP tools use the full arguments JSON as params
                tracing::debug!(
                    "MCP tool call detected: {} with arguments: {:?}",
                    name,
                    args
                );
                tool_calls_detected.push(ToolCallInfo {
                    tool_type: name,
                    query: String::new(),
                    params: Some(args),
                });
            } else if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
                // Non-MCP tools (web_search, file_search) require a "query" field
                tracing::debug!(
                    "Tool call detected: {} with query: {} and params: {:?}",
                    name,
                    query,
                    args
                );
                tool_calls_detected.push(ToolCallInfo {
                    tool_type: name,
                    query: query.to_string(),
                    params: Some(args),
                });
            } else {
                tracing::warn!(
                    "Tool call {} (index {}) has no 'query' field in arguments: {}",
                    name,
                    idx,
                    args_str
                );
                // Create an error tool call to inform the LLM about missing query
                tool_calls_detected.push(ToolCallInfo {
                    tool_type: ERROR_TOOL_TYPE.to_string(),
                    query: format!(
                        "Tool call for '{name}' (index {idx}) is missing the required 'query' field in its arguments. Please include a 'query' parameter. Arguments provided: {args_str}"
                    ),
                    params: Some(serde_json::json!({
                        "error_type": "missing_query_field",
                        "tool_name": name,
                        "index": idx,
                        "arguments": args_str
                    })),
                });
            }
        } else {
            tracing::warn!(
                "Failed to parse tool call {} (index {}) arguments: {}",
                name,
                idx,
                args_str
            );
            // Create an error tool call to inform the LLM about invalid JSON
            tool_calls_detected.push(ToolCallInfo {
                tool_type: ERROR_TOOL_TYPE.to_string(),
                query: format!(
                    "Tool call for '{name}' (index {idx}) has invalid JSON arguments. Please ensure arguments are valid JSON. Arguments provided: {args_str}"
                ),
                params: Some(serde_json::json!({
                    "error_type": "invalid_json",
                    "tool_name": name,
                    "index": idx,
                    "arguments": args_str
                })),
            });
        }
    }

    tool_calls_detected
}
