//! Tempo JSON-RPC over HTTP: the outbound adapter behind [`TempoRpc`].
//!
//! Every call has the configured timeout, redirects are not followed, and the response body
//! is read in bounded chunks. Nothing here logs and no error carries the endpoint URL (it can
//! embed a provider key), the node's text, or a transaction.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::ports::{RpcError, RpcLog, RpcReceipt, TempoRpc};

/// Largest JSON-RPC response read.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Largest number of logs accepted in one receipt.
const MAX_LOGS: usize = 2048;
/// Largest `data` of one log.
const MAX_LOG_DATA_BYTES: usize = 64 * 1024;
/// A log has at most four topics.
const MAX_TOPICS: usize = 4;

/// [`TempoRpc`] over `reqwest`.
#[derive(Clone)]
pub struct HttpTempoRpc {
    client: reqwest::Client,
    url: reqwest::Url,
}

impl std::fmt::Debug for HttpTempoRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpTempoRpc { url: <redacted> }")
    }
}

impl HttpTempoRpc {
    /// `url` is the validated `MPP_TEMPO_RPC_URL`; `timeout` bounds each whole call.
    pub fn new(url: &str, timeout: Duration) -> Result<Self, RpcError> {
        let url = reqwest::Url::parse(url).map_err(|_| RpcError::InvalidResponse)?;
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| RpcError::Unavailable)?;
        Ok(Self { client, url })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut response = self
            .client
            .post(self.url.clone())
            .json(&request)
            .send()
            .await
            .map_err(|_| RpcError::Unavailable)?;
        if !response.status().is_success() {
            return Err(RpcError::Unavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| RpcError::Unavailable)? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(RpcError::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        let mut envelope: Value =
            serde_json::from_slice(&body).map_err(|_| RpcError::InvalidResponse)?;
        if envelope.get("error").is_some_and(|error| !error.is_null()) {
            return Err(RpcError::Rejected);
        }
        Ok(envelope
            .get_mut("result")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }
}

#[async_trait]
impl TempoRpc for HttpTempoRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        let hex = format!("0x{}", hex::encode(raw));
        let result = self
            .call("eth_sendRawTransactionSync", json!([hex]))
            .await?;
        parse_receipt(&result)
    }

    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        let result = self
            .call(
                "eth_getTransactionReceipt",
                json!([format!("0x{}", hex::encode(hash))]),
            )
            .await
            // A node that answers an unknown hash with an error object is not "unknown":
            // treat it as unavailable so the caller does not report a missing payment.
            .map_err(|error| match error {
                RpcError::Rejected => RpcError::Unavailable,
                other => other,
            })?;
        if result.is_null() {
            return Ok(None);
        }
        parse_receipt(&result).map(Some)
    }
}

fn hex_bytes(value: &Value, max_bytes: usize) -> Result<Vec<u8>, RpcError> {
    let digits = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .ok_or(RpcError::InvalidResponse)?;
    if digits.len() > max_bytes.saturating_mul(2) {
        return Err(RpcError::InvalidResponse);
    }
    hex::decode(digits).map_err(|_| RpcError::InvalidResponse)
}

fn hex_array<const N: usize>(value: &Value) -> Result<[u8; N], RpcError> {
    hex_bytes(value, N)?
        .try_into()
        .map_err(|_| RpcError::InvalidResponse)
}

