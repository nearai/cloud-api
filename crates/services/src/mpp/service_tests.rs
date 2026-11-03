use std::collections::BTreeMap;

use chrono::Duration as ChronoDuration;
use config::{MppModelPrice, MppSettings, MppTempoSettings};
use serde_json::json;

use super::*;
use crate::mpp::memory::InMemoryClaimRepository;
use crate::mpp::ports::ClaimState;
use crate::mpp::protocol::{b64url_decode, b64url_encode, compute_challenge_id};
use crate::mpp::testing::{
    bound_payment_receipt, credential_header, hash_payload, payment_receipt, transaction_payload,
    FakeTempo, SendMode,
};

const REALM: &str = "cloud-api.near.ai";
const SECRET: &str = "0123456789abcdef0123456789abcdef-hmac";
const SERVICE_KEY: &str = "sk-live-service-key-0001";
const MODEL: &str = "zai-org/GLM-5";
const CURRENCY: [u8; 20] = [0x20; 20];
const RECIPIENT: [u8; 20] = [0xaa; 20];
const PAYER: [u8; 20] = [0x40; 20];
const AMOUNT: u128 = 20_000;
const CHAIN: u64 = 42431;

fn address(a: [u8; 20]) -> String {
    format!("0x{}", hex::encode(a))
}

fn settings() -> MppSettings {
    let mut prices = BTreeMap::new();
    prices.insert(
        MODEL.to_string(),
        MppModelPrice {
            amount: AMOUNT,
            max_tokens: 2048,
            max_request_bytes: 4096,
        },
    );
    MppSettings {
        realm: REALM.to_string(),
        secret_key: SECRET.to_string(),
        service_api_key: SERVICE_KEY.to_string(),
        challenge_ttl_secs: 600,
        tempo: MppTempoSettings {
            rpc_url: "https://rpc.test.invalid/secret-provider-key".to_string(),
            chain_id: CHAIN,
            currency: address(CURRENCY),
            recipient: address(RECIPIENT),
            rpc_timeout_secs: 5,
        },
        model_prices: prices,
    }
}

struct Fixture {
    service: Arc<MppService>,
    repo: Arc<InMemoryClaimRepository>,
    chain: Arc<FakeTempo>,
}

fn fixture() -> Fixture {
    let repo = Arc::new(InMemoryClaimRepository::default());
    let chain = Arc::new(FakeTempo::default());
    let service = MppService::new(&settings(), repo.clone(), chain.clone())
        .unwrap()
        .with_recover_poll(3, Duration::ZERO);
    Fixture {
        service: Arc::new(service),
        repo,
        chain,
    }
}

