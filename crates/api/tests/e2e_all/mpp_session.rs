// INTEGRATION: the production MPP router, real V0100 Postgres ledger, and a scripted Tempo RPC.
// Each run re-signs an open transaction with a random salt and owns its channel row.

use crate::common::*;
use config::{MppConfig, MppModelPrice, MppSessionSettings, MppSettings, MppTempoSettings};
use database::repositories::MppChannelRepository;
use serde_json::{json, Value};
use services::mpp::session::{
    close_authorization_digest, decode_signed, open_calldata, sign_digest, tx_hash,
    verify_open_transaction, voucher_digest, OpenTerms,
};
use services::mpp::session_wire::{descriptor_json, hex_prefixed};
use services::mpp::{protocol, ChannelLifecycle, ChannelRepository};
use std::collections::BTreeMap;
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const VECTORS: &str = include_str!("../../../services/src/mpp/testdata/session_vectors.json");
const TOKEN_PRICE: u128 = 2;
const DEPOSIT: u128 = 5_000_000;
const GOOD_VOUCHER: u128 = 100_000;

fn bytes(value: &Value, path: &str) -> Vec<u8> {
    let v = path.split('.').fold(value, |v, k| &v[k]);
    hex::decode(v.as_str().unwrap().strip_prefix("0x").unwrap()).unwrap()
}

fn fixed<const N: usize>(value: &Value, path: &str) -> [u8; N] {
    bytes(value, path).try_into().unwrap()
}

fn challenge(header: &str) -> Value {
    let params: BTreeMap<_, _> = header
        .strip_prefix("Payment ")
        .unwrap()
        .split(", ")
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap();
            (k.to_owned(), Value::String(v.trim_matches('"').to_owned()))
        })
        .collect();
    serde_json::to_value(params).unwrap()
}

fn credential(challenge: &Value, payload: Value) -> String {
    format!(
        "Payment {}",
        protocol::b64url_encode(
            json!({
                "challenge": challenge, "payload": payload
            })
            .to_string()
            .as_bytes()
        )
    )
}

fn receipt(header: &str) -> Value {
    serde_json::from_slice(&protocol::b64url_decode(header).unwrap()).unwrap()
}

