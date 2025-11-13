//! Helper structures and functions for the response service
//!
//! This module contains helper types that make the main service code more readable
//! by grouping related state and providing focused utility functions.

use crate::conversations::models::ConversationId;
use crate::responses::{errors, models};
use futures::channel::mpsc::UnboundedSender;
use uuid::Uuid;

/// Context for processing a response stream
///
/// This struct holds all the state needed during response processing,
/// reducing the number of parameters passed between functions.
pub struct ResponseStreamContext {
    pub response_id: models::ResponseId,
    pub api_key_id: Uuid,
    pub conversation_id: Option<ConversationId>,
    pub sequence_number: u64,
    pub output_item_index: usize,
    /// Accumulated token usage from all completion calls
    pub total_input_tokens: i32,
    pub total_output_tokens: i32,
    /// Response metadata for enriching output items
    pub response_id_str: String,
    pub previous_response_id: Option<String>,
    pub created_at: i64,
}

impl ResponseStreamContext {
    pub fn new(
        response_id: models::ResponseId,
        api_key_id: Uuid,
        conversation_id: Option<ConversationId>,
        response_id_str: String,
        previous_response_id: Option<String>,
        created_at: i64,
    ) -> Self {
        Self {
            response_id,
            api_key_id,
            conversation_id,
            sequence_number: 0,
            output_item_index: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            response_id_str,
            previous_response_id,
            created_at,
        }
    }

    /// Increment and return the current sequence number
    pub fn next_sequence(&mut self) -> u64 {
        let current = self.sequence_number;
        self.sequence_number += 1;
        current
    }

    /// Increment output item index
    pub fn next_output_index(&mut self) {
        self.output_item_index += 1;
    }

    /// Add usage from a completion call
    pub fn add_usage(&mut self, input_tokens: i32, output_tokens: i32) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
    }
}

/// Helper for emitting stream events
pub struct EventEmitter {
    pub(crate) tx: UnboundedSender<models::ResponseStreamEvent>,
}

impl EventEmitter {
    pub fn new(tx: UnboundedSender<models::ResponseStreamEvent>) -> Self {
        Self { tx }
    }

    /// Emit response.created event
    pub async fn emit_created(
        &mut self,
        ctx: &mut ResponseStreamContext,
        response: models::ResponseObject,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.created".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: Some(response),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit response.in_progress event
    pub async fn emit_in_progress(
        &mut self,
        ctx: &mut ResponseStreamContext,
        response: models::ResponseObject,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.in_progress".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: Some(response),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit response.completed event
    pub async fn emit_completed(
        &mut self,
        ctx: &mut ResponseStreamContext,
        response: models::ResponseObject,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.completed".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: Some(response),
            output_index: None,
            content_index: None,
            item: None,
            item_id: None,
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit output_item.added event
    pub async fn emit_item_added(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item: models::ResponseOutputItem,
        item_id: String,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.output_item.added".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: None,
            item: Some(item),
            item_id: Some(item_id),
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit output_item.done event
    pub async fn emit_item_done(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item: models::ResponseOutputItem,
        item_id: String,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.output_item.done".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: None,
            item: Some(item),
            item_id: Some(item_id),
            part: None,
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit content_part.added event
    pub async fn emit_content_part_added(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item_id: String,
        part: models::ResponseOutputContent,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.content_part.added".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: Some(0),
            item: None,
            item_id: Some(item_id),
            part: Some(part),
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit content_part.done event
    pub async fn emit_content_part_done(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item_id: String,
        part: models::ResponseOutputContent,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.content_part.done".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: Some(0),
            item: None,
            item_id: Some(item_id),
            part: Some(part),
            delta: None,
            text: None,
            logprobs: None,
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit output_text.delta event
    pub async fn emit_text_delta(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item_id: String,
        delta: String,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.output_text.delta".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: Some(0),
            item: None,
            item_id: Some(item_id),
            part: None,
            delta: Some(delta),
            text: None,
            logprobs: Some(vec![]),
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Emit output_text.done event
    pub async fn emit_text_done(
        &mut self,
        ctx: &mut ResponseStreamContext,
        item_id: String,
        text: String,
    ) -> Result<(), errors::ResponseError> {
        let event = models::ResponseStreamEvent {
            event_type: "response.output_text.done".to_string(),
            sequence_number: Some(ctx.next_sequence()),
            response: None,
            output_index: Some(ctx.output_item_index),
            content_index: Some(0),
            item: None,
            item_id: Some(item_id),
            part: None,
            delta: None,
            text: Some(text),
            logprobs: Some(vec![]),
            obfuscation: None,
            annotation_index: None,
            annotation: None,
            conversation_title: None,
        };
        self.send(event).await
    }

    /// Send an event to the stream
    async fn send(
        &mut self,
        event: models::ResponseStreamEvent,
    ) -> Result<(), errors::ResponseError> {
        use futures::SinkExt;
        self.tx
            .send(event)
            .await
            .map_err(|_e| errors::ResponseError::InternalError("Failed to send event".to_string()))
    }

    /// Send a raw event - useful for custom event types
    pub async fn send_raw(
        &mut self,
        event: models::ResponseStreamEvent,
    ) -> Result<(), errors::ResponseError> {
        self.send(event).await
    }
}

/// Information about a detected tool call from the LLM
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub tool_type: String,
    pub query: String,
    /// Additional parameters parsed from the tool call arguments
    pub params: Option<serde_json::Value>,
}

/// Accumulator for streaming tool calls
pub type ToolCallAccumulator = std::collections::HashMap<i64, (Option<String>, String)>;
