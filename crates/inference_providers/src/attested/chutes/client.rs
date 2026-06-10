//! HTTP client for Chutes' discovery + E2EE invoke endpoints.
//!
//! Two hosts (see [`super`]): model→`chute_id` resolution uses the OpenAI-style
//! listing on `llm.chutes.ai`; the attested E2EE path uses `api.chutes.ai`:
//!
//! - `GET  {models_base}/v1/models`              → resolve a model id to its `chute_id`
//! - `GET  {api_base}/e2e/instances/{chute_id}`  → live instances + ML-KEM `e2e_pubkey` + single-use nonce tokens
//! - `GET  {api_base}/chutes/{chute_id}/evidence?nonce=` → TDX quote + GPU evidence + cert per instance
//! - `POST {api_base}/e2e/invoke`                → send an E2EE request blob to a specific attested instance
//!
//! The client only does transport; it performs **no** attestation checks — the
//! caller must verify an instance (see `services` `ChutesBackendVerifier`) before
//! trusting any `e2e_pubkey` it returns.

use serde::Deserialize;

use super::evidence::EvidenceResponse;

/// Errors from talking to Chutes' HTTP endpoints.
#[derive(Debug, thiserror::Error)]
pub enum ChutesClientError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Chutes returned HTTP {status}{}", body_suffix(.body))]
    Status { status: u16, body: String },
    #[error("model '{0}' not found in Chutes /v1/models (no matching id)")]
    ModelNotFound(String),
    #[error("model '{0}' has no chute_id in /v1/models")]
    MissingChuteId(String),
    #[error("decoding {what}: {source}")]
    Decode {
        what: &'static str,
        #[source]
        source: reqwest::Error,
    },
}

fn body_suffix(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        // Truncate to keep errors bounded; never logged with secrets (these are
        // gateway error bodies, not inference content).
        let b: String = body.chars().take(200).collect();
        format!(": {b}")
    }
}

/// One instance from `GET /e2e/instances/{chute_id}`.
#[derive(Debug, Clone, Deserialize)]
pub struct E2eInstance {
    pub instance_id: String,
    /// Base64-encoded ML-KEM-768 public key (1184 raw bytes).
    pub e2e_pubkey: String,
    /// Single-use nonce tokens bound to this instance (consumed by `/e2e/invoke`).
    #[serde(default)]
    pub nonces: Vec<String>,
}

/// Response of `GET /e2e/instances/{chute_id}`.
#[derive(Debug, Clone, Deserialize)]
pub struct E2eInstancesResponse {
    #[serde(default)]
    pub instances: Vec<E2eInstance>,
    #[serde(default)]
    pub nonce_expires_in: Option<i64>,
    #[serde(default)]
    pub nonce_expires_at: Option<i64>,
}

/// Minimal view of `GET /v1/models` needed to resolve a `chute_id`.
#[derive(Debug, Clone, Deserialize)]
struct ModelsList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    chute_id: Option<String>,
}

/// Whether `/e2e/invoke` should stream the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeMode {
    NonStream,
    Stream,
}

/// Parameters for one `/e2e/invoke` call. `blob` is the raw E2EE request body
/// built by [`super::e2ee::build_request`].
pub struct InvokeRequest<'a> {
    pub chute_id: &'a str,
    pub instance_id: &'a str,
    /// A single-use nonce token belonging to `instance_id` (from `/e2e/instances`).
    pub nonce_token: &'a str,
    /// OpenAI sub-path to invoke inside the chute, e.g. `/v1/chat/completions`.
    pub path: &'a str,
    pub mode: InvokeMode,
    pub blob: Vec<u8>,
}

/// Client for Chutes' discovery + invoke endpoints. Holds the API key (a secret;
/// never derives `Debug`).
pub struct ChutesClient {
    http: reqwest::Client,
    api_base: String,
    models_base: String,
    api_key: String,
}

