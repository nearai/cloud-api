use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::{header::RETRY_AFTER, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

#[derive(Clone, Copy, Debug)]
pub enum FakeItaMode {
    Success,
    RateLimited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedAttest {
    pub policy_ids: Vec<String>,
    pub policy_must_match: bool,
}

#[derive(Clone)]
pub struct FakeIta {
    pub base_url: String,
    state: Arc<FakeItaState>,
}

#[derive(Debug)]
struct FakeItaState {
    mode: FakeItaMode,
    paths: Mutex<Vec<String>>,
    attests: Mutex<Vec<ObservedAttest>>,
}

impl FakeIta {
    pub async fn start(mode: FakeItaMode) -> Self {
        let state = Arc::new(FakeItaState {
            mode,
            paths: Mutex::new(Vec::new()),
            attests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/appraisal/v2/nonce", get(fake_nonce))
            .route("/appraisal/v2/attest", post(fake_attest))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake ITA should bind");
        let addr = listener
            .local_addr()
            .expect("fake ITA should have an address");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base_url: format!("http://{addr}"),
            state,
        }
    }

    pub fn paths(&self) -> Vec<&'static str> {
        self.state
            .paths
            .lock()
            .expect("fake ITA path log should be available")
            .iter()
            .map(|path| match path.as_str() {
                "/appraisal/v2/nonce" => "/appraisal/v2/nonce",
                "/appraisal/v2/attest" => "/appraisal/v2/attest",
                _ => "<unexpected>",
            })
            .collect()
    }

    pub fn attest_observations(&self) -> Vec<ObservedAttest> {
        self.state
            .attests
            .lock()
            .expect("fake ITA attest observations should be available")
            .clone()
    }
}

async fn fake_nonce(State(state): State<Arc<FakeItaState>>) -> Json<Value> {
    record_path(&state, "/appraisal/v2/nonce");
    Json(json!({
        "val": "dmVyaWZpZXItdmFsdWU=",
        "iat": "aWF0LWJ5dGVz",
        "signature": "dmVyaWZpZXItc2lnbmF0dXJl"
    }))
}

async fn fake_attest(State(state): State<Arc<FakeItaState>>, body: Bytes) -> Response {
    record_path(&state, "/appraisal/v2/attest");
    record_attest(&state, &body);
    match state.mode {
        FakeItaMode::Success => Json(json!({"token": "gateway.jwt"})).into_response(),
        FakeItaMode::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            // Deliberately distinct from the router's default Retry-After (2) so
            // e2e tests prove the upstream value is preserved, not re-inserted.
            [(RETRY_AFTER, "7")],
            Json(json!({})),
        )
            .into_response(),
    }
}

fn record_path(state: &FakeItaState, path: &str) {
    state
        .paths
        .lock()
        .expect("fake ITA path log should be available")
        .push(path.to_string());
}

fn record_attest(state: &FakeItaState, body: &[u8]) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    let policy_ids = value["policy_ids"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let policy_must_match = value["policy_must_match"].as_bool().unwrap_or(false);
    state
        .attests
        .lock()
        .expect("fake ITA attest observations should be available")
        .push(ObservedAttest {
            policy_ids,
            policy_must_match,
        });
}
