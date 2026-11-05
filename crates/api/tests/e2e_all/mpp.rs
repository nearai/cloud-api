//! E2E tests for MPP (HTTP 402 payments): the Postgres claim repository and the full paid
//! request through the real router, with a scripted Tempo node (wiremock on loopback).
//!
//! They need the `mpp_payment_claims` table (`V0098__mpp_payment_claims.sql` in
//! nearai/cloud-platform), which arrives with the schema pin in
//! `scripts/cloud-platform-schema.env`.

use crate::common::*;
use config::{MppConfig, MppModelPrice, MppSettings, MppTempoSettings};
use serde_json::{json, Value};
use services::mpp::testing::{credential_header, hash_payload, payment_receipt};
use services::mpp::{
    protocol, tempo, Challenge, ClaimLease, ClaimOutcome, ClaimState, PaymentClaimRepository,
    RpcReceipt, ServeOutcome,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "mpp_claim_regressions.rs"]
mod claim_regressions;

const REALM: &str = "mpp-e2e.test";
const SECRET: &str = "e2e-hmac-secret-0123456789abcdef0123456789";
const AMOUNT: u128 = 20_000;
const CURRENCY: [u8; 20] = [0x20; 20];
const RECIPIENT: [u8; 20] = [0xaa; 20];
const PAYER: [u8; 20] = [0x40; 20];

fn unique_key() -> String {
    format!(
        "tempo:0x{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

async fn state_of(database: &Arc<database::Database>, key: &str) -> Option<String> {
    let client = database.pool().get().await.expect("connection");
    client
        .query_opt(
            "SELECT state FROM mpp_payment_claims WHERE payment_key = $1",
            &[&key],
        )
        .await
        .expect("select state")
        .map(|row| row.get::<_, String>(0))
}

fn repository(
    database: &Arc<database::Database>,
) -> database::repositories::MppPaymentClaimRepository {
    database::repositories::MppPaymentClaimRepository::new(database.pool().clone())
}

async fn claimed(
    repo: &database::repositories::MppPaymentClaimRepository,
    key: &str,
    challenge: &str,
) -> ClaimLease {
    match repo.claim(key, challenge).await.unwrap() {
        ClaimOutcome::Claimed(lease) => lease,
        ClaimOutcome::AlreadyUsed => panic!("expected a claim"),
    }
}

// ----- the Postgres claim repository -----

#[tokio::test]
async fn claim_is_single_use_and_unserved_is_retryable_by_the_same_challenge_only() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = repository(&database);
    let key = unique_key();

    let first = claimed(&repo, &key, "challenge-a").await;
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("serving"));
    assert!(repo.is_spent(&key, "challenge-a").await.unwrap());
    assert!(repo.is_spent(&key, "challenge-b").await.unwrap());

    // A replay while serving, by the same and by another challenge.
    assert_eq!(
        repo.claim(&key, "challenge-a").await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );
    assert_eq!(
        repo.claim(&key, "challenge-b").await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );

    repo.finish(&key, first, ServeOutcome::Unserved)
        .await
        .unwrap();
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("unserved"));
    assert!(!repo.is_spent(&key, "challenge-a").await.unwrap());
    assert!(repo.is_spent(&key, "challenge-b").await.unwrap());

    // Another challenge cannot take an unserved payment; the same one can.
    assert_eq!(
        repo.claim(&key, "challenge-b").await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("unserved"));
    let second = claimed(&repo, &key, "challenge-a").await;
    assert_ne!(second, first, "every attempt gets its own lease");
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("serving"));

    // A stale finish of the first attempt (a retried statement whose reply was lost) must
    // not release the second attempt's claim.
    repo.finish(&key, first, ServeOutcome::Unserved)
        .await
        .unwrap();
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("serving"));

    // Served is terminal: it cannot be reclaimed, and a late finish cannot reopen it.
    repo.finish(&key, second, ServeOutcome::Served)
        .await
        .unwrap();
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("served"));
    assert!(repo.is_spent(&key, "challenge-a").await.unwrap());
    assert!(repo.is_spent(&key, "challenge-b").await.unwrap());
    assert_eq!(
        repo.claim(&key, "challenge-a").await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );
    repo.finish(&key, second, ServeOutcome::Unserved)
        .await
        .unwrap();
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("served"));
}

