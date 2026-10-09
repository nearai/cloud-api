//! Database-backed coverage for prompt-cache writes on Anthropic models, on
//! both the OpenAI-compatible plane and the native Messages plane.
//!
//! A model with a text pricing profile must bill cache writes at the
//! `cacheWrite` rate of the context band the prompt falls in. A flat-priced
//! model keeps the five-minute rate derived from its input price.

use crate::common::*;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Nano-USD per token for the fixture profile below, per context band.
struct BandRates {
    uncached_input: i64,
    cached_input: i64,
    cache_write: i64,
    output: i64,
}

const SHORT: BandRates = BandRates {
    uncached_input: 100,
    cached_input: 10,
    cache_write: 125,
    output: 500,
};
const LONG: BandRates = BandRates {
    uncached_input: 200,
    cached_input: 20,
    cache_write: 250,
    output: 750,
};

/// Two rate cards split at a 100K-token prompt. The long band's cache-write
/// rate is not 1.25x the short input rate, so a long-band total can only come
/// from the profile row and never from the flat five-minute rule.
fn profile_json() -> Value {
    json!({
        "version": 1,
        "currency": "USD",
        "unit": "million_tokens",
        "longContextThreshold": 100000,
        "tiers": {
            "default": {
                "short": {"uncachedInput": "0.10", "cachedInput": "0.01", "cacheWrite": "0.125", "output": "0.50"},
                "long": {"uncachedInput": "0.20", "cachedInput": "0.02", "cacheWrite": "0.25", "output": "0.75"}
            }
        }
    })
}

/// Token counts as Anthropic reports them: the three input classes do not
/// overlap, and the billed prompt is their sum.
#[derive(Clone, Copy)]
struct UpstreamUsage {
    input_tokens: i32,
    cache_read_input_tokens: i32,
    cache_creation_input_tokens: i32,
    output_tokens: i32,
}

impl UpstreamUsage {
    fn prompt_tokens(self) -> i32 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }

    fn cost(self, rates: &BandRates) -> i64 {
        i64::from(self.input_tokens) * rates.uncached_input
            + i64::from(self.cache_read_input_tokens) * rates.cached_input
            + i64::from(self.cache_creation_input_tokens) * rates.cache_write
            + i64::from(self.output_tokens) * rates.output
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Plane {
    ChatCompletions,
    Messages,
}

#[derive(Clone, Copy)]
enum Pricing {
    Profiled,
    /// Flat input/output price in nano-USD per token.
    Flat {
        input: i64,
        output: i64,
    },
}

async fn setup_server() -> axum_test::TestServer {
    let (server, _, _, _) = setup_test_server_with_pool_and_config(|config| {
        config.external_providers.enable_anthropic_messages = true;
        // The test default of zero would time out before the upstream answers.
        config.external_providers.timeout_seconds = 5;
    })
    .await;
    server
}

/// An Anthropic upstream that answers every Messages request with `usage`.
async fn anthropic_upstream(usage: UpstreamUsage, stream: bool) -> MockServer {
    let upstream = MockServer::start().await;
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let response = if stream {
        let events = [
            json!({"type": "message_start", "message": {
                "id": message_id, "type": "message", "role": "assistant",
                "model": "upstream-fixture", "content": [], "stop_reason": null,
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "output_tokens": 1
                }
            }}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": usage.output_tokens}}),
            json!({"type": "message_stop"}),
        ];
        let body: String = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().expect("event type")
                )
            })
            .collect();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": message_id, "type": "message", "role": "assistant",
            "model": "upstream-fixture",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {
                "input_tokens": usage.input_tokens,
                "cache_read_input_tokens": usage.cache_read_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                "output_tokens": usage.output_tokens
            }
        }))
    };
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(response)
        .expect(1)
        .mount(&upstream)
        .await;
    upstream
}