/// Default hosts (see [`super`]).
pub const DEFAULT_API_BASE: &str = "https://api.chutes.ai";
pub const DEFAULT_MODELS_BASE: &str = "https://llm.chutes.ai";

impl ChutesClient {
    /// Build a client with the default hosts and a per-request timeout (seconds).
    pub fn new(api_key: String, timeout_seconds: u64) -> Result<Self, ChutesClientError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_seconds))
            .build()?;
        Ok(Self {
            http,
            api_base: DEFAULT_API_BASE.to_string(),
            models_base: DEFAULT_MODELS_BASE.to_string(),
            api_key,
        })
    }

    /// Override the hosts (tests / staging).
    pub fn with_hosts(
        mut self,
        api_base: impl Into<String>,
        models_base: impl Into<String>,
    ) -> Self {
        self.api_base = api_base.into();
        self.models_base = models_base.into();
        self
    }

    /// Resolve a model id (e.g. `zai-org/GLM-5.1-TEE`) to its `chute_id`.
    pub async fn resolve_chute_id(&self, model: &str) -> Result<String, ChutesClientError> {
        let url = format!("{}/v1/models", self.models_base);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let resp = error_for_status(resp).await?;
        let list: ModelsList = resp.json().await.map_err(|e| ChutesClientError::Decode {
            what: "/v1/models",
            source: e,
        })?;
        pick_chute_id(&list, model)
    }

    /// Discover live, E2E-capable instances for a chute (each with its
    /// `e2e_pubkey` and single-use nonce tokens).
    pub async fn discover_instances(
        &self,
        chute_id: &str,
    ) -> Result<E2eInstancesResponse, ChutesClientError> {
        let url = format!("{}/e2e/instances/{}", self.api_base, chute_id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let resp = error_for_status(resp).await?;
        resp.json().await.map_err(|e| ChutesClientError::Decode {
            what: "/e2e/instances",
            source: e,
        })
    }

    /// Fetch TEE evidence (TDX quote + GPU + cert) for every live instance of a
    /// chute, bound to `boot_nonce` (the freshness anchor in `report_data[0:32]`).
    pub async fn fetch_evidence(
        &self,
        chute_id: &str,
        boot_nonce: &str,
    ) -> Result<EvidenceResponse, ChutesClientError> {
        let url = format!("{}/chutes/{}/evidence", self.api_base, chute_id);
        let resp = self
            .http
            .get(&url)
            .query(&[("nonce", boot_nonce)])
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let resp = error_for_status(resp).await?;
        resp.json().await.map_err(|e| ChutesClientError::Decode {
            what: "/evidence",
            source: e,
        })
    }

    /// Send an E2EE request blob to a specific attested instance, returning the
    /// raw response blob bytes (decrypt with the request's `ResponseSession`).
    /// Non-streaming only; streaming SSE is handled separately.
    pub async fn invoke_nonstream(
        &self,
        req: &InvokeRequest<'_>,
    ) -> Result<Vec<u8>, ChutesClientError> {
        let resp = self.invoke_request(req).send().await?;
        let resp = error_for_status(resp).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Send an E2EE streaming request and return the (status-checked) response
    /// for the caller to drive (`bytes_stream()` → the E2EE SSE adapter).
    pub async fn invoke_stream(
        &self,
        req: &InvokeRequest<'_>,
    ) -> Result<reqwest::Response, ChutesClientError> {
        let resp = self.invoke_request(req).send().await?;
        error_for_status(resp).await
    }

    /// Build the `/e2e/invoke` request with all required headers. Exposed so the
    /// streaming caller can drive the response itself.
    pub fn invoke_request(&self, req: &InvokeRequest<'_>) -> reqwest::RequestBuilder {
        let url = format!("{}/e2e/invoke", self.api_base);
        let stream = matches!(req.mode, InvokeMode::Stream);
        self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/octet-stream")
            .header("X-Chute-Id", req.chute_id)
            .header("X-Instance-Id", req.instance_id)
            .header("X-E2E-Nonce", req.nonce_token)
            .header("X-E2E-Stream", if stream { "true" } else { "false" })
            .header("X-E2E-Path", req.path)
            .body(req.blob.clone())
    }
}

/// Read an HTTP error response into a [`ChutesClientError::Status`] (capturing
/// the body for diagnostics), or pass a success response through.
async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response, ChutesClientError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(ChutesClientError::Status {
        status: status.as_u16(),
        body,
    })
}

