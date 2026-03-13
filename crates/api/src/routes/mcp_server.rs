use crate::middleware::auth::AuthenticatedApiKey;
use crate::middleware::usage::{check_usage_for_api_key, UsageState};
use crate::models::ErrorResponse;
use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use services::web_search::{
    WebSearchRequest, WebSearchService, WebSearchServiceError, WebSearchUsageContext,
};
use std::sync::Arc;
use uuid::Uuid;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_SERVER_VERSION: &str = "1.0.0";
const WEB_SEARCH_TOOL_NAME: &str = "web_search";
const WEB_SEARCH_TOOL_DESCRIPTION: &str = "Search the web and return structured search results.";

struct McpToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
}

const MCP_TOOLS: &[McpToolDefinition] = &[McpToolDefinition {
    name: WEB_SEARCH_TOOL_NAME,
    description: WEB_SEARCH_TOOL_DESCRIPTION,
    input_schema: web_search_input_schema,
}];

#[derive(Clone)]
pub struct McpRouteState {
    pub web_search_service: Arc<WebSearchService>,
    pub usage_state: UsageState,
}

#[derive(Debug, Deserialize)]
pub struct McpRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct McpResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpErrorBody>,
}

#[derive(Debug, Serialize)]
pub struct McpErrorBody {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpWebSearchArgs {
    query: String,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    search_lang: Option<String>,
    #[serde(default)]
    ui_lang: Option<String>,
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    safesearch: Option<String>,
    #[serde(default)]
    freshness: Option<String>,
    #[serde(default)]
    text_decorations: Option<bool>,
    #[serde(default)]
    spellcheck: Option<bool>,
    #[serde(default)]
    units: Option<String>,
    #[serde(default)]
    extra_snippets: Option<bool>,
    #[serde(default)]
    summary: Option<bool>,
    #[serde(default)]
    result_filter: Option<String>,
    #[serde(default)]
    goggles: Option<String>,
    #[serde(default)]
    enable_rich_callback: Option<bool>,
    #[serde(default)]
    include_fetch_metadata: Option<bool>,
    #[serde(default)]
    operators: Option<bool>,
}

fn ok_response(id: Value, result: Value) -> ResponseJson<McpResponse> {
    ResponseJson(McpResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> ResponseJson<McpResponse> {
    ResponseJson(McpResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(McpErrorBody {
            code,
            message: message.into(),
        }),
    })
}

fn map_mcp_service_error(id: Value, error: WebSearchServiceError) -> ResponseJson<McpResponse> {
    match error {
        WebSearchServiceError::EmptyQuery => error_response(
            id,
            -32602,
            "Tool argument 'query' is required and cannot be empty",
        ),
        WebSearchServiceError::CountOutOfRange => {
            error_response(id, -32602, "Tool argument 'count' must be between 1 and 20")
        }
        WebSearchServiceError::OffsetOutOfRange => {
            error_response(id, -32602, "Tool argument 'offset' must be between 0 and 9")
        }
        WebSearchServiceError::NotConfigured => {
            error_response(id, -32001, "Web search is not configured")
        }
        WebSearchServiceError::ProviderFailure => {
            error_response(id, -32002, "Web search request failed")
        }
        WebSearchServiceError::UsageRecordingFailed | WebSearchServiceError::Internal => {
            error_response(id, -32603, "Internal server error")
        }
    }
}

fn web_search_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search query"},
            "country": {"type": "string"},
            "search_lang": {"type": "string"},
            "ui_lang": {"type": "string"},
            "count": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5},
            "offset": {"type": "integer", "minimum": 0, "maximum": 9},
            "safesearch": {"type": "string"},
            "freshness": {"type": "string"},
            "text_decorations": {"type": "boolean"},
            "spellcheck": {"type": "boolean"},
            "units": {"type": "string"},
            "extra_snippets": {"type": "boolean"},
            "summary": {"type": "boolean"},
            "result_filter": {"type": "string"},
            "goggles": {"type": "string"},
            "enable_rich_callback": {"type": "boolean"},
            "include_fetch_metadata": {"type": "boolean"},
            "operators": {"type": "boolean"}
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

fn tool_definition(tool: &McpToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": (tool.input_schema)(),
    })
}