fn body() -> Vec<u8> {
    json!({"model": MODEL, "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]})
        .to_string()
        .into_bytes()
}

impl Fixture {
    fn challenge(&self, body: &[u8]) -> Challenge {
        self.service
            .challenge_for_body(CHAT_COMPLETIONS_ROUTE, body)
            .unwrap()
    }

    /// Challenge, a mined paying receipt, and the credential header for it.
    fn paid_hash_credential(&self, body: &[u8], tx_hash: [u8; 32]) -> (Challenge, String) {
        let challenge = self.challenge(body);
        self.chain.mine(bound_payment_receipt(
            tx_hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
        ));
        let header = credential_header(&challenge, hash_payload(tx_hash), None);
        (challenge, header)
    }

    fn verify(&self, header: &str, body: &[u8]) -> Result<VerifiedCredential, MppError> {
        self.service.verify(header, CHAT_COMPLETIONS_ROUTE, body)
    }
}

// ----- pricing -----

fn priced(service: &MppService, body: serde_json::Value) -> Pricing {
    service.price_body(body.to_string().as_bytes())
}

#[test]
fn flat_price_applies_when_the_limits_hold() {
    let f = fixture();
    assert_eq!(
        f.service.price_body(&body()),
        Pricing::Priced(Price { amount: AMOUNT })
    );
    // n = 1 explicitly, null n, and the exact caps are fine.
    for extra in [
        json!({"n": 1}),
        json!({"n": null}),
        json!({"max_tokens": 2048}),
    ] {
        let mut request = json!({"model": MODEL, "max_tokens": 10});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            priced(&f.service, request),
            Pricing::Priced(Price { amount: AMOUNT })
        );
    }
}

#[test]
fn unpriced_and_unparseable_requests_fall_through() {
    let f = fixture();
    assert_eq!(
        priced(&f.service, json!({"model": "other/model", "max_tokens": 1})),
        Pricing::Unpriced
    );
    assert_eq!(f.service.price_body(b"not json"), Pricing::Unpriced);
    assert_eq!(f.service.price_body(b""), Pricing::Unpriced);
    assert_eq!(f.service.price_body(b"[]"), Pricing::Unpriced);
    assert_eq!(
        priced(&f.service, json!({"max_tokens": 1})),
        Pricing::Unpriced
    );
    assert_eq!(
        priced(&f.service, json!({"model": 5, "max_tokens": 1})),
        Pricing::Unpriced
    );
    // A duplicated key is a parser-differential risk: refuse to price it.
    let duplicate = format!(r#"{{"model":"{MODEL}","model":"x","max_tokens":1}}"#);
    assert_eq!(
        f.service.price_body(duplicate.as_bytes()),
        Pricing::Unpriced
    );
    // Case and padding are not normalised.
    assert_eq!(
        priced(
            &f.service,
            json!({"model": "ZAI-ORG/GLM-5", "max_tokens": 1})
        ),
        Pricing::Unpriced
    );
    assert_eq!(
        priced(
            &f.service,
            json!({"model": " zai-org/GLM-5", "max_tokens": 1})
        ),
        Pricing::Unpriced
    );
}

#[test]
fn limit_violations_name_the_limit() {
    let f = fixture();
    let cases = [
        (
            json!({"model": MODEL}),
            LimitViolation::MaxTokensRequired { cap: 2048 },
        ),
        (
            json!({"model": MODEL, "max_tokens": null}),
            LimitViolation::MaxTokensRequired { cap: 2048 },
        ),
        (
            json!({"model": MODEL, "max_tokens": 2049}),
            LimitViolation::MaxTokensTooLarge { cap: 2048 },
        ),
        (
            json!({"model": MODEL, "max_completion_tokens": 99999}),
            LimitViolation::MaxTokensTooLarge { cap: 2048 },
        ),
        // The larger of the two decides, so a small max_tokens cannot hide a big one.
        (
            json!({"model": MODEL, "max_tokens": 1, "max_completion_tokens": 9000}),
            LimitViolation::MaxTokensTooLarge { cap: 2048 },
        ),
        (
            json!({"model": MODEL, "max_tokens": 9000, "max_completion_tokens": 1}),
            LimitViolation::MaxTokensTooLarge { cap: 2048 },
        ),
        (
            json!({"model": MODEL, "max_tokens": 0}),
            LimitViolation::MaxTokensInvalid,
        ),
        (
            json!({"model": MODEL, "max_tokens": -5}),
            LimitViolation::MaxTokensInvalid,
        ),
        (
            json!({"model": MODEL, "max_tokens": "100"}),
            LimitViolation::MaxTokensInvalid,
        ),
        (
            json!({"model": MODEL, "max_tokens": 100.5}),
            LimitViolation::MaxTokensInvalid,
        ),
        (
            json!({"model": MODEL, "max_tokens": 10, "max_completion_tokens": "x"}),
            LimitViolation::MaxTokensInvalid,
        ),
        (
            json!({"model": MODEL, "max_tokens": 10, "n": 2}),
            LimitViolation::MultipleChoices,
        ),
        (
            json!({"model": MODEL, "max_tokens": 10, "n": 0}),
            LimitViolation::MultipleChoices,
        ),
        (
            json!({"model": MODEL, "max_tokens": 10, "n": "1"}),
            LimitViolation::MultipleChoices,
        ),
    ];
    for (request, expected) in cases {
        assert_eq!(
            priced(&f.service, request.clone()),
            Pricing::Violation(expected),
            "{request}"
        );
        assert!(!expected.message().is_empty());
    }
    // Body size.
    let padded = json!({"model": MODEL, "max_tokens": 10, "pad": "x".repeat(5000)});
    assert_eq!(
        priced(&f.service, padded),
        Pricing::Violation(LimitViolation::BodyTooLarge { cap: 4096 })
    );
    // Messages name the configured limit and never echo request content.
    assert!(LimitViolation::MaxTokensTooLarge { cap: 2048 }
        .message()
        .contains("2048"));
    assert!(LimitViolation::BodyTooLarge { cap: 4096 }
        .message()
        .contains("4096"));
}

#[test]
fn body_cap_is_inclusive() {
    let f = fixture();
    let mut request = json!({"model": MODEL, "max_tokens": 10, "pad": ""});
    let base = request.to_string().len();
    request["pad"] = json!("x".repeat(4096 - base));
    assert_eq!(request.to_string().len(), 4096);
    assert!(matches!(
        priced(&f.service, request.clone()),
        Pricing::Priced(_)
    ));
    request["pad"] = json!("x".repeat(4096 - base + 1));
    assert!(matches!(
        priced(&f.service, request),
        Pricing::Violation(LimitViolation::BodyTooLarge { .. })
    ));
}

// ----- challenge -----

#[test]
fn challenge_is_hmac_bound_and_expires_after_the_ttl() {
    let f = fixture();
    let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00.789Z")
        .unwrap()
        .with_timezone(&Utc);
    let price = Price { amount: AMOUNT };
    let challenge = f
        .service
        .challenge_at(now, CHAT_COMPLETIONS_ROUTE, &body(), &price)
        .unwrap();
    assert_eq!(challenge.expires_string(), "2030-01-01T00:10:00Z");
    assert_eq!(challenge.realm, REALM);
    assert_eq!(challenge.digest, body_digest(&body()));
    assert_eq!(
        b64url_decode(&challenge.request).unwrap(),
        format!(
            r#"{{"amount":"20000","currency":"{}","methodDetails":{{"chainId":42431}},"recipient":"{}"}}"#,
            address(CURRENCY),
            address(RECIPIENT)
        )
        .into_bytes()
    );
    // Deterministic for equal inputs, different for a different body.
    let again = f
        .service
        .challenge_at(now, CHAT_COMPLETIONS_ROUTE, &body(), &price)
        .unwrap();
    assert_eq!(challenge.id, again.id);
    let other = f
        .service
        .challenge_at(now, CHAT_COMPLETIONS_ROUTE, b"{\"model\":\"x\"}", &price)
        .unwrap();
    assert_ne!(challenge.id, other.id);
    let header = challenge.to_header().unwrap();
    assert!(header.starts_with("Payment id=\""));
}

#[test]
fn challenge_for_body_rejects_what_cannot_be_paid_for() {
    let f = fixture();
    assert_eq!(
        f.service
            .challenge_for_body(CHAT_COMPLETIONS_ROUTE, b"{\"model\":\"nope\"}"),
        Err(MppError::NotPriced)
    );
    assert!(matches!(
        f.service
            .challenge_for_body(CHAT_COMPLETIONS_ROUTE, br#"{"model":"zai-org/GLM-5"}"#),
        Err(MppError::Limit(LimitViolation::MaxTokensRequired { .. }))
    ));
}

// ----- verify (stateless) -----

#[test]
fn a_genuine_credential_verifies_for_its_own_body_and_route() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let header = credential_header(&challenge, hash_payload([1; 32]), None);
    let verified = f.verify(&header, &body()).unwrap();
    assert_eq!(verified.challenge_id(), challenge.id);
    assert_eq!(verified.amount(), AMOUNT);
}

/// Re-sign a tampered echo with the server secret so only the semantic checks can catch it.
fn credential_with(
    f: &Fixture,
    challenge: &Challenge,
    edit: impl FnOnce(&mut serde_json::Value),
    resign: bool,
) -> String {
    let mut credential = json!({
        "challenge": {
            "id": challenge.id, "realm": challenge.realm, "method": "tempo", "intent": "charge",
            "request": challenge.request, "expires": challenge.expires_string(),
            "digest": challenge.digest, "opaque": challenge.opaque,
        },
        "payload": hash_payload([1; 32]),
    });
    edit(&mut credential);
    if resign {
        let echo = &credential["challenge"];
        let text = |name: &str| echo[name].as_str().map(str::to_string);
        let id = compute_challenge_id(
            SECRET.as_bytes(),
            &BoundFields {
                realm: &text("realm").unwrap(),
                method: &text("method").unwrap(),
                intent: &text("intent").unwrap(),
                request: &text("request").unwrap(),
                expires: text("expires").as_deref(),
                digest: text("digest").as_deref(),
                opaque: text("opaque").as_deref(),
            },
        )
        .unwrap();
        credential["challenge"]["id"] = json!(id);
    }
    let _ = f;
    format!(
        "Payment {}",
        b64url_encode(credential.to_string().as_bytes())
    )
}

#[test]
fn tampered_challenge_fields_are_invalid_challenge() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let other_request = ChargeRequest {
        amount: 1,
        currency: address(CURRENCY),
        chain_id: CHAIN,
        recipient: address(RECIPIENT),
    }
    .to_b64()
    .unwrap();
    let later = (challenge.expires + ChronoDuration::hours(5)).to_rfc3339();
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "request",
            Box::new(move |c| c["challenge"]["request"] = json!(other_request.clone())),
        ),
        (
            "expires",
            Box::new(move |c| c["challenge"]["expires"] = json!(later.clone())),
        ),
        (
            "digest",
            Box::new(|c| c["challenge"]["digest"] = json!(body_digest(b"other"))),
        ),
        (
            "opaque",
            Box::new(|c| c["challenge"]["opaque"] = json!(opaque_for_route("/v1/other").unwrap())),
        ),
        ("id", Box::new(|c| c["challenge"]["id"] = json!("AAAA"))),
        (
            "realm",
            Box::new(|c| c["challenge"]["realm"] = json!("evil.example")),
        ),
    ];
    for (label, edit) in edits {
        let header = credential_with(&f, &challenge, edit, false);
        assert!(
            matches!(
                f.verify(&header, &body()),
                Err(MppError::InvalidChallenge(_))
            ),
            "{label}: {:?}",
            f.verify(&header, &body())
        );
    }
    // Missing bound optional fields change the HMAC input.
    for field in ["expires", "digest", "opaque"] {
        let header = credential_with(
            &f,
            &challenge,
            |c| {
                c["challenge"].as_object_mut().unwrap().remove(field);
            },
            false,
        );
        assert!(
            matches!(
                f.verify(&header, &body()),
                Err(MppError::InvalidChallenge(_))
            ),
            "{field}"
        );
    }
}