/// Register a test-owned Anthropic-backed model served by `upstream`.
async fn setup_anthropic_model(
    server: &axum_test::TestServer,
    upstream: &MockServer,
    pricing: Pricing,
) -> String {
    let model = format!("anthropic/cache-write-fixture-{}", uuid::Uuid::new_v4());
    let mut definition = json!({
        "modelDisplayName": "Cache-write pricing fixture",
        "modelDescription": "Cache-write pricing integration fixture",
        "contextLength": 1000000,
        "isActive": true,
        "ownedBy": "anthropic",
        "verifiable": false,
        "attestationSupported": false,
        "providerType": "external",
        "providerConfig": {
            "backend": "anthropic",
            "base_url": upstream.uri(),
            "api_key": "synthetic-test-key"
        }
    });
    match pricing {
        Pricing::Profiled => definition["textPricing"] = profile_json(),
        Pricing::Flat { input, output } => {
            definition["inputCostPerToken"] = json!({"amount": input, "currency": "USD"});
            definition["outputCostPerToken"] = json!({"amount": output, "currency": "USD"});
        }
    }
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({ &model: definition }))
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    model
}

/// The one usage row of a test-owned organization. Billing for a stream runs
/// after the response body ends, so poll for the row instead of racing it.
async fn only_usage(
    server: &axum_test::TestServer,
    organization_id: &str,
) -> api::routes::usage::UsageHistoryEntryResponse {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        let response = server
            .get(&format!(
                "/v1/organizations/{organization_id}/usage/history?limit=2&offset=0"
            ))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let mut history: api::routes::usage::UsageHistoryResponse = response.json();
        assert!(history.data.len() <= 1, "one request must bill one row");
        if let Some(entry) = history.data.pop() {
            return entry;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "usage was not recorded within 10s"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// Send one cache-writing request through `plane` to a fresh model and
/// organization, and return the usage row it billed.
async fn bill_one_request(
    server: &axum_test::TestServer,
    plane: Plane,
    stream: bool,
    pricing: Pricing,
    usage: UpstreamUsage,
) -> api::routes::usage::UsageHistoryEntryResponse {
    let upstream = anthropic_upstream(usage, stream).await;
    let model = setup_anthropic_model(server, &upstream, pricing).await;
    let organization = setup_org_with_credits(server, 10_000_000_000).await;
    let api_key = get_api_key_for_org(server, organization.id.clone()).await;

    let mut body = json!({
        "model": model,
        "max_tokens": 64,
        "cache_control": {"type": "ephemeral"},
        "messages": [{"role": "user", "content": "hello"}],
        "stream": stream
    });
    let response = match plane {
        Plane::ChatCompletions => {
            if stream {
                body["stream_options"] = json!({"include_usage": true});
            }
            server
                .post("/v1/chat/completions")
                .add_header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .await
        }
        Plane::Messages => {
            server
                .post("/v1/messages")
                .add_header("x-api-key", api_key)
                .add_header("anthropic-version", "2023-06-01")
                .json(&body)
                .await
        }
    };
    assert_eq!(response.status_code(), 200, "{}", response.text());

    let entry = only_usage(server, &organization.id).await;
    assert_eq!(entry.model, model);
    assert_eq!(entry.input_tokens, usage.prompt_tokens());
    assert_eq!(entry.output_tokens, usage.output_tokens);
    assert_eq!(entry.cache_read_tokens, usage.cache_read_input_tokens);
    entry
}

/// Assert that both the non-streaming and the streaming request of `plane`
/// bill `usage` from the profile row of `band`.
async fn assert_profiled_cache_write_cost(
    plane: Plane,
    usage: UpstreamUsage,
    band: &str,
    rates: &BandRates,
    cache_write_rate: &str,
    expected_cost: i64,
) {
    assert_eq!(usage.cost(rates), expected_cost, "fixture arithmetic");
    let server = setup_server().await;
    for stream in [false, true] {
        let entry = bill_one_request(&server, plane, stream, Pricing::Profiled, usage).await;
        let case = format!("{plane:?} stream={stream}");
        assert_eq!(
            entry.cache_write_tokens, usage.cache_creation_input_tokens,
            "{case}"
        );
        assert_eq!(entry.total_cost, expected_cost, "{case}");
        assert_eq!(entry.context_band.as_deref(), Some(band), "{case}");
        let snapshot = entry.billing_details.expect("billing snapshot");
        assert_eq!(snapshot["rates"]["cacheWrite"], cache_write_rate, "{case}");
        assert_eq!(
            snapshot["rounding"]["roundedTotal"], expected_cost,
            "{case}"
        );
        assert!(snapshot.get("criticalFallbackReason").is_none(), "{case}");
    }
}

// Short band: the prompt stays under the 100K threshold.
//   4 uncached * 100 + 5135 written * 125 + 36 output * 500 = 660,275
// Billing the writes as uncached input would give 531,900 instead.
const CHAT_SHORT: UpstreamUsage = UpstreamUsage {
    input_tokens: 4,
    cache_read_input_tokens: 0,
    cache_creation_input_tokens: 5135,
    output_tokens: 36,
};

//   4 uncached * 100 + 5134 written * 125 + 28 output * 500 = 656,150
const MESSAGES_SHORT: UpstreamUsage = UpstreamUsage {
    input_tokens: 4,
    cache_read_input_tokens: 0,
    cache_creation_input_tokens: 5134,
    output_tokens: 28,
};

// Long band: a 122,010-token prompt, most of it written to the cache.
//   10 uncached * 200 + 2000 read * 20 + 120000 written * 250 + 20 output * 750
//   = 30,057,000
const LONG_PROMPT: UpstreamUsage = UpstreamUsage {
    input_tokens: 10,
    cache_read_input_tokens: 2_000,
    cache_creation_input_tokens: 120_000,
    output_tokens: 20,
};

#[tokio::test]
async fn chat_completions_bills_short_band_cache_writes_at_the_profile_rate() {
    assert_profiled_cache_write_cost(
        Plane::ChatCompletions,
        CHAT_SHORT,
        "short",
        &SHORT,
        "0.125",
        660_275,
    )
    .await;
}

#[tokio::test]
async fn chat_completions_bills_long_band_cache_writes_at_the_profile_rate() {
    assert_profiled_cache_write_cost(
        Plane::ChatCompletions,
        LONG_PROMPT,
        "long",
        &LONG,
        "0.25",
        30_057_000,
    )
    .await;
}

#[tokio::test]
async fn messages_bills_short_band_cache_writes_at_the_profile_rate() {
    assert_profiled_cache_write_cost(
        Plane::Messages,
        MESSAGES_SHORT,
        "short",
        &SHORT,
        "0.125",
        656_150,
    )
    .await;
}

#[tokio::test]
async fn messages_bills_long_band_cache_writes_at_the_profile_rate() {
    assert_profiled_cache_write_cost(
        Plane::Messages,
        LONG_PROMPT,
        "long",
        &LONG,
        "0.25",
        30_057_000,
    )
    .await;
}

/// A model without a profile keeps the flat rule: cache writes at 1.25x the
/// input price, rounded half-up, and no pricing snapshot on the row.
#[tokio::test]
async fn flat_priced_model_keeps_the_five_minute_cache_write_rate() {
    let pricing = Pricing::Flat {
        input: 250,
        output: 1_250,
    };
    let usage = UpstreamUsage {
        input_tokens: 10,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 4_000,
        output_tokens: 20,
    };
    // 10 uncached * 250 + 4000 written * 313 (250 * 1.25 = 312.5) + 20 * 1250
    let expected_cost = 1_279_500;

    let server = setup_server().await;
    for plane in [Plane::ChatCompletions, Plane::Messages] {
        for stream in [false, true] {
            let entry = bill_one_request(&server, plane, stream, pricing, usage).await;
            let case = format!("{plane:?} stream={stream}");
            assert_eq!(entry.cache_write_tokens, 4_000, "{case}");
            assert_eq!(entry.total_cost, expected_cost, "{case}");
            assert_eq!(entry.context_band, None, "{case}");
            assert!(entry.billing_details.is_none(), "{case}");
        }
    }
}
