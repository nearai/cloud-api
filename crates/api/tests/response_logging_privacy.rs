//! Source-level privacy regression checks for Responses tracing.
//!
//! The response path handles customer prompts, model output, tool arguments, and
//! provider failures. These checks intentionally inspect tracing macro invocations
//! rather than exercising a live provider so a newly added log field cannot leak
//! request or response content unnoticed.

const RESPONSE_LOG_SOURCES: &[(&str, &str)] = &[
    (
        "api responses route",
        include_str!("../src/routes/responses.rs"),
    ),
    (
        "response errors",
        include_str!("../../services/src/responses/errors.rs"),
    ),
    (
        "response module",
        include_str!("../../services/src/responses/mod.rs"),
    ),
    (
        "response models",
        include_str!("../../services/src/responses/models.rs"),
    ),
    (
        "response ports",
        include_str!("../../services/src/responses/ports.rs"),
    ),
    (
        "response service",
        include_str!("../../services/src/responses/service.rs"),
    ),
    (
        "response helpers",
        include_str!("../../services/src/responses/service_helpers.rs"),
    ),
    (
        "citation tracker",
        include_str!("../../services/src/responses/citation_tracker.rs"),
    ),
    (
        "Brave tool provider",
        include_str!("../../services/src/responses/tools/brave.rs"),
    ),
    (
        "tool executor",
        include_str!("../../services/src/responses/tools/executor.rs"),
    ),
    (
        "file search tool",
        include_str!("../../services/src/responses/tools/file_search.rs"),
    ),
    (
        "function tool",
        include_str!("../../services/src/responses/tools/function.rs"),
    ),
    (
        "MCP tool",
        include_str!("../../services/src/responses/tools/mcp.rs"),
    ),
    (
        "tool module",
        include_str!("../../services/src/responses/tools/mod.rs"),
    ),
    (
        "tool ports",
        include_str!("../../services/src/responses/tools/ports.rs"),
    ),
    (
        "tool configuration",
        include_str!("../../services/src/responses/tools/tool_config.rs"),
    ),
    (
        "web context search tool",
        include_str!("../../services/src/responses/tools/web_context_search.rs"),
    ),
    (
        "web search tool",
        include_str!("../../services/src/responses/tools/web_search.rs"),
    ),
];

fn tracing_invocations(source: &str) -> Vec<&str> {
    const MACRO_PREFIXES: &[&str] = &[
        "tracing::debug!(",
        "tracing::event!(",
        "tracing::info!(",
        "tracing::span!(",
        "tracing::warn!(",
        "tracing::error!(",
        "tracing::trace!(",
        "debug!(",
        "event!(",
        "info!(",
        "span!(",
        "warn!(",
        "error!(",
        "trace!(",
    ];

    let mut starts = MACRO_PREFIXES
        .iter()
        .flat_map(|prefix| source.match_indices(prefix).map(|(index, _)| index))
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();

    starts
        .into_iter()
        .filter_map(|start| {
            let invocation = &source[start..];
            let open_paren = invocation.find('(')?;
            let mut depth = 0_usize;
            let mut in_string = false;
            let mut escaped = false;

            for (offset, character) in invocation[open_paren..].char_indices() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        in_string = false;
                    }
                    continue;
                }

                match character {
                    '"' => in_string = true,
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(&invocation[..open_paren + offset + 1]);
                        }
                    }
                    _ => {}
                }
            }

            None
        })
        .collect()
}

fn is_outside_string(value: &str, index: usize) -> bool {
    let mut in_string = false;
    let mut escaped = false;

    for character in value[..index].chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
        }
    }

    !in_string
}

fn contains_field(invocation: &str, field: &str) -> bool {
    for separator in [" =", "="] {
        let pattern = format!("{field}{separator}");
        if invocation.match_indices(&pattern).any(|(index, _)| {
            is_outside_string(invocation, index)
                && invocation[..index]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
        }) {
            return true;
        }
    }

    false
}

fn contains_rendered_identifier(invocation: &str, prefix: &str, identifier: &str) -> bool {
    let pattern = format!("{prefix}{identifier}");
    invocation.match_indices(&pattern).any(|(index, _)| {
        let next_index = index + pattern.len();
        is_outside_string(invocation, index)
            && invocation[next_index..]
                .chars()
                .next()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
    })
}

fn contains_error_rendering(invocation: &str) -> bool {
    ["%", "?", "{"]
        .into_iter()
        .any(|prefix| contains_rendered_identifier(invocation, prefix, "e"))
        || ["%", "?", "{"]
            .into_iter()
            .any(|prefix| contains_rendered_identifier(invocation, prefix, "error"))
        || [", e)", ", e,", ", error)", ", error,"]
            .into_iter()
            .any(|pattern| {
                invocation
                    .match_indices(pattern)
                    .any(|(index, _)| is_outside_string(invocation, index))
            })
}

#[test]
fn responses_tracing_does_not_include_customer_data_or_error_text() {
    // Customer-supplied values must never be structured tracing fields. The suffixes
    // used for safe measurements (`delta_len`, `content_index`, etc.) do not match.
    const PROHIBITED_FIELDS: &[&str] = &[
        "args",
        "arguments",
        "available_tools",
        "body",
        "conversation_title",
        "content",
        "delta",
        "error",
        "event",
        "filename",
        "image_url",
        "inferred_tool",
        "input",
        "instructions",
        "item",
        "metadata",
        "output",
        "prompt",
        "query",
        "request",
        "request_hash",
        "response",
        "response_hash",
        "server_label",
        "server_url",
        "snippet",
        "text",
        "title",
        "thought_signature",
        "tool",
        "url",
    ];
    const PROHIBITED_DERIVED_FIELDS: &[&str] = &[
        "tool_name = name",
        "tool_name = %tool_name",
        "tool_name = %name",
    ];

    let mut violations = Vec::new();

    for (source_name, source) in RESPONSE_LOG_SOURCES {
        for invocation in tracing_invocations(source) {
            for field in PROHIBITED_FIELDS {
                if contains_field(invocation, field) {
                    violations.push(format!(
                        "{source_name} tracing invocation exposes prohibited field `{field}`: {invocation}"
                    ));
                }
            }
            if contains_error_rendering(invocation) {
                violations.push(format!(
                    "{source_name} tracing invocation renders an error value: {invocation}"
                ));
            }
            for field in PROHIBITED_DERIVED_FIELDS {
                if invocation.contains(field) {
                    violations.push(format!(
                        "{source_name} tracing invocation exposes a request-derived field `{field}`: {invocation}"
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Responses tracing privacy violations:\n{}",
        violations.join("\n")
    );
}
