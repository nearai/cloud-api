use super::{
    legacy_tool_id, message, stop_reason, tool_input, upstream, usage, valid_message_id,
    RouteResult,
};
use crate::routes::anthropic::STREAM_ID_PEEK_TIMEOUT;
use axum::body::Bytes;
use futures_util::{future::ready, stream, StreamExt};
use inference_providers::{
    anthropic_raw::AnthropicRawBody,
    models::{ChatCompletionChunk, FunctionCallDelta},
    FinishReason, StreamChunk, StreamingResult, TokenUsage,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

pub(super) async fn convert(
    source: StreamingResult,
    model: String,
    show_thinking: bool,
    requested_stops: Vec<String>,
) -> RouteResult<(AnthropicRawBody, Option<String>)> {
    let mut chunks = Box::pin(
        source
            .filter_map(|event| {
                ready(match event {
                    Ok(event) => match event.chunk {
                        Some(StreamChunk::Chat(chunk)) => Some(Ok(chunk)),
                        None => None,
                        _ => Some(Err(upstream("Unexpected upstream stream format"))),
                    },
                    Err(_) => Some(Err(upstream("Chat completion stream failed"))),
                })
            })
            .peekable(),
    );
    let first_id = match tokio::time::timeout(STREAM_ID_PEEK_TIMEOUT, chunks.as_mut().peek()).await
    {
        Ok(Some(Ok(c))) if valid_message_id(&c.id) => Some(c.id.clone()),
        Ok(_) => return Err(upstream("Chat stream did not return a valid message")),
        Err(_) => None,
    };
    let state = (
        chunks,
        Encoder::new(model, show_thinking, requested_stops),
        VecDeque::new(),
        false,
    );
    let output = stream::unfold(
        state,
        |(mut chunks, mut encoder, mut queue, mut done)| async move {
            loop {
                if let Some(bytes) = queue.pop_front() {
                    return Some((Ok(bytes), (chunks, encoder, queue, done)));
                }
                if done {
                    return None;
                }
                let result = match chunks.next().await {
                    Some(Ok(chunk)) => encoder.push(chunk, &mut queue),
                    Some(Err(e)) => Err(e),
                    None => {
                        done = true;
                        encoder.finish(&mut queue)
                    }
                };
                if let Err(e) = result {
                    done = true;
                    queue.clear();
                    queue.push_back(frame(
                        json!({"type":"error","error":{"type":"api_error","message":e.message}}),
                    ));
                }
            }
        },
    );
    Ok((Box::pin(output), first_id))
}

type Frames = VecDeque<Bytes>;

const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;

// Bound the encoded event size even when an upstream puts a whole file in one delta.
fn fragments(mut text: &str, mut emit: impl FnMut(&str)) {
    while !text.is_empty() {
        let (part, rest) = text.split_at(text.floor_char_boundary(text.len().min(32 * 1024)));
        emit(part);
        text = rest;
    }
}

fn frame(data: Value) -> Bytes {
    let kind = data["type"].as_str().unwrap_or("error");
    Bytes::from(format!("event: {kind}\ndata: {data}\n\n"))
}

#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    args: String,
}

struct Encoder {
    model: String,
    show_thinking: bool,
    message_id: String,
    index: usize,
    open_text: Option<&'static str>,
    tools: BTreeMap<i64, Tool>,
    buffered: usize,
    trailing_text: Vec<(&'static str, String)>,
    usage: TokenUsage,
    finish: Option<FinishReason>,
    requested_stops: Vec<String>,
    reported_stop: Option<String>,
}

impl Encoder {
    fn new(model: String, show_thinking: bool, requested_stops: Vec<String>) -> Self {
        Self {
            model,
            show_thinking,
            message_id: String::new(),
            index: 0,
            open_text: None,
            tools: BTreeMap::new(),
            buffered: 0,
            trailing_text: Vec::new(),
            usage: TokenUsage::default(),
            finish: None,
            requested_stops,
            reported_stop: None,
        }
    }

    fn push(&mut self, chunk: ChatCompletionChunk, out: &mut Frames) -> RouteResult<()> {
        if let Some(usage) = chunk.usage {
            self.usage = usage;
        }
        if self.message_id.is_empty() {
            if !valid_message_id(&chunk.id) {
                return Err(upstream("Chat stream has no message ID"));
            }
            self.message_id = chunk.id;
            out.push_back(frame(json!({"type":"message_start","message":message(&self.message_id,&self.model,json!([]),None,None,usage(&self.usage))})));
        }
        for choice in chunk.choices.into_iter().filter(|c| c.index == 0) {
            if let Some(mut delta) = choice.delta {
                if self.show_thinking {
                    self.text(
                        "thinking",
                        delta.reasoning_content.or(delta.reasoning).as_deref(),
                        out,
                    )?;
                }
                self.text("text", delta.content.as_deref(), out)?;
                self.text(
                    "text",
                    delta.extra.get("refusal").and_then(Value::as_str),
                    out,
                )?;
                let calls = delta.tool_calls.unwrap_or_default();
                if let Some(function) = delta
                    .extra
                    .remove("function_call")
                    .filter(|f| calls.is_empty() && !f.is_null())
                {
                    let function = serde_json::from_value(function)
                        .map_err(|_| upstream("Invalid legacy function call in chat stream"))?;
                    let id =
                        (!self.tools.contains_key(&0)).then(|| legacy_tool_id(&self.message_id));
                    self.tool(0, id, Some(function), out)?;
                }
                for call in calls {
                    self.tool(call.index.unwrap_or(0), call.id, call.function, out)?;
                }
            }
            if choice.finish_reason.is_some() {
                self.finish = choice.finish_reason;
            }
            if let Some(Value::String(marker)) = choice.stop_reason {
                self.reported_stop = Some(marker);
            }
        }
        Ok(())
    }

