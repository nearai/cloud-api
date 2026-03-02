//! Benchmarks for request validation and body-hashing hot paths.
//!
//! Covers ChatCompletionRequest::validate() with varying message counts,
//! multimodal content validation, has_image_content() scanning,
//! serde_json::to_value() for content types, body SHA-256 hashing at
//! various sizes, and Bytes::clone() overhead.

use std::collections::HashMap;

use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use sha2::{Digest, Sha256};

use api::models::{
    ChatCompletionRequest, Message, MessageContent, MessageContentPart, MessageImageUrl,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_text_message(role: &str, text: &str) -> Message {
    Message {
        role: role.to_string(),
        content: Some(MessageContent::Text(text.to_string())),
        name: None,
    }
}

fn make_multimodal_message() -> Message {
    Message {
        role: "user".to_string(),
        content: Some(MessageContent::Parts(vec![
            MessageContentPart::Text {
                text: "Describe this image".to_string(),
            },
            MessageContentPart::ImageUrl {
                image_url: MessageImageUrl::String("https://example.com/image.png".to_string()),
                detail: None,
            },
        ])),
        name: None,
    }
}

fn make_request(message_count: usize) -> ChatCompletionRequest {
    let mut messages = vec![make_text_message("system", "You are a helpful assistant.")];
    for i in 0..message_count {
        if i % 2 == 0 {
            messages.push(make_text_message("user", "Hello, how are you?"));
        } else {
            messages.push(make_text_message(
                "assistant",
                "I'm doing well, thanks for asking!",
            ));
        }
    }
    ChatCompletionRequest {
        model: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        messages,
        max_tokens: Some(1024),
        temperature: Some(1.0),
        top_p: Some(1.0),
        n: Some(1),
        stream: Some(false),
        stop: None,
        presence_penalty: None,
        frequency_penalty: None,
        extra: HashMap::new(),
    }
}

fn make_multimodal_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "Qwen/Qwen3-30B-A3B-Instruct-2507".to_string(),
        messages: vec![
            make_text_message("system", "You are a helpful assistant."),
            make_multimodal_message(),
        ],
        max_tokens: Some(1024),
        temperature: Some(1.0),
        top_p: Some(1.0),
        n: Some(1),
        stream: Some(false),
        stop: None,
        presence_penalty: None,
        frequency_penalty: None,
        extra: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Benchmark group: validate
// ---------------------------------------------------------------------------

fn bench_validate(c: &mut Criterion) {
    let mut group = c.benchmark_group("validate_request");

    for n in [10, 50] {
        let req = make_request(n);
        group.bench_with_input(BenchmarkId::new("text_messages", n), &req, |b, req| {
            b.iter(|| black_box(black_box(req).validate()))
        });
    }

    let multimodal = make_multimodal_request();
    group.bench_function("multimodal_message", |b| {
        b.iter(|| black_box(black_box(&multimodal).validate()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: serde_to_value
// ---------------------------------------------------------------------------

fn bench_serde_to_value(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde_to_value");

    let text_content = MessageContent::Text("Hello, how are you?".to_string());
    let multimodal_content = MessageContent::Parts(vec![
        MessageContentPart::Text {
            text: "Describe this image".to_string(),
        },
        MessageContentPart::ImageUrl {
            image_url: MessageImageUrl::String("https://example.com/image.png".to_string()),
            detail: None,
        },
    ]);

    group.bench_function("text_content", |b| {
        b.iter(|| black_box(serde_json::to_value(black_box(&text_content)).unwrap()))
    });

    group.bench_function("multimodal_content", |b| {
        b.iter(|| black_box(serde_json::to_value(black_box(&multimodal_content)).unwrap()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: has_image_content
// ---------------------------------------------------------------------------

fn bench_has_image_content(c: &mut Criterion) {
    let mut group = c.benchmark_group("has_image_content");

    let text_only = make_request(50);
    let with_images = make_multimodal_request();

    group.bench_function("text_only_50_messages", |b| {
        b.iter(|| black_box(black_box(&text_only).has_image_content()))
    });

    group.bench_function("with_images", |b| {
        b.iter(|| black_box(black_box(&with_images).has_image_content()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: body_sha256
// ---------------------------------------------------------------------------

fn bench_body_sha256(c: &mut Criterion) {
    let mut group = c.benchmark_group("body_sha256");

    for size_kb in [1, 10, 100] {
        let body = vec![b'x'; size_kb * 1024];

        group.throughput(Throughput::Bytes(body.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("hash", format!("{}kb", size_kb)),
            &body,
            |b, body| {
                b.iter(|| {
                    let mut hasher = Sha256::new();
                    hasher.update(black_box(body));
                    let hash = hasher.finalize();
                    black_box(hex::encode(hash))
                })
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark group: bytes_clone
// ---------------------------------------------------------------------------

fn bench_bytes_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("bytes_clone");

    let data = Bytes::from(vec![b'x'; 10 * 1024]);

    group.bench_function("clone_10kb", |b| {
        b.iter(|| black_box(black_box(&data).clone()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_validate,
    bench_serde_to_value,
    bench_has_image_content,
    bench_body_sha256,
    bench_bytes_clone,
);
criterion_main!(benches);
