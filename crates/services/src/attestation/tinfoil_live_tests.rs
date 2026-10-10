//! Live smoke test against the real Tinfoil router. `#[ignore]`d: it needs a
//! key and network, and it only passes while the router's current measurement
//! is among the pins in `testdata/tinfoil/test_pins.json`.
//!
//! It lives in `services` (not `inference_providers`) because the policy
//! verifier is here and `services` already depends on `inference_providers`.
//!
//! ```text
//! TINFOIL_API_KEY_FILE=~/.config/tinfoil_api_key \
//!   cargo nextest run -p services --run-ignored only -E 'test(/tinfoil.*live/)'
//! ```

use std::sync::Arc;

use futures_util::StreamExt;
use inference_providers::attested::tinfoil::{Config, Provider, TinfoilRouterSession};
use inference_providers::{ChatCompletionParams, InferenceProvider, StreamChunk};

use super::tinfoil::TinfoilPolicyVerifier;
use super::tinfoil_pins::TinfoilPins;

/// The model the recorded test pins cover. `gpt-oss-120b` (the brief's choice)
/// has no row in `test_pins.json` and pins must not be edited by hand, so the
/// smoke test uses the one model the test pins do contain.
const SLUG: &str = "glm-5-3";

#[tokio::test]
#[ignore = "live: needs TINFOIL_API_KEY_FILE and network"]
async fn tinfoil_live_streaming_chat_smoke() {
    let key_path = std::env::var("TINFOIL_API_KEY_FILE").expect("TINFOIL_API_KEY_FILE");
    let key = std::fs::read_to_string(key_path)
        .expect("read key file")
        .trim()
        .to_string();
    let pins: TinfoilPins =
        serde_json::from_str(include_str!("testdata/tinfoil/test_pins.json")).unwrap();
    let verifier = Arc::new(TinfoilPolicyVerifier::new(pins));
    let cfg = Config::new(key, 120);
    let session = TinfoilRouterSession::new(cfg.clone(), verifier).expect("session");

    // A mismatch here means the live router no longer matches the test pins
    // (Tinfoil released); the reason is the failure message.
    if let Err(e) = session.verify_now().await {
        panic!("live verification failed: {}", e.reason());
    }
    session
        .model_status(SLUG)
        .unwrap_or_else(|e| panic!("{SLUG} not pinned: {}", e.reason()));

    let provider = Provider::new(
        session.clone(),
        &cfg,
        SLUG.to_string(),
        format!("test/{SLUG}"),
    );
    let params: ChatCompletionParams = serde_json::from_value(serde_json::json!({
        "model": format!("test/{SLUG}"),
        "messages": [{"role": "user", "content": "Say OK"}],
        "stream": true,
        "max_tokens": 8,
        "stream_options": {"include_usage": true},
        "x_org_id": "internal"
    }))
    .unwrap();
    let mut stream = provider
        .chat_completion_stream(params, "live-smoke".to_string())
        .await
        .expect("stream");
    let mut chunks = 0usize;
    let mut completion_tokens = 0u32;
    while let Some(ev) = stream.next().await {
        let ev = ev.expect("stream event");
        if let Some(StreamChunk::Chat(c)) = ev.chunk {
            chunks += 1;
            if let Some(u) = c.usage {
                completion_tokens = completion_tokens.max(u.completion_tokens.max(0) as u32);
            }
        }
    }
    assert!(chunks > 0, "stream carried no chat chunks");
    assert!(completion_tokens > 0, "stream carried no usage");
}
