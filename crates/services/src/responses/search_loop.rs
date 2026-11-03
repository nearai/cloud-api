//! Request-local orchestration for Cloud-managed web search. Client tools are
//! returned to the caller and never executed by this loop.

use super::{errors, models, tools, ProcessStreamContext, ResponseServiceImpl};
use crate::completions::ports::{CompletionMessage, CompletionToolCall};
use crate::responses::service_helpers::{
    ClientManagedFunctionCall, EventEmitter, HostedSearchCall, ResponseStreamContext,
    ToolCallAccumulator,
};
use serde_json::Value;
use uuid::Uuid;

const MAX_COMPLETION_ROUNDS: usize = 10;

/// Separate hosted search from client functions before parsing arguments. In
/// particular, a client function named `web_search` is still a client function
/// when the built-in tool was not declared.
pub(super) fn split_tool_calls(
    accumulated: ToolCallAccumulator,
    request: &models::CreateResponseRequest,
) -> Result<(Vec<ClientManagedFunctionCall>, Vec<HostedSearchCall>), errors::ResponseError> {
    let search_names: Vec<String> = request
        .tools
        .iter()
        .flatten()
        .filter_map(|tool| match tool {
            models::ResponseTool::WebSearch { .. } => Some(tools::WEB_SEARCH_TOOL_NAME.to_string()),
            models::ResponseTool::WebContextSearch {} => {
                Some(tools::WEB_CONTEXT_SEARCH_TOOL_NAME.to_string())
            }
            _ => None,
        })
        .collect();
    let available_names = tools::get_tool_names(request);
    let mut client_calls = ToolCallAccumulator::new();
    let mut search_calls = Vec::new();
    let mut entries: Vec<_> = accumulated.into_iter().collect();
    entries.sort_by_key(|(index, _)| *index);
    for (index, mut entry) in entries {
        // Preserve the existing search-only fallback for models that omit the
        // sole tool name. Never infer a client function's name or arguments.
        if entry
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
            && available_names.len() == 1
        {
            if let Some(name) = available_names
                .first()
                .filter(|name| search_names.contains(name))
            {
                entry.name = Some(name.clone());
            }
        }
        let Some(name) = entry
            .name
            .as_ref()
            .filter(|name| search_names.contains(name))
            .cloned()
        else {
            client_calls.insert(index, entry);
            continue;
        };
        if entry.id.as_deref().is_some_and(|id| id.trim().is_empty()) {
            return Err(errors::ResponseError::InvalidParams(
                "Model returned a search call with an empty call_id.".to_string(),
            ));
        }
        let arguments = entry.arguments.clone();
        // Only fixed, declared search names reach the legacy parser and its
        // diagnostics. Customer-named client tools retain the raw conversion.
        let parsed = tools::convert_tool_calls(
            [(index, entry)].into_iter().collect(),
            &request.model,
            &search_names,
            &[],
        );
        for call in parsed {
            // Hosted search historically repairs malformed model JSON. Replay
            // the parsed arguments used by the executor; only client functions
            // require byte-for-byte argument preservation.
            let arguments = call
                .params
                .as_ref()
                .map_or_else(|| arguments.clone(), serde_json::Value::to_string);
            search_calls.push(HostedSearchCall {
                call,
                name: name.clone(),
                arguments,
            });
        }
    }
    let client_calls = ResponseServiceImpl::convert_client_function_calls(client_calls, request)?;
    Ok((client_calls, search_calls))
}

