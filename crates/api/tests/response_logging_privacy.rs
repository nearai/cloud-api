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

fn macro_open_paren(source: &[u8], start: usize) -> Option<usize> {
    const MACRO_NAMES: &[&str] = &["debug", "event", "info", "span", "warn", "error", "trace"];

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
            if source.get(index) == Some(&b'(') {
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

fn tracing_invocation_end(source: &str, open_paren: usize) -> Result<usize, String> {
    let bytes = source.as_bytes();
    let mut index = open_paren;
    let mut depth = 0_usize;

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)? {
            index = next;
            continue;
        }

        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    format!(
                        "unbalanced closing parenthesis in tracing invocation: {}",
                        invocation_snippet(source, open_paren)
                    )
                })?;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }

    Err(format!(
        "unterminated tracing invocation: {}",
        invocation_snippet(source, open_paren)
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

        if let Some(open_paren) = macro_open_paren(bytes, index) {
            let end = tracing_invocation_end(source, open_paren)?;
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

fn format_message_contains_capture(invocation: &str, identifier: &str) -> bool {
    let bytes = invocation.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if let Some((content_start, hashes)) = raw_string_start(bytes, index) {
            let end = skip_raw_string(bytes, index)
                .expect("tracing invocation parser accepted an invalid raw string literal");
            if is_format_message_start(bytes, index)
                && format_string_contains_capture(
                    &invocation[content_start..end - hashes - 1],
                    identifier,
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
                && format_string_contains_capture(&invocation[index + 1..end - 1], identifier)
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
        Some(b'(' | b',')
    )
}

fn format_string_contains_capture(format_string: &str, identifier: &str) -> bool {
    let pattern = format!("{{{identifier}");

    format_string.match_indices(&pattern).any(|(index, _)| {
        let opening_braces = format_string[..index]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'{')
            .count();
        let is_escaped = opening_braces % 2 == 1;
        let format_specifier = format_string[index + pattern.len()..].chars().next();

        !is_escaped && matches!(format_specifier, Some('}' | ':'))
    })
}

fn contains_positional_identifier(invocation: &str, identifier: &str) -> bool {
    let bytes = invocation.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if let Some(next) = skip_non_code(bytes, index)
            .expect("tracing invocation parser accepted invalid source syntax")
        {
            index = next;
            continue;
        }

        if bytes[index] != b',' {
            index += 1;
            continue;
        }

        let mut candidate = index + 1;
        while bytes.get(candidate).is_some_and(u8::is_ascii_whitespace) {
            candidate += 1;
        }
        while matches!(bytes.get(candidate), Some(b'&' | b'*')) {
            candidate += 1;
            while bytes.get(candidate).is_some_and(u8::is_ascii_whitespace) {
                candidate += 1;
            }
        }
        if bytes[candidate..].starts_with(identifier.as_bytes()) {
            let after = candidate + identifier.len();
            if bytes
                .get(after)
                .is_none_or(|byte| !is_identifier_byte(*byte))
            {
                return true;
            }
        }

        index += 1;
    }

    false
}

fn contains_rendered_value(invocation: &str, identifier: &str) -> bool {
    ["%", "?"]
        .into_iter()
        .any(|prefix| contains_rendered_identifier(invocation, prefix, identifier))
        || format_message_contains_capture(invocation, identifier)
        || contains_positional_identifier(invocation, identifier)
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
    const PROHIBITED_RENDERED_IDENTIFIERS: &[&str] = &["e", "err", "error", "hash"];

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
            for identifier in PROHIBITED_RENDERED_IDENTIFIERS {
                if contains_rendered_value(invocation, identifier) {
                    violations.push(format!(
                        "{source_name} tracing invocation renders sensitive value `{identifier}`: {invocation}"
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
        }
    }

    assert!(
        violations.is_empty(),
        "Responses tracing privacy violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn tracing_privacy_parser_rejects_sensitive_rendering_and_malformed_invocations() {
    assert!(contains_rendered_value(
        r#"tracing::error!("Failed to parse response: {e}")"#,
        "e"
    ));
    assert!(contains_rendered_value(
        r#"tracing::error!("Failed to parse response: {error:?}")"#,
        "error"
    ));
    assert!(contains_rendered_value(
        r#"debug!("Request body hash computed: {}", hash)"#,
        "hash"
    ));
    assert!(contains_rendered_value(
        r#"tracing::error!("Failed to parse response: {}", err)"#,
        "err"
    ));
    assert!(contains_rendered_value(
        r#"tracing::error!("Failed to parse response: {}", &e)"#,
        "e"
    ));
    assert!(contains_rendered_value(
        r##"tracing::error!(r#"Failed to parse response: {error}"#)"##,
        "error"
    ));
    assert_eq!(
        tracing_invocations(
            r##"
                // tracing::error!("comment")
                tracing::error!(r#"literal with ) and {error}"#);
            "##
        )
        .expect("valid tracing invocation"),
        vec![r##"tracing::error!(r#"literal with ) and {error}"#)"##]
    );
    assert!(tracing_invocations(r#"tracing::error!("missing closing parenthesis"#).is_err());
}
