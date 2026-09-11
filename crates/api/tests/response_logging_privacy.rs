//! Source-level privacy regression checks for Responses tracing.
//!
//! The response path handles customer prompts, model output, tool arguments, and
//! provider failures. These checks intentionally inspect tracing macro invocations
//! rather than exercising a live provider so a newly added log field cannot leak
//! request or response content unnoticed.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Load every Responses implementation source at test time.  Keeping the source
/// list directory-based is deliberate: a new implementation file must be checked
/// automatically rather than relying on someone remembering to update this test.
fn response_log_sources() -> Result<Vec<(PathBuf, String)>, String> {
    let api_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![
        api_root.join("src/routes/responses.rs"),
        api_root.join("src/middleware/body_hash.rs"),
    ];
    collect_rust_sources(&api_root.join("../services/src/responses"), &mut paths)?;
    paths.sort_unstable();
    paths.dedup();

    paths
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            Ok((path, source))
        })
        .collect()
}

fn collect_rust_sources(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;

    for entry in entries {
        let entry = entry
            .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;

        if file_type.is_dir() {
            collect_rust_sources(&path, paths)?;
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        {
            paths.push(path);
        }
    }

    Ok(())
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn skip_quoted_string(source: &[u8], start: usize, quote: u8) -> Result<usize, String> {
    let mut index = start + 1;

    while let Some(&byte) = source.get(index) {
        if byte == b'\\' {
            index += 2;
        } else if byte == quote {
            return Ok(index + 1);
        } else {
            index += 1;
        }
    }

    Err("unterminated quoted literal".to_string())
}

fn raw_string_start(source: &[u8], start: usize) -> Option<(usize, usize)> {
    let (literal_start, raw_marker) = match source.get(start) {
        Some(b'r') => (start, start),
        Some(b'b') if source.get(start + 1) == Some(&b'r') => (start, start + 1),
        _ => return None,
    };

    if literal_start > 0 && is_identifier_byte(source[literal_start - 1]) {
        return None;
    }

    let mut quote = raw_marker + 1;
    let mut hashes = 0;
    while source.get(quote) == Some(&b'#') {
        hashes += 1;
        quote += 1;
    }

    (source.get(quote) == Some(&b'"')).then_some((quote + 1, hashes))
}

fn skip_raw_string(source: &[u8], start: usize) -> Result<usize, String> {
    let (mut index, hashes) = raw_string_start(source, start)
        .ok_or_else(|| "expected a raw string literal".to_string())?;

    while index < source.len() {
        if source[index] == b'"'
            && source
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Ok(index + 1 + hashes);
        }
        index += 1;
    }

    Err("unterminated raw string literal".to_string())
}

fn skip_line_comment(source: &[u8], start: usize) -> usize {
    source[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |offset| start + offset + 1)
}

fn skip_block_comment(source: &[u8], start: usize) -> Result<usize, String> {
    let mut index = start + 2;
    let mut depth = 1_usize;

    while index + 1 < source.len() {
        match (source[index], source[index + 1]) {
            (b'/', b'*') => {
                depth += 1;
                index += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => index += 1,
        }
    }

    Err("unterminated block comment".to_string())
}

fn skip_char_literal(source: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    let mut escaped = false;

    while let Some(&byte) = source.get(index) {
        if byte == b'\n' {
            return None;
        }
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'\'' {
            return Some(index + 1);
        }
        index += 1;
    }

    None
}

/// Skip comments and literals while scanning Rust source.  The source-level
/// check must never treat a macro-like string or comment as a real invocation.
fn skip_non_code(source: &[u8], start: usize) -> Result<Option<usize>, String> {
    match (source.get(start), source.get(start + 1)) {
        (Some(b'/'), Some(b'/')) => Ok(Some(skip_line_comment(source, start + 2))),
        (Some(b'/'), Some(b'*')) => skip_block_comment(source, start).map(Some),
        (Some(b'r' | b'b'), _) if raw_string_start(source, start).is_some() => {
            skip_raw_string(source, start).map(Some)
        }
        (Some(b'"'), _) => skip_quoted_string(source, start, b'"').map(Some),
        (Some(b'\''), _) => Ok(skip_char_literal(source, start)),
        _ => Ok(None),
    }
}

fn macro_open_delimiter(source: &[u8], start: usize) -> Option<usize> {
    const MACRO_NAMES: &[&str] = &[
        "debug",
        "debug_span",
        "event",
        "error",
        "error_span",
        "info",
        "info_span",
        "span",
        "trace",
        "trace_span",
        "warn",
        "warn_span",
    ];

    for name in MACRO_NAMES {
        for (prefix, qualified) in [
            (format!("tracing::{name}!"), true),
            (format!("{name}!"), false),
        ] {
            if !source[start..].starts_with(prefix.as_bytes()) {
                continue;
            }

            if start > 0 {
                let previous = source[start - 1];
                if is_identifier_byte(previous) || (!qualified && previous == b':') {
                    continue;
                }
            }

            let mut index = start + prefix.len();
            while source.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if matches!(source.get(index), Some(b'(' | b'[' | b'{')) {
                return Some(index);
            }
        }
    }

    None
}

fn invocation_snippet(source: &str, start: usize) -> String {
    source[start..]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(160)
        .collect()
}

fn matching_delimiter(open: u8, close: u8) -> bool {
    matches!((open, close), (b'(', b')') | (b'[', b']') | (b'{', b'}'))
}

fn tracing_invocation_end(source: &str, open_delimiter: usize) -> Result<usize, String> {
    let bytes = source.as_bytes();
    let mut index = open_delimiter;
    let mut delimiters = Vec::new();

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)? {
            index = next;
            continue;
        }

        match bytes[index] {
            b'(' | b'[' | b'{' => delimiters.push(bytes[index]),
            b')' | b']' | b'}' => {
                let open = delimiters.pop().ok_or_else(|| {
                    format!(
                        "unbalanced closing delimiter in tracing invocation: {}",
                        invocation_snippet(source, open_delimiter)
                    )
                })?;
                if !matching_delimiter(open, bytes[index]) {
                    return Err(format!(
                        "mismatched delimiter in tracing invocation: {}",
                        invocation_snippet(source, open_delimiter)
                    ));
                }
                if delimiters.is_empty() {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }

    Err(format!(
        "unterminated tracing invocation: {}",
        invocation_snippet(source, open_delimiter)
    ))
}

fn tracing_invocations(source: &str) -> Result<Vec<&str>, String> {
    let bytes = source.as_bytes();
    let mut invocations = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)? {
            index = next;
            continue;
        }

        if let Some(open_delimiter) = macro_open_delimiter(bytes, index) {
            let end = tracing_invocation_end(source, open_delimiter)?;
            invocations.push(&source[index..end]);
            index = end;
        } else {
            index += 1;
        }
    }

    Ok(invocations)
}

fn is_outside_string(value: &str, index: usize) -> bool {
    let bytes = value.as_bytes();
    let mut cursor = 0;

    while cursor < index {
        if let Some(next) = skip_non_code(bytes, cursor)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            if next > index {
                return false;
            }
            cursor = next;
        } else {
            cursor += 1;
        }
    }

    true
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

fn format_message_contains_interpolation(invocation: &str) -> bool {
    let bytes = invocation.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if let Some((content_start, hashes)) = raw_string_start(bytes, index) {
            let end = skip_raw_string(bytes, index)
                .expect("tracing invocation parser accepted an invalid raw string literal");
            if is_format_message_start(bytes, index)
                && format_string_contains_interpolation(
                    &invocation[content_start..end - hashes - 1],
                )
            {
                return true;
            }
            index = end;
            continue;
        }

        if bytes[index] == b'"' {
            let end = skip_quoted_string(bytes, index, b'"')
                .expect("tracing invocation parser accepted an invalid quoted literal");
            if is_format_message_start(bytes, index)
                && format_string_contains_interpolation(&invocation[index + 1..end - 1])
            {
                return true;
            }
            index = end;
            continue;
        }

        if let Some(next) = skip_non_code(bytes, index)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            index = next;
        } else {
            index += 1;
        }
    }

    false
}

