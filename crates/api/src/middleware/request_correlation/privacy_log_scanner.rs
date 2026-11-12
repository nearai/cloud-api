pub(super) fn assert_production_logs_exclude_forbidden_expressions() {
    let sources = [
        (
            "crates/api/src/routes/completions/mod.rs",
            include_str!("../../routes/completions/mod.rs"),
        ),
        (
            "crates/api/src/routes/completions/audio.rs",
            include_str!("../../routes/completions/audio.rs"),
        ),
        (
            "crates/api/src/routes/completions/billing.rs",
            include_str!("../../routes/completions/billing.rs"),
        ),
        (
            "crates/api/src/routes/completions/embeddings.rs",
            include_str!("../../routes/completions/embeddings.rs"),
        ),
        (
            "crates/api/src/routes/completions/images.rs",
            include_str!("../../routes/completions/images.rs"),
        ),
        (
            "crates/api/src/routes/completions/privacy.rs",
            include_str!("../../routes/completions/privacy.rs"),
        ),
        (
            "crates/api/src/routes/completions/privacy/signatures.rs",
            include_str!("../../routes/completions/privacy/signatures.rs"),
        ),
        (
            "crates/api/src/routes/completions/rerank.rs",
            include_str!("../../routes/completions/rerank.rs"),
        ),
        (
            "crates/api/src/routes/completions/score.rs",
            include_str!("../../routes/completions/score.rs"),
        ),
        (
            "crates/api/src/routes/completions/chat.rs",
            include_str!("../../routes/completions/chat.rs"),
        ),
        (
            "crates/api/src/routes/completions/chat_auto_redact.rs",
            include_str!("../../routes/completions/chat_auto_redact.rs"),
        ),
        (
            "crates/api/src/routes/completions/chat_stream.rs",
            include_str!("../../routes/completions/chat_stream.rs"),
        ),
        (
            "crates/api/src/routes/completions/models.rs",
            include_str!("../../routes/completions/models.rs"),
        ),
        (
            "crates/api/src/routes/completions/text.rs",
            include_str!("../../routes/completions/text.rs"),
        ),
        (
            "crates/api/src/routes/completions/tests.rs",
            include_str!("../../routes/completions/tests.rs"),
        ),
        (
            "crates/api/src/routes/responses.rs",
            include_str!("../../routes/responses.rs"),
        ),
        (
            "crates/api/src/routes/admin.rs",
            include_str!("../../routes/admin.rs"),
        ),
        (
            "crates/api/src/routes/auth.rs",
            include_str!("../../routes/auth.rs"),
        ),
        (
            "crates/api/src/routes/conversations.rs",
            include_str!("../../routes/conversations.rs"),
        ),
        (
            "crates/api/src/routes/users.rs",
            include_str!("../../routes/users.rs"),
        ),
        (
            "crates/api/src/middleware/auth.rs",
            include_str!("../auth.rs"),
        ),
        (
            "crates/api/src/routes/files.rs",
            include_str!("../../routes/files.rs"),
        ),
        (
            "crates/api/src/routes/organizations.rs",
            include_str!("../../routes/organizations.rs"),
        ),
        (
            "crates/api/src/routes/workspaces.rs",
            include_str!("../../routes/workspaces.rs"),
        ),
        (
            "crates/database/src/repositories/oauth_state.rs",
            include_str!("../../../../database/src/repositories/oauth_state.rs"),
        ),
        (
            "crates/services/src/auth/oauth.rs",
            include_str!("../../../../services/src/auth/oauth.rs"),
        ),
        (
            "crates/api/src/routes/models.rs",
            include_str!("../../routes/models.rs"),
        ),
        (
            "crates/api/src/routes/ohttp.rs",
            include_str!("../../routes/ohttp.rs"),
        ),
        (
            "crates/services/src/aml.rs",
            include_str!("../../../../services/src/aml.rs"),
        ),
        (
            "crates/services/src/auth/mod.rs",
            include_str!("../../../../services/src/auth/mod.rs"),
        ),
        (
            "crates/services/src/auth/near.rs",
            include_str!("../../../../services/src/auth/near.rs"),
        ),
        (
            "crates/services/src/completions/mod.rs",
            include_str!("../../../../services/src/completions/mod.rs"),
        ),
        (
            "crates/services/src/inference_provider_pool/mod.rs",
            include_str!("../../../../services/src/inference_provider_pool/mod.rs"),
        ),
    ];
    let forbidden = [
        "json_data",
        "event.delta",
        "admin_token.name",
        "workspace.name",
        "organization.name",
        "response_body",
        "request_body",
        "content_preview",
        "tool_arguments",
    ];

    // Splitting a route file must not silently drop the new files from this scan.
    let completions_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/routes/completions");
    assert_directory_is_covered(
        std::path::Path::new(completions_dir),
        "crates/api/src/routes/completions",
        &sources,
    );

    // Files where error values are logged as a kind, status or length only.
    let formatted_error_sources = [
        "crates/api/src/routes/models.rs",
        "crates/api/src/routes/workspaces.rs",
        "crates/services/src/aml.rs",
        "crates/services/src/auth/mod.rs",
        "crates/services/src/auth/near.rs",
        "crates/services/src/auth/oauth.rs",
        "crates/services/src/inference_provider_pool/mod.rs",
    ];

    for (path, source) in sources {
        for (line, body) in logging_macro_bodies(source) {
            let code = string_literals_removed(&body);
            for expression in forbidden {
                assert!(
                    !body.contains(expression),
                    "{path}:{line} production logging macro must not contain {expression}: {body}"
                );
            }
            for identifier in [
                "auth_value",
                "bearer_token",
                "session_token",
                "raw_signature",
                "signature_body",
            ] {
                assert!(
                    !contains_standalone_identifier(&code, identifier),
                    "{path}:{line} production logging macro must not contain raw auth identifier {identifier}: {body}"
                );
            }
            for identifier in ["user_agent", "user_agent_header"] {
                assert!(
                    !logs_raw_identifier(&code, identifier),
                    "{path}:{line} production logging macro must not log raw User-Agent identifier {identifier}: {body}"
                );
            }
            assert!(
                !logs_raw_oauth_state(&code),
                "{path}:{line} production logging macro must not log raw OAuth state: {body}"
            );
            assert!(
                !logs_api_key_debug_dump(&body, &code),
                "{path}:{line} production logging macro must not dump full ApiKey with Debug: {body}"
            );
            assert!(
                !logs_raw_email_field(&code),
                "{path}:{line} production logging macro must not log raw email fields: {body}"
            );
            assert!(
                !logs_name_field(&code),
                "{path}:{line} logging macro must log name fields as a length: {body}"
            );
            assert!(
                !logs_unbounded_error_field(&code),
                "{path}:{line} logging macro must use bounded error fields: {body}"
            );
            assert!(
                !(formatted_error_sources.contains(&path) && logs_formatted_error_value(&body)),
                "{path}:{line} logging macro must log an error kind, status or length: {body}"
            );
        }
    }
}