/// Find a model's `chute_id` in a `/v1/models` listing (pure; unit-tested).
fn pick_chute_id(list: &ModelsList, model: &str) -> Result<String, ChutesClientError> {
    let entry = list
        .data
        .iter()
        .find(|m| m.id == model)
        .ok_or_else(|| ChutesClientError::ModelNotFound(model.to_string()))?;
    entry
        .chute_id
        .clone()
        .ok_or_else(|| ChutesClientError::MissingChuteId(model.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_e2e_instances_response() {
        let r: E2eInstancesResponse = serde_json::from_str(
            r#"{"instances":[{"instance_id":"i1","e2e_pubkey":"QUJD","nonces":["t1","t2"]}],
                "nonce_expires_in":60,"nonce_expires_at":1781000000}"#,
        )
        .unwrap();
        assert_eq!(r.instances.len(), 1);
        assert_eq!(r.instances[0].instance_id, "i1");
        assert_eq!(r.instances[0].nonces, vec!["t1", "t2"]);
        assert_eq!(r.nonce_expires_in, Some(60));
    }

    #[test]
    fn tolerates_instance_without_nonces() {
        let r: E2eInstancesResponse =
            serde_json::from_str(r#"{"instances":[{"instance_id":"i","e2e_pubkey":"QQ=="}]}"#)
                .unwrap();
        assert!(r.instances[0].nonces.is_empty());
    }

    #[test]
    fn resolves_chute_id() {
        let list: ModelsList = serde_json::from_str(
            r#"{"data":[{"id":"other","chute_id":"c-other"},
                        {"id":"zai-org/GLM-5.1-TEE","chute_id":"b048fe26"}]}"#,
        )
        .unwrap();
        assert_eq!(
            pick_chute_id(&list, "zai-org/GLM-5.1-TEE").unwrap(),
            "b048fe26"
        );
    }

    #[test]
    fn missing_model_and_missing_chute_id_are_distinct_errors() {
        let list: ModelsList = serde_json::from_str(r#"{"data":[{"id":"m-no-cid"}]}"#).unwrap();
        assert!(matches!(
            pick_chute_id(&list, "nope"),
            Err(ChutesClientError::ModelNotFound(_))
        ));
        assert!(matches!(
            pick_chute_id(&list, "m-no-cid"),
            Err(ChutesClientError::MissingChuteId(_))
        ));
    }

    #[test]
    fn invoke_request_sets_all_headers() {
        let c = ChutesClient::new("cpk_secret".to_string(), 30).unwrap();
        let req = InvokeRequest {
            chute_id: "cid",
            instance_id: "iid",
            nonce_token: "tok",
            path: "/v1/chat/completions",
            mode: InvokeMode::Stream,
            blob: vec![1, 2, 3],
        };
        let built = c.invoke_request(&req).build().unwrap();
        let h = built.headers();
        assert_eq!(h.get("X-Chute-Id").unwrap(), "cid");
        assert_eq!(h.get("X-Instance-Id").unwrap(), "iid");
        assert_eq!(h.get("X-E2E-Nonce").unwrap(), "tok");
        assert_eq!(h.get("X-E2E-Stream").unwrap(), "true");
        assert_eq!(h.get("X-E2E-Path").unwrap(), "/v1/chat/completions");
        assert_eq!(h.get("Content-Type").unwrap(), "application/octet-stream");
        assert!(h.contains_key("authorization"));
        assert_eq!(built.method(), reqwest::Method::POST);
    }
}