fn is_format_message_start(source: &[u8], start: usize) -> bool {
    let mut index = start;
    while index > 0 && source[index - 1].is_ascii_whitespace() {
        index -= 1;
    }
    matches!(
        index.checked_sub(1).and_then(|index| source.get(index)),
        Some(b'(' | b'{' | b',')
    )
}

fn format_string_contains_interpolation(format_string: &str) -> bool {
    let bytes = format_string.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'{' {
            if bytes.get(index + 1) == Some(&b'{') {
                index += 2;
                continue;
            }
            return true;
        }
        index += 1;
    }

    false
}

fn rendered_field_name_before(invocation: &str, marker_index: usize) -> Option<&str> {
    let bytes = invocation.as_bytes();
    let mut index = marker_index;

    while index > 0 && bytes[index - 1].is_ascii_whitespace() {
        index -= 1;
    }
    if index == 0 || bytes[index - 1] != b'=' {
        return None;
    }
    index -= 1;

    while index > 0 && bytes[index - 1].is_ascii_whitespace() {
        index -= 1;
    }
    let end = index;
    while index > 0 && is_identifier_byte(bytes[index - 1]) {
        index -= 1;
    }

    (index < end).then_some(&invocation[index..end])
}

fn rendered_expression_after(invocation: &str, marker_index: usize) -> &str {
    let bytes = invocation.as_bytes();
    let mut index = marker_index + 1;
    let start = index;
    let mut paren_depth = 0_usize;
    let mut bracket_depth = 0_usize;
    let mut brace_depth = 0_usize;

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            index = next;
            continue;
        }

        match bytes[index] {
            b'(' => paren_depth += 1,
            b'[' => bracket_depth += 1,
            b'{' => brace_depth += 1,
            b')' | b'}' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => break,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => break,
            _ => {}
        }
        index += 1;
    }

    invocation[start..index].trim()
}