fn assert_directory_is_covered(dir: &std::path::Path, label: &str, sources: &[(&str, &str)]) {
    for entry in std::fs::read_dir(dir).expect("read production source directory") {
        let entry = entry.expect("production source entry");
        let file_name = entry.file_name().into_string().expect("utf-8 file name");
        let label = format!("{label}/{file_name}");
        if entry
            .file_type()
            .expect("production source entry type")
            .is_dir()
        {
            assert_directory_is_covered(&entry.path(), &label, sources);
        } else if file_name.ends_with(".rs") {
            assert!(
                sources.iter().any(|(path, _)| *path == label),
                "{label} is not covered by the production log privacy scan; add it to `sources`"
            );
        }
    }
}

fn logging_macro_bodies(source: &str) -> Vec<(usize, String)> {
    let prefixes = [
        "tracing::debug!(",
        "tracing::info!(",
        "tracing::warn!(",
        "tracing::error!(",
        "tracing::trace!(",
        "debug!(",
        "info!(",
        "warn!(",
        "error!(",
        "trace!(",
    ];
    let mut bodies = Vec::new();
    let mut search_start = 0;

    while let Some((start, prefix_len)) = next_logging_macro(source, search_start, &prefixes) {
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        if let Some((body, end)) = read_parenthesized_body(source, start + prefix_len) {
            bodies.push((line, body));
            search_start = end;
        } else {
            search_start = start + prefix_len;
        }
    }

    bodies
}