pub(super) async fn run(
    ctx: &mut ResponseStreamContext,
    emitter: &mut EventEmitter,
    messages: &mut Vec<CompletionMessage>,
    context: &mut ProcessStreamContext,
    definitions: &[inference_providers::ToolDefinition],
    tool_choice: &Option<inference_providers::ToolChoice>,
) -> Result<(), errors::ResponseError> {
    let mut malformed_rounds = 0;
    let mut round_tool_choice = tool_choice.clone();
    for _ in 0..MAX_COMPLETION_ROUNDS {
        let result = ResponseServiceImpl::run_completion_once(
            ctx,
            emitter,
            messages,
            context,
            definitions,
            &round_tool_choice,
        )
        .await?;
        if result.stream_error {
            return Err(result
                .stream_error_cause
                .unwrap_or(errors::ResponseError::StreamInterrupted));
        }
        if !result.text.is_empty() {
            ctx.next_output_index();
        }
        if !result.tool_calls.is_empty() {
            // A stateless client resumes with its own tool results. Prioritize
            // that handoff over searches proposed in the same model turn, so
            // no hidden search result is executed then lost across requests.
            ResponseServiceImpl::emit_client_function_calls(
                ctx,
                emitter,
                &context.response_items_repository,
                result.tool_calls,
            )
            .await?;
            return Ok(());
        }
        if result.search_calls.is_empty() {
            return Ok(());
        }
        if result
            .search_calls
            .iter()
            .any(|search| search.call.tool_type == tools::tool_config::ERROR_TOOL_TYPE)
        {
            malformed_rounds += 1;
            if malformed_rounds >= tools::MAX_CONSECUTIVE_TOOL_FAILURES {
                return Err(errors::ResponseError::InternalError(
                    "Search calls contained malformed arguments in three consecutive inference rounds.".to_string(),
                ));
            }
        } else {
            malformed_rounds = 0;
        }
        let mut calls = Vec::with_capacity(result.search_calls.len());
        for search in &result.search_calls {
            calls.push(CompletionToolCall {
                id: call_id(search)?.to_string(),
                name: search.name.clone(),
                arguments: search.arguments.clone(),
                thought_signature: search.call.thought_signature.clone(),
            });
        }
        messages.push(CompletionMessage {
            reasoning_content: result.reasoning,
            role: "assistant".to_string(),
            content: Value::String(result.text),
            tool_call_id: None,
            tool_calls: Some(calls),
        });
        let mut search_succeeded = false;
        for search in result.search_calls {
            let (mut content, instruction) = execute(ctx, emitter, context, &search).await?;
            search_succeeded |= search.call.tool_type != tools::tool_config::ERROR_TOOL_TYPE;
            if let Some(instruction) = instruction {
                content.push_str("\n\n");
                content.push_str(&instruction);
            }
            messages.push(CompletionMessage {
                reasoning_content: None,
                role: "tool".to_string(),
                // Keep citation guidance with the result it describes. A
                // trailing system message breaks providers that only accept
                // leading instructions and interrupts parallel tool results.
                content: Value::String(content),
                tool_call_id: Some(call_id(&search)?.to_string()),
                tool_calls: None,
            });
        }
        // The initial choice is satisfied by a successful hosted search. Let
        // the next inference answer or request more tools instead of forcing
        // the same search until the request reaches its round limit.
        if search_succeeded {
            round_tool_choice = Some(inference_providers::ToolChoice::String("auto".to_string()));
        }
    }
    Err(errors::ResponseError::InternalError(
        "Search execution exceeded the limit of 10 inference rounds.".to_string(),
    ))
}

fn call_id(search: &HostedSearchCall) -> Result<&str, errors::ResponseError> {
    search.call.id.as_deref().ok_or_else(|| {
        errors::ResponseError::InternalError(
            "Search call is missing its correlation ID.".to_string(),
        )
    })
}

