//! PostgreSQL top-up refresh and production streaming usage accounting.

use super::*;
use services::mpp::{abi, session::ChannelDescriptor, ChannelRecord};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOP_UP: u128 = 1_000_000;
const PROMPT: &str = "one two three four five six seven eight";
const ANSWER: &str = "alpha beta gamma delta epsilon";

fn address_word(address: &[u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(address);
    word
}

fn uint_word(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

struct Fixture {
    server: axum_test::TestServer,
    repo: MppChannelRepository,
    payer_key: k256::SecretKey,
    descriptor: ChannelDescriptor,
    escrow: [u8; 20],
    id: [u8; 32],
    raw_open: Vec<u8>,
    offer: Value,
    body: String,
    deposit: Arc<Mutex<u128>>,
    top_ups: Arc<Mutex<HashSet<[u8; 32]>>>,
    _node: MockServer,
}

impl Fixture {
    async fn new() -> Self {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::level_filters::LevelFilter::WARN)
            .try_init();
        let vectors: Value = serde_json::from_str(VECTORS).unwrap();
        let payer_key = k256::SecretKey::from_slice(&bytes(&vectors, "payerKey")).unwrap();
        let payee = fixed::<20>(&vectors, "payee");
        let currency = fixed::<20>(&vectors, "token");
        let escrow = fixed::<20>(&vectors, "escrow");
        let mut salt = [0; 32];
        salt[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        salt[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let (mut open, _) = decode_signed(&bytes(&vectors, "open.serialized")).unwrap();
        open.calls[0].input =
            open_calldata(&payee, &[0; 20], &currency, DEPOSIT, &salt, &[0; 20]).unwrap();
        open.set_expiring_nonce(u64::try_from(chrono::Utc::now().timestamp()).unwrap())
            .unwrap();
        let raw_open =
            open.serialize_signed(&sign_digest(&payer_key, &open.signing_hash()).unwrap());
        let verified = verify_open_transaction(
            &raw_open,
            &OpenTerms {
                escrow,
                chain_id: 42431,
                payee,
                token: currency,
            },
        )
        .unwrap();
        let node = MockServer::start().await;
        let deposit = Arc::new(Mutex::new(DEPOSIT));
        let top_ups = Arc::new(Mutex::new(HashSet::new()));
        let chain_deposit = deposit.clone();
        let seen = top_ups.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method":"eth_sendRawTransactionSync"})))
            .respond_with(move |request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let raw = hex::decode(body["params"][0].as_str().unwrap().strip_prefix("0x").unwrap()).unwrap();
                let (tx, signature) = decode_signed(&raw).unwrap();
                let sender = tx.recover_sender(&signature).unwrap();
                let hash = tx_hash(&raw);
                if tx.calls[0].input.starts_with(&abi::TOP_UP_SELECTOR)
                    && seen.lock().unwrap().insert(hash) {
                    *chain_deposit.lock().unwrap() += TOP_UP;
                }
                ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0", "id":1, "result": {
                    "transactionHash":hex_prefixed(&hash), "status":"0x1", "from":hex_prefixed(&sender),
                    "blockNumber":"0x1", "logs":[]
                }}))
            }).mount(&node).await;
        let chain_deposit = deposit.clone();
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method":"eth_call"})))
            .respond_with(move |_: &Request| {
                let mut state = [0u8; 96];
                state[48..64].copy_from_slice(&chain_deposit.lock().unwrap().to_be_bytes());
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc":"2.0", "id":1, "result":hex_prefixed(&state)}))
            })
            .mount(&node)
            .await;
        for (rpc_method, result) in [
            ("eth_gasPrice", json!("0x1")),
            ("eth_getTransactionReceipt", Value::Null),
        ] {
            Mock::given(method("POST"))
                .and(body_partial_json(json!({"method":rpc_method})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"jsonrpc":"2.0", "id":1, "result":result})),
                )
                .mount(&node)
                .await;
        }
        let bootstrap = setup_test_server().await;
        let org = setup_org_with_credits(&bootstrap, 10_000_000_000).await;
        let service_key = get_api_key_for_org(&bootstrap, org.id.clone()).await;
        let settings = MppSettings {
            realm: "session-e2e.test".into(),
            secret_key: "session-e2e-hmac-secret-0123456789abcdef".into(),
            service_api_key: service_key,
            challenge_ttl_secs: 600,
            tempo: MppTempoSettings {
                rpc_url: node.uri(),
                chain_id: 42431,
                currency: hex_prefixed(&currency),
                recipient: hex_prefixed(&payee),
                rpc_timeout_secs: 5,
            },
            model_prices: BTreeMap::from([(
                E2E_QWEN_MODEL_NAME.to_string(),
                MppModelPrice {
                    amount: 0,
                    token_price: Some(TOKEN_PRICE),
                    max_tokens: 256,
                    max_request_bytes: 8192,
                },
            )]),
            session: Some(MppSessionSettings {
                signer_key: Some(vectors["payeeKey"].as_str().unwrap().to_string()),
                min_voucher_delta: Some(1),
                suggested_deposit: DEPOSIT,
                settle_interval_secs: 3600,
                settle_amount: DEPOSIT,
                close_supervision_secs: 60,
            }),
        };
        let (server, _pool, mock, database) = setup_test_server_with_pool_and_config(|config| {
            config.mpp = MppConfig::Enabled(Box::new(settings));
        })
        .await;
        mock.set_default_response(inference_providers::mock::ResponseTemplate::new(ANSWER))
            .await;
        let body = json!({"model":E2E_QWEN_MODEL_NAME, "max_tokens":64,
            "messages":[{"role":"user", "content":PROMPT}]})
        .to_string();
        let unpaid = server
            .post("/v1/chat/completions")
            .add_header("Content-Type", "application/json")
            .bytes(body.clone().into())
            .await;
        assert_eq!(unpaid.status_code(), 402, "{}", unpaid.text());
        let offer = challenge(unpaid.header("www-authenticate").to_str().unwrap());
        let fixture = Self {
            server,
            repo: MppChannelRepository::new(database.pool().clone()),
            payer_key,
            descriptor: verified.descriptor,
            escrow,
            id: verified.channel_id,
            raw_open,
            offer,
            body,
            deposit,
            top_ups,
            _node: node,
        };
        let mut payload = fixture.voucher(GOOD_VOUCHER);
        payload["action"] = json!("open");
        payload["type"] = json!("transaction");
        payload["transaction"] = json!(hex_prefixed(&fixture.raw_open));
        let opened = fixture.post(&fixture.body, payload).await;
        assert_eq!(opened.status_code(), 200, "{}", opened.text());
        assert_eq!(fixture.row().await.spent, 0);
        fixture
    }

    fn voucher(&self, amount: u128) -> Value {
        let digest = voucher_digest(&self.escrow, 42431, &self.id, amount).unwrap();
        json!({"action":"voucher", "channelId":hex_prefixed(&self.id),
            "descriptor":descriptor_json(&self.descriptor), "cumulativeAmount":amount.to_string(),
            "signature":hex_prefixed(&sign_digest(&self.payer_key, &digest).unwrap())})
    }

    async fn post(&self, body: &str, payload: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/chat/completions")
            .add_header("Content-Type", "application/json")
            .add_header("Authorization", credential(&self.offer, payload))
            .bytes(body.to_owned().into())
            .await
    }

    async fn row(&self) -> ChannelRecord {
        self.repo.get(&self.id).await.unwrap().unwrap()
    }

    async fn charged(&self, units: u64) -> ChannelRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = self.row().await;
                if row.units >= units && row.reserved == 0 {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("usage observed and reservation released")
    }
}

