use std::collections::BTreeMap;

use chrono::Duration as ChronoDuration;
use config::{MppModelPrice, MppSettings, MppTempoSettings};
use serde_json::json;

use super::*;
use crate::mpp::error::{
    ConsumedReason, MppError, MppResponse, ProblemStatus, UnavailableReason, VerifyFailure,
};
use crate::mpp::memory::InMemoryClaimRepository;
use crate::mpp::ports::{ClaimOutcome, ClaimState, RpcError, RpcReceipt, ServeOutcome};
use crate::mpp::protocol::ProblemCode;
use crate::mpp::protocol::{b64url_decode, b64url_encode, compute_challenge_id, opaque_for};
use crate::mpp::tempo;
use crate::mpp::testing::{
    bound_payment_receipt, credential_header, hash_payload, payment_receipt, FakeTempo, SendMode,
};

const REALM: &str = "cloud-api.near.ai";
const SECRET: &str = "0123456789abcdef0123456789abcdef-hmac";
const SERVICE_KEY: &str = "sk-live-service-key-0001";
const MODEL: &str = "zai-org/GLM-5";
/// A second priced model with a larger body cap, so a body past MODEL's cap is still parsed.
const BIG_MODEL: &str = "zai-org/GLM-5-long";
const BIG_BODY_CAP: usize = 65_536;
const CURRENCY: [u8; 20] = [0x20; 20];
const RECIPIENT: [u8; 20] = [0xaa; 20];
const PAYER: [u8; 20] = [0x40; 20];
const AMOUNT: u128 = 20_000;
const CHAIN: u64 = 42431;
const TX: [u8; 32] = [0x11; 32];
const TX_B: [u8; 32] = [0x12; 32];