#[tokio::test]
async fn concurrent_claims_of_one_payment_have_exactly_one_winner() {
    let (_server, database) = setup_test_server_with_database().await;
    let key = unique_key();
    let mut tasks = Vec::new();
    for n in 0..16 {
        let repo = repository(&database);
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            repo.claim(&key, &format!("challenge-{}", n % 2))
                .await
                .unwrap()
        }));
    }
    let mut claimed = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), ClaimOutcome::Claimed(_)) {
            claimed += 1;
        }
    }
    assert_eq!(claimed, 1, "one payment, one winner, however many race");
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("serving"));
}

#[tokio::test]
async fn purge_deletes_only_claims_older_than_the_cutoff() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = repository(&database);
    let (old_a, old_b, fresh) = (unique_key(), unique_key(), unique_key());
    for key in [&old_a, &old_b, &fresh] {
        claimed(&repo, key, "challenge-a").await;
    }
    let client = database.pool().get().await.expect("connection");
    client
        .execute(
            "UPDATE mpp_payment_claims SET created_at = NOW() - INTERVAL '25 hours' \
             WHERE payment_key = ANY($1)",
            &[&vec![old_a.clone(), old_b.clone()]],
        )
        .await
        .expect("age two claims");

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
    // Other tests share the table; only this test ages rows, so at least its two go.
    let purged = repo.purge_older_than(cutoff).await.unwrap();
    assert!(purged >= 2, "purged {purged}");
    assert_eq!(state_of(&database, &old_a).await, None);
    assert_eq!(state_of(&database, &old_b).await, None);
    assert_eq!(
        state_of(&database, &fresh).await.as_deref(),
        Some("serving")
    );
    // Nothing old is left for a second run to find among this test's rows.
    repo.purge_older_than(cutoff).await.unwrap();
    assert_eq!(
        state_of(&database, &fresh).await.as_deref(),
        Some("serving")
    );
}

#[tokio::test]
async fn finish_ignores_unknown_keys_and_the_table_rejects_unknown_states() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = repository(&database);
    let unknown = unique_key();
    repo.finish(
        &unknown,
        ClaimLease::new(chrono::Utc::now()),
        ServeOutcome::Served,
    )
    .await
    .unwrap();
    assert_eq!(state_of(&database, &unknown).await, None);

    // The CHECK constraint is the closed state set.
    let client = database.pool().get().await.expect("connection");
    let bad = unique_key();
    let result = client
        .execute(
            "INSERT INTO mpp_payment_claims (payment_key, challenge_id, state) VALUES ($1, 'c', 'done')",
            &[&bad],
        )
        .await;
    assert!(
        result.is_err(),
        "state outside serving/served/unserved must be refused"
    );
    for state in [
        ClaimState::Serving,
        ClaimState::Served,
        ClaimState::Unserved,
    ] {
        let key = unique_key();
        client
            .execute(
                "INSERT INTO mpp_payment_claims (payment_key, challenge_id, state) VALUES ($1, 'c', $2)",
                &[&key, &state.as_str()],
            )
            .await
            .expect("each closed state is storable");
    }
}

// ----- the full paid request -----

fn receipt_json(receipt: &RpcReceipt) -> Value {
    let hex = |bytes: &[u8]| format!("0x{}", hex::encode(bytes));
    json!({
        "transactionHash": hex(&receipt.tx_hash),
        "status": if receipt.success { "0x1" } else { "0x0" },
        "from": hex(&receipt.from),
        "blockNumber": format!("0x{:x}", receipt.block_number),
        "logs": receipt.logs.iter().map(|log| json!({
            "address": hex(&log.address),
            "topics": log.topics.iter().map(|t| hex(t)).collect::<Vec<_>>(),
            "data": hex(&log.data),
        })).collect::<Vec<_>>(),
    })
}

/// Parse `Payment k="v", …` (the values this server emits contain no escapes).
fn parse_challenge(header: &str) -> Challenge {
    let rest = header.strip_prefix("Payment ").expect("Payment scheme");
    let mut params = BTreeMap::new();
    for part in rest.split(", ") {
        let (name, value) = part.split_once('=').expect("k=v");
        params.insert(name.to_string(), value.trim_matches('"').to_string());
    }
    Challenge {
        id: params["id"].clone(),
        realm: params["realm"].clone(),
        request: params["request"].clone(),
        expires: chrono::DateTime::parse_from_rfc3339(&params["expires"])
            .expect("expires")
            .with_timezone(&chrono::Utc),
        digest: params["digest"].clone(),
        opaque: params["opaque"].clone(),
    }
}