#[test]
fn a_validly_signed_but_wrong_challenge_is_still_refused() {
    let f = fixture();
    let challenge = f.challenge(&body());
    // Properly HMAC-signed, so only the semantic checks can refuse these.
    let wrong_recipient = ChargeRequest {
        amount: AMOUNT,
        currency: address(CURRENCY),
        chain_id: CHAIN,
        recipient: address([0xbb; 20]),
    }
    .to_b64()
    .unwrap();
    let wrong_chain = ChargeRequest {
        amount: AMOUNT,
        currency: address(CURRENCY),
        chain_id: 1,
        recipient: address(RECIPIENT),
    }
    .to_b64()
    .unwrap();
    let wrong_currency = ChargeRequest {
        amount: AMOUNT,
        currency: address([0x21; 20]),
        chain_id: CHAIN,
        recipient: address(RECIPIENT),
    }
    .to_b64()
    .unwrap();
    let zero_amount = ChargeRequest {
        amount: 0,
        currency: address(CURRENCY),
        chain_id: CHAIN,
        recipient: address(RECIPIENT),
    }
    .to_b64()
    .unwrap();
    for (label, request) in [
        ("recipient", wrong_recipient),
        ("chain", wrong_chain),
        ("currency", wrong_currency),
        ("zero amount", zero_amount),
        ("garbage request", b64url_encode(b"[1,2]")),
    ] {
        let header = credential_with(
            &f,
            &challenge,
            |c| c["challenge"]["request"] = json!(request),
            true,
        );
        assert!(
            matches!(
                f.verify(&header, &body()),
                Err(MppError::InvalidChallenge(_))
            ),
            "{label}"
        );
    }
    // A signed challenge that omits the digest or opaque binding is not one we issue.
    for field in ["digest", "opaque", "expires"] {
        let header = credential_with(
            &f,
            &challenge,
            |c| {
                c["challenge"].as_object_mut().unwrap().remove(field);
            },
            true,
        );
        assert!(
            matches!(
                f.verify(&header, &body()),
                Err(MppError::InvalidChallenge(_))
            ),
            "{field}"
        );
    }
    // Signed with another secret.
    let foreign = MppService::new(
        &MppSettings {
            secret_key: "another-secret-key-0123456789abcdefghij".to_string(),
            ..settings()
        },
        f.repo.clone(),
        f.chain.clone(),
    )
    .unwrap();
    let theirs = foreign
        .challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body())
        .unwrap();
    let header = credential_header(&theirs, hash_payload([1; 32]), None);
    assert!(matches!(
        f.verify(&header, &body()),
        Err(MppError::InvalidChallenge("id"))
    ));
}

