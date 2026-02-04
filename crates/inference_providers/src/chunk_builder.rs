//! Shared chunk builder for creating OpenAI-compatible streaming chunks
//!
//! All external backends (Anthropic, Gemini, OpenAI-compatible) convert their
//! native streaming formats to OpenAI-compatible `ChatCompletionChunk`. This
//! module provides helper functions to reduce duplication across backends.

use crate::{
    ChatChoice, ChatCompletionChunk, ChatDelta, FinishReason, FunctionCallDelta, MessageRole,
    TokenUsage, ToolCallDelta,
};

/// Parameters common to all chunks in a streaming response
#[derive(Debug, Clone)]
pub struct ChunkContext {
    pub id: String,
    pub model: String,
    pub created: i64,
}

impl ChunkContext {
    pub fn new(id: String, model: String, created: i64) -> Self {
        Self { id, model, created }
    }

    /// Create a chunk with the given delta, finish reason, and usage
    fn build(
        &self,
        delta: ChatDelta,
        finish_reason: Option<FinishReason>,
        usage: Option<TokenUsage>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            system_fingerprint: None,
            choices: vec![ChatChoice {
                index: 0,
                delta: Some(delta),
                logprobs: None,
                finish_reason,
                token_ids: None,
            }],
            usage,
            prompt_token_ids: None,
            modality: None,
        }
    }

    /// Create a chunk with assistant role (typically first chunk)
    pub fn role_chunk(&self) -> ChatCompletionChunk {
        self.build(
            ChatDelta {
                role: Some(MessageRole::Assistant),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
            },
            None,
            None,
        )
    }

    /// Create a chunk with text content
    pub fn text_chunk(&self, text: String) -> ChatCompletionChunk {
        self.build(
            ChatDelta {
                role: None,
                content: Some(text),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
            },
            None,
            None,
        )
    }

    /// Create a chunk starting a tool call (with id and function name)
    pub fn tool_call_start_chunk(
        &self,
        index: i64,
        tool_call_id: String,
        function_name: String,
    ) -> ChatCompletionChunk {
        self.build(
            ChatDelta {
                role: None,
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCallDelta {
                    index: Some(index),
                    id: Some(tool_call_id),
                    type_: Some("function".to_string()),
                    function: Some(FunctionCallDelta {
                        name: Some(function_name),
                        arguments: None,
                    }),
                    thought_signature: None,
                }]),
                reasoning_content: None,
                reasoning: None,
            },
            None,
            None,
        )
    }

    /// Create a chunk with tool call argument delta
    pub fn tool_call_args_chunk(&self, index: i64, arguments: String) -> ChatCompletionChunk {
        self.build(
            ChatDelta {
                role: None,
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCallDelta {
                    index: Some(index),
                    id: None,
                    type_: None,
                    function: Some(FunctionCallDelta {
                        name: None,
                        arguments: Some(arguments),
                    }),
                    thought_signature: None,
                }]),
                reasoning_content: None,
                reasoning: None,
            },
            None,
            None,
        )
    }

    /// Create a chunk with complete tool calls (for providers like Gemini that don't stream tool calls)
    pub fn tool_calls_chunk(
        &self,
        tool_calls: Vec<crate::ToolCall>,
        finish_reason: Option<FinishReason>,
        usage: Option<TokenUsage>,
    ) -> ChatCompletionChunk {
        // Convert ToolCall to ToolCallDelta
        let deltas: Vec<ToolCallDelta> = tool_calls
            .into_iter()
            .map(|tc| ToolCallDelta {
                index: tc.index,
                id: tc.id,
                type_: tc.type_,
                function: Some(FunctionCallDelta {
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                }),
                thought_signature: tc.thought_signature,
            })
            .collect();

        self.build(
            ChatDelta {
                role: None,
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(deltas),
                reasoning_content: None,
                reasoning: None,
            },
            finish_reason,
            usage,
        )
    }

    /// Create a finish chunk with reason and usage stats
    pub fn finish_chunk(
        &self,
        finish_reason: Option<FinishReason>,
        usage: TokenUsage,
    ) -> ChatCompletionChunk {
        self.build(
            ChatDelta {
                role: None,
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
            },
            finish_reason,
            Some(usage),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> ChunkContext {
        ChunkContext::new("test-id".to_string(), "test-model".to_string(), 1234567890)
    }

    #[test]
    fn test_role_chunk() {
        let ctx = test_context();
        let chunk = ctx.role_chunk();

        assert_eq!(chunk.id, "test-id");
        assert_eq!(chunk.model, "test-model");
        assert_eq!(chunk.created, 1234567890);
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(
            chunk.choices[0].delta.as_ref().unwrap().role,
            Some(MessageRole::Assistant)
        );
        assert!(chunk.choices[0].delta.as_ref().unwrap().content.is_none());
    }

    #[test]
    fn test_text_chunk() {
        let ctx = test_context();
        let chunk = ctx.text_chunk("Hello world".to_string());

        assert_eq!(
            chunk.choices[0].delta.as_ref().unwrap().content,
            Some("Hello world".to_string())
        );
        assert!(chunk.choices[0].delta.as_ref().unwrap().role.is_none());
    }

    #[test]
    fn test_tool_call_start_chunk() {
        let ctx = test_context();
        let chunk = ctx.tool_call_start_chunk(0, "call_123".to_string(), "web_search".to_string());

        let delta = chunk.choices[0].delta.as_ref().unwrap();
        let tool_calls = delta.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].index, Some(0));
        assert_eq!(tool_calls[0].id, Some("call_123".to_string()));
        assert_eq!(tool_calls[0].type_, Some("function".to_string()));
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().name,
            Some("web_search".to_string())
        );
    }

    #[test]
    fn test_tool_call_args_chunk() {
        let ctx = test_context();
        let chunk = ctx.tool_call_args_chunk(0, r#"{"query":"test"}"#.to_string());

        let delta = chunk.choices[0].delta.as_ref().unwrap();
        let tool_calls = delta.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].index, Some(0));
        assert!(tool_calls[0].id.is_none());
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().arguments,
            Some(r#"{"query":"test"}"#.to_string())
        );
    }

    #[test]
    fn test_finish_chunk() {
        let ctx = test_context();
        let usage = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            prompt_tokens_details: None,
        };
        let chunk = ctx.finish_chunk(Some(FinishReason::Stop), usage);

        assert_eq!(chunk.choices[0].finish_reason, Some(FinishReason::Stop));
        assert!(chunk.usage.is_some());
        assert_eq!(chunk.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(chunk.usage.as_ref().unwrap().completion_tokens, 20);
    }
}
