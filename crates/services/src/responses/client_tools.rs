//! Request-local adaptation of client-managed Responses tools to Chat functions.
//! No tool is executed here. Stable names allow client-owned transcripts to be
//! replayed even when the current tool catalog changes.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

// Codex 0.166's exec grammar has an unrestricted, nonempty plain_source branch.
// Recognize that language explicitly; never silently discard other grammars.
pub const CODEX_EXEC_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

#[derive(Clone, Debug)]
enum CustomFormat {
    Text,
    NonemptyText,
}

#[derive(Clone, Debug)]
struct Tool {
    name: String,
    namespace: Option<String>,
    custom: Option<CustomFormat>,
}

impl Tool {
    fn provider_name(&self) -> String {
        if self.namespace.is_none() && self.custom.is_none() {
            return self.name.clone();
        }
        let identity = json!([self.namespace, self.name, self.custom.is_some()]);
        let digest = hex::encode(Sha256::digest(identity.to_string().as_bytes()));
        format!("nearai_tool_{}", &digest[..32])
    }

    fn custom_input(&self, arguments: &str) -> Result<String, String> {
        let parsed: Value = serde_json::from_str(arguments)
            .map_err(|_| "Model returned malformed JSON for a custom tool".to_string())?;
        let input = parsed
            .get("input")
            .and_then(Value::as_str)
            .ok_or("Model must return an input string for a custom tool")?;
        if matches!(self.custom, Some(CustomFormat::NonemptyText)) && input.is_empty() {
            return Err("Model returned empty input for a nonempty custom-tool grammar".into());
        }
        Ok(input.to_owned())
    }
}

#[derive(Debug, Default)]
struct Registry {
    // Only current declarations may validate or restore newly generated calls.
    tools: HashMap<String, Tool>,
    original_tools: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct ClientToolAdapter {
    registry: Arc<Registry>,
}

impl ClientToolAdapter {
    /// Validate the internal wrapper before the service emits any custom call.
    pub fn validate_arguments(&self, name: &str, arguments: &str) -> Result<(), String> {
        if let Some(tool) = self.registry.tools.get(name).filter(|t| t.custom.is_some()) {
            tool.custom_input(arguments)?;
        }
        Ok(())
    }

    /// Restore the caller's schema before serialization and digest attestation.
    pub fn response(&self, mut response: Value) -> Result<Value, String> {
        if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
            for item in output {
                self.item(item)?;
            }
        }
        if let Some(tools) = &self.registry.original_tools {
            response["tools"] = tools.clone();
        }
        if let Some(choice) = response.get_mut("tool_choice") {
            if let Some(name) = choice.pointer("/function/name").and_then(Value::as_str) {
                if let Some(tool) = self.registry.tools.get(name) {
                    *choice = json!({"type": if tool.custom.is_some() {"custom"} else {"function"}, "name": tool.name});
                    if let Some(namespace) = &tool.namespace {
                        choice["namespace"] = json!(namespace);
                    }
                }
            }
        }
        Ok(response)
    }

    fn item(&self, item: &mut Value) -> Result<Option<Tool>, String> {
        if item["type"] != "function_call" {
            return Ok(None);
        }
        let Some(tool) = item
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| self.registry.tools.get(name))
        else {
            return Ok(None);
        };
        item["name"] = json!(tool.name);
        if let Some(namespace) = &tool.namespace {
            item["namespace"] = json!(namespace);
        }
        if tool.custom.is_some() {
            let arguments = item["arguments"].as_str().unwrap_or_default();
            // The empty added item precedes argument generation. Completed
            // wrappers have already been validated by the service.
            let input = if item["status"] == "in_progress" && arguments.is_empty() {
                String::new()
            } else {
                tool.custom_input(arguments)?
            };
            item["type"] = json!("custom_tool_call");
            item["input"] = json!(input);
            if let Some(fields) = item.as_object_mut() {
                fields.remove("arguments");
            }
        }
        Ok(Some(tool.clone()))
    }

    pub fn stream(&self) -> ClientToolStream {
        ClientToolStream {
            adapter: self.clone(),
            items: HashMap::new(),
        }
    }
}

pub struct ClientToolStream {
    adapter: ClientToolAdapter,
    items: HashMap<String, Tool>,
}

