use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::*;
use crate::mpp::channel::channel_id;
use crate::mpp::protocol::{
    parse_any_authorization, parse_authorization, verify_challenge_id, ParsedAuthorization,
};
use crate::mpp::session_vectors::Vectors;

const SECRET: &[u8] = b"test-secret-key-12345";
const ESCROW: &str = "0x4d50500000000000000000000000000000000000";
const TOKEN: &str = "0x20c0000000000000000000000000000000000000";
const PAYEE: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
const CHANNEL: &str = "0xbf6845b6ee5f37bd139093d0f2d6765a23201ea2af587db27b93659b7cf613a2";
const VOUCHER_SIG: &str = "0x75b419cc6d2a2cd4d5629e924d62deb3671cb6d1b9f2cee9406ddc53ab90c0b56384a4a82e0d6524583b3edc8b1269fcde369b736c913f5e55c3162c7c4c18501b";
const NONCE_HASH: &str = "0x3b8570e0efaae36f92efaf33760304be75e89d3aae8abfb740176b3c6b4cb347";
const TX_HASH: &str = "0x6e0444bc1e22b457eb3a509c0d4319600d9ac37e26d7f9d58e8d80e8733b4c68";

fn terms() -> SessionTerms {
    SessionTerms {
        amount: 20,
        currency: hex_exact(TOKEN).unwrap(),
        recipient: hex_exact(PAYEE).unwrap(),
        suggested_deposit: 1_000_000,
        chain_id: 42431,
        escrow: hex_exact(ESCROW).unwrap(),
        min_voucher_delta: 1280,
        snapshot: None,
    }
}

fn snapshot() -> SessionSnapshot {
    let v = Vectors::load();
    SessionSnapshot {
        accepted_cumulative: 1_250_000,
        chain_id: 42431,
        channel_id: v.fixed("channelId"),
        deposit: 5_000_000,
        descriptor: v.descriptor(),
        escrow: v.escrow(),
        highest_voucher: HighestVoucher {
            channel_id: v.fixed("channelId"),
            cumulative_amount: 1_250_000,
            signature: v.fixed("voucher.signature"),
        },
        required_cumulative: 1_500_000,
        settled: 0,
        spent: 1_000_000,
        units: 50_000,
    }
}

const REQUEST_JSON: &str = r#"{"amount":"20","currency":"0x20c0000000000000000000000000000000000000","methodDetails":{"chainId":42431,"escrowContract":"0x4d50500000000000000000000000000000000000","minVoucherDelta":"1280","sessionProtocol":"v2"},"recipient":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc","suggestedDeposit":"1000000","unitType":"llm_token"}"#;

const REQUEST_WITH_SNAPSHOT_JSON: &str = concat!(
    r#"{"amount":"20","currency":"0x20c0000000000000000000000000000000000000","methodDetails":{"#,
    r#""chainId":42431,"escrowContract":"0x4d50500000000000000000000000000000000000","#,
    r#""minVoucherDelta":"1280","sessionProtocol":"v2","sessionSnapshot":{"#,
    r#""acceptedCumulative":"1250000","chainId":42431,"#,
    r#""channelId":"0xbf6845b6ee5f37bd139093d0f2d6765a23201ea2af587db27b93659b7cf613a2","#,
    r#""deposit":"5000000","descriptor":{"#,
    r#""authorizedSigner":"0x0000000000000000000000000000000000000000","#,
    r#""expiringNonceHash":"0x3b8570e0efaae36f92efaf33760304be75e89d3aae8abfb740176b3c6b4cb347","#,
    r#""operator":"0x0000000000000000000000000000000000000000","#,
    r#""payee":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc","#,
    r#""payer":"0x70997970c51812dc3a010c7d01b50e0d17dc79c8","#,
    r#""salt":"0x1111111111111111111111111111111111111111111111111111111111111111","#,
    r#""token":"0x20c0000000000000000000000000000000000000"},"#,
    r#""escrow":"0x4d50500000000000000000000000000000000000","highestVoucher":{"#,
    r#""channelId":"0xbf6845b6ee5f37bd139093d0f2d6765a23201ea2af587db27b93659b7cf613a2","#,
    r#""cumulativeAmount":"1250000","#,
    r#""signature":"0x75b419cc6d2a2cd4d5629e924d62deb3671cb6d1b9f2cee9406ddc53ab90c0b56384a4a82e0d6524583b3edc8b1269fcde369b736c913f5e55c3162c7c4c18501b"},"#,
    r#""requiredCumulative":"1500000","settled":"0","spent":"1000000","units":50000}},"#,
    r#""recipient":"0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc","suggestedDeposit":"1000000","unitType":"llm_token"}"#
);

// ---------------------------------------------------------------------------------------
// Challenge request
// ---------------------------------------------------------------------------------------