#[test]
fn wrong_method_intent_and_realm_are_refused() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let header = credential_with(
        &f,
        &challenge,
        |c| c["challenge"]["method"] = json!("stripe"),
        true,
    );
    assert_eq!(
        f.verify(&header, &body()).unwrap_err(),
        MppError::MethodUnsupported
    );
    let header = credential_with(
        &f,
        &challenge,
        |c| c["challenge"]["intent"] = json!("session"),
        true,
    );
    assert!(matches!(
        f.verify(&header, &body()),
        Err(MppError::InvalidChallenge("intent"))
    ));
    let header = credential_with(
        &f,
        &challenge,
        |c| c["challenge"]["realm"] = json!("payments.other.com"),
        true,
    );
    assert!(matches!(
        f.verify(&header, &body()),
        Err(MppError::InvalidChallenge("realm"))
    ));
}

#[test]
fn a_challenge_is_bound_to_its_body_and_route() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let header = credential_header(&challenge, hash_payload([1; 32]), None);
    let mut other_body = body();
    other_body.push(b' ');
    assert!(matches!(
        f.verify(&header, &other_body),
        Err(MppError::InvalidChallenge("digest"))
    ));
    assert!(matches!(
        f.service.verify(&header, "/v1/completions", &body()),
        Err(MppError::InvalidChallenge("opaque"))
    ));
}