impl ClientToolStream {
    pub fn event(&mut self, mut event: Value) -> Result<Value, String> {
        if let Some(item) = event.get_mut("item") {
            if let Some(tool) = self.adapter.item(item)? {
                if let Some(id) = item["id"].as_str() {
                    self.items.insert(id.to_owned(), tool);
                }
            }
        }
        if let Some(tool) = event["item_id"]
            .as_str()
            .and_then(|id| self.items.get(id))
            .filter(|tool| tool.custom.is_some())
        {
            match event["type"].as_str() {
                // The typed service emits accumulated, validated arguments in
                // one delta, retaining the provider's bytes for normal calls.
                Some("response.function_call_arguments.delta") => {
                    let input = tool.custom_input(event["delta"].as_str().unwrap_or_default())?;
                    event["type"] = json!("response.custom_tool_call_input.delta");
                    event["delta"] = json!(input);
                }
                Some("response.function_call_arguments.done") => {
                    let input =
                        tool.custom_input(event["arguments"].as_str().unwrap_or_default())?;
                    event["type"] = json!("response.custom_tool_call_input.done");
                    event["input"] = json!(input);
                    if let Some(fields) = event.as_object_mut() {
                        fields.remove("arguments");
                    }
                }
                _ => {}
            }
        }
        if let Some(response) = event.get_mut("response") {
            *response = self.adapter.response(std::mem::take(response))?;
        }
        Ok(event)
    }
}

/// Move Codex's prompt-only tool prefix to the public tools field. All message
/// and replay items retain their relative order; no server state is introduced.
fn promote_inline_tools(body: &mut Value) -> Result<(), String> {
    let mut inline = Vec::new();
    let mut found_prefix = false;
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        let mut messages_started = false;
        for item in input.iter() {
            if item["type"] == "additional_tools" {
                if messages_started || item["role"] != "developer" {
                    return Err(
                        "additional_tools must be a leading developer tool declaration".into(),
                    );
                }
                let tools = item["tools"]
                    .as_array()
                    .ok_or("additional_tools.tools must be an array")?;
                inline.extend(tools.iter().cloned());
                found_prefix = true;
            } else {
                messages_started = true;
            }
        }
        input.retain(|item| item["type"] != "additional_tools");
    }
    if found_prefix {
        let tools = body
            .as_object_mut()
            .ok_or("Response request must be an object")?
            .entry("tools")
            .or_insert_with(|| json!([]));
        if tools.is_null() {
            *tools = json!([]);
        }
        tools
            .as_array_mut()
            .ok_or("tools must be an array")?
            .extend(inline);
    }
    Ok(())
}

fn tool_name(tool: &Value) -> Result<String, String> {
    tool["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .ok_or("Client-managed tools require a nonempty name".into())
}

fn is_cloud_search(tool: &Value) -> bool {
    matches!(
        tool["type"].as_str(),
        Some("web_search" | "web_context_search")
    )
}

fn visit_tools<'a>(
    tools: &'a [Value],
    allow_cloud_search: bool,
    mut visitor: impl FnMut(&'a Value, Option<&'a str>) -> Result<(), String>,
) -> Result<(), String> {
    for tool in tools {
        if tool["type"] == "namespace" {
            let name = tool["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("A tool namespace requires a nonempty name")?;
            let children = tool["tools"]
                .as_array()
                .ok_or("A tool namespace requires a tools array")?;
            for child in children {
                if !matches!(child["type"].as_str(), Some("function" | "custom")) {
                    return Err(
                        "Tool namespaces support client-managed function and custom tools only"
                            .into(),
                    );
                }
                visitor(child, Some(name))?;
            }
        } else {
            if !(matches!(tool["type"].as_str(), Some("function" | "custom"))
                || allow_cloud_search && is_cloud_search(tool))
            {
                return Err(format!(
                    "Unsupported Responses tool type '{}'",
                    tool["type"].as_str().unwrap_or("unknown")
                ));
            }
            visitor(tool, None)?;
        }
    }
    Ok(())
}

fn validate_native_tools(body: &Value) -> Result<(), String> {
    if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
        for tool in tools.as_array().ok_or("tools must be an array")? {
            // Native web search is executed by the upstream Responses provider.
            // Its configuration has no client-tool name and stays unmodified.
            if tool["type"] == "web_search" {
                continue;
            }
            visit_tools(std::slice::from_ref(tool), false, |tool, _| {
                tool_name(tool).map(|_| ())
            })?;
        }
    }
    Ok(())
}