#[test]
fn request_json_is_the_exact_jcs_string_mppx_emits() {
    assert_eq!(terms().request_json().unwrap(), REQUEST_JSON);
    let with_snapshot = SessionTerms {
        snapshot: Some(snapshot()),
        ..terms()
    };
    assert_eq!(
        with_snapshot.request_json().unwrap(),
        REQUEST_WITH_SNAPSHOT_JSON
    );
    // Never advertised: a fee payer, a fee token, an operator, a machine token (the
    // snapshot's descriptor has its own `operator`, so look at the request's details).
    let request: Value = serde_json::from_str(REQUEST_WITH_SNAPSHOT_JSON).unwrap();
    let details = &request["methodDetails"];
    for key in ["feePayer", "feeToken", "operator", "machineTokenEnabled"] {
        assert!(details.get(key).is_none(), "{key}");
    }
    assert!(!REQUEST_WITH_SNAPSHOT_JSON.contains("feePayer"));
    assert_eq!(
        terms().to_b64().unwrap(),
        b64url_encode(REQUEST_JSON.as_bytes())
    );
}

#[test]
fn request_round_trips_strictly_with_and_without_a_snapshot() {
    for original in [
        terms(),
        SessionTerms {
            snapshot: Some(snapshot()),
            ..terms()
        },
    ] {
        let parsed = SessionTerms::from_b64(&original.to_b64().unwrap()).unwrap();
        assert_eq!(parsed, original);
    }
    // Padded and standard-alphabet input decodes like the rest of the protocol.
    let padded = format!("{}=", b64url_encode(REQUEST_JSON.as_bytes()));
    assert_eq!(SessionTerms::from_b64(&padded).unwrap(), terms());
}

fn request_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(REQUEST_JSON).unwrap();
    edit(&mut value);
    b64url_encode(value.to_string().as_bytes())
}

#[test]
fn echoed_requests_are_refused_unless_they_are_exactly_ours() {
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Box<dyn Fn(&mut Value)>, WireError)> = vec![
        (
            "fee payer smuggled in",
            Box::new(|v| v["methodDetails"]["feePayer"] = json!(true)),
            WireError::Malformed("invalid_request"),
        ),
        (
            "operator smuggled in",
            Box::new(|v| v["methodDetails"]["operator"] = json!(PAYEE)),
            WireError::Malformed("invalid_request"),
        ),
        (
            "top-level extra key",
            Box::new(|v| v["feeToken"] = json!(TOKEN)),
            WireError::Malformed("invalid_request"),
        ),
        (
            "other unit type",
            Box::new(|v| v["unitType"] = json!("request")),
            WireError::Malformed("invalid_unit_type"),
        ),
        (
            "v1 protocol",
            Box::new(|v| v["methodDetails"]["sessionProtocol"] = json!("v1")),
            WireError::Malformed("invalid_session_protocol"),
        ),
        (
            "missing minVoucherDelta",
            Box::new(|v| {
                v["methodDetails"]
                    .as_object_mut()
                    .unwrap()
                    .remove("minVoucherDelta");
            }),
            WireError::Malformed("invalid_request"),
        ),
        (
            "numeric amount",
            Box::new(|v| v["amount"] = json!(20)),
            WireError::Malformed("invalid_request"),
        ),
        (
            "zero price",
            Box::new(|v| v["amount"] = json!("0")),
            WireError::AmountOutOfRange,
        ),
        (
            "price over uint96",
            Box::new(|v| v["amount"] = json!("79228162514264337593543950336")),
            WireError::AmountOutOfRange,
        ),
        (
            "zero suggested deposit",
            Box::new(|v| v["suggestedDeposit"] = json!("0")),
            WireError::AmountOutOfRange,
        ),
        (
            "short currency",
            Box::new(|v| v["currency"] = json!("0x20c0")),
            WireError::Malformed("hex_length"),
        ),
        (
            "snapshot with a short signature",
            Box::new(|v| {
                let mut snap = snapshot().to_json();
                snap["highestVoucher"]["signature"] = json!(format!("0x{}", "ab".repeat(64)));
                v["methodDetails"]["sessionSnapshot"] = snap;
            }),
            WireError::UnsupportedSignature,
        ),
        (
            "snapshot with an extra key",
            Box::new(|v| {
                let mut snap = snapshot().to_json();
                snap["closeRequestedAt"] = json!("1");
                v["methodDetails"]["sessionSnapshot"] = snap;
            }),
            WireError::Malformed("invalid_snapshot"),
        ),
    ];
    for (name, edit, expected) in cases {
        assert_eq!(
            SessionTerms::from_b64(&request_with(edit)),
            Err(expected),
            "{name}"
        );
    }
    assert_eq!(
        SessionTerms::from_b64("!!!"),
        Err(WireError::Malformed("invalid base64url"))
    );
    assert_eq!(
        SessionTerms::from_b64(&b64url_encode(b"[]")),
        Err(WireError::Malformed("invalid_request"))
    );
}