#[tokio::test]
async fn session_top_up_refreshes_postgres_deposit_and_replay_does_not_double_credit() {
    let f = Fixture::new().await;
    let before = f.row().await;
    assert_eq!(before.deposit, DEPOSIT);
    let (mut tx, _) = decode_signed(&f.raw_open).unwrap();
    let d = &f.descriptor;
    let mut calldata = abi::TOP_UP_SELECTOR.to_vec();
    for address in [&d.payer, &d.payee, &d.operator, &d.token] {
        calldata.extend_from_slice(&address_word(address));
    }
    calldata.extend_from_slice(&d.salt);
    calldata.extend_from_slice(&address_word(&d.authorized_signer));
    calldata.extend_from_slice(&d.expiring_nonce_hash);
    calldata.extend_from_slice(&uint_word(TOP_UP));
    tx.calls[0].input = calldata;
    let raw = tx.serialize_signed(&sign_digest(&f.payer_key, &tx.signing_hash()).unwrap());
    let payload = json!({"action":"topUp", "type":"transaction", "channelId":hex_prefixed(&f.id),
        "descriptor":descriptor_json(d), "additionalDeposit":TOP_UP.to_string(), "transaction":hex_prefixed(&raw)});
    for _ in 0..2 {
        let response = f.post(&f.body, payload.clone()).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        let payment = receipt(response.header("payment-receipt").to_str().unwrap());
        assert_eq!(payment["reference"], hex_prefixed(&f.id));
        assert_eq!(payment["txHash"], hex_prefixed(&tx_hash(&raw)));
        let row = f.row().await;
        assert_eq!(row.deposit, DEPOSIT + TOP_UP);
        assert_eq!(
            (row.spent, row.reserved, row.units),
            (before.spent, before.reserved, before.units)
        );
        assert_eq!(row.highest_cumulative, before.highest_cumulative);
    }
    assert_eq!(*f.deposit.lock().unwrap(), DEPOSIT + TOP_UP);
    assert_eq!(
        f.top_ups.lock().unwrap().len(),
        1,
        "one unique mined top-up"
    );
}