pub fn normalize_native_request(mut body: Value) -> Result<Value, String> {
    promote_inline_tools(&mut body)?;
    validate_native_tools(&body)?;
    Ok(body)
}

fn custom_format(tool: &Value) -> Result<CustomFormat, String> {
    let Some(format) = tool.get("format").filter(|value| !value.is_null()) else {
        return Ok(CustomFormat::Text);
    };
    match format["type"].as_str() {
        Some("text") => Ok(CustomFormat::Text),
        Some("grammar") if format["syntax"] == "lark" => {
            let definition = format["definition"].as_str().ok_or("Custom grammar definition must be a string")?;
            let lines = |text: &str| text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n");
            if lines(definition) == lines(CODEX_EXEC_GRAMMAR) {
                Ok(CustomFormat::NonemptyText)
            } else {
                Err("This custom-tool grammar requires a native Responses backend; the Chat adapter supports text and the Codex exec source grammar".into())
            }
        }
        _ => Err("This custom-tool format requires a native Responses backend; the Chat adapter supports text and the Codex exec source grammar".into()),
    }
}

fn describe(tool: &Value, namespace: Option<&str>, custom: bool) -> String {
    let name = tool["name"].as_str().unwrap_or_default();
    let qualified = namespace.map_or_else(|| name.to_string(), |ns| format!("{ns}.{name}"));
    let description = tool["description"].as_str().unwrap_or_default();
    if custom {
        format!("Client-managed tool {qualified}. {description}\nSupply the exact tool input in the input string. The client executes it.")
    } else {
        format!("Client-managed tool {qualified}. {description}")
    }
}

fn register(tools: &mut HashMap<String, Tool>, tool: Tool) -> Result<String, String> {
    let alias = tool.provider_name();
    if let Some(existing) = tools.get(&alias) {
        if existing.name != tool.name
            || existing.namespace != tool.namespace
            || existing.custom.is_some() != tool.custom.is_some()
        {
            return Err("Client tool names collide with an internal namespace alias".into());
        }
    } else {
        tools.insert(alias.clone(), tool);
    }
    Ok(alias)
}

