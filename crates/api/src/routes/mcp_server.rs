use crate::middleware::auth::AuthenticatedApiKey;
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

const MCP_TOOL_NAME: &str = "web_search";
const MCP_PROTOCOL_VERSION: &str = "2026-03-13";

#[derive(Clone)]
pub struct McpRouteState {
    pub web_search_service: Arc<WebSearchService>,
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
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct McpWebSearchArgs {
    query: String,
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    search_lang: Option<String>,
    #[serde(default)]
    ui_lang: Option<String>,
    #[serde(default)]
    freshness: Option<String>,
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

fn tool_definition() -> Value {
    json!({
        "name": MCP_TOOL_NAME,
        "description": "Search the web and return structured search results.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "count": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5},
                "country": {"type": "string"},
                "search_lang": {"type": "string"},
                "ui_lang": {"type": "string"},
                "freshness": {"type": "string"}
            },
            "required": ["query"],
            "additionalProperties": false
        }
    })
}

pub async fn handle_mcp_request(
    State(state): State<McpRouteState>,
    Extension(api_key): Extension<AuthenticatedApiKey>,
    Json(request): Json<McpRequest>,
) -> Result<ResponseJson<McpResponse>, StatusCode> {
    let _ = request.jsonrpc.as_deref();

    match request.method.as_str() {
        "initialize" => {
            let protocol_version = request
                .params
                .clone()
                .and_then(|params| serde_json::from_value::<InitializeParams>(params).ok())
                .and_then(|params| params.protocol_version)
                .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());

            Ok(ok_response(
                request.id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "cloud-api",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ))
        }
        "tools/list" => Ok(ok_response(
            request.id,
            json!({
                "tools": [tool_definition()]
            }),
        )),
        "tools/call" => {
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

            if params.name != MCP_TOOL_NAME {
                return Ok(error_response(request.id, -32601, "Unknown tool"));
            }

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

            let result = state
                .web_search_service
                .execute(
                    WebSearchRequest {
                        query: args.query,
                        count: args.count,
                        country: args.country,
                        search_lang: args.search_lang,
                        ui_lang: args.ui_lang,
                        freshness: args.freshness,
                    },
                    WebSearchUsageContext {
                        organization_id: api_key.organization.id.0,
                        workspace_id: api_key.workspace.id.0,
                        api_key_id,
                    },
                )
                .await;

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