fn mpp_settings(rpc_url: &str, service_key: &str) -> MppSettings {
    let mut prices = BTreeMap::new();
    prices.insert(
        E2E_QWEN_MODEL_NAME.to_string(),
        MppModelPrice {
            amount: AMOUNT,
            token_price: None,
            max_tokens: 256,
            max_request_bytes: 8192,
        },
    );
    MppSettings {
        realm: REALM.to_string(),
        secret_key: SECRET.to_string(),
        service_api_key: service_key.to_string(),
        challenge_ttl_secs: 600,
        tempo: MppTempoSettings {
            rpc_url: rpc_url.to_string(),
            chain_id: 42431,
            currency: format!("0x{}", hex::encode(CURRENCY)),
            recipient: format!("0x{}", hex::encode(RECIPIENT)),
            rpc_timeout_secs: 5,
        },
        model_prices: prices,
        session: None,
    }
}

#[tokio::test]
async fn paid_chat_completion_end_to_end() {
    // The service workspace and key live in the shared database, so a first server can mint
    // them for the MPP-enabled one.
    let bootstrap = setup_test_server().await;
    let org = setup_org_with_credits(&bootstrap, 10_000_000_000i64).await;
    let service_key = get_api_key_for_org(&bootstrap, org.id.clone()).await;

    let tempo_node = MockServer::start().await;
    let settings = mpp_settings(&tempo_node.uri(), &service_key);
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.mpp = MppConfig::Enabled(Box::new(settings));
    })
    .await;

    let body = json!({
        "model": E2E_QWEN_MODEL_NAME,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello over MPP"}],
    })
    .to_string();
    let post = |authorization: Option<String>| {
        let mut request = server
            .post("/v1/chat/completions")
            .add_header("Content-Type", "application/json")
            .bytes(body.clone().into());
        if let Some(value) = authorization {
            request = request.add_header("Authorization", value);
        }
        request
    };

    // 1. No credential: a challenge.
    let unpaid = post(None).await;
    assert_eq!(unpaid.status_code(), 402, "{}", unpaid.text());
    assert_eq!(unpaid.header("cache-control"), "no-store");
    let challenge = parse_challenge(unpaid.header("www-authenticate").to_str().unwrap());
    assert_eq!(challenge.realm, REALM);

    // 2. The client paid: the node knows a receipt with this challenge's memo.
    let tx_hash = [0x5a; 32];
    let tx_hash = {
        let mut hash = tx_hash;
        hash[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        hash
    };
    let memo = tempo::attribution_memo(REALM, &challenge.id);
    let receipt = payment_receipt(tx_hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, Some(memo));
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "eth_getTransactionReceipt"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"jsonrpc": "2.0", "id": 1, "result": receipt_json(&receipt)}),
            ),
        )
        .mount(&tempo_node)
        .await;
    let credential = credential_header(&challenge, hash_payload(tx_hash), None);

    // 3. Paid: the completion, a receipt, and usage recorded against the service workspace.
    let paid = post(Some(credential.clone())).await;
    assert_eq!(paid.status_code(), 200, "{}", paid.text());
    // The settle layer marks the response `private`; the app-wide request-correlation layer
    // then sets `no-store` on every response, which is stricter, so a receipt is never cached.
    assert_eq!(paid.header("cache-control"), "no-store");
    let receipt_header = paid.header("payment-receipt").to_str().unwrap().to_string();
    let decoded: Value =
        serde_json::from_slice(&protocol::b64url_decode(&receipt_header).unwrap()).unwrap();
    assert_eq!(decoded["reference"], format!("0x{}", hex::encode(tx_hash)));
    let completion: api::models::ChatCompletionResponse = paid.json();
    assert!(completion.usage.completion_tokens > 0);
    let key = protocol::payment_key(SECRET.as_bytes(), &tx_hash).unwrap();
    assert_eq!(state_of(&database, &key).await.as_deref(), Some("served"));

    // 4. Replaying the credential is a 403 with no new challenge.
    let replay = post(Some(credential)).await;
    assert_eq!(replay.status_code(), 403, "{}", replay.text());
    assert!(replay.maybe_header("www-authenticate").is_none());

    // 5. A Bearer caller is unaffected by MPP being on.
    let bearer = post(Some(format!("Bearer {service_key}"))).await;
    assert_eq!(bearer.status_code(), 200, "{}", bearer.text());
    assert!(bearer.maybe_header("payment-receipt").is_none());
}

#[tokio::test]
async fn mpp_disabled_keeps_todays_401_for_an_unauthenticated_request() {
    let server = setup_test_server().await;
    let response = server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": E2E_QWEN_MODEL_NAME,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;
    assert_eq!(response.status_code(), 401);
    assert!(response.maybe_header("www-authenticate").is_none());
}