pub fn normalize_request(mut body: Value) -> Result<(Value, ClientToolAdapter), String> {
    promote_inline_tools(&mut body)?;
    let mut registry = Registry {
        original_tools: body.get("tools").cloned(),
        ..Default::default()
    };
    let mut normalized = Vec::new();
    let namespace_descriptions: HashMap<String, String> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool["type"] == "namespace")
        .filter_map(|tool| {
            Some((
                tool["name"].as_str()?.to_string(),
                tool["description"].as_str()?.to_string(),
            ))
        })
        .collect();
    if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
        visit_tools(
            tools.as_array().ok_or("tools must be an array")?,
            true,
            |tool, namespace| {
                if is_cloud_search(tool) {
                    normalized.push(tool.clone());
                    return Ok(());
                }
                let custom = tool["type"] == "custom";
                let spec = Tool {
                    name: tool_name(tool)?,
                    namespace: namespace.map(str::to_owned),
                    custom: if custom {
                        Some(custom_format(tool)?)
                    } else {
                        None
                    },
                };
                let alias = register(&mut registry.tools, spec.clone())?;
                if normalized.iter().any(|tool: &Value| tool["name"] == alias) {
                    return Err("Duplicate client-managed tool name within a namespace".into());
                }
                let mut definition = if custom {
                    let mut input = json!({"type":"string", "description":"Exact input for the client-managed tool"});
                    if matches!(spec.custom, Some(CustomFormat::NonemptyText)) {
                        input["minLength"] = json!(1);
                    }
                    json!({"type":"function", "name":alias, "description":describe(tool, namespace, true),
                    "parameters":{"type":"object", "properties":{"input":input}, "required":["input"], "additionalProperties":false}})
                } else {
                    tool.clone()
                };
                definition["name"] = json!(alias);
                if namespace.is_some() && !custom {
                    definition["description"] = json!(describe(tool, namespace, false));
                }
                if let Some(description) =
                    namespace.and_then(|name| namespace_descriptions.get(name))
                {
                    let child = definition["description"].as_str().unwrap_or_default();
                    definition["description"] = json!(format!("{description}\n\n{child}"));
                }
                normalized.push(definition);
                Ok(())
            },
        )?;
        if normalized
            .iter()
            .filter(|tool| is_cloud_search(tool))
            .any(|search| {
                normalized
                    .iter()
                    .any(|tool| tool["type"] == "function" && tool["name"] == search["type"])
            })
        {
            return Err(
                "A client function conflicts with a configured Cloud search tool of the same name"
                    .into(),
            );
        }
        body["tools"] = json!(normalized);
    }

    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        // Completed searches are informational output markers, not client tool
        // calls. The replayed assistant text supplies context; do not execute
        // searches again or create an unmatched tool-result message.
        if input
            .iter()
            .any(|item| item["type"] == "web_search_call" && item["status"] != "completed")
        {
            return Err("Only completed web_search_call items can be replayed".into());
        }
        input.retain(|item| item["type"] != "web_search_call");
        // History needs stable aliases without enabling removed tools for new output.
        let mut replay_tools = registry.tools.clone();
        for item in input {
            match item["type"].as_str() {
                Some("function_call" | "custom_tool_call") => {
                    let custom = item["type"] == "custom_tool_call";
                    let spec = Tool {
                        name: tool_name(item)?,
                        namespace: item
                            .get("namespace")
                            .filter(|v| !v.is_null())
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .ok_or("namespace must be a string")
                            })
                            .transpose()?,
                        custom: custom.then_some(CustomFormat::Text),
                    };
                    let alias = if !custom && spec.namespace.is_none() {
                        // Unknown calls can contain a raw alias from an earlier response.
                        // Preserve it without registering a second identity for that alias.
                        spec.name
                    } else {
                        register(&mut replay_tools, spec)?
                    };
                    if custom {
                        let input = item["input"]
                            .as_str()
                            .ok_or("custom_tool_call.input must be a string")?;
                        item["arguments"] = json!(json!({"input":input}).to_string());
                        item["type"] = json!("function_call");
                    }
                    item["name"] = json!(alias);
                    if let Some(fields) = item.as_object_mut() {
                        fields.remove("namespace");
                        fields.remove("input");
                    }
                }
                Some("custom_tool_call_output" | "function_call_output") => {
                    item["type"] = json!("function_call_output");
                    if let Some(parts) = item["output"].as_array() {
                        let mut text = Vec::new();
                        for part in parts {
                            if !matches!(
                                part["type"].as_str(),
                                Some("input_text" | "output_text" | "text")
                            ) {
                                return Err(
                                    "The Chat Responses adapter supports text tool outputs only"
                                        .into(),
                                );
                            }
                            text.push(
                                part["text"]
                                    .as_str()
                                    .ok_or("Tool output text must be a string")?,
                            );
                        }
                        item["output"] = json!(text.join("\n"));
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(choice) = body.get_mut("tool_choice").filter(|value| !value.is_null()) {
        if let Some(mode) = choice.as_str() {
            if !matches!(mode, "auto" | "none" | "required") {
                return Err(
                    "tool_choice must be auto, none, required, or a client tool choice".into(),
                );
            }
        } else if choice.is_object() {
            let kind = choice["type"]
                .as_str()
                .ok_or("tool_choice requires a type")?;
            if !matches!(kind, "function" | "custom") {
                return Err("Unsupported tool_choice type".into());
            }
            let name = choice
                .get("name")
                .or_else(|| choice.pointer("/function/name"))
                .and_then(Value::as_str)
                .ok_or("tool_choice requires a name")?;
            let namespace = choice
                .get("namespace")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("namespace must be a string")
                })
                .transpose()?;
            let spec = Tool {
                name: name.to_string(),
                namespace,
                custom: (kind == "custom").then_some(CustomFormat::Text),
            };
            let alias = spec.provider_name();
            let declared_search = kind == "function"
                && spec.namespace.is_none()
                && normalized
                    .iter()
                    .any(|tool| is_cloud_search(tool) && tool["type"] == alias);
            if !declared_search && !normalized.iter().any(|tool| tool["name"] == alias) {
                return Err("tool_choice must name a declared tool".into());
            }
            *choice = json!({"type":"function", "function":{"name":alias}});
        } else {
            return Err("tool_choice must be a string or a client tool choice object".into());
        }
    }
    Ok((
        body,
        ClientToolAdapter {
            registry: Arc::new(registry),
        },
    ))
}

#[cfg(test)]
#[path = "client_tools_tests.rs"]
mod tests;
