pub(super) fn assert_production_logs_exclude_forbidden_expressions() {
    let sources = [
        (
            "crates/api/src/routes/completions.rs",
            include_str!("../../routes/completions.rs"),
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
            "crates/database/src/repositories/oauth_state.rs",
            include_str!("../../../../database/src/repositories/oauth_state.rs"),
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