#[test]
fn expired_challenges_are_payment_expired_not_invalid() {
    let f = fixture();
    let issued = Utc::now() - ChronoDuration::seconds(601);
    let challenge = f
        .service
        .challenge_at(
            issued,
            CHAT_COMPLETIONS_ROUTE,
            &body(),
            &Price { amount: AMOUNT },
        )
        .unwrap();
    let header = credential_header(&challenge, hash_payload([1; 32]), None);
    assert_eq!(f.verify(&header, &body()).unwrap_err(), MppError::Expired);
    // One second before expiry still verifies; at expiry it does not.
    let expires = challenge.expires;
    assert!(f
        .service
        .verify_at(
            expires - ChronoDuration::seconds(1),
            &header,
            CHAT_COMPLETIONS_ROUTE,
            &body()
        )
        .is_ok());
    assert_eq!(
        f.service
            .verify_at(expires, &header, CHAT_COMPLETIONS_ROUTE, &body())
            .unwrap_err(),
        MppError::Expired
    );
}

#[test]
fn source_must_be_a_did_on_the_configured_chain() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let did = |chain: u64| format!("did:pkh:eip155:{chain}:{}", address(PAYER));
    let ok = credential_header(&challenge, hash_payload([1; 32]), Some(&did(CHAIN)));
    assert!(f.verify(&ok, &body()).is_ok());
    let wrong_chain = credential_header(&challenge, hash_payload([1; 32]), Some(&did(1)));
    assert_eq!(
        f.verify(&wrong_chain, &body()).unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::SourceChain)
    );
    let garbage = credential_header(
        &challenge,
        hash_payload([1; 32]),
        Some("did:web:example.com"),
    );
    assert!(matches!(
        f.verify(&garbage, &body()),
        Err(MppError::Malformed(_))
    ));
}

#[test]
fn proof_payloads_and_oversized_or_wrong_type_transactions_are_refused() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let proof = credential_header(
        &challenge,
        json!({"type": "proof", "signature": "0x00"}),
        None,
    );
    assert_eq!(
        f.verify(&proof, &body()).unwrap_err(),
        MppError::MethodUnsupported
    );

    let wrong_type = credential_header(
        &challenge,
        json!({"type": "transaction", "signature": format!("0x02{}", "00".repeat(60))}),
        None,
    );
    assert!(matches!(
        f.verify(&wrong_type, &body()),
        Err(MppError::Malformed(_))
    ));
    // Right type byte, bytes that are not an RLP list.
    let not_rlp = credential_header(
        &challenge,
        json!({"type": "transaction", "signature": format!("0x76{}", "ff".repeat(60))}),
        None,
    );
    assert!(matches!(
        f.verify(&not_rlp, &body()),
        Err(MppError::Malformed(_))
    ));
    // Over the 16 KiB raw cap: refused before decoding (the header cap trips first).
    let oversize = credential_header(
        &challenge,
        json!({"type": "transaction", "signature": format!("0x76{}", "00".repeat(20_000))}),
        None,
    );
    assert!(matches!(
        f.verify(&oversize, &body()),
        Err(MppError::Malformed(_))
    ));
    let junk = format!("Payment {}", "A".repeat(20_000));
    assert!(matches!(
        f.verify(&junk, &body()),
        Err(MppError::Malformed("authorization too long"))
    ));
}

// ----- settle -----

const TX: [u8; 32] = [0x11; 32];

fn key(hash: [u8; 32]) -> String {
    format!("tempo:0x{}", hex::encode(hash))
}

#[tokio::test]
async fn a_paid_hash_credential_settles_and_is_claimed_serving() {
    let f = fixture();
    let (challenge, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();
    let settled = f.service.settle(&verified).await.unwrap();
    assert_eq!(settled.payment_key(), key(TX));
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Serving));
    let receipt = settled.receipt_header(Utc::now()).unwrap();
    let decoded: serde_json::Value =
        serde_json::from_slice(&b64url_decode(&receipt).unwrap()).unwrap();
    assert_eq!(decoded["method"], "tempo");
    assert_eq!(decoded["status"], "success");
    assert_eq!(decoded["reference"], format!("0x{}", hex::encode(TX)));
    let _ = challenge;
}

#[tokio::test]
async fn a_paid_transaction_credential_broadcasts_and_settles() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let (payload, raw) = transaction_payload(7);
    let hash = tempo::transaction_hash(&raw);
    f.chain.mine(bound_payment_receipt(
        hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    ));
    let header = credential_header(&challenge, payload, None);
    let settled = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();
    assert_eq!(settled.payment_key(), key(hash));
    assert_eq!(f.chain.send_calls(), 1);
    assert_eq!(f.chain.lookup_calls(), 0);
}