#[test]
fn terms_outside_the_uint96_range_are_not_issued() {
    let base = terms();
    let too_big = MAX_UINT96 + 1;
    let cases = [
        SessionTerms {
            amount: 0,
            ..base.clone()
        },
        SessionTerms {
            amount: too_big,
            ..base.clone()
        },
        SessionTerms {
            suggested_deposit: 0,
            ..base.clone()
        },
        SessionTerms {
            suggested_deposit: too_big,
            ..base.clone()
        },
        SessionTerms {
            min_voucher_delta: too_big,
            ..base.clone()
        },
    ];
    for case in cases {
        assert_eq!(case.to_b64(), Err(WireError::AmountOutOfRange));
    }
    // The edges are fine: a free-running delta of zero and a deposit of exactly uint96.
    let edge = SessionTerms {
        min_voucher_delta: 0,
        suggested_deposit: MAX_UINT96,
        ..base
    };
    assert_eq!(
        SessionTerms::from_b64(&edge.to_b64().unwrap()).unwrap(),
        edge
    );
}

#[test]
fn same_offer_ignores_only_the_snapshot() {
    let base = terms();
    let with_snapshot = SessionTerms {
        snapshot: Some(snapshot()),
        ..terms()
    };
    assert!(base.same_offer(&with_snapshot));
    assert!(with_snapshot.same_offer(&base));
    let changed = [
        SessionTerms {
            amount: 21,
            ..terms()
        },
        SessionTerms {
            currency: [1; 20],
            ..terms()
        },
        SessionTerms {
            recipient: [1; 20],
            ..terms()
        },
        SessionTerms {
            suggested_deposit: 1,
            ..terms()
        },
        SessionTerms {
            chain_id: 1,
            ..terms()
        },
        SessionTerms {
            escrow: [1; 20],
            ..terms()
        },
        SessionTerms {
            min_voucher_delta: 1,
            ..terms()
        },
    ];
    for other in changed {
        assert!(!base.same_offer(&other));
        assert!(!with_snapshot.same_offer(&other));
    }
}

#[test]
fn snapshot_json_round_trips_and_has_the_keys_the_client_reads() {
    let snap = snapshot();
    let json = snap.to_json();
    let keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "acceptedCumulative",
            "chainId",
            "channelId",
            "deposit",
            "descriptor",
            "escrow",
            "highestVoucher",
            "requiredCumulative",
            "settled",
            "spent",
            "units"
        ]
    );
    // mppx's reader: strings for amounts, numbers for chainId and units.
    assert!(json["chainId"].is_u64() && json["units"].is_u64());
    for key in [
        "acceptedCumulative",
        "deposit",
        "requiredCumulative",
        "settled",
        "spent",
    ] {
        assert!(json[key].is_string(), "{key}");
    }
    assert_eq!(SessionSnapshot::from_value(&json).unwrap(), snap);
}

// ---------------------------------------------------------------------------------------
// Challenge
// ---------------------------------------------------------------------------------------

fn expires() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2030-05-06T07:08:09Z")
        .unwrap()
        .with_timezone(&Utc)
}

const SESSION_REQUEST_B64: &str = "eyJhbW91bnQiOiIyMCIsImN1cnJlbmN5IjoiMHgyMGMwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwIiwibWV0aG9kRGV0YWlscyI6eyJjaGFpbklkIjo0MjQzMSwiZXNjcm93Q29udHJhY3QiOiIweDRkNTA1MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJtaW5Wb3VjaGVyRGVsdGEiOiIxMjgwIiwic2Vzc2lvblByb3RvY29sIjoidjIifSwicmVjaXBpZW50IjoiMHgzYzQ0Y2RkZGI2YTkwMGZhMmI1ODVkZDI5OWUwM2QxMmZhNDI5M2JjIiwic3VnZ2VzdGVkRGVwb3NpdCI6IjEwMDAwMDAiLCJ1bml0VHlwZSI6ImxsbV90b2tlbiJ9";
const OPAQUE: &str =
    "eyJub25jZSI6IkJ3Y0hCd2NIQndjSEJ3Y0hCd2NIQnciLCJyb3V0ZSI6Ii92MS9jaGF0L2NvbXBsZXRpb25zIn0";
/// `HMAC-SHA256(secret, "cloud-api.near.ai|tempo|session|<request>|2030-05-06T07:08:09Z||<opaque>")`,
/// computed outside this crate (Python `hmac`).
const SESSION_ID: &str = "97cUsXAZNijws5Gpa00wBoKwbZFKyuM-WXyW9AVJfQc";

fn challenge() -> Challenge {
    session_challenge(
        SECRET,
        "cloud-api.near.ai",
        &terms(),
        expires(),
        "/v1/chat/completions",
        &[7; OPAQUE_NONCE_BYTES],
    )
    .unwrap()
}

#[test]
fn session_challenge_header_has_no_digest_and_the_pinned_id() {
    let challenge = challenge();
    assert_eq!(challenge.id, SESSION_ID);
    assert_eq!(challenge.request, SESSION_REQUEST_B64);
    assert_eq!(challenge.opaque, OPAQUE);
    assert!(challenge.digest.is_empty());
    assert_eq!(challenge.intent(), "session");
    let header = challenge.to_header().unwrap();
    assert_eq!(
        header,
        format!(
            "Payment id=\"{SESSION_ID}\", realm=\"cloud-api.near.ai\", method=\"tempo\", \
             intent=\"session\", request=\"{SESSION_REQUEST_B64}\", \
             expires=\"2030-05-06T07:08:09Z\", opaque=\"{OPAQUE}\""
        )
    );
    assert!(!header.contains("digest"));
}