    fn close_text(&mut self, out: &mut Frames) {
        if self.open_text.take().is_some() {
            self.close_block(out);
        }
    }

    fn close_block(&mut self, out: &mut Frames) {
        out.push_back(frame(
            json!({"type":"content_block_stop","index":self.index}),
        ));
        self.index += 1;
    }

    fn text(
        &mut self,
        kind: &'static str,
        text: Option<&str>,
        out: &mut Frames,
    ) -> RouteResult<()> {
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            return Ok(());
        };
        if !self.tools.is_empty() {
            self.buffered += text.len();
            if self.buffered > MAX_BUFFERED_BYTES {
                return Err(upstream("Chat content exceeded the stream buffer"));
            }
            if let Some((last_kind, last)) = self.trailing_text.last_mut() {
                if *last_kind == kind {
                    last.push_str(text);
                    return Ok(());
                }
            }
            self.trailing_text.push((kind, text.to_owned()));
            return Ok(());
        }
        if self.open_text != Some(kind) {
            self.close_text(out);
            self.open_text = Some(kind);
            let block = if kind == "text" {
                json!({"type":"text","text":""})
            } else {
                json!({"type":"thinking","thinking":"","signature":""})
            };
            out.push_back(frame(
                json!({"type":"content_block_start","index":self.index,"content_block":block}),
            ));
        }
        fragments(text, |part| {
            let delta = if kind == "text" {
                json!({"type":"text_delta","text":part})
            } else {
                json!({"type":"thinking_delta","thinking":part})
            };
            out.push_back(frame(
                json!({"type":"content_block_delta","index":self.index,"delta":delta}),
            ));
        });
        Ok(())
    }

    fn tool(
        &mut self,
        key: i64,
        id: Option<String>,
        function: Option<FunctionCallDelta>,
        out: &mut Frames,
    ) -> RouteResult<()> {
        self.close_text(out);
        if !(0..128).contains(&key) {
            return Err(upstream("Invalid chat tool index"));
        }
        let tool = self.tools.entry(key).or_default();
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            if !tool.id.is_empty() && tool.id != id {
                return Err(upstream("Chat tool ID changed during streaming"));
            }
            tool.id = id;
        }
        if let Some(f) = function {
            if let Some(name) = f.name.filter(|n| !n.is_empty() && *n != tool.name) {
                self.buffered += name.len();
                tool.name.push_str(&name);
            }
            if let Some(arg) = f.arguments {
                self.buffered += arg.len();
                tool.args.push_str(&arg);
            }
        }
        if self.buffered > MAX_BUFFERED_BYTES {
            return Err(upstream("Chat tool arguments exceeded the stream buffer"));
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Frames) -> RouteResult<()> {
        let reason = match self.finish {
            Some(FinishReason::Stop) => "stop",
            Some(FinishReason::Length) => "length",
            Some(FinishReason::ToolCalls) => "tool_calls",
            Some(FinishReason::ContentFilter) => "content_filter",
            None => return Err(upstream("Chat stream ended before its finish reason")),
        };
        let reported_stop = self.reported_stop.take();
        let (reason, stop_sequence) = stop_reason(
            Some(reason),
            !self.tools.is_empty(),
            reported_stop.as_deref(),
            &self.requested_stops,
        )?;
        if reason == "tool_use" && self.tools.is_empty() {
            return Err(upstream("Chat stream ended with a missing tool call"));
        }
        self.close_text(out);
        // A Messages tool block cannot be retracted if filtering interrupts its arguments.
        for tool in std::mem::take(&mut self.tools).into_values() {
            if reason == "max_tokens" {
                if tool.id.is_empty() || tool.name.is_empty() {
                    continue;
                }
            } else if let Err(error) = tool_input(&tool.id, &tool.name, &tool.args) {
                if reason == "refusal" {
                    continue;
                }
                return Err(error);
            }
            out.push_back(frame(json!({"type":"content_block_start","index":self.index,"content_block":{"type":"tool_use","id":tool.id,"name":tool.name,"input":{}}})));
            fragments(&tool.args, |part| {
                out.push_back(frame(json!({"type":"content_block_delta","index":self.index,"delta":{"type":"input_json_delta","partial_json":part}})))
            });
            self.close_block(out);
        }
        for (kind, text) in std::mem::take(&mut self.trailing_text) {
            self.text(kind, Some(&text), out)?;
        }
        self.close_text(out);
        out.push_back(frame(json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":stop_sequence},"usage":usage(&self.usage)})));
        out.push_back(frame(json!({"type":"message_stop"})));
        Ok(())
    }
}