#[tokio::test]
async fn a_known_transaction_is_recovered_by_hash_after_a_broadcast_error() {
    let f = fixture();
    f.chain.set_send_mode(SendMode::Reject);
    let challenge = f.challenge(&body());
    let (payload, raw) = transaction_payload(8);
    let hash = tempo::transaction_hash(&raw);
    f.chain.mine(bound_payment_receipt(
        hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    ));
    let header = credential_header(&challenge, payload, None);
    let settled = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();
    assert_eq!(settled.payment_key(), key(hash));
    assert_eq!(f.chain.send_calls(), 1);
    assert_eq!(f.chain.lookup_calls(), 1);
}

#[tokio::test]
async fn an_unknown_transaction_the_node_refused_is_a_retryable_402() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let (payload, _) = transaction_payload(9);
    let header = credential_header(&challenge, payload, None);
    let error = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        MppError::VerificationFailed(VerifyFailure::ReceiptNotFound)
    );
    assert_eq!(error.status(), 402);
    // The lookup is bounded: exactly the configured number of attempts.
    assert_eq!(f.chain.lookup_calls(), 3);
    assert_eq!(f.chain.send_calls(), 1);
}

#[tokio::test]
async fn an_unreachable_node_is_unavailable_not_a_missing_payment() {
    let f = fixture();
    // Hash payload, RPC down.
    f.chain.set_lookup_error(Some(RpcError::Unavailable));
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let error = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap_err();
    assert_eq!(error, MppError::Unavailable(UnavailableReason::Rpc));
    assert_eq!(error.status(), 503);
    assert_eq!(
        f.repo.state_of(&key(TX)),
        None,
        "nothing is claimed when verification could not run"
    );

    // Transaction payload, transport failure on the broadcast and nothing found afterwards.
    let f = fixture();
    f.chain.set_send_mode(SendMode::Unavailable);
    let challenge = f.challenge(&body());
    let (payload, _) = transaction_payload(3);
    let header = credential_header(&challenge, payload, None);
    assert_eq!(
        f.service
            .settle(&f.verify(&header, &body()).unwrap())
            .await
            .unwrap_err(),
        MppError::Unavailable(UnavailableReason::Rpc)
    );
    // The broadcast error is transport, but the tx is already mined: the lookup rescues it.
    let (payload, raw) = transaction_payload(4);
    f.chain.mine(bound_payment_receipt(
        tempo::transaction_hash(&raw),
        PAYER,
        CURRENCY,
        RECIPIENT,
        AMOUNT,
        REALM,
        &challenge,
    ));
    let header = credential_header(&challenge, payload, None);
    assert!(f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .is_ok());
}

#[tokio::test]
async fn hash_not_found_is_a_retryable_402() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let header = credential_header(&challenge, hash_payload([0x99; 32]), None);
    let error = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        MppError::VerificationFailed(VerifyFailure::ReceiptNotFound)
    );
}

#[tokio::test]
async fn a_node_answering_with_another_transactions_receipt_is_refused() {
    let f = fixture();
    let challenge = f.challenge(&body());
    // Mined under hash [0x99] but claiming to be [0x98].
    let mut wrong = bound_payment_receipt(
        [0x98; 32], PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge,
    );
    wrong.tx_hash = [0x98; 32];
    f.chain.mine(wrong.clone());
    f.chain.mined_alias([0x99; 32], wrong);
    let header = credential_header(&challenge, hash_payload([0x99; 32]), None);
    assert_eq!(
        f.service
            .settle(&f.verify(&header, &body()).unwrap())
            .await
            .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::ReceiptMismatch)
    );
}