fn bound<'a>(intent: &'a str, request: &'a str, expires: &'a str) -> BoundFields<'a> {
    BoundFields {
        realm: "cloud-api.near.ai",
        method: METHOD,
        intent,
        request,
        expires: Some(expires),
        digest: None,
        opaque: Some(OPAQUE),
    }
}

#[test]
fn session_challenge_id_binds_the_intent_and_every_field() {
    let challenge = challenge();
    let on = "2030-05-06T07:08:09Z";
    assert_eq!(challenge.expires_string(), on);
    assert!(verify_challenge_id(
        SECRET,
        &challenge.id,
        &bound("session", SESSION_REQUEST_B64, on)
    ));
    // The same bytes as a charge, a changed request or a changed expiry are other ids.
    for other in [
        bound("charge", SESSION_REQUEST_B64, on),
        bound("session", "e30", on),
        bound("session", SESSION_REQUEST_B64, "2030-05-06T07:08:10Z"),
    ] {
        assert!(!verify_challenge_id(SECRET, &challenge.id, &other));
    }
    // A different nonce is a different challenge.
    let other = session_challenge(
        SECRET,
        "cloud-api.near.ai",
        &terms(),
        expires(),
        "/v1/chat/completions",
        &[8; OPAQUE_NONCE_BYTES],
    )
    .unwrap();
    assert_ne!(other.id, challenge.id);
    assert_ne!(other.opaque, challenge.opaque);
}