async fn execute(
    ctx: &mut ResponseStreamContext,
    emitter: &mut EventEmitter,
    context: &mut ProcessStreamContext,
    search: &HostedSearchCall,
) -> Result<(String, Option<String>), errors::ResponseError> {
    if search.call.tool_type == tools::tool_config::ERROR_TOOL_TYPE {
        return Ok((format!("ERROR: {}", search.call.query), None));
    }
    if !context.search_tools.can_handle(&search.name) {
        return Err(errors::ResponseError::InternalError(
            "The requested Cloud-managed search provider is unavailable.".to_string(),
        ));
    }
    // Output item IDs must remain unique when a provider reuses call IDs across
    // inference rounds. The provider ID is retained in the Chat transcript.
    let item_id = format!("ws_{}", Uuid::new_v4().simple());
    context
        .search_tools
        .emit_start(
            &search.call,
            &mut tools::ToolEventContext {
                stream_ctx: ctx,
                emitter,
                tool_call_id: &item_id,
                response_items_repository: None,
            },
        )
        .await?;
    let output = context
        .search_tools
        .execute(
            &search.call,
            &tools::ToolExecutionContext {
                request: &context.request,
            },
        )
        .await;
    let status = if output.is_ok() {
        models::ResponseItemStatus::Completed
    } else {
        models::ResponseItemStatus::Failed
    };
    let item = models::ResponseOutputItem::WebSearchCall {
        id: item_id.clone(),
        response_id: ctx.response_id_str.clone(),
        previous_response_id: None,
        next_response_ids: Vec::new(),
        created_at: ctx.created_at,
        status,
        action: models::WebSearchAction::Search {
            query: search.call.query.clone(),
        },
        model: ctx.model.clone(),
    };
    let stored = context
        .response_items_repository
        .create(ctx.response_id.clone(), ctx.api_key_id, None, item)
        .await
        .map_err(|_| {
            errors::ResponseError::InternalError(
                "Failed to retain the request-local search output.".to_string(),
            )
        })?;
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            emitter.emit_item_done(ctx, stored, item_id).await?;
            ctx.next_output_index();
            return Err(error);
        }
    };
    let tools::ToolOutput::WebSearch { sources } = output else {
        return Err(errors::ResponseError::InternalError(
            "Search provider returned an unsupported result type.".to_string(),
        ));
    };
    let start_index = context
        .source_registry
        .as_ref()
        .map_or(0, |registry| registry.web_sources.len());
    let formatted = tools::web_search::format_results(&sources, start_index);
    if let Some(registry) = &mut context.source_registry {
        registry.web_sources.extend(sources);
    } else {
        context.source_registry = Some(models::SourceRegistry::with_results(sources));
    }
    context
        .search_tools
        .emit_complete(
            &search.call,
            &mut tools::ToolEventContext {
                stream_ctx: ctx,
                emitter,
                tool_call_id: &item_id,
                response_items_repository: None,
            },
        )
        .await?;
    Ok((formatted.formatted, formatted.instruction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::service_helpers::ToolCallAccumulatorEntry;

    fn request() -> models::CreateResponseRequest {
        serde_json::from_value(serde_json::json!({"model":"test", "store":false})).unwrap()
    }

    #[test]
    fn mixed_search_and_client_calls_preserve_client_argument_bytes() {
        let mut request = request();
        request.tools = Some(vec![
            models::ResponseTool::WebSearch {
                count: Some(0),
                filters: None,
                search_context_size: None,
                user_location: None,
            },
            models::ResponseTool::Function {
                name: "lookup".to_string(),
                description: None,
                parameters: None,
            },
        ]);
        let raw = "{ \"input\":  42 }";
        let (clients, searches) = split_tool_calls(
            [
                (
                    0,
                    ToolCallAccumulatorEntry {
                        id: Some("call_search".to_string()),
                        name: Some("web_search".to_string()),
                        arguments: "{\"query\":\"weather\"}".to_string(),
                        thought_signature: None,
                    },
                ),
                (
                    1,
                    ToolCallAccumulatorEntry {
                        id: Some("call_client".to_string()),
                        name: Some("lookup".to_string()),
                        arguments: raw.to_string(),
                        thought_signature: Some("signature".to_string()),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            &request,
        )
        .unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].arguments, raw);
        assert_eq!(clients[0].thought_signature.as_deref(), Some("signature"));
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0].call.query, "weather");
        assert_eq!(call_id(&searches[0]).unwrap(), "call_search");
    }

    #[test]
    fn client_function_named_like_search_never_enters_hosted_parser() {
        let mut request = request();
        request.tools = Some(vec![models::ResponseTool::Function {
            name: "web_search".to_string(),
            description: None,
            parameters: None,
        }]);
        let raw = "{ \"query\":  \"client-owned\" }";
        let (clients, searches) = split_tool_calls(
            [(
                0,
                ToolCallAccumulatorEntry {
                    id: Some("client_call".to_string()),
                    name: Some("web_search".to_string()),
                    arguments: raw.to_string(),
                    thought_signature: None,
                },
            )]
            .into_iter()
            .collect(),
            &request,
        )
        .unwrap();
        assert!(searches.is_empty());
        assert_eq!(clients[0].arguments, raw);
    }
}