#[tokio::test]
async fn verification_negatives_map_to_the_documented_errors() {
    let f = fixture();
    let challenge = f.challenge(&body());
    let settle = |tx: [u8; 32], receipt: RpcReceipt| {
        let f = &f;
        let challenge = challenge.clone();
        async move {
            f.chain.mine(RpcReceipt {
                tx_hash: tx,
                ..receipt
            });
            let header = credential_header(&challenge, hash_payload(tx), None);
            f.service.settle(&f.verify(&header, &body()).unwrap()).await
        }
    };
    let memo = Some(tempo::attribution_memo(REALM, &challenge.id));
    let ok = |hash| payment_receipt(hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, memo);

    let mut reverted = ok([1; 32]);
    reverted.success = false;
    assert_eq!(
        settle([1; 32], reverted).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::Reverted)
    );
    assert_eq!(
        settle(
            [2; 32],
            payment_receipt([2; 32], PAYER, CURRENCY, [0xbb; 20], AMOUNT, memo)
        )
        .await
        .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    assert_eq!(
        settle(
            [3; 32],
            payment_receipt([3; 32], PAYER, CURRENCY, RECIPIENT, AMOUNT - 1, memo)
        )
        .await
        .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    assert_eq!(
        settle(
            [4; 32],
            payment_receipt([4; 32], PAYER, [0x21; 20], RECIPIENT, AMOUNT, memo)
        )
        .await
        .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    // Sent by someone other than the transaction sender.
    let mut other_sender = payment_receipt([5; 32], [0x77; 20], CURRENCY, RECIPIENT, AMOUNT, memo);
    other_sender.from = PAYER;
    assert_eq!(
        settle([5; 32], other_sender).await.unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    // Funds moved without this challenge's memo: final, 403, no new challenge.
    for (hash, memo) in [
        ([6; 32], None),
        (
            [7; 32],
            Some(tempo::attribution_memo(REALM, "another-challenge-id")),
        ),
        (
            [8; 32],
            Some(tempo::attribution_memo("evil.example", &challenge.id)),
        ),
    ] {
        let error = settle(
            hash,
            payment_receipt(hash, PAYER, CURRENCY, RECIPIENT, AMOUNT, memo),
        )
        .await
        .unwrap_err();
        assert_eq!(error, MppError::Consumed(ConsumedReason::MemoNotBound));
        assert_eq!(error.status(), 403);
        assert!(!error.carries_challenge());
        assert_eq!(
            f.repo.state_of(&key(hash)),
            None,
            "an unbound payment is never claimed"
        );
    }
    // None of the failures claimed anything.
    for n in 1..=8u8 {
        assert_eq!(f.repo.state_of(&key([n; 32])), None);
    }
}

#[tokio::test]
async fn the_source_did_decides_who_must_have_paid() {
    let f = fixture();
    let challenge = f.challenge(&body());
    // A relayer sent the transaction; PAYER's funds moved.
    let mut receipt =
        bound_payment_receipt(TX, PAYER, CURRENCY, RECIPIENT, AMOUNT, REALM, &challenge);
    receipt.from = [0x55; 20];
    f.chain.mine(receipt);
    let did = |a: [u8; 20]| format!("did:pkh:eip155:{CHAIN}:{}", address(a));
    let without = credential_header(&challenge, hash_payload(TX), None);
    assert_eq!(
        f.service
            .settle(&f.verify(&without, &body()).unwrap())
            .await
            .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    let wrong = credential_header(&challenge, hash_payload(TX), Some(&did([0x66; 20])));
    assert_eq!(
        f.service
            .settle(&f.verify(&wrong, &body()).unwrap())
            .await
            .unwrap_err(),
        MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer)
    );
    let right = credential_header(&challenge, hash_payload(TX), Some(&did(PAYER)));
    assert!(f
        .service
        .settle(&f.verify(&right, &body()).unwrap())
        .await
        .is_ok());
}

// ----- claim state machine through the service -----

#[tokio::test]
async fn a_replayed_credential_is_consumed() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();
    f.service.settle(&verified).await.unwrap();
    let replay = f.service.settle(&verified).await.unwrap_err();
    assert_eq!(replay, MppError::Consumed(ConsumedReason::AlreadyClaimed));
    assert_eq!(replay.status(), 403);
    assert!(!replay.carries_challenge());
}

#[tokio::test]
async fn an_unserved_payment_is_retried_without_a_second_payment() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let verified = f.verify(&header, &body()).unwrap();
    let first = f.service.settle(&verified).await.unwrap();
    f.service
        .finish(&first, ServeOutcome::Unserved)
        .await
        .unwrap();
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Unserved));
    let retry = f.service.settle(&verified).await.unwrap();
    assert_eq!(retry.payment_key(), first.payment_key());
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Serving));
    // Served is final.
    f.service
        .finish(&retry, ServeOutcome::Served)
        .await
        .unwrap();
    assert_eq!(
        f.service.settle(&verified).await.unwrap_err(),
        MppError::Consumed(ConsumedReason::AlreadyClaimed)
    );
}

#[tokio::test]
async fn a_second_challenge_cannot_reuse_a_claimed_payment_even_when_unserved() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let first = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();
    f.service
        .finish(&first, ServeOutcome::Unserved)
        .await
        .unwrap();
    // Same transaction, presented under a different challenge (different body). The memo is
    // bound to the first challenge, so chain verification refuses it before the claim; the
    // claim store would refuse it too.
    let mut other_body = body();
    other_body.extend_from_slice(b"\n");
    let other_challenge = f.challenge(&other_body);
    let other_header = credential_header(&other_challenge, hash_payload(TX), None);
    assert_eq!(
        f.service
            .settle(&f.verify(&other_header, &other_body).unwrap())
            .await
            .unwrap_err(),
        MppError::Consumed(ConsumedReason::MemoNotBound)
    );
    assert_eq!(
        f.repo.claim(&key(TX), &other_challenge.id).await.unwrap(),
        ClaimOutcome::AlreadyUsed
    );
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Unserved));
}

