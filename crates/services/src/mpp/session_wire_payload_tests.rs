use serde_json::{json, Value};

use super::*;
use crate::mpp::channel::channel_id;
use crate::mpp::session_vectors::Vectors;

const CHANNEL: &str = "0xbf6845b6ee5f37bd139093d0f2d6765a23201ea2af587db27b93659b7cf613a2";
const VOUCHER_SIG: &str = "0x75b419cc6d2a2cd4d5629e924d62deb3671cb6d1b9f2cee9406ddc53ab90c0b56384a4a82e0d6524583b3edc8b1269fcde369b736c913f5e55c3162c7c4c18501b";
const NONCE_HASH: &str = "0x3b8570e0efaae36f92efaf33760304be75e89d3aae8abfb740176b3c6b4cb347";
const TX_HASH: &str = "0x6e0444bc1e22b457eb3a509c0d4319600d9ac37e26d7f9d58e8d80e8733b4c68";

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
pub(super) fn voucher_payload() -> Value {
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

pub(super) fn all_payloads() -> [(SessionAction, Value); 4] {
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