/// The ledger key of a payment: a keyed digest, never the transaction hash itself.
fn key(hash: [u8; 32]) -> String {
    protocol::payment_key(SECRET.as_bytes(), &hash).unwrap()
}

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
    prices.insert(
        BIG_MODEL.to_string(),
        MppModelPrice {
            amount: AMOUNT * 2,
            max_tokens: 4096,
            max_request_bytes: BIG_BODY_CAP as u32,
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
fn a_body_past_every_priced_cap_is_not_priced_and_not_parsed() {
    let f = fixture();
    // Under the largest cap it is a priced model's limit violation; over it, not a request
    // MPP takes at all (the body is never parsed).
    let at_cap = |len: usize| {
        let mut request = json!({"model": MODEL, "max_tokens": 10, "pad": ""});
        let base = request.to_string().len();
        request["pad"] = json!("x".repeat(len - base));
        request
    };
    assert_eq!(
        priced(&f.service, at_cap(BIG_BODY_CAP)),
        Pricing::Violation(LimitViolation::BodyTooLarge { cap: 4096 })
    );
    assert_eq!(
        priced(&f.service, at_cap(BIG_BODY_CAP + 1)),
        Pricing::Unpriced
    );
    // Not even valid JSON: past the cap it is Unpriced before any parse could fail or succeed.
    assert_eq!(
        f.service.price_body(&vec![b'['; BIG_BODY_CAP + 1]),
        Pricing::Unpriced
    );
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
    // The same body in the same second is still a different challenge (a random nonce is
    // bound into the opaque value), so a payment made for one cannot be front-run onto the
    // other.
    let again = f
        .service
        .challenge_at(now, CHAT_COMPLETIONS_ROUTE, &body(), &price)
        .unwrap();
    assert_ne!(challenge.id, again.id);
    assert_ne!(challenge.opaque, again.opaque);
    assert_eq!(
        protocol::opaque_route(&challenge.opaque).as_deref(),
        Some(CHAT_COMPLETIONS_ROUTE)
    );
    assert_eq!(
        protocol::opaque_route(&again.opaque).as_deref(),
        Some(CHAT_COMPLETIONS_ROUTE)
    );
    // Both still verify for the body they were issued for.
    for issued in [&challenge, &again] {
        let header = credential_header(issued, hash_payload(TX), None);
        assert!(f
            .service
            .verify_at(now, &header, CHAT_COMPLETIONS_ROUTE, &body())
            .is_ok());
    }
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
        Err(MppError::Limit(LimitViolation::ModelNotPriced))
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
            Box::new(|c| {
                c["challenge"]["opaque"] = json!(opaque_for("/v1/other", &[9; 16]).unwrap())
            }),
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
    // The opaque value decides only the route: a signed legacy route-only value is accepted,
    // a signed value for another route, or one that is not a string map with a route, is not.
    let legacy = b64url_encode(br#"{"route":"/v1/chat/completions"}"#);
    let header = credential_with(
        &f,
        &challenge,
        |c| c["challenge"]["opaque"] = json!(legacy),
        true,
    );
    assert!(f.verify(&header, &body()).is_ok());
    for (label, opaque) in [
        ("other route", opaque_for("/v1/other", &[1; 16]).unwrap()),
        ("no route", b64url_encode(br#"{"nonce":"abc"}"#)),
        ("not a map", b64url_encode(b"[\"/v1/chat/completions\"]")),
        ("non-string route", b64url_encode(br#"{"route":5}"#)),
    ] {
        let header = credential_with(
            &f,
            &challenge,
            |c| c["challenge"]["opaque"] = json!(opaque),
            true,
        );
        assert_eq!(
            f.verify(&header, &body()).err(),
            Some(MppError::InvalidChallenge("opaque")),
            "{label}"
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

#[path = "service_tests/payment.rs"]
mod payment;

// ----- errors and redaction -----

#[test]
fn every_error_has_exactly_one_response() {
    use std::time::Duration;
    let challenge = |code| MppResponse::Challenge(code);
    let cases = [
        (
            MppError::Malformed("x"),
            challenge(ProblemCode::MalformedCredential),
        ),
        (
            MppError::InvalidChallenge("id"),
            challenge(ProblemCode::InvalidChallenge),
        ),
        (MppError::Expired, challenge(ProblemCode::PaymentExpired)),
        (
            MppError::VerificationFailed(VerifyFailure::Reverted),
            challenge(ProblemCode::VerificationFailed),
        ),
        (
            MppError::VerificationFailed(VerifyFailure::NoMatchingTransfer),
            challenge(ProblemCode::VerificationFailed),
        ),
        (
            MppError::VerificationFailed(VerifyFailure::ReceiptMismatch),
            challenge(ProblemCode::VerificationFailed),
        ),
        (
            MppError::VerificationFailed(VerifyFailure::SourceChain),
            challenge(ProblemCode::VerificationFailed),
        ),
        (
            MppError::VerificationFailed(VerifyFailure::Transaction(
                crate::mpp::transaction::TransactionMismatch::NoPaymentCall,
            )),
            challenge(ProblemCode::VerificationFailed),
        ),
        (
            MppError::MethodUnsupported,
            MppResponse::Problem(ProblemStatus::BadRequest, ProblemCode::MethodUnsupported),
        ),
        (
            MppError::Consumed(ConsumedReason::AlreadyClaimed),
            MppResponse::Problem(ProblemStatus::Forbidden, ProblemCode::VerificationFailed),
        ),
        (
            MppError::Consumed(ConsumedReason::MemoNotBound),
            MppResponse::Problem(ProblemStatus::Forbidden, ProblemCode::VerificationFailed),
        ),
        // A payment that is not found yet is retried with the same credential: no challenge.
        (
            MppError::ReceiptNotFound,
            MppResponse::Retry(Duration::from_secs(2)),
        ),
        (
            MppError::Unavailable(UnavailableReason::Rpc),
            MppResponse::Retry(Duration::from_secs(5)),
        ),
        (
            MppError::Unavailable(UnavailableReason::Store),
            MppResponse::Retry(Duration::from_secs(5)),
        ),
        (MppError::Config, MppResponse::Retry(Duration::from_secs(5))),
        (
            MppError::Limit(LimitViolation::MultipleChoices),
            MppResponse::Limit(LimitViolation::MultipleChoices),
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.response(), expected, "{error:?}");
        assert!(!error.kind().is_empty());
        assert!(!error.detail().is_empty());
    }
    assert_eq!(ProblemStatus::BadRequest.code(), 400);
    assert_eq!(ProblemStatus::Forbidden.code(), 403);
}

#[tokio::test]
async fn debug_output_never_prints_secrets_or_payment_data() {
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
    // Neither the proof of payment nor the claimed payment prints a key or a hash.
    let (_, header) = f.paid_hash_credential(&body(), [0x42; 32]);
    let paid = f
        .service
        .verify_payment(&f.verify(&header, &body()).unwrap())
        .await
        .unwrap();
    let settled = f.service.claim(&paid).await.unwrap();
    assert_eq!(
        format!("{paid:?} {settled:?}"),
        "PaidCredential { .. } SettledPayment { .. }"
    );
    let credential = protocol::parse_authorization(&header).unwrap();
    let printed = format!("{credential:?} {:?}", credential.payload);
    assert!(!printed.contains(&hex::encode(TX)));
    assert!(!printed.contains(&hex::encode(PAYER)));
}