fn next_logging_macro(
    source: &str,
    search_start: usize,
    prefixes: &[&str],
) -> Option<(usize, usize)> {
    prefixes
        .iter()
        .filter_map(|prefix| {
            source[search_start..]
                .find(prefix)
                .map(|offset| (search_start + offset, prefix.len()))
        })
        .min_by_key(|(start, _)| *start)
}

fn read_parenthesized_body(source: &str, body_start: usize) -> Option<(String, usize)> {
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in source[body_start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let end = body_start + offset;
                    return Some((source[body_start..end].to_string(), end + 1));
                }
            }
            _ => {}
        }
    }

    None
}

fn string_literals_removed(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut in_string = false;
    let mut escaped = false;

    for ch in body.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            result.push(' ');
            continue;
        }

        if ch == '"' {
            in_string = true;
            result.push(' ');
        } else {
            result.push(ch);
        }
    }

    result
}

fn logs_raw_oauth_state(code: &str) -> bool {
    code.contains("params.state")
        || code.contains("state.state")
        || has_positional_argument_named(code, "state")
}

fn logs_api_key_debug_dump(body: &str, code: &str) -> bool {
    body.contains("{:?}") && has_positional_argument_named(code, "api_key")
}

fn logs_raw_email_field(code: &str) -> bool {
    code.match_indices(".email").any(|(start, _)| {
        let after = &code[start + ".email".len()..];
        if after
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return false;
        }

        let after = after.trim_start();
        !(after.starts_with(".len(") || after.starts_with(".is_empty("))
    })
}

/// Error fields that must not be logged; use the error kind, status or length.
fn logs_unbounded_error_field(code: &str) -> bool {
    ["error_detail", "provider_message", "detail"]
        .into_iter()
        .any(|field| assigns_field(code, field))
}

fn assigns_field(code: &str, field: &str) -> bool {
    code.match_indices(field).any(|(start, _)| {
        let before = start
            .checked_sub(1)
            .and_then(|index| code.as_bytes().get(index))
            .copied();
        if is_identifier_byte(before) || before == Some(b'.') {
            return false;
        }
        let after = code[start + field.len()..].trim_start();
        after.starts_with('=') && !after.starts_with("==")
    })
}

/// Error values logged as a field, a `%`/`?` shorthand, a positional argument
/// or an inline format argument.
fn logs_formatted_error_value(body: &str) -> bool {
    let code = string_literals_removed(body);
    let code = code.as_str();
    ["error", "err", "e"].into_iter().any(|name| {
        body.contains(&format!("{{{name}}}"))
            || body.contains(&format!("{{{name}:"))
            || assigns_non_literal(code, name)
            || has_sigil_identifier(code, name)
            || code.split(',').skip(1).any(|argument| {
                let argument = argument.trim().trim_start_matches('&');
                argument == name
                    || argument.strip_prefix(name).is_some_and(|tail| {
                        tail.starts_with(')') || (tail.starts_with('.') && tail != ".len()")
                    })
            })
    })
}

fn assigns_non_literal(code: &str, field: &str) -> bool {
    code.match_indices(field).any(|(start, _)| {
        let before = start
            .checked_sub(1)
            .and_then(|index| code.as_bytes().get(index))
            .copied();
        if is_identifier_byte(before) || before == Some(b'.') {
            return false;
        }
        let after = code[start + field.len()..].trim_start();
        let Some(value) = after.strip_prefix('=') else {
            return false;
        };
        if value.starts_with('=') {
            return false;
        }
        let value = value.trim_start();
        !(value.is_empty() || value.starts_with(','))
    })
}

fn has_sigil_identifier(code: &str, name: &str) -> bool {
    code.match_indices(name).any(|(start, _)| {
        let before = start
            .checked_sub(1)
            .and_then(|index| code.as_bytes().get(index))
            .copied();
        let after = code.as_bytes().get(start + name.len()).copied();
        matches!(before, Some(b'%' | b'?')) && !is_identifier_byte(after)
    })
}

/// Name fields that may only be logged as a length.
fn logs_name_field(code: &str) -> bool {
    ["request.name", ".filename", "provider_user_id"]
        .into_iter()
        .any(|pattern| {
            code.match_indices(pattern).any(|(start, _)| {
                let after = &code[start + pattern.len()..];
                if is_identifier_byte(after.as_bytes().first().copied()) {
                    return false;
                }
                let after = after.trim_start();
                !(after.starts_with(".len(") || after.starts_with(".is_empty("))
            })
        })
}

