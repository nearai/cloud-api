use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::payload_tests::{all_payloads, voucher_payload};
use super::*;
use crate::mpp::protocol::{
    parse_any_authorization, parse_authorization, verify_challenge_id, ParsedAuthorization,
};
use crate::mpp::session_vectors::Vectors;

const SECRET: &[u8] = b"test-secret-key-12345";
const ESCROW: &str = "0x4d50500000000000000000000000000000000000";
const TOKEN: &str = "0x20c0000000000000000000000000000000000000";
const PAYEE: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
const CHANNEL: &str = "0xbf6845b6ee5f37bd139093d0f2d6765a23201ea2af587db27b93659b7cf613a2";
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
