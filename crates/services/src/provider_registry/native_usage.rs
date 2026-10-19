//! Bounded, best-effort procurement observation. Parsing never changes client
//! bytes; invalid or oversized usage remains unknown and is never zero-priced.
use inference_providers::TokenUsage;
use serde_json::{json, Value};

const LIMIT: usize = 16 * 1024 * 1024;

pub(crate) struct NativeUsage {
    anthropic: bool,
    streaming: bool,
    buffer: Vec<u8>,
    invalid: bool,
    input: Option<i32>,
    output: Option<i32>,
    read: i32,
    written: i32,
    pub usage: Option<TokenUsage>,
    pub outcome: Option<&'static str>,
}

impl NativeUsage {
    pub fn new(anthropic: bool, streaming: bool) -> Self {
        Self {
            anthropic,
            streaming,
            buffer: Vec::new(),
            invalid: false,
            input: None,
            output: None,
            read: 0,
            written: 0,
            usage: None,
            outcome: None,
        }
    }

    fn invalidate(&mut self) {
        self.invalid = true;
        self.buffer.clear();
        self.usage = None;
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.invalid {
            return;
        }
        if !self.streaming {
            if self.buffer.len().saturating_add(bytes.len()) > LIMIT {
                self.invalidate();
            } else {
                self.buffer.extend_from_slice(bytes);
            }
            return;
        }
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            if self.buffer.len().saturating_add(segment.len()) > LIMIT {
                self.invalidate();
                return;
            }
            self.buffer.extend_from_slice(segment);
            if self.buffer.last() == Some(&b'\n') {
                self.flush();
            }
            if self.invalid {
                return;
            }
        }
    }

    pub fn eof(&mut self) {
        if self.invalid {
            return;
        }
        if self.streaming {
            self.flush();
        } else {
            let bytes = std::mem::take(&mut self.buffer);
            match serde_json::from_slice(&bytes)
                .ok()
                .and_then(|body| self.apply(&body))
            {
                Some(()) => {}
                None => self.invalidate(),
            }
        }
    }

    fn flush(&mut self) {
        let bytes = std::mem::take(&mut self.buffer);
        let Some(data) = bytes.strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii();
        if data == b"[DONE]" {
            return;
        }
        if serde_json::from_slice(data)
            .ok()
            .and_then(|body| self.apply(&body))
            .is_none()
        {
            self.invalidate();
        }
    }

    fn apply(&mut self, body: &Value) -> Option<()> {
        if body["type"] == "error" {
            self.outcome = Some("provider_error");
            return Some(());
        }
        let response = if self.anthropic {
            body.get("message").unwrap_or(body)
        } else {
            body.get("response").unwrap_or(body)
        };
        if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
            if !usage.is_object() {
                return None;
            }
            let count = |key: &str| -> Option<Option<i32>> {
                match usage.get(key) {
                    None => Some(None),
                    Some(value) => Some(Some(i32::try_from(value.as_u64()?).ok()?)),
                }
            };
            if let Some(input) = count("input_tokens")? {
                self.input = Some(input);
            }
            if let Some(output) = count("output_tokens")? {
                self.output = Some(output);
            }
            if self.anthropic {
                if let Some(read) = count("cache_read_input_tokens")? {
                    self.read = read;
                }
                if let Some(written) = count("cache_creation_input_tokens")? {
                    self.written = written;
                }
            }
            if let (Some(input), Some(output)) = (self.input, self.output) {
                let input = if self.anthropic {
                    input.checked_add(self.read)?.checked_add(self.written)?
                } else {
                    input
                };
                self.usage = Some(TokenUsage {
                    prompt_tokens: input,
                    completion_tokens: output,
                    total_tokens: input.checked_add(output)?,
                    prompt_tokens_details: if self.anthropic {
                        Some(
                            json!({"cached_tokens":self.read,"cache_creation_tokens":self.written}),
                        )
                    } else {
                        usage.get("input_tokens_details").cloned()
                    },
                    ..Default::default()
                });
            }
        }
        if self.anthropic {
            if body["type"] == "message_stop"
                || (!self.streaming && response["stop_reason"].is_string())
            {
                self.outcome = Some("success");
            }
        } else {
            self.outcome = match response["status"].as_str() {
                Some("completed") => Some("success"),
                Some("incomplete") => Some("incomplete"),
                Some("failed" | "cancelled") => Some("provider_error"),
                _ => self.outcome,
            };
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_native_usage_keeps_cache_and_terminal_status() {
        let bytes = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4,\"cache_read_input_tokens\":20,\"cache_creation_input_tokens\":3}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut parser = NativeUsage::new(true, true);
        for byte in bytes {
            parser.push(&[*byte]);
        }
        parser.eof();
        let usage = parser.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.cached_tokens(),
                usage.cache_creation_tokens()
            ),
            (27, 9, 20, 3)
        );
        assert_eq!(parser.outcome, Some("success"));
        let mut parser = NativeUsage::new(false, true);
        parser.push(b"data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n");
        parser.eof();
        assert_eq!(parser.outcome, Some("incomplete"));
        assert_eq!(parser.usage.unwrap().cached_tokens(), 4);
    }

    #[test]
    fn invalid_and_oversized_usage_stays_unknown() {
        let mut parser = NativeUsage::new(false, false);
        parser.push(br#"{"status":"completed","usage":{"input_tokens":-1,"output_tokens":2}}"#);
        parser.eof();
        assert!(parser.usage.is_none());
        let mut parser = NativeUsage::new(false, false);
        parser.push(&vec![b' '; LIMIT + 1]);
        assert!(parser.invalid);
        assert!(parser.buffer.is_empty());
        assert!(parser.usage.is_none());
    }
}