#[tokio::test]
async fn a_transaction_credential_cannot_broadcast_after_the_challenge_expired() {
    let f = fixture();
    let issued = Utc::now() - ChronoDuration::seconds(599);
    let challenge = f
        .service
        .challenge_at(
            issued,
            CHAT_COMPLETIONS_ROUTE,
            &body(),
            &Price { amount: AMOUNT },
        )
        .unwrap();
    let (payload, raw) = transaction_payload(5);
    f.chain.mine(bound_payment_receipt(
        tempo::transaction_hash(&raw),
        PAYER,
        CURRENCY,
        RECIPIENT,
        AMOUNT,
        REALM,
        &challenge,
    ));
    let header = credential_header(&challenge, payload, None);
    let verified = f.verify(&header, &body()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    assert_eq!(
        f.service.settle(&verified).await.unwrap_err(),
        MppError::Expired
    );
    assert_eq!(
        f.chain.send_calls(),
        0,
        "an expired challenge must not reach the node"
    );
}

// ----- serving guard -----

#[tokio::test]
async fn the_guard_marks_unserved_when_dropped_and_respects_finish() {
    let f = fixture();
    let (_, header) = f.paid_hash_credential(&body(), TX);
    let settled = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();

    let guard = f.service.serving(settled.clone());
    assert_eq!(
        guard.payment().map(|p| p.payment_key().to_string()),
        Some(key(TX))
    );
    drop(guard);
    // The spawned finish runs on the same runtime; wait for it deterministically.
    for _ in 0..100 {
        if f.repo.state_of(&key(TX)) == Some(ClaimState::Unserved) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Unserved));

    // Re-claim, finish served through the guard, then nothing changes on drop.
    let settled = f
        .service
        .settle(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();
    f.service
        .serving(settled)
        .finish(ServeOutcome::Served)
        .await;
    assert_eq!(f.repo.state_of(&key(TX)), Some(ClaimState::Served));
}

// ----- errors and redaction -----

#[test]
fn error_statuses_and_problem_types() {
    let cases = [
        (
            MppError::Malformed("x"),
            402,
            Some("malformed-credential"),
            true,
        ),
        (
            MppError::MethodUnsupported,
            400,
            Some("method-unsupported"),
            false,
        ),
        (
            MppError::InvalidChallenge("id"),
            402,
            Some("invalid-challenge"),
            true,
        ),
        (MppError::Expired, 402, Some("payment-expired"), true),
        (
            MppError::VerificationFailed(VerifyFailure::Reverted),
            402,
            Some("verification-failed"),
            true,
        ),
        (
            MppError::Consumed(ConsumedReason::AlreadyClaimed),
            403,
            Some("verification-failed"),
            false,
        ),
        (
            MppError::Consumed(ConsumedReason::MemoNotBound),
            403,
            Some("verification-failed"),
            false,
        ),
        (
            MppError::Unavailable(UnavailableReason::Rpc),
            503,
            None,
            false,
        ),
        (
            MppError::Unavailable(UnavailableReason::Store),
            503,
            None,
            false,
        ),
    ];
    for (error, status, problem, challenge) in cases {
        assert_eq!(error.status(), status, "{error:?}");
        assert_eq!(
            error.problem().map(ProblemCode::as_str),
            problem,
            "{error:?}"
        );
        assert_eq!(error.carries_challenge(), challenge, "{error:?}");
        assert!(!error.kind().is_empty());
        assert!(!error.detail().is_empty());
    }
}

#[test]
fn debug_output_never_prints_secrets_or_payment_data() {
    let f = fixture();
    let printed = format!("{:?}", f.service);
    assert!(printed.contains(REALM));
    for secret in [
        SECRET,
        SERVICE_KEY,
        "secret-provider-key",
        &address(RECIPIENT),
    ] {
        assert!(!printed.contains(secret), "{printed}");
    }
    let challenge = f.challenge(&body());
    let header = credential_header(
        &challenge,
        hash_payload(TX),
        Some(&format!("did:pkh:eip155:{CHAIN}:{}", address(PAYER))),
    );
    let verified = f.verify(&header, &body()).unwrap();
    let printed = format!("{verified:?}");
    assert!(!printed.contains(&challenge.id));
    let credential = protocol::parse_authorization(&header).unwrap();
    let printed = format!("{credential:?} {:?}", credential.payload);
    assert!(!printed.contains(&hex::encode(TX)));
    assert!(!printed.contains(&hex::encode(PAYER)));
}