fn get_tool_definition(name: &str) -> Option<&'static McpToolDefinition> {
    MCP_TOOLS.iter().find(|tool| tool.name == name)
}

pub async fn handle_mcp_request(
    State(state): State<McpRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Json(request): Json<McpRequest>,
) -> Result<ResponseJson<McpResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    if request.jsonrpc.as_deref() != Some("2.0") {
        return Ok(error_response(
            request.id,
            -32600,
            "Invalid jsonrpc version, must be \"2.0\"",
        ));
    }

    match request.method.as_str() {
        "initialize" => Ok(ok_response(
            request.id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "cloud-api",
                    "version": MCP_SERVER_VERSION
                }
            }),
        )),
        "tools/list" => Ok(ok_response(
            request.id,
            json!({
                "tools": MCP_TOOLS.iter().map(tool_definition).collect::<Vec<_>>()
            }),
        )),
        "tools/call" => {
            check_usage_for_api_key(&state.usage_state, &api_key).await?;

            let params = match request.params {
                Some(params) => params,
                None => {
                    return Ok(error_response(
                        request.id,
                        -32602,
                        "Missing tools/call params",
                    ));
                }
            };
            let params: CallToolParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(_) => {
                    return Ok(error_response(
                        request.id,
                        -32602,
                        "Invalid tools/call params",
                    ));
                }
            };

            let Some(tool) = get_tool_definition(&params.name) else {
                return Ok(error_response(request.id, -32601, "Unknown tool"));
            };

            let args_value = params.arguments.unwrap_or_else(|| json!({}));
            let mut args: McpWebSearchArgs = match serde_json::from_value(args_value) {
                Ok(args) => args,
                Err(_) => return Ok(error_response(request.id, -32602, "Invalid tool arguments")),
            };

            if args.count.is_none() {
                args.count = Some(5);
            }
            if args.count.is_some_and(|count| count > 20 || count == 0) {
                return Ok(error_response(
                    request.id,
                    -32602,
                    "Tool argument 'count' must be between 1 and 20",
                ));
            }

            let api_key_id = match Uuid::parse_str(&api_key.api_key.id.0) {
                Ok(api_key_id) => api_key_id,
                Err(_) => return Ok(error_response(request.id, -32603, "Invalid API key id")),
            };

            let result = match tool.name {
                WEB_SEARCH_TOOL_NAME => {
                    state
                        .web_search_service
                        .execute(
                            WebSearchRequest {
                                query: args.query,
                                country: args.country,
                                search_lang: args.search_lang,
                                ui_lang: args.ui_lang,
                                count: args.count,
                                offset: args.offset,
                                safesearch: args.safesearch,
                                freshness: args.freshness,
                                text_decorations: args.text_decorations,
                                spellcheck: args.spellcheck,
                                units: args.units,
                                extra_snippets: args.extra_snippets,
                                summary: args.summary,
                                result_filter: args.result_filter,
                                goggles: args.goggles,
                                enable_rich_callback: args.enable_rich_callback,
                                include_fetch_metadata: args.include_fetch_metadata,
                                operators: args.operators,
                            },
                            WebSearchUsageContext {
                                organization_id: api_key.organization.id.0,
                                workspace_id: api_key.workspace.id.0,
                                api_key_id,
                            },
                        )
                        .await
                }
                _ => return Ok(error_response(request.id, -32601, "Unknown tool")),
            };

            let result = match result {
                Ok(result) => result,
                Err(error) => return Ok(map_mcp_service_error(request.id, error)),
            };

            let payload = json!({
                "query": result.query,
                "result_count": result.result_count,
                "results": result.results.into_iter().map(|item| json!({
                    "title": item.title,
                    "url": item.url,
                    "description": item.description,
                    "published": item.published,
                    "site_name": item.site_name,
                })).collect::<Vec<_>>()
            });

            Ok(ok_response(
                request.id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": payload.to_string()
                    }],
                    "structuredContent": payload,
                    "isError": false
                }),
            ))
        }
        _ => Ok(error_response(request.id, -32601, "Method not found")),
    }
}