/// Strict read of an `eth_getTransactionReceipt` result. Anything unexpected is an error:
/// a malformed field never turns into a default.
pub fn parse_receipt(value: &Value) -> Result<RpcReceipt, RpcError> {
    let object = value.as_object().ok_or(RpcError::InvalidResponse)?;
    let field = |name: &str| object.get(name).ok_or(RpcError::InvalidResponse);

    let success = match field("status")?.as_str() {
        Some("0x1") => true,
        Some("0x0") => false,
        _ => return Err(RpcError::InvalidResponse),
    };
    let logs_json = field("logs")?.as_array().ok_or(RpcError::InvalidResponse)?;
    if logs_json.len() > MAX_LOGS {
        return Err(RpcError::InvalidResponse);
    }
    let mut logs = Vec::with_capacity(logs_json.len());
    for log in logs_json {
        let log = log.as_object().ok_or(RpcError::InvalidResponse)?;
        let topics_json = log
            .get("topics")
            .and_then(Value::as_array)
            .ok_or(RpcError::InvalidResponse)?;
        if topics_json.len() > MAX_TOPICS {
            return Err(RpcError::InvalidResponse);
        }
        let topics = topics_json
            .iter()
            .map(hex_array::<32>)
            .collect::<Result<Vec<_>, _>>()?;
        logs.push(RpcLog {
            address: hex_array::<20>(log.get("address").ok_or(RpcError::InvalidResponse)?)?,
            topics,
            data: hex_bytes(
                log.get("data").ok_or(RpcError::InvalidResponse)?,
                MAX_LOG_DATA_BYTES,
            )?,
        });
    }
    Ok(RpcReceipt {
        tx_hash: hex_array::<32>(field("transactionHash")?)?,
        success,
        from: hex_array::<20>(field("from")?)?,
        logs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
    const FROM: &str = "0x4040404040404040404040404040404040404040";

    fn receipt_json() -> Value {
        json!({
            "transactionHash": HASH,
            "status": "0x1",
            "from": FROM,
            "blockNumber": "0x10",
            "logs": [{
                "address": "0x2020202020202020202020202020202020202020",
                "topics": [
                    format!("0x{}", "dd".repeat(32)),
                    format!("0x{}{}", "00".repeat(12), "40".repeat(20)),
                ],
                "data": format!("0x{}", "00".repeat(32)),
                "logIndex": "0x0",
            }],
        })
    }

    fn rpc(server: &MockServer, timeout: Duration) -> HttpTempoRpc {
        HttpTempoRpc::new(&server.uri(), timeout).unwrap()
    }

    #[test]
    fn parses_a_receipt_strictly() {
        let receipt = parse_receipt(&receipt_json()).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.tx_hash, [0x11; 32]);
        assert_eq!(receipt.from, [0x40; 20]);
        assert_eq!(receipt.logs.len(), 1);
        assert_eq!(receipt.logs[0].address, [0x20; 20]);
        assert_eq!(receipt.logs[0].topics.len(), 2);
        assert_eq!(receipt.logs[0].topics[1][12..], [0x40; 20]);
        assert_eq!(receipt.logs[0].data, vec![0u8; 32]);

        let mut reverted = receipt_json();
        reverted["status"] = json!("0x0");
        assert!(!parse_receipt(&reverted).unwrap().success);
    }

    #[test]
    fn rejects_malformed_receipts_instead_of_defaulting() {
        type Edit = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "status missing",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("status");
                }),
            ),
            ("status other", Box::new(|v| v["status"] = json!("0x2"))),
            ("status numeric", Box::new(|v| v["status"] = json!(1))),
            (
                "hash short",
                Box::new(|v| v["transactionHash"] = json!("0x11")),
            ),
            (
                "hash not hex",
                Box::new(|v| v["transactionHash"] = json!(format!("0x{}", "zz".repeat(32)))),
            ),
            (
                "hash no prefix",
                Box::new(|v| v["transactionHash"] = json!("11".repeat(32))),
            ),
            ("from short", Box::new(|v| v["from"] = json!("0x4040"))),
            ("logs not array", Box::new(|v| v["logs"] = json!({}))),
            ("log not object", Box::new(|v| v["logs"] = json!(["x"]))),
            (
                "topic short",
                Box::new(|v| v["logs"][0]["topics"][0] = json!("0xdd")),
            ),
            (
                "five topics",
                Box::new(|v| {
                    v["logs"][0]["topics"] = json!(vec![format!("0x{}", "00".repeat(32)); 5])
                }),
            ),
            (
                "address short",
                Box::new(|v| v["logs"][0]["address"] = json!("0x20")),
            ),
            (
                "data odd",
                Box::new(|v| v["logs"][0]["data"] = json!("0x1")),
            ),
            (
                "data huge",
                Box::new(|v| {
                    v["logs"][0]["data"] =
                        json!(format!("0x{}", "00".repeat(MAX_LOG_DATA_BYTES + 1)))
                }),
            ),
            (
                "too many logs",
                Box::new(|v| {
                    v["logs"] = json!(vec![
                        json!({"address": "0x2020202020202020202020202020202020202020", "topics": [], "data": "0x"});
                        MAX_LOGS + 1
                    ])
                }),
            ),
        ];
        for (label, mutate) in cases {
            let mut value = receipt_json();
            mutate(&mut value);
            assert_eq!(
                parse_receipt(&value),
                Err(RpcError::InvalidResponse),
                "{label}"
            );
        }
        assert_eq!(parse_receipt(&Value::Null), Err(RpcError::InvalidResponse));
        assert_eq!(parse_receipt(&json!([])), Err(RpcError::InvalidResponse));
    }

    #[tokio::test]
    async fn get_receipt_sends_the_hash_and_returns_none_for_null() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "method": "eth_getTransactionReceipt",
                "params": [HASH],
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": receipt_json()})),
            )
            .mount(&server)
            .await;
        let found = rpc(&server, Duration::from_secs(5))
            .get_transaction_receipt([0x11; 32])
            .await
            .unwrap();
        assert_eq!(found.unwrap().tx_hash, [0x11; 32]);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": null})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([0x11; 32])
                .await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn send_raw_transaction_sync_posts_the_hex_and_reads_the_receipt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "method": "eth_sendRawTransactionSync",
                "params": ["0x76aabb"],
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": receipt_json()})),
            )
            .mount(&server)
            .await;
        let receipt = rpc(&server, Duration::from_secs(5))
            .send_raw_transaction_sync(&[0x76, 0xaa, 0xbb])
            .await
            .unwrap();
        assert!(receipt.success);
    }

    #[tokio::test]
    async fn failures_map_to_closed_errors() {
        // JSON-RPC error object: a send is rejected, a lookup is unavailable.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "nonce too low"}}),
            ))
            .mount(&server)
            .await;
        let client = rpc(&server, Duration::from_secs(5));
        assert_eq!(
            client.send_raw_transaction_sync(&[0x76]).await,
            Err(RpcError::Rejected)
        );
        assert_eq!(
            client.get_transaction_receipt([1; 32]).await,
            Err(RpcError::Unavailable)
        );

        // HTTP error status.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::Unavailable)
        );

        // Not JSON, and an invalid receipt.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::InvalidResponse)
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result": {"status": "0x1"}})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::InvalidResponse)
        );

        // A send whose result is null is not a receipt.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": null})))
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .send_raw_transaction_sync(&[0x76])
                .await,
            Err(RpcError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn oversized_and_slow_responses_are_cut_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE_BYTES + 1)),
            )
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::InvalidResponse)
        );

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"result": null}))
                    .set_delay(Duration::from_secs(3)),
            )
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        assert_eq!(
            rpc(&server, Duration::from_millis(150))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::Unavailable)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": null})))
            .expect(0)
            .mount(&target)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", target.uri().as_str()),
            )
            .mount(&server)
            .await;
        assert_eq!(
            rpc(&server, Duration::from_secs(5))
                .get_transaction_receipt([1; 32])
                .await,
            Err(RpcError::Unavailable)
        );
    }

    #[test]
    fn debug_never_prints_the_endpoint() {
        let rpc = HttpTempoRpc::new(
            "https://rpc.example/v1/secret-provider-key",
            Duration::from_secs(1),
        )
        .unwrap();
        let printed = format!("{rpc:?}");
        assert!(!printed.contains("secret-provider-key"));
        assert!(!printed.contains("rpc.example"));
    }
}