fn rendered_fields(invocation: &str) -> Vec<(Option<&str>, u8, &str)> {
    let bytes = invocation.as_bytes();
    let mut fields = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            index = next;
            continue;
        }

        if matches!(bytes[index], b'%' | b'?') {
            fields.push((
                rendered_field_name_before(invocation, index),
                bytes[index],
                rendered_expression_after(invocation, index),
            ));
        }
        index += 1;
    }

    fields
}

fn invocation_outer_delimiters(invocation: &str) -> Result<(usize, usize), String> {
    let open = macro_open_delimiter(invocation.as_bytes(), 0)
        .ok_or_else(|| format!("missing tracing macro delimiter: {invocation}"))?;
    let close = invocation
        .len()
        .checked_sub(1)
        .filter(|close| {
            matching_delimiter(invocation.as_bytes()[open], invocation.as_bytes()[*close])
        })
        .ok_or_else(|| format!("invalid tracing macro delimiters: {invocation}"))?;
    Ok((open, close))
}

fn top_level_arguments(invocation: &str) -> Result<Vec<&str>, String> {
    let bytes = invocation.as_bytes();
    let (open, close) = invocation_outer_delimiters(invocation)?;
    let mut arguments = Vec::new();
    let mut argument_start = open + 1;
    let mut index = argument_start;
    let mut delimiters = Vec::new();

    while index < close {
        if let Some(next) = skip_non_code(bytes, index)? {
            index = next;
            continue;
        }

        match bytes[index] {
            b'(' | b'[' | b'{' => delimiters.push(bytes[index]),
            b')' | b']' | b'}' => {
                let nested_open = delimiters.pop().ok_or_else(|| {
                    format!(
                        "unbalanced delimiter in tracing argument: {}",
                        invocation_snippet(invocation, open)
                    )
                })?;
                if !matching_delimiter(nested_open, bytes[index]) {
                    return Err(format!(
                        "mismatched delimiter in tracing argument: {}",
                        invocation_snippet(invocation, open)
                    ));
                }
            }
            b',' if delimiters.is_empty() => {
                arguments.push(invocation[argument_start..index].trim());
                argument_start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }

    if !delimiters.is_empty() {
        return Err(format!(
            "unterminated nested delimiter in tracing argument: {}",
            invocation_snippet(invocation, open)
        ));
    }

    let final_argument = invocation[argument_start..close].trim();
    if !final_argument.is_empty() {
        arguments.push(final_argument);
    }
    Ok(arguments)
}

fn is_string_literal(argument: &str) -> bool {
    let argument = argument.trim();
    let bytes = argument.as_bytes();

    match bytes.first() {
        Some(b'"') => skip_quoted_string(bytes, 0, b'"').is_ok_and(|end| end == bytes.len()),
        Some(b'r' | b'b') if raw_string_start(bytes, 0).is_some() => {
            skip_raw_string(bytes, 0).is_ok_and(|end| end == bytes.len())
        }
        _ => false,
    }
}

fn is_static_level(argument: &str) -> bool {
    // The generic `event!` and `span!` forms take a level before their name
    // and fields. Accept only the fixed enum spellings, never a variable.
    matches!(
        argument.trim(),
        "Level::TRACE"
            | "Level::DEBUG"
            | "Level::INFO"
            | "Level::WARN"
            | "Level::ERROR"
            | "tracing::Level::TRACE"
            | "tracing::Level::DEBUG"
            | "tracing::Level::INFO"
            | "tracing::Level::WARN"
            | "tracing::Level::ERROR"
    )
}

fn simple_identifier(argument: &str) -> Option<&str> {
    let identifier = argument.trim();
    (!identifier.is_empty() && identifier.bytes().all(is_identifier_byte)).then_some(identifier)
}

fn top_level_field_assignment(argument: &str) -> Option<(&str, Option<u8>, &str)> {
    let bytes = argument.as_bytes();
    let mut index = 0;
    let mut delimiters = Vec::new();

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            index = next;
            continue;
        }

        match bytes[index] {
            b'(' | b'[' | b'{' => delimiters.push(bytes[index]),
            b')' | b']' | b'}' => {
                let nested_open = delimiters.pop()?;
                if !matching_delimiter(nested_open, bytes[index]) {
                    return None;
                }
            }
            b'=' if delimiters.is_empty()
                && !matches!(
                    bytes.get(index.saturating_sub(1)),
                    Some(b'=' | b'!' | b'<' | b'>')
                )
                && !matches!(bytes.get(index + 1), Some(b'=' | b'>')) =>
            {
                let field = simple_identifier(&argument[..index])?;
                let expression = argument[index + 1..].trim();
                let renderer = expression
                    .as_bytes()
                    .first()
                    .copied()
                    .filter(|byte| matches!(byte, b'%' | b'?'));
                let expression = if renderer.is_some() {
                    expression[1..].trim()
                } else {
                    expression
                };
                return Some((field, renderer, expression));
            }
            _ => {}
        }
        index += 1;
    }

    None
}