async fn assert_streaming_charge(include_usage: bool) {
    let f = Fixture::new().await;
    // Both mock transports derive prompt tokens from words and completion tokens from
    // this explicit response. Read a nonstream completion to anchor the stream expectation.
    let baseline = f.post(&f.body, f.voucher(GOOD_VOUCHER)).await;
    assert_eq!(baseline.status_code(), 200, "{}", baseline.text());
    let baseline: Value = baseline.json();
    let prompt = baseline["usage"]["prompt_tokens"].as_u64().unwrap();
    let completion = baseline["usage"]["completion_tokens"].as_u64().unwrap();
    assert_eq!((prompt, completion), (8, 5));
    let units = prompt + completion;
    let before = f.charged(units).await;
    assert_eq!(before.units, units);
    assert_eq!(before.spent, TOKEN_PRICE * u128::from(units));
    let mut body: Value = serde_json::from_str(&f.body).unwrap();
    body["stream"] = json!(true);
    if include_usage {
        body["stream_options"] = json!({"include_usage":true});
    }
    // The digest binds the exact streaming body, so request its own session challenge.
    let body = body.to_string();
    let unpaid = f
        .server
        .post("/v1/chat/completions")
        .add_header("Content-Type", "application/json")
        .bytes(body.clone().into())
        .await;
    assert_eq!(unpaid.status_code(), 402, "{}", unpaid.text());
    let offer = challenge(unpaid.header("www-authenticate").to_str().unwrap());
    let stream = f
        .server
        .post("/v1/chat/completions")
        .add_header("Content-Type", "application/json")
        .add_header("Authorization", credential(&offer, f.voucher(GOOD_VOUCHER)))
        .bytes(body.into())
        .await;
    assert_eq!(stream.status_code(), 200, "{}", stream.text());
    assert!(stream
        .header("content-type")
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert!(stream.text().contains("data: [DONE]"));
    let mut content = String::new();
    for line in stream
        .text()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        if line == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(line).unwrap();
        if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
            content.push_str(delta);
        }
    }
    assert_eq!(content, ANSWER);
    let after = f.charged(before.units + units).await;
    assert_eq!(
        after.units,
        before.units + units,
        "prompt plus completion, charged once"
    );
    assert_eq!(after.spent, before.spent + TOKEN_PRICE * u128::from(units));
    assert_eq!(after.reserved, 0);
}

#[tokio::test]
async fn session_chat_streaming_charges_exact_units_with_usage() {
    assert_streaming_charge(true).await;
}

#[tokio::test]
async fn session_chat_streaming_charges_exact_units_without_usage() {
    assert_streaming_charge(false).await;
}