/// The credential `mppx` sends back for `challenge`: the echoed parameters, the payload and
/// the payer DID, serialized as `Credential.serialize` does.
fn credential_header(challenge: &Challenge, payload: Value) -> String {
    let wire = json!({
        "challenge": {
            "id": challenge.id,
            "realm": challenge.realm,
            "method": "tempo",
            "intent": "session",
            "request": challenge.request,
            "expires": challenge.expires_string(),
            "opaque": challenge.opaque,
        },
        "payload": payload,
        "source": "did:pkh:eip155:42431:0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    });
    format!("Payment {}", b64url_encode(wire.to_string().as_bytes()))
}

#[test]
fn a_session_credential_is_routed_by_its_echoed_intent_and_its_id_verifies() {
    let challenge = challenge();
    let header = credential_header(&challenge, voucher_payload());
    let ParsedAuthorization::Session(credential) = parse_any_authorization(&header).unwrap() else {
        panic!("expected a session credential");
    };
    assert_eq!(credential.challenge.intent, "session");
    assert_eq!(credential.challenge.digest, None);
    assert_eq!(credential.challenge.id, challenge.id);
    assert!(verify_challenge_id(
        SECRET,
        &credential.challenge.id,
        &credential.challenge.bound_fields()
    ));
    assert_eq!(
        SessionTerms::from_b64(&credential.challenge.request).unwrap(),
        terms()
    );
    assert!(matches!(
        SessionPayload::parse(&credential.payload).unwrap(),
        SessionPayload::Voucher { .. }
    ));
    assert!(credential.source.is_some());
    // The charge entry point still reads every payload as a charge payload, so a voucher is
    // no credential for it.
    assert!(parse_authorization(&header).is_err());
}

// ---------------------------------------------------------------------------------------
// Payloads, written exactly as the mppx client writes them
// ---------------------------------------------------------------------------------------

fn descriptor_payload() -> Value {
    let v = Vectors::load();
    json!({
        "authorizedSigner": v.text("descriptor.authorizedSigner"),
        "expiringNonceHash": v.text("descriptor.expiringNonceHash"),
        "operator": v.text("descriptor.operator"),
        "payee": v.text("descriptor.payee"),
        "payer": v.text("descriptor.payer"),
        "salt": v.text("descriptor.salt"),
        "token": v.text("descriptor.token"),
    })
}

/// `createOpenPayload`.
fn open_payload() -> Value {
    let v = Vectors::load();
    json!({
        "action": "open",
        "type": "transaction",
        "channelId": v.text("channelId"),
        "transaction": v.text("open.serialized"),
        "signature": v.text("voucher.signature"),
        "descriptor": descriptor_payload(),
        "cumulativeAmount": "1250000",
        "authorizedSigner": v.text("descriptor.authorizedSigner"),
    })
}

/// `createTopUpPayload` (the open vector's transaction stands in for a topUp transaction:
/// this layer checks only that it is one `0x76` RLP list).
fn top_up_payload() -> Value {
    let v = Vectors::load();
    json!({
        "action": "topUp",
        "type": "transaction",
        "channelId": v.text("channelId"),
        "transaction": v.text("open.serialized"),
        "descriptor": descriptor_payload(),
        "additionalDeposit": "2000000",
    })
}

/// `createVoucherPayload`.
fn voucher_payload() -> Value {
    let v = Vectors::load();
    json!({
        "action": "voucher",
        "channelId": v.text("channelId"),
        "descriptor": descriptor_payload(),
        "cumulativeAmount": "1250000",
        "signature": v.text("voucher.signature"),
    })
}

/// `createClosePayload`.
fn close_payload() -> Value {
    let v = Vectors::load();
    json!({
        "action": "close",
        "channelId": v.text("channelId"),
        "descriptor": descriptor_payload(),
        "cumulativeAmount": "1250000",
        "signature": v.text("voucher.signature"),
        "closeSignature": v.text("closeAuthorization.signature"),
    })
}

fn all_payloads() -> [(SessionAction, Value); 4] {
    [
        (SessionAction::Open, open_payload()),
        (SessionAction::TopUp, top_up_payload()),
        (SessionAction::Voucher, voucher_payload()),
        (SessionAction::Close, close_payload()),
    ]
}

#[test]
fn each_payload_parses_to_the_golden_values() {
    let v = Vectors::load();
    let descriptor = v.descriptor();
    let sig: [u8; 65] = v.fixed("voucher.signature");

    let open = SessionPayload::parse(&open_payload()).unwrap();
    assert_eq!(
        open,
        SessionPayload::Open {
            channel_id: v.fixed("channelId"),
            transaction: v.bytes("open.serialized"),
            descriptor,
            cumulative_amount: 1_250_000,
            signature: sig,
        }
    );
    assert_eq!(
        SessionPayload::parse(&top_up_payload()).unwrap(),
        SessionPayload::TopUp {
            channel_id: v.fixed("channelId"),
            transaction: v.bytes("open.serialized"),
            descriptor,
            additional_deposit: 2_000_000,
        }
    );
    assert_eq!(
        SessionPayload::parse(&voucher_payload()).unwrap(),
        SessionPayload::Voucher {
            channel_id: v.fixed("channelId"),
            descriptor,
            cumulative_amount: 1_250_000,
            signature: sig,
        }
    );
    assert_eq!(
        SessionPayload::parse(&close_payload()).unwrap(),
        SessionPayload::Close {
            channel_id: v.fixed("channelId"),
            descriptor,
            cumulative_amount: 1_250_000,
            signature: sig,
            close_signature: v.fixed("closeAuthorization.signature"),
        }
    );
}

#[test]
fn every_parsed_descriptor_hashes_to_the_vector_channel_id() {
    let v = Vectors::load();
    for (action, payload) in all_payloads() {
        let parsed = SessionPayload::parse(&payload).unwrap();
        assert_eq!(parsed.action(), action);
        assert_eq!(
            channel_id(parsed.descriptor(), &v.escrow(), v.chain_id()),
            *parsed.channel_id(),
            "{action:?}"
        );
        assert_eq!(*parsed.channel_id(), v.fixed::<32>("channelId"));
    }
    assert_eq!(CHANNEL, v.text("channelId"));
    assert_eq!(VOUCHER_SIG, v.text("voucher.signature"));
    assert_eq!(NONCE_HASH, v.text("descriptor.expiringNonceHash"));
    assert_eq!(TX_HASH, v.text("open.txHash"));
}

#[test]
fn management_actions_are_open_top_up_and_close() {
    for (action, payload) in all_payloads() {
        assert_eq!(
            SessionPayload::parse(&payload)
                .unwrap()
                .action()
                .is_management(),
            !matches!(action, SessionAction::Voucher)
        );
    }
    let names: Vec<&str> = all_payloads().iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(names, ["open", "topUp", "voucher", "close"]);
}

#[test]
fn hex_case_and_a_top_level_authorized_signer_do_not_matter() {
    let v = Vectors::load();
    // viem writes checksummed (mixed-case) addresses.
    let mut payload = voucher_payload();
    payload["descriptor"]["payer"] = json!(v
        .text("descriptor.payer")
        .to_uppercase()
        .replace("0X", "0x"));
    payload["channelId"] = json!(v.text("channelId").to_uppercase().replace("0X", "0x"));
    assert_eq!(
        SessionPayload::parse(&payload).unwrap(),
        SessionPayload::parse(&voucher_payload()).unwrap()
    );

    // The descriptor's own authorizedSigner is authoritative; the copy mppx also puts at the
    // top level of an open is ignored, whatever it holds.
    for top_level in [
        json!(format!("0x{}", "42".repeat(20))),
        json!("not an address"),
        json!(7),
    ] {
        let mut payload = open_payload();
        payload["authorizedSigner"] = top_level;
        let parsed = SessionPayload::parse(&payload).unwrap();
        assert_eq!(parsed.descriptor().authorized_signer, [0u8; 20]);
    }
    let mut payload = open_payload();
    payload.as_object_mut().unwrap().remove("authorizedSigner");
    assert!(SessionPayload::parse(&payload).is_ok());

    // Unknown top-level keys on a voucher or close (mppx writes no `type` there) are ignored.
    let mut payload = voucher_payload();
    payload["type"] = json!("anything");
    assert!(SessionPayload::parse(&payload).is_ok());
}

#[test]
fn descriptor_json_is_the_inverse_of_parse_descriptor() {
    let v = Vectors::load();
    assert_eq!(descriptor_json(&v.descriptor()), descriptor_payload());
    assert_eq!(
        parse_descriptor(&descriptor_json(&v.descriptor())).unwrap(),
        v.descriptor()
    );
    let nonzero = ChannelDescriptor {
        authorized_signer: [0x42; 20],
        operator: [0x07; 20],
        ..v.descriptor()
    };
    assert_eq!(
        parse_descriptor(&descriptor_json(&nonzero)).unwrap(),
        nonzero
    );
}

// ---------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------

fn payload_with(base: &Value, edit: impl FnOnce(&mut Value)) -> Value {
    let mut value = base.clone();
    edit(&mut value);
    value
}

fn refusal(payload: &Value) -> WireError {
    SessionPayload::parse(payload).expect_err("payload must be refused")
}

#[test]
fn a_missing_field_is_refused_for_every_action() {
    for (action, base) in all_payloads() {
        let keys: Vec<String> = base.as_object().unwrap().keys().cloned().collect();
        for key in keys {
            let payload = payload_with(&base, |v| {
                v.as_object_mut().unwrap().remove(&key);
            });
            let outcome = SessionPayload::parse(&payload);
            if key == "authorizedSigner" {
                // The only key that may be absent.
                assert!(outcome.is_ok(), "{action:?} without {key}");
                continue;
            }
            let expected = match key.as_str() {
                "action" => WireError::Malformed("missing_action"),
                "descriptor" => WireError::Malformed("missing_descriptor"),
                "type" => WireError::Malformed("missing_type"),
                _ => WireError::Malformed("missing_field"),
            };
            assert_eq!(outcome, Err(expected), "{action:?} without {key}");
        }
    }
}

#[test]
fn the_type_of_an_open_or_top_up_must_be_transaction() {
    for base in [open_payload(), top_up_payload()] {
        for bad in [
            json!("hash"),
            json!("Transaction"),
            json!(""),
            json!(1),
            json!(null),
        ] {
            let payload = payload_with(&base, |v| v["type"] = bad.clone());
            assert_eq!(
                refusal(&payload),
                WireError::Malformed("invalid_type"),
                "{bad}"
            );
        }
    }
}

#[test]
fn unknown_or_mistyped_actions_are_refused() {
    for bad in [
        json!("settle"),
        json!("Open"),
        json!("topup"),
        json!("requestClose"),
        json!(""),
        json!(1),
        json!(null),
        json!(["open"]),
    ] {
        let payload = payload_with(&voucher_payload(), |v| v["action"] = bad.clone());
        assert_eq!(refusal(&payload), WireError::UnknownAction, "{bad}");
    }
    for not_an_object in [json!("voucher"), json!(["voucher"]), json!(null), json!(7)] {
        assert_eq!(
            refusal(&not_an_object),
            WireError::Malformed("payload_not_object")
        );
    }
}

#[test]
fn a_settlement_route_is_refused_whatever_it_holds() {
    for (action, base) in all_payloads() {
        for key in ["settlementRoute", "settlement_route"] {
            for value in [
                json!(null),
                json!({}),
                json!({"adapter": "0x1", "recipient": "0x2", "targetToken": "0x3"}),
                json!("x"),
            ] {
                let payload = payload_with(&base, |v| v[key] = value.clone());
                assert_eq!(
                    refusal(&payload),
                    WireError::Unsupported("settlement_route"),
                    "{action:?} {key} {value}"
                );
            }
        }
    }
}

#[test]
fn hex_fields_must_have_exact_lengths() {
    let v = Vectors::load();
    let channel = v.text("channelId");
    let cases = [
        (
            "short channel id",
            json!(&channel[..channel.len() - 2]),
            "channelId",
        ),
        (
            "long channel id",
            json!(format!("{channel}00")),
            "channelId",
        ),
        (
            "channel id without prefix",
            json!(&channel[2..]),
            "channelId",
        ),
        (
            "non-hex channel id",
            json!(format!("0x{}", "zz".repeat(32))),
            "channelId",
        ),
        ("numeric channel id", json!(7), "channelId"),
    ];
    for (name, bad, key) in cases {
        let payload = payload_with(&voucher_payload(), |v| v[key] = bad.clone());
        let expected = match name {
            "numeric channel id" => WireError::Malformed("missing_field"),
            "channel id without prefix" => WireError::Malformed("hex_prefix"),
            "non-hex channel id" => WireError::Malformed("hex_digits"),
            _ => WireError::Malformed("hex_length"),
        };
        assert_eq!(refusal(&payload), expected, "{name}");
    }
    // Every descriptor field is exact: one byte short or long is refused.
    for key in [
        "payer",
        "payee",
        "operator",
        "token",
        "salt",
        "authorizedSigner",
        "expiringNonceHash",
    ] {
        let original = descriptor_payload()[key].as_str().unwrap().to_string();
        for bad in [
            original[..original.len() - 2].to_string(),
            format!("{original}00"),
            "0x".to_string(),
            original[2..].to_string(),
        ] {
            let payload = payload_with(&voucher_payload(), |v| v["descriptor"][key] = json!(bad));
            assert!(
                matches!(refusal(&payload), WireError::Malformed(_)),
                "descriptor.{key} = {bad}"
            );
        }
    }
    // A descriptor with a missing, extra or mistyped key, or that is no object, is refused.
    for edit in [
        (|v: &mut Value| {
            v["descriptor"].as_object_mut().unwrap().remove("salt");
        }) as fn(&mut Value),
        |v| v["descriptor"]["feePayer"] = json!(true),
        |v| v["descriptor"]["payer"] = json!(5),
        |v| v["descriptor"] = json!("0x00"),
        |v| v["descriptor"] = json!([]),
        |v| v["descriptor"] = json!(null),
    ] {
        let payload = payload_with(&voucher_payload(), edit);
        assert_eq!(
            refusal(&payload),
            WireError::Malformed("invalid_descriptor")
        );
    }
}

#[test]
fn signatures_must_be_exactly_65_bytes() {
    let sig = Vectors::load().text("voucher.signature").to_string();
    let shorter = sig[..sig.len() - 2].to_string();
    let longer = format!("{sig}00");
    let compact = format!("0x{}", "ab".repeat(64));
    let empty = "0x".to_string();
    for (action, base, key) in [
        (SessionAction::Open, open_payload(), "signature"),
        (SessionAction::Voucher, voucher_payload(), "signature"),
        (SessionAction::Close, close_payload(), "signature"),
        (SessionAction::Close, close_payload(), "closeSignature"),
    ] {
        for bad in [&shorter, &longer, &compact, &empty] {
            let payload = payload_with(&base, |v| v[key] = json!(bad));
            assert_eq!(
                refusal(&payload),
                WireError::UnsupportedSignature,
                "{action:?}.{key} {} chars",
                bad.len()
            );
        }
        let non_hex = payload_with(&base, |v| v[key] = json!(format!("0x{}", "zz".repeat(65))));
        assert_eq!(refusal(&non_hex), WireError::Malformed("hex_digits"));
        let no_prefix = payload_with(&base, |v| v[key] = json!(sig[2..]));
        assert_eq!(refusal(&no_prefix), WireError::Malformed("hex_prefix"));
    }
}

#[test]
fn amounts_are_canonical_decimal_strings_within_uint96() {
    let amount_keys = [
        (open_payload(), "cumulativeAmount"),
        (top_up_payload(), "additionalDeposit"),
        (voucher_payload(), "cumulativeAmount"),
        (close_payload(), "cumulativeAmount"),
    ];
    let not_decimal = [
        "", "-1", "+1", "1.5", "0x10", " 1", "1 ", "1e3", "01", "00", "١٢",
    ];
    let too_big = [
        "79228162514264337593543950336",           // uint96 max + 1
        "340282366920938463463374607431768211455", // u128 max
        "340282366920938463463374607431768211456", // u128 max + 1
        "1000000000000000000000000000000000000000000",
    ];
    let fine = ["0", "1", "1250000", "79228162514264337593543950335"]; // uint96 max
    for (base, key) in amount_keys {
        for bad in not_decimal {
            let payload = payload_with(&base, |v| v[key] = json!(bad));
            assert_eq!(
                refusal(&payload),
                WireError::Malformed("invalid_amount"),
                "{key}={bad}"
            );
        }
        for bad in too_big {
            let payload = payload_with(&base, |v| v[key] = json!(bad));
            assert_eq!(
                refusal(&payload),
                WireError::AmountOutOfRange,
                "{key}={bad}"
            );
        }
        for good in fine {
            let payload = payload_with(&base, |v| v[key] = json!(good));
            assert!(SessionPayload::parse(&payload).is_ok(), "{key}={good}");
        }
        // A JSON number is not an amount.
        let payload = payload_with(&base, |v| v[key] = json!(5));
        assert_eq!(refusal(&payload), WireError::Malformed("missing_field"));
    }
}

/// A well-formed `0x76` transaction of exactly `total` bytes: the type byte and one long-form
/// RLP list.
fn transaction_hex(total: usize) -> String {
    let payload_len = total - 4;
    let mut raw = vec![0x76, 0xf9];
    raw.extend((u16::try_from(payload_len).unwrap()).to_be_bytes());
    raw.extend(vec![0u8; payload_len]);
    assert_eq!(raw.len(), total);
    format!("0x{}", hex::encode(raw))
}

#[test]
fn raw_transactions_are_bounded_and_structurally_checked() {
    for (base, key) in [
        (open_payload(), "transaction"),
        (top_up_payload(), "transaction"),
    ] {
        // Exactly the cap is accepted, one byte over is not.
        let at_cap = payload_with(&base, |v| v[key] = json!(transaction_hex(MAX_RAW_TX_BYTES)));
        let (SessionPayload::Open { transaction, .. } | SessionPayload::TopUp { transaction, .. }) =
            SessionPayload::parse(&at_cap).unwrap()
        else {
            panic!("expected a transaction payload");
        };
        assert_eq!(transaction.len(), MAX_RAW_TX_BYTES);
        let over = payload_with(&base, |v| {
            v[key] = json!(format!("0x{}", "00".repeat(MAX_RAW_TX_BYTES + 1)));
        });
        assert_eq!(
            refusal(&over),
            WireError::Malformed("transaction_too_large")
        );

        let cases = [
            ("0x", "not_tempo_transaction"),
            ("0x02f8", "not_tempo_transaction"),
            ("0x76", "invalid_transaction_encoding"),
            ("0x76deadbeef", "invalid_transaction_encoding"),
            ("0x76c0c0", "invalid_transaction_encoding"),
            ("0x76c", "hex_digits"),
            ("0xzz", "hex_digits"),
            ("76c0", "hex_prefix"),
        ];
        for (bad, reason) in cases {
            let payload = payload_with(&base, |v| v[key] = json!(bad));
            assert_eq!(refusal(&payload), WireError::Malformed(reason), "{bad}");
        }
        let payload = payload_with(&base, |v| v[key] = json!(5));
        assert_eq!(refusal(&payload), WireError::Malformed("missing_field"));
    }
}

#[test]
fn payload_debug_names_the_action_and_nothing_else() {
    for (action, payload) in all_payloads() {
        let parsed = SessionPayload::parse(&payload).unwrap();
        assert_eq!(
            format!("{parsed:?}"),
            format!("SessionPayload({})", action.as_str())
        );
    }
    assert_eq!(format!("{:?}", snapshot()), "SessionSnapshot { .. }");
    assert_eq!(
        format!("{:?}", snapshot().highest_voucher),
        "HighestVoucher { .. }"
    );
}

#[test]
fn wire_error_reasons_are_fixed_codes() {
    assert_eq!(WireError::UnknownAction.reason(), "unknown_action");
    assert_eq!(
        WireError::UnsupportedSignature.reason(),
        "unsupported_signature"
    );
    assert_eq!(WireError::AmountOutOfRange.reason(), "amount_out_of_range");
    assert_eq!(
        WireError::Unsupported("settlement_route").reason(),
        "settlement_route"
    );
    assert_eq!(
        WireError::from(ProtocolError::Malformed("invalid base64url")),
        WireError::Malformed("invalid base64url")
    );
    assert_eq!(
        WireError::from(ProtocolError::MethodUnsupported),
        WireError::Unsupported("method_unsupported")
    );
    assert_eq!(
        WireError::UnknownAction.to_string(),
        "session wire: unknown_action"
    );
}

// ---------------------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------------------

fn receipt(tx_hash: Option<[u8; 32]>) -> SessionReceipt {
    SessionReceipt {
        challenge_id: SESSION_ID.to_string(),
        channel_id: Vectors::load().fixed("channelId"),
        accepted_cumulative: 1_250_000,
        spent: 1_000_000,
        units: 50_000,
        tx_hash,
    }
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2030-05-06T07:08:09.123456Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn receipt_header_decodes_to_the_exact_object() {
    let without = format!(
        r#"{{"acceptedCumulative":"1250000","challengeId":"{SESSION_ID}","channelId":"{CHANNEL}","intent":"session","method":"tempo","reference":"{CHANNEL}","spent":"1000000","status":"success","timestamp":"2030-05-06T07:08:09.123Z","units":50000}}"#
    );
    let header = receipt(None).header(at()).unwrap();
    assert_eq!(b64url_decode(&header).unwrap(), without.into_bytes());
    assert!(!header.contains('=') && !header.contains('+') && !header.contains('/'));

    let with = format!(
        r#"{{"acceptedCumulative":"1250000","challengeId":"{SESSION_ID}","channelId":"{CHANNEL}","intent":"session","method":"tempo","reference":"{CHANNEL}","spent":"1000000","status":"success","timestamp":"2030-05-06T07:08:09.123Z","txHash":"{TX_HASH}","units":50000}}"#
    );
    let header = receipt(Some(Vectors::load().fixed("open.txHash")))
        .header(at())
        .unwrap();
    assert_eq!(b64url_decode(&header).unwrap(), with.into_bytes());
}

#[test]
fn receipt_satisfies_mppx_is_session_receipt() {
    // `isSessionReceipt`: method, intent and status literals; strings for the timestamp,
    // reference, challenge id, channel id and both amounts; a number for units; a string txHash.
    let header = receipt(Some([0xab; 32])).header(at()).unwrap();
    let value: Value = serde_json::from_slice(&b64url_decode(&header).unwrap()).unwrap();
    assert_eq!(value["method"], "tempo");
    assert_eq!(value["intent"], "session");
    assert_eq!(value["status"], "success");
    for key in [
        "timestamp",
        "reference",
        "challengeId",
        "channelId",
        "acceptedCumulative",
        "spent",
        "txHash",
    ] {
        assert!(value[key].is_string(), "{key}");
    }
    assert!(value["units"].is_u64());
    assert_eq!(value["reference"], value["channelId"]);
    assert_eq!(format!("{:?}", receipt(None)), "SessionReceipt { .. }");
}