fn logs_raw_identifier(code: &str, identifier: &str) -> bool {
    has_positional_argument_named(code, identifier) || has_raw_prefixed_identifier(code, identifier)
}

fn has_raw_prefixed_identifier(code: &str, identifier: &str) -> bool {
    code.match_indices(identifier).any(|(start, _)| {
        let before = code[..start]
            .bytes()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace());
        let after = code[start + identifier.len()..]
            .bytes()
            .find(|byte| !byte.is_ascii_whitespace());

        before.is_some_and(|byte| matches!(byte, b'=' | b'?' | b'%'))
            && !matches!(after, Some(byte) if is_identifier_byte(Some(byte)) || byte == b'.')
    })
}

fn has_positional_argument_named(code: &str, identifier: &str) -> bool {
    code.split(',').skip(1).any(|argument| {
        let trimmed = argument.trim();
        trimmed == identifier
            || trimmed
                .strip_prefix(identifier)
                .is_some_and(|tail| tail.trim_start().starts_with(')'))
    })
}

fn contains_standalone_identifier(body: &str, identifier: &str) -> bool {
    body.match_indices(identifier).any(|(start, _)| {
        let before = start
            .checked_sub(1)
            .and_then(|index| body.as_bytes().get(index))
            .copied();
        let after = body.as_bytes().get(start + identifier.len()).copied();

        !is_identifier_byte(before) && !is_identifier_byte(after)
    })
}

fn is_identifier_byte(byte: Option<u8>) -> bool {
    byte.is_some_and(|value| value.is_ascii_alphanumeric() || value == b'_')
}

#[test]
fn error_fields_must_be_bounded() {
    for line in [
        "model_id = %model_id, error_detail = %e, \"value-a\"",
        "status_code, provider_message = %msg, \"value-a\"",
        "status = 400, detail = %message, \"value-a\"",
        "detail=%message, \"value-a\"",
    ] {
        assert!(
            logs_unbounded_error_field(&string_literals_removed(line)),
            "expected a match: {line}"
        );
    }
    for line in [
        "status_code, error_kind, error_message_len = message.len(), \"value-a\"",
        "\"detail = %message\"",
        "has_detail = true, \"value-a\"",
        "if request.detail == expected",
    ] {
        assert!(
            !logs_unbounded_error_field(&string_literals_removed(line)),
            "expected no match: {line}"
        );
    }
}

#[test]
fn name_fields_must_be_lengths() {
    for line in [
        "\"value-a: {}\", request.name",
        "filename = %params.filename, \"value-a\"",
        "provider_user_id = %item.provider_user_id, \"value-a\"",
    ] {
        assert!(
            logs_name_field(&string_literals_removed(line)),
            "expected a match: {line}"
        );
    }
    for line in [
        "filename_len = params.filename.len(), \"value-a\"",
        "name_len = request.name.len(), \"value-a\"",
        "user_id = %item.id, \"filename provider_user_id\"",
    ] {
        assert!(
            !logs_name_field(&string_literals_removed(line)),
            "expected no match: {line}"
        );
    }
}

#[test]
fn error_values_must_use_kinds() {
    for line in [
        "model = %model, error = %e, \"value-a\"",
        "error = ?err, \"value-a\"",
        "error=%e.without_url(), \"value-a\"",
        "%item_id, %error, \"value-a\"",
        "?e, \"value-a\"",
        "\"value-a: {}\", e",
        "\"value-a: {}\", err.to_string()",
        "\"value-a: {:?}\", &error",
        "\"value-a: {e}\"",
        "\"value-a: {err:?}\"",
    ] {
        assert!(logs_formatted_error_value(line), "expected a match: {line}");
    }
    for line in [
        "status_code, error_kind = Self::kind(&e), error_message_len = e.len(), \"value-a\"",
        "error_category = %category, \"value-a\"",
        "error = \"value-a\", \"value-b\"",
        "\"error = %e\"",
        "expired = matches!(e, Kind::A), \"value-a\"",
        "\"value-a: {}\", item.error_count",
        "\"value-a: {}\", e.len()",
        "\"value-a: {error_count}\"",
    ] {
        assert!(
            !logs_formatted_error_value(line),
            "expected no match: {line}"
        );
    }
}