fn plain_fields(invocation: &str) -> Result<Vec<(Option<&str>, &str)>, String> {
    let mut fields = Vec::new();

    for argument in top_level_arguments(invocation)? {
        // `parent:` selects a span relationship rather than recording the
        // supplied value as a field; the selected span's own fields are
        // checked at its construction.
        if is_string_literal(argument)
            || is_static_level(argument)
            || argument.trim_start().starts_with("parent:")
        {
            continue;
        }

        if let Some(target) = argument.trim_start().strip_prefix("target:") {
            // A tracing target is emitted as metadata, so make its value a
            // source literal instead of allowing a request-derived alias.
            fields.push((Some("target"), target.trim()));
            continue;
        }

        if let Some((field, renderer, expression)) = top_level_field_assignment(argument) {
            if renderer.is_none() {
                fields.push((Some(field), expression));
            }
        } else if let Some(field) = simple_identifier(argument) {
            fields.push((Some(field), field));
        } else {
            fields.push((None, argument));
        }
    }

    Ok(fields)
}

fn normalize_expression(expression: &str) -> String {
    expression
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn is_allowed_plain_field(field: Option<&str>, expression: &str) -> bool {
    // `field = value` normally uses tracing's `Value` path, but that does not
    // make `value` safe by itself: `payload = prompt` would still record the
    // full prompt. Keep the field *and* its source expression explicit. This
    // deliberately fails closed for a new field, an alias, or a changed
    // expression until it receives a privacy review.
    const ALLOWED_PLAIN_FIELDS: &[(&str, &str)] = &[
        ("accumulated_content_len", "content.len()"),
        ("arguments_len", "args_str.len()"),
        ("attempt", "failure_count"),
        ("body_size_bytes", "body_bytes.len()"),
        ("citation_count", "self.completed_citations.len()"),
        ("citation_index", "idx"),
        ("cited_text_len", "active.accumulated_content.len()"),
        ("clean_position", "self.clean_position"),
        ("connection_count", "self.connections.len()"),
        ("consecutive_error_count", "consecutive_error_count"),
        ("content_index", "cidx"),
        ("delta_count", "delta_count"),
        ("delta_len", "clean_text.len()"),
        ("delta_len", "delta.len()"),
        ("delta_len", "delta_len"),
        ("delta_len", "reasoning.len()"),
        ("elapsed_ms", "started_at.elapsed().as_millis()asu64"),
        ("empty_result", "results.is_empty()"),
        ("empty_result", "sources.is_empty()"),
        ("end_index", "citation.end_index"),
        ("end_index", "self.clean_position"),
        ("endpoint", r#""llm_context""#),
        ("endpoint", r#""web_search""#),
        ("error_category", r#""file_content_fetch_failed""#),
        ("error_category", r#""image_edit_provider_failure""#),
        ("error_category", r#""image_generation_provider_failure""#),
        ("error_category", r#""invalid_json""#),
        ("error_category", r#""title_generation_task_panicked""#),
        ("error_category", "e.log_category()"),
        ("error_category", "error.log_category()"),
        ("error_category", "error_category"),
        ("error_category", "error_cause.log_category()"),
        ("event_count", "event_count"),
        ("event_type", "event_type"),
        ("failures", "failure_count"),
        ("has_delta", "has_delta"),
        ("index", "idx"),
        ("input_tokens", "ctx.total_input_tokens"),
        ("input_tokens", "usage.prompt_tokens"),
        ("iteration", "*iteration"),
        ("loaded_message_count", "loaded_count"),
        ("max_retries", "MAX_CONSECUTIVE_TOOL_FAILURES"),
        ("model", "model"),
        ("output_index", "idx"),
        ("output_tokens", "ctx.total_output_tokens"),
        ("output_tokens", "usage.completion_tokens"),
        ("reasoning_tokens", "ctx.reasoning_tokens"),
        (
            "requested_count",
            "search_params.count.unwrap_or(DEFAULT_COUNT)",
        ),
        (
            "requested_count",
            "search_params.count.unwrap_or(WEB_SEARCH_MAX_COUNT)",
        ),
        (
            "requested_max_snippets",
            "search_params.maximum_number_of_snippets.unwrap_or(DEFAULT_MAX_SNIPPETS)",
        ),
        (
            "requested_max_snippets_per_url",
            "search_params.maximum_number_of_snippets_per_url.unwrap_or(DEFAULT_MAX_SNIPPETS_PER_URL)",
        ),
        (
            "requested_max_tokens",
            "search_params.maximum_number_of_tokens.unwrap_or(DEFAULT_MAX_TOKENS)",
        ),
        (
            "requested_max_tokens_per_url",
            "search_params.maximum_number_of_tokens_per_url.unwrap_or(DEFAULT_MAX_TOKENS_PER_URL)",
        ),
        (
            "requested_max_urls",
            "search_params.maximum_number_of_urls.unwrap_or(DEFAULT_MAX_URLS)",
        ),
        ("response_id", "response.id.as_str()"),
        ("response_id", "response_id.0.to_string()"),
        ("response_id", "response_id.as_str()"),
        ("response_status", r#""failed""#),
        ("result_count", "results.len()"),
        ("result_count", "sources.len()"),
        ("snippet_count", "snippet_count"),
        ("source_id", "citation.source_id"),
        ("source_id", "source_id"),
        (
            "spellcheck",
            "search_params.spellcheck.unwrap_or(DEFAULT_SPELLCHECK)",
        ),
        ("start_index", "active.start_index"),
        ("start_index", "citation.start_index"),
        ("status_code", "200_u16"),
        ("status_code", "e.http_status_code()"),
        ("status_code", "error_cause.http_status_code()"),
        ("status_code", "status.as_u16()"),
        ("status_code", "status_code.as_u16()"),
        ("status_code", "status_code"),
        ("tag_name", "tag_name.as_str()"),
        ("text_len", "text.len()"),
        (
            "threshold_mode",
            "search_params.context_threshold_mode.as_deref().unwrap_or(DEFAULT_THRESHOLD_MODE)",
        ),
        (
            "threshold_mode",
            r#"threshold_mode.as_deref().unwrap_or("balanced")"#,
        ),
        ("title_length", "title.len()"),
        ("token_buffer_len", "self.token_buffer.len()"),
        ("tool_call_count", "stream_result.tool_calls.len()"),
        ("tool_call_id", "tool_call_id"),
        ("tool_count", "all_tools.len()"),
        ("tool_count", "cached.len()"),
        ("tool_name", "WEB_CONTEXT_SEARCH_TOOL_NAME"),
        ("tool_name", "WEB_SEARCH_TOOL_NAME"),
        ("tool_result_message_count", "messages.len()"),
        ("tool_type", r#""mcp""#),
        ("total_snippet_chars", "total_snippet_chars"),
        (
            "total_tokens",
            "ctx.total_input_tokens+ctx.total_output_tokens",
        ),
        ("truncated", "title.len()>60"),
        ("workspace_id", "auth.workspace.id.0.to_string()"),
    ];

    if field == Some("target") {
        return is_string_literal(expression);
    }

    let expression = normalize_expression(expression);
    field.is_some_and(|field| {
        ALLOWED_PLAIN_FIELDS
            .iter()
            .any(|(allowed_field, allowed_expression)| {
                field == *allowed_field && expression == *allowed_expression
            })
    })
}

fn is_allowed_rendered_field(field: Option<&str>, renderer: u8, expression: &str) -> bool {
    // Rendering values with `%` or `?` bypasses the normal `Value`-trait path.
    // Keep the exceptional IDs and bounded request measurements explicit, so an
    // aliased request, response, error, or hash cannot silently re-enter logs.
    const ALLOWED_RENDERED_FIELDS: &[(&str, u8, &str)] = &[
        ("approval_id", b'%', "approval_id"),
        ("call_id", b'%', "call_id"),
        ("conversation_id", b'%', "conversation_id"),
        ("file_id", b'%', "file_uuid"),
        ("item_id", b'%', "message_item_id"),
        ("item_id", b'%', "reasoning_id"),
        ("item_id", b'%', "reasoning_item_id"),
        ("model", b'%', "_context.request.model"),
        ("model", b'%', "context.request.model"),
        ("model", b'%', "model"),
        ("model", b'%', "model.model_name"),
        ("requested_count", b'?', "requested_count"),
        ("requested_max_snippets", b'?', "requested_max_snippets"),
        (
            "requested_max_snippets_per_url",
            b'?',
            "requested_max_snippets_per_url",
        ),
        ("requested_max_tokens", b'?', "requested_max_tokens"),
        (
            "requested_max_tokens_per_url",
            b'?',
            "requested_max_tokens_per_url",
        ),
        ("requested_max_urls", b'?', "requested_max_urls"),
        ("requested_spellcheck", b'?', "requested_spellcheck"),
        ("response_id", b'%', "ctx.response_id_str"),
        ("response_id", b'%', "event_ctx.stream_ctx.response_id_str"),
        ("response_id", b'%', "response_id"),
        ("response_id", b'%', "rid"),
        ("response_id", b'%', "self.stream_ctx.response_id_str"),
        ("tool_call_id", b'%', "self.tool_call_id"),
        ("user_id", b'%', "api_key.api_key.created_by_user_id.0"),
    ];

    let expression = normalize_expression(expression);
    field.is_some_and(|field| {
        ALLOWED_RENDERED_FIELDS.iter().any(
            |(allowed_field, allowed_renderer, allowed_expression)| {
                field == *allowed_field
                    && renderer == *allowed_renderer
                    && expression == *allowed_expression
            },
        )
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
        "e",
        "err",
        "error",
        "event",
        "filename",
        "hash",
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
    // `status` was previously emitted as both a numeric HTTP status and a
    // string response lifecycle state. Keep the two telemetry types separate
    // so OTel/Datadog can index them consistently.
    const AMBIGUOUS_FIELDS: &[&str] = &["status"];
    let mut violations = Vec::new();

    for (source_path, source) in response_log_sources().expect("load Responses log sources") {
        let source_name = source_path.display();
        let invocations = tracing_invocations(&source).unwrap_or_else(|error| {
            panic!("Failed to parse {source_name} tracing invocation: {error}")
        });

        for invocation in invocations {
            for field in PROHIBITED_FIELDS {
                if contains_field(invocation, field) {
                    violations.push(format!(
                        "{source_name} tracing invocation exposes prohibited field `{field}`: {invocation}"
                    ));
                }
            }
            if format_message_contains_interpolation(invocation) {
                violations.push(format!(
                    "{source_name} tracing invocation interpolates a runtime value into its message: {invocation}"
                ));
            }
            for (field, renderer, expression) in rendered_fields(invocation) {
                if !is_allowed_rendered_field(field, renderer, expression) {
                    violations.push(format!(
                        "{source_name} tracing invocation renders an unapproved value `{}`{}: {invocation}",
                        field.unwrap_or("<unlabeled>"),
                        if expression.is_empty() {
                            String::new()
                        } else {
                            format!(" = {}{}", renderer as char, expression)
                        },
                    ));
                }
            }
            for (field, expression) in plain_fields(invocation).unwrap_or_else(|error| {
                panic!("Failed to parse {source_name} tracing fields: {error}")
            }) {
                if !is_allowed_plain_field(field, expression) {
                    violations.push(format!(
                        "{source_name} tracing invocation records an unapproved plain field `{}` = `{expression}`: {invocation}",
                        field.unwrap_or("<unlabeled>"),
                    ));
                }
            }
            for field in PROHIBITED_DERIVED_FIELDS {
                if invocation.contains(field) {
                    violations.push(format!(
                        "{source_name} tracing invocation exposes a request-derived field `{field}`: {invocation}"
                    ));
                }
            }
            for field in AMBIGUOUS_FIELDS {
                if contains_field(invocation, field) {
                    violations.push(format!(
                        "{source_name} tracing invocation uses ambiguous field `{field}`; use `status_code` or `response_status`: {invocation}"
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

#[test]
fn tracing_privacy_parser_rejects_interpolation_and_unapproved_fields() {
    assert!(format_message_contains_interpolation(
        r#"tracing::error!("Failed to parse response: {cause}")"#
    ));
    assert!(format_message_contains_interpolation(
        r#"tracing::error!("Failed to parse response: {}", create_err)"#
    ));
    assert!(format_message_contains_interpolation(
        r##"tracing::error!(r#"Failed to parse response: {update_err:?}"#)"##
    ));
    assert!(!format_message_contains_interpolation(
        r#"tracing::debug!("literal braces: {{ok}}")"#
    ));
    let (field, renderer, expression) =
        rendered_fields(r#"tracing::info!(payload = ?request, "received")"#)
            .into_iter()
            .next()
            .expect("payload field");
    assert!(!is_allowed_rendered_field(field, renderer, expression));

    let (field, renderer, expression) =
        rendered_fields(r#"tracing::info!(response_id = ?request, "received")"#)
            .into_iter()
            .next()
            .expect("aliased response_id field");
    assert!(!is_allowed_rendered_field(field, renderer, expression));

    let (field, renderer, expression) =
        rendered_fields(r#"tracing::info!(response_id = %rid, "received")"#)
            .into_iter()
            .next()
            .expect("approved response id");
    assert!(is_allowed_rendered_field(field, renderer, expression));

    let (field, expression) = plain_fields(r#"tracing::info!(payload = prompt, "received")"#)
        .expect("valid plain field")
        .into_iter()
        .next()
        .expect("payload field");
    assert!(!is_allowed_plain_field(field, expression));

    let (field, expression) = plain_fields(r#"tracing::info!(response_id = prompt, "received")"#)
        .expect("valid aliased plain field")
        .into_iter()
        .next()
        .expect("aliased response id field");
    assert!(!is_allowed_plain_field(field, expression));

    let (field, expression) = plain_fields(r#"tracing::info!(status_code = 200_u16, "done")"#)
        .expect("valid approved plain field")
        .into_iter()
        .next()
        .expect("status code field");
    assert!(is_allowed_plain_field(field, expression));

    let (field, expression) = plain_fields(r#"tracing::info!(target: prompt, "received")"#)
        .expect("valid dynamic target")
        .into_iter()
        .next()
        .expect("target");
    assert!(!is_allowed_plain_field(field, expression));

    let invocations = tracing_invocations(
        r##"
                // tracing::error!("comment")
                tracing::error!(r#"literal with ) and {error}"#);
                tracing::info_span!("response", payload = ?request);
                tracing::debug_span!("response", payload = ?request);
                tracing::info! { payload = prompt, "received" };
                tracing::info![payload = prompt, "received"];
                tracing::info_span! { "response", payload = prompt };
                tracing::debug_span! { "response", payload = prompt };
            "##,
    )
    .expect("valid tracing invocation");
    assert_eq!(
        invocations,
        vec![
            r##"tracing::error!(r#"literal with ) and {error}"#)"##,
            r#"tracing::info_span!("response", payload = ?request)"#,
            r#"tracing::debug_span!("response", payload = ?request)"#,
            r#"tracing::info! { payload = prompt, "received" }"#,
            r#"tracing::info![payload = prompt, "received"]"#,
            r#"tracing::info_span! { "response", payload = prompt }"#,
            r#"tracing::debug_span! { "response", payload = prompt }"#,
        ]
    );
    let (field, renderer, expression) = rendered_fields(invocations[1])
        .into_iter()
        .next()
        .expect("info span payload field");
    assert!(!is_allowed_rendered_field(field, renderer, expression));
    let (field, renderer, expression) = rendered_fields(invocations[2])
        .into_iter()
        .next()
        .expect("debug span payload field");
    assert!(!is_allowed_rendered_field(field, renderer, expression));
    for invocation in &invocations[3..] {
        let (field, expression) = plain_fields(invocation)
            .expect("valid non-parenthesized tracing invocation")
            .into_iter()
            .next()
            .expect("plain payload field");
        assert_eq!(field, Some("payload"));
        assert_eq!(expression, "prompt");
        assert!(!is_allowed_plain_field(field, expression));
    }
    assert!(tracing_invocations(r#"tracing::error!("missing closing parenthesis"#).is_err());
    assert!(tracing_invocations(r#"tracing::error! { missing_closing_brace"#).is_err());
}