#[tokio::test]
async fn session_open_short_voucher_charge_and_close() {
    // Common setup otherwise enables driver DEBUG logs, including ledger parameters.
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
    let signature = sign_digest(&payer_key, &open.signing_hash()).unwrap();
    let raw = open.serialize_signed(&signature);
    let verified = verify_open_transaction(
        &raw,
        &OpenTerms {
            escrow,
            chain_id: 42431,
            payee,
            token: currency,
        },
    )
    .unwrap();
    let id = verified.channel_id;
    let id_hex = hex_prefixed(&id);
    let descriptor = descriptor_json(&verified.descriptor);
    let voucher = |amount: u128| {
        let digest = voucher_digest(&escrow, 42431, &id, amount).unwrap();
        json!({"action":"voucher", "channelId":id_hex, "descriptor":descriptor,
            "cumulativeAmount":amount.to_string(), "signature":hex_prefixed(&sign_digest(&payer_key, &digest).unwrap())})
    };

    let bootstrap = setup_test_server().await;
    let org = setup_org_with_credits(&bootstrap, 10_000_000_000).await;
    let service_key = get_api_key_for_org(&bootstrap, org.id.clone()).await;
    let node = MockServer::start().await;
    Mock::given(method("POST")).and(body_partial_json(json!({"method":"eth_sendRawTransactionSync"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let raw = hex::decode(body["params"][0].as_str().unwrap().strip_prefix("0x").unwrap()).unwrap();
            let (transaction, signature) = decode_signed(&raw).unwrap();
            let sender = transaction.recover_sender(&signature).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0", "id":1, "result": {
                "transactionHash":hex_prefixed(&tx_hash(&raw)), "status":"0x1", "from":hex_prefixed(&sender), "blockNumber":"0x1", "logs":[]
            }}))
        }).mount(&node).await;
    let mut state = [0u8; 96];
    state[48..64].copy_from_slice(&DEPOSIT.to_be_bytes());
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method":"eth_call"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0", "id":1, "result":hex_prefixed(&state)})),
        )
        .mount(&node)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method":"eth_gasPrice"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0", "id":1, "result":"0x1"})),
        )
        .mount(&node)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method":"eth_getTransactionReceipt"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0", "id":1, "result":null})),
        )
        .mount(&node)
        .await;
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
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.mpp = MppConfig::Enabled(Box::new(settings))
    })
    .await;
    let repo = MppChannelRepository::new(database.pool().clone());
    let body = json!({"model":E2E_QWEN_MODEL_NAME, "max_tokens":64, "messages":[{"role":"user", "content":"hello"}]}).to_string();
    let post = |auth: Option<String>| {
        let mut req = server
            .post("/v1/chat/completions")
            .add_header("Content-Type", "application/json")
            .bytes(body.clone().into());
        if let Some(auth) = auth {
            req = req.add_header("Authorization", auth);
        }
        req
    };
    let unpaid = post(None).await;
    assert_eq!(unpaid.status_code(), 402, "{}", unpaid.text());
    let offer = challenge(unpaid.header("www-authenticate").to_str().unwrap());
    assert_eq!(offer["intent"], "session");
    let mut open_payload = voucher(1);
    open_payload["action"] = json!("open");
    open_payload["type"] = json!("transaction");
    open_payload["transaction"] = json!(hex_prefixed(&raw));
    let opened = post(Some(credential(&offer, open_payload))).await;
    assert_eq!(opened.status_code(), 200, "{}", opened.text());
    let open_receipt = receipt(opened.header("payment-receipt").to_str().unwrap());
    assert_eq!(open_receipt["reference"], id_hex);
    assert_eq!(open_receipt["txHash"], hex_prefixed(&tx_hash(&raw)));
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(row.highest_cumulative, 1);
    assert_eq!(row.spent, 0);

    let short = post(Some(credential(&offer, voucher(1)))).await;
    assert_eq!(short.status_code(), 402, "{}", short.text());
    let retry = challenge(short.header("www-authenticate").to_str().unwrap());
    let terms: Value = serde_json::from_slice(
        &protocol::b64url_decode(retry["request"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let snapshot = &terms["methodDetails"]["sessionSnapshot"];
    assert_eq!(snapshot["channelId"], id_hex);
    let cap = TOKEN_PRICE * (body.len().div_ceil(4) as u128 + 64);
    assert_eq!(snapshot["requiredCumulative"], cap.to_string());
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_eq!((row.spent, row.reserved), (0, 0));

    let paid = post(Some(credential(&retry, voucher(GOOD_VOUCHER)))).await;
    assert_eq!(paid.status_code(), 200, "{}", paid.text());
    let paid_receipt = receipt(paid.header("payment-receipt").to_str().unwrap());
    assert_eq!(paid_receipt["reference"], id_hex);
    let completion: Value = paid.json();
    let units = completion["usage"]["prompt_tokens"].as_u64().unwrap()
        + completion["usage"]["completion_tokens"].as_u64().unwrap();
    assert!(units > 0);
    assert_eq!(paid_receipt["units"], units);
    assert_eq!(
        paid_receipt["spent"],
        (TOKEN_PRICE * u128::from(units)).to_string()
    );
    let row = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = repo.get(&id).await.unwrap().unwrap();
            if row.reserved == 0 && row.units == units {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("completion usage charged before deadline");
    assert_eq!(row.reserved, 0);
    assert_eq!(row.units, units);
    assert_eq!(row.spent, TOKEN_PRICE * u128::from(units));

    let mut close = voucher(GOOD_VOUCHER);
    close["action"] = json!("close");
    let digest = close_authorization_digest(&escrow, 42431, &id, GOOD_VOUCHER).unwrap();
    close["closeSignature"] = json!(hex_prefixed(&sign_digest(&payer_key, &digest).unwrap()));
    let closed = post(Some(credential(&retry, close))).await;
    assert_eq!(closed.status_code(), 200, "{}", closed.text());
    let close_receipt = receipt(closed.header("payment-receipt").to_str().unwrap());
    assert_eq!(close_receipt["reference"], id_hex);
    assert!(close_receipt["txHash"].as_str().is_some());
    let row = repo.get(&id).await.unwrap().unwrap();
    assert_eq!(row.state, ChannelLifecycle::Finalized);
    assert_eq!(row.settled_on_chain, TOKEN_PRICE * u128::from(units));
}
