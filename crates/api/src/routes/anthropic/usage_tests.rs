use super::{SseUsageParser, MAX_SSE_USAGE_LINE_BYTES};

#[test]
fn many_data_lines_are_parsed_once_at_the_event_boundary() {
    let mut parser = SseUsageParser::default();
    parser.push(b"data: {\n");
    // A single transport chunk containing many data lines exercises both
    // bounded JSON parsing and the line scanner (no repeated Vec::drain).
    parser.push(&b"data: \n".repeat(16_384));
    parser.push(b"data: \"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n");
    assert_eq!(parser.parsed_events, 0);
    assert_eq!(parser.usage.output_tokens_for_billing(), 0);
    parser.push(b"\n");
    assert_eq!(parser.parsed_events, 1);
    assert_eq!(parser.finish().output_tokens_for_billing(), 9);
    assert_eq!(parser.parsed_events, 1);
}

#[test]
fn multiple_json_values_in_one_event_do_not_apply_partial_usage() {
    let mut parser = SseUsageParser::default();
    parser.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n");
    parser.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":99}}\n\n");
    assert_eq!(parser.parsed_events, 1);
    assert_eq!(parser.finish().output_tokens_for_billing(), 0);
}

#[test]
fn finish_flushes_multiline_usage_with_or_without_a_final_line_ending() {
    for ending in ["", "\n", "\r\n"] {
        let mut parser = SseUsageParser::default();
        parser.push(b"data: {\"type\":\"message_delta\",\ndata: \"usage\":{\"output_tokens\":9}}");
        parser.push(ending.as_bytes());
        assert_eq!(parser.parsed_events, 0);
        assert_eq!(parser.finish().output_tokens_for_billing(), 9);
        assert_eq!(parser.parsed_events, 1);
        assert_eq!(parser.finish().output_tokens_for_billing(), 0);
        assert_eq!(parser.parsed_events, 1);
    }
}

#[test]
fn oversized_event_discards_its_tail_until_the_next_separator() {
    let mut parser = SseUsageParser::default();
    parser.push(b"data: {\n");
    parser.push(&b"data: \n".repeat(MAX_SSE_USAGE_LINE_BYTES));
    assert!(parser.data_buffer.is_empty());
    assert_eq!(parser.parsed_events, 0);
    // This is still part of the oversized event, not a new usage event.
    parser.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":999}}\n\n");
    assert_eq!(parser.parsed_events, 0);
    assert_eq!(parser.usage.output_tokens_for_billing(), 0);
    parser.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n");
    assert_eq!(parser.finish().output_tokens_for_billing(), 9);
    assert_eq!(parser.parsed_events, 1);
}

#[test]
fn oversized_line_in_one_chunk_does_not_bypass_the_limit() {
    let mut parser = SseUsageParser::default();
    let mut raw = b"data: ".to_vec();
    raw.extend(vec![b' '; MAX_SSE_USAGE_LINE_BYTES]);
    raw.extend_from_slice(b"{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":999}}\n\n");
    parser.push(&raw);
    assert!(parser.line_buffer.is_empty());
    assert!(parser.data_buffer.is_empty());
    assert_eq!(parser.parsed_events, 0);
    parser.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n");
    assert_eq!(parser.finish().output_tokens_for_billing(), 9);
}

#[test]
fn invalid_first_id_cannot_be_replaced_by_a_later_message_start() {
    let mut parser = SseUsageParser::default();
    parser.push(b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"\",\"usage\":{\"input_tokens\":3}}}\n\n");
    parser.push(b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_later\",\"usage\":{\"input_tokens\":99}}}\n\n");
    assert!(parser.message_started);
    let usage = parser.finish();
    assert_eq!(usage.provider_request_id, None);
    assert_eq!(usage.input_tokens_for_billing(), 3);
}
