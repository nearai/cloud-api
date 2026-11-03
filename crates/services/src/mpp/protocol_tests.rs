use super::*;

const SECRET: &[u8] = b"test-secret-key-12345";

fn id_for(secret: &[u8], realm: &str, method: &str, intent: &str, request: &str) -> String {
    compute_challenge_id(
        secret,
        &BoundFields {
            realm,
            method,
            intent,
            request,
            expires: None,
            digest: None,
            opaque: None,
        },
    )
    .unwrap()
}

/// Golden vector from mpp-rs `binding.rs::test_compute_challenge_id_cross_sdk`.
#[test]
fn challenge_id_matches_the_cross_sdk_vector() {
    let request = b64url_encode(
        br#"{"amount":"1000000","currency":"0x20c0000000000000000000000000000000000000","recipient":"0x1234567890abcdef1234567890abcdef12345678"}"#,
    );
    let id = id_for(SECRET, "api.example.com", "tempo", "charge", &request);
    assert_eq!(id, "XmJ98SdsAdzwP9Oa-8In322Uh6yweMO6rywdomWk_V4");
    let fields = BoundFields {
        realm: "api.example.com",
        method: "tempo",
        intent: "charge",
        request: &request,
        expires: None,
        digest: None,
        opaque: None,
    };
    assert!(verify_challenge_id(SECRET, &id, &fields));
}

/// The reference's golden vectors (shared with mppx and pympp), every optional slot.
#[test]
fn challenge_id_matches_all_reference_golden_vectors() {
    let secret = b"test-vector-secret";
    let amount = b64url_encode(br#"{"amount":"1000000"}"#);
    let multi = b64url_encode(br#"{"amount":"1000000","currency":"0x1234","recipient":"0xabcd"}"#);
    let nested = b64url_encode(
        br#"{"amount":"1000000","currency":"0x1234","methodDetails":{"chainId":42431}}"#,
    );
    let empty = b64url_encode(b"{}");
    let digest = Some("sha-256=X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE");
    let expires = Some("2025-01-06T12:00:00Z");

    #[allow(clippy::type_complexity)]
    let vectors: Vec<(&str, &str, &str, &str, Option<&str>, Option<&str>, &str)> = vec![
        (
            "api.example.com",
            "tempo",
            "charge",
            &amount,
            None,
            None,
            "X6v1eo7fJ76gAxqY0xN9Jd__4lUyDDYmriryOM-5FO4",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &amount,
            expires,
            None,
            "ChPX33RkKSZoSUyZcu8ai4hhkvjZJFkZVnvWs5s0iXI",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &amount,
            None,
            digest,
            "JHB7EFsPVb-xsYCo8LHcOzeX1gfXWVoUSzQsZhKAfKM",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &amount,
            expires,
            digest,
            "m39jbWWCIfmfJZSwCfvKFFtBl0Qwf9X4nOmDb21peLA",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &multi,
            None,
            None,
            "_H5TOnnlW0zduQ5OhQ3EyLVze_TqxLDPda2CGZPZxOc",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &nested,
            None,
            None,
            "TqujwpuDDg_zsWGINAd5XObO2rRe6uYufpqvtDmr6N8",
        ),
        (
            "api.example.com",
            "tempo",
            "charge",
            &empty,
            None,
            None,
            "yLN7yChAejW9WNmb54HpJIWpdb1WWXeA3_aCx4dxmkU",
        ),
        (
            "payments.other.com",
            "tempo",
            "charge",
            &amount,
            None,
            None,
            "3F5bOo2a9RUihdwKk4hGRvBvzQmVPBMDvW0YM-8GD00",
        ),
        (
            "api.example.com",
            "stripe",
            "charge",
            &amount,
            None,
            None,
            "o0ra2sd7HcB4Ph0Vns69gRDUhSj5WNOnUopcDqKPLz4",
        ),
        (
            "api.example.com",
            "tempo",
            "session",
            &amount,
            None,
            None,
            "aAY7_IEDzsznNYplhOSE8cERQxvjFcT4Lcn-7FHjLVE",
        ),
    ];
    for (realm, method, intent, request, expires, digest, expected) in vectors {
        let fields = BoundFields {
            realm,
            method,
            intent,
            request,
            expires,
            digest,
            opaque: None,
        };
        assert_eq!(compute_challenge_id(secret, &fields).unwrap(), expected);
        assert!(verify_challenge_id(secret, expected, &fields));
    }
}

/// The reference's opaque vectors: JCS key sorting and the opaque slot.
#[test]
fn challenge_id_matches_the_reference_opaque_vectors() {
    let secret = b"test-vector-secret";
    let request = b64url_encode(br#"{"amount":"1000000"}"#);
    let opaque = |value: Value| b64url_encode(canonical_json(&value).unwrap().as_bytes());
    let cases = [
        (
            opaque(serde_json::json!({"pi": "pi_3abc123XYZ"})),
            None,
            "rxzKZ2qjXvinqCH96RORTZEPs1KXsA-0AUjrCAPFOWc",
        ),
        (
            opaque(serde_json::json!({"pi": "pi_3abc123XYZ"})),
            Some("2025-01-06T12:00:00Z"),
            "KAfoMrA4fnzS1DPWN_cUv_b3_yHxCizdp6OhH7gluMY",
        ),
        (
            opaque(serde_json::json!({})),
            None,
            "vb4IyH-0LdJ3s7L0QAw8jIzcZkyxksPhIvEfmHmzA9k",
        ),
        (
            // Keys given out of order: JCS must sort them.
            opaque(serde_json::json!({"pi": "pi_3abc123XYZ", "deposit": "dep_456"})),
            None,
            "aKskU8sadR5ZuFbUCsIwhO-ENxuVpTw17FdwHEXsJDk",
        ),
    ];
    for (opaque, expires, expected) in cases {
        let fields = BoundFields {
            realm: "api.example.com",
            method: "tempo",
            intent: "charge",
            request: &request,
            expires,
            digest: None,
            opaque: Some(&opaque),
        };
        assert_eq!(compute_challenge_id(secret, &fields).unwrap(), expected);
    }
}

#[test]
fn verify_challenge_id_rejects_any_changed_field_or_a_noncanonical_id() {
    let request = b64url_encode(br#"{"amount":"1"}"#);
    let fields = BoundFields {
        realm: "r.example",
        method: "tempo",
        intent: "charge",
        request: &request,
        expires: Some("2030-01-01T00:00:00Z"),
        digest: Some("sha-256=:abc=:"),
        opaque: Some("b3BhcXVl"),
    };
    let id = compute_challenge_id(SECRET, &fields).unwrap();
    assert!(verify_challenge_id(SECRET, &id, &fields));
    assert!(!verify_challenge_id(
        b"another-secret-key-0123456789abcdef",
        &id,
        &fields
    ));

    let mut changed = fields;
    changed.realm = "other.example";
    assert!(!verify_challenge_id(SECRET, &id, &changed));
    let mut changed = fields;
    changed.expires = Some("2030-01-01T00:00:01Z");
    assert!(!verify_challenge_id(SECRET, &id, &changed));
    let mut changed = fields;
    changed.digest = None;
    assert!(!verify_challenge_id(SECRET, &id, &changed));
    let mut changed = fields;
    changed.opaque = Some("b3BhcXVmZQ");
    assert!(!verify_challenge_id(SECRET, &id, &changed));
    let mut changed = fields;
    changed.method = "stripe";
    assert!(!verify_challenge_id(SECRET, &id, &changed));

    // Same MAC, non-canonical text (padding): refused, one challenge = one id string.
    assert!(!verify_challenge_id(SECRET, &format!("{id}="), &fields));
    assert!(!verify_challenge_id(SECRET, "", &fields));
    assert!(!verify_challenge_id(SECRET, "!!!", &fields));
}

#[test]
fn base64url_decode_accepts_padded_unpadded_and_standard_alphabet() {
    let data = [0xfb, 0xff, 0xfe, 0x01, 0x02];
    let unpadded = b64url_encode(&data);
    assert!(!unpadded.contains('='));
    assert_eq!(b64url_decode(&unpadded).unwrap(), data);
    assert_eq!(b64url_decode(&format!("{unpadded}=")).unwrap(), data);
    assert_eq!(b64url_decode(&STANDARD.encode(data)).unwrap(), data);
    assert!(b64url_decode("YW!JjZA").is_err());
    // Trailing non-zero bits are not canonical.
    assert!(b64url_decode("YR").is_err());
}

#[test]
fn jcs_sorts_keys_and_emits_no_whitespace() {
    let value = serde_json::json!({
        "recipient": "0xb",
        "amount": "1",
        "methodDetails": {"z": 1, "chainId": 42431},
        "currency": "0xa",
    });
    assert_eq!(
        canonical_json(&value).unwrap(),
        r#"{"amount":"1","currency":"0xa","methodDetails":{"chainId":42431,"z":1},"recipient":"0xb"}"#
    );
    // UTF-16 order, not UTF-8 order: U+FF5E sorts after U+10000 (surrogate D800).
    let value = serde_json::json!({"\u{ff5e}": 1, "\u{10000}": 2});
    assert_eq!(
        canonical_json(&value).unwrap(),
        "{\"\u{10000}\":2,\"\u{ff5e}\":1}"
    );
    assert_eq!(
        canonical_json(&serde_json::json!({"a": "q\"\\\n\u{1}/é"})).unwrap(),
        "{\"a\":\"q\\\"\\\\\\n\\u0001/é\"}"
    );
    assert!(canonical_json(&serde_json::json!({"a": 1.5})).is_err());
}

#[test]
fn charge_request_is_canonical_and_round_trips_strictly() {
    let request = ChargeRequest {
        amount: 340_282_366_920_938_463_463_374_607_431_768_211_455,
        currency: "0x20c0000000000000000000000000000000000000".to_string(),
        chain_id: 42431,
        recipient: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
    };
    assert_eq!(
        request.to_json().unwrap(),
        r#"{"amount":"340282366920938463463374607431768211455","currency":"0x20c0000000000000000000000000000000000000","methodDetails":{"chainId":42431},"recipient":"0x1234567890abcdef1234567890abcdef12345678"}"#
    );
    assert_eq!(
        ChargeRequest::from_b64(&request.to_b64().unwrap()).unwrap(),
        request
    );

    for bad in [
        r#"{"amount":"1","currency":"0xa","methodDetails":{"chainId":1},"recipient":"0xb","splits":[]}"#,
        r#"{"amount":"1","currency":"0xa","methodDetails":{"chainId":1,"feePayer":true},"recipient":"0xb"}"#,
        r#"{"amount":1,"currency":"0xa","methodDetails":{"chainId":1},"recipient":"0xb"}"#,
        r#"{"amount":"-1","currency":"0xa","methodDetails":{"chainId":1},"recipient":"0xb"}"#,
        r#"{"amount":"340282366920938463463374607431768211456","currency":"0xa","methodDetails":{"chainId":1},"recipient":"0xb"}"#,
        r#"{"amount":"1","currency":"0xa","recipient":"0xb"}"#,
    ] {
        assert!(
            ChargeRequest::from_b64(&b64url_encode(bad.as_bytes())).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn body_digest_uses_the_rfc_9530_byte_sequence_form() {
    assert_eq!(
        body_digest(b""),
        "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:"
    );
    assert_ne!(body_digest(b"{}"), body_digest(b"{} "));
}

fn sample_challenge() -> Challenge {
    let request = ChargeRequest {
        amount: 20_000,
        currency: "0x20c0000000000000000000000000000000000000".to_string(),
        chain_id: 42431,
        recipient: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
    }
    .to_b64()
    .unwrap();
    let opaque = opaque_for("/v1/chat/completions", &[7; OPAQUE_NONCE_BYTES]).unwrap();
    let digest = body_digest(b"{\"model\":\"m\"}");
    let expires = DateTime::parse_from_rfc3339("2030-05-06T07:08:09Z")
        .unwrap()
        .with_timezone(&Utc);
    let expires_string = format_timestamp(expires);
    let id = compute_challenge_id(
        SECRET,
        &BoundFields {
            realm: "cloud-api.near.ai",
            method: METHOD,
            intent: INTENT_CHARGE,
            request: &request,
            expires: Some(&expires_string),
            digest: Some(&digest),
            opaque: Some(&opaque),
        },
    )
    .unwrap();
    Challenge {
        id,
        realm: "cloud-api.near.ai".to_string(),
        request,
        expires,
        digest,
        opaque,
    }
}

#[test]
fn opaque_carries_a_nonce_and_names_its_route() {
    let a = opaque_for("/v1/chat/completions", &[1; OPAQUE_NONCE_BYTES]).unwrap();
    let b = opaque_for("/v1/chat/completions", &[2; OPAQUE_NONCE_BYTES]).unwrap();
    assert_ne!(a, b);
    assert_eq!(opaque_route(&a).as_deref(), Some("/v1/chat/completions"));
    assert_eq!(opaque_route(&b).as_deref(), Some("/v1/chat/completions"));
    // The legacy route-only object still names its route.
    let legacy = b64url_encode(br#"{"route":"/v1/chat/completions"}"#);
    assert_eq!(
        opaque_route(&legacy).as_deref(),
        Some("/v1/chat/completions")
    );
    // Not a flat string map with a route: nothing.
    for bad in [
        "",
        "!!!",
        &b64url_encode(b"[]"),
        &b64url_encode(br#"{"nonce":"x"}"#),
        &b64url_encode(br#"{"route":5}"#),
        &b64url_encode(br#"{"route":{"a":"b"}}"#),
    ] {
        assert_eq!(opaque_route(bad), None, "{bad}");
    }
}

#[test]
fn payment_key_is_a_keyed_digest_of_the_transaction_hash() {
    let tx = [0x11u8; 32];
    let key = payment_key(SECRET, &tx).unwrap();
    assert_eq!(key.len(), "tempo:".len() + 64);
    assert!(key.starts_with("tempo:"));
    assert!(
        !key.contains(&hex::encode(tx)),
        "the raw hash is not stored"
    );
    assert_eq!(key, payment_key(SECRET, &tx).unwrap(), "deterministic");
    assert_ne!(
        key,
        payment_key(b"another-secret-0123456789abcdefgh", &tx).unwrap()
    );
    assert_ne!(key, payment_key(SECRET, &[0x12u8; 32]).unwrap());
    // Pinned against the MAC, so a change of construction is a deliberate act.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET).unwrap();
    mac.update(&tx);
    assert_eq!(
        key,
        format!("tempo:{}", hex::encode(mac.finalize().into_bytes()))
    );
}

/// Parse `Payment k="v", …` back into pairs, honouring `\` escapes (test-only inverse of
/// `Challenge::to_header`).
fn parse_params(header: &str) -> Vec<(String, String)> {
    let rest = header.strip_prefix("Payment ").unwrap();
    let mut params = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            name.push(c);
            chars.next();
        }
        assert_eq!(chars.next(), Some('='));
        assert_eq!(chars.next(), Some('"'));
        let mut value = String::new();
        loop {
            match chars.next().unwrap() {
                '\\' => value.push(chars.next().unwrap()),
                '"' => break,
                c => value.push(c),
            }
        }
        params.push((name.trim().to_string(), value));
        match chars.next() {
            None => break,
            Some(',') => {
                assert_eq!(chars.next(), Some(' '));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    params
}

#[test]
fn www_authenticate_has_the_exact_parameter_order_and_round_trips() {
    let challenge = sample_challenge();
    let header = challenge.to_header().unwrap();
    assert_eq!(
        header,
        format!(
            "Payment id=\"{}\", realm=\"cloud-api.near.ai\", method=\"tempo\", \
             intent=\"charge\", request=\"{}\", expires=\"2030-05-06T07:08:09Z\", \
             digest=\"{}\", opaque=\"{}\"",
            challenge.id, challenge.request, challenge.digest, challenge.opaque
        )
    );
    let params = parse_params(&header);
    let names: Vec<&str> = params.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["id", "realm", "method", "intent", "request", "expires", "digest", "opaque"]
    );
    let get = |name: &str| {
        params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    };
    assert_eq!(get("id"), challenge.id);
    assert_eq!(get("digest"), challenge.digest);
    // The echoed values bind to the id exactly as the server computed it.
    assert!(verify_challenge_id(
        SECRET,
        get("id"),
        &BoundFields {
            realm: get("realm"),
            method: get("method"),
            intent: get("intent"),
            request: get("request"),
            expires: Some(get("expires")),
            digest: Some(get("digest")),
            opaque: Some(get("opaque")),
        }
    ));
    // The opaque value is base64url(JCS {"nonce": …, "route": …}).
    assert_eq!(
        b64url_decode(get("opaque")).unwrap(),
        format!(
            r#"{{"nonce":"{}","route":"/v1/chat/completions"}}"#,
            b64url_encode(&[7; OPAQUE_NONCE_BYTES])
        )
        .into_bytes()
    );
}

#[test]
fn quoted_values_are_escaped_and_control_characters_refused() {
    assert_eq!(escape_quoted("a\"b\\c").unwrap(), "a\\\"b\\\\c");
    assert!(escape_quoted("a\r\nb").is_err());
    assert!(escape_quoted("é").is_err());
}

fn credential_header(challenge: &Challenge, payload: Value, source: Option<&str>) -> String {
    let mut credential = serde_json::json!({
        "challenge": {
            "id": challenge.id,
            "realm": challenge.realm,
            "method": "tempo",
            "intent": "charge",
            "request": challenge.request,
            "expires": challenge.expires_string(),
            "digest": challenge.digest,
            "opaque": challenge.opaque,
        },
        "payload": payload,
    });
    if let Some(source) = source {
        credential["source"] = Value::String(source.to_string());
    }
    format!(
        "Payment {}",
        b64url_encode(credential.to_string().as_bytes())
    )
}

#[test]
fn credential_round_trips_for_hash_and_transaction_payloads() {
    let challenge = sample_challenge();
    let hash_hex = format!("0x{}", "ab".repeat(32));
    let source = "did:pkh:eip155:42431:0x1111111111111111111111111111111111111111";

    let header = credential_header(
        &challenge,
        serde_json::json!({"type": "hash", "hash": hash_hex}),
        Some(source),
    );
    let credential = parse_authorization(&header).unwrap();
    assert_eq!(credential.payload, PaymentPayload::Hash([0xab; 32]));
    assert_eq!(credential.source.as_deref(), Some(source));
    assert_eq!(credential.challenge.id, challenge.id);
    assert_eq!(
        credential.challenge.opaque.as_deref(),
        Some(challenge.opaque.as_str())
    );
    assert_eq!(
        credential.challenge.digest.as_deref(),
        Some(challenge.digest.as_str())
    );
    assert!(verify_challenge_id(
        SECRET,
        &credential.challenge.id,
        &credential.challenge.bound_fields()
    ));

    let raw = format!("0x76{}", rlp_list_hex(40));
    let header = credential_header(
        &challenge,
        serde_json::json!({"type": "transaction", "signature": raw}),
        None,
    );
    let credential = parse_authorization(&header).unwrap();
    let PaymentPayload::Transaction(bytes) = credential.payload else {
        panic!("expected a transaction payload");
    };
    assert_eq!(bytes.len(), 42);
    assert_eq!(bytes[0], 0x76);
    assert!(credential.source.is_none());

    // Scheme match is case-insensitive; padded base64 is accepted.
    let lower = header.replacen("Payment", "payment", 1);
    assert!(parse_authorization(&lower).is_ok());
}

#[test]
fn credential_parsing_is_bounded_and_strict() {
    let challenge = sample_challenge();
    let hash_payload =
        serde_json::json!({"type": "hash", "hash": format!("0x{}", "cd".repeat(32))});
    let good = credential_header(&challenge, hash_payload, None);
    assert!(parse_authorization(&good).is_ok());

    // Over 16 KiB is refused before any decoding.
    let huge = format!("Payment {}", "A".repeat(MAX_AUTHORIZATION_LEN));
    assert_eq!(
        parse_authorization(&huge),
        Err(ProtocolError::Malformed("authorization too long"))
    );

    for bad in [
        "".to_string(),
        "Bearer abc".to_string(),
        "Payment".to_string(),
        "Payment ".to_string(),
        "Payment !!!".to_string(),
        format!("{good}, Bearer x"),
        format!("Payment {}", b64url_encode(b"not json")),
        format!("Payment {}", b64url_encode(b"{}")),
    ] {
        assert!(parse_authorization(&bad).is_err(), "{bad}");
    }

    // `proof` is a well-formed credential this server does not take.
    let proof = credential_header(
        &challenge,
        serde_json::json!({"type": "proof", "signature": "0x00"}),
        None,
    );
    assert_eq!(
        parse_authorization(&proof),
        Err(ProtocolError::MethodUnsupported)
    );

    // Payload shape errors.
    let cases = [
        serde_json::json!({"type": "hash", "hash": "0x1234"}),
        serde_json::json!({"type": "hash", "hash": "ab".repeat(32)}),
        serde_json::json!({"type": "hash", "hash": format!("0x{}", "zz".repeat(32))}),
        serde_json::json!({"type": "hash"}),
        serde_json::json!({"type": "transaction", "signature": "0x02aabb"}),
        // Right type byte, but not one exact RLP list.
        serde_json::json!({"type": "transaction", "signature": "0x76"}),
        serde_json::json!({"type": "transaction", "signature": "0x76deadbeef"}),
        serde_json::json!({"type": "transaction", "signature": format!("0x76{}00", rlp_list_hex(40))}),
        serde_json::json!({"type": "transaction", "signature": "0x"}),
        serde_json::json!({"type": "transaction", "signature": format!("0x76{}", "00".repeat(MAX_RAW_TX_BYTES))}),
        serde_json::json!({"type": "weird", "hash": "0x00"}),
        serde_json::json!("hash"),
    ];
    for payload in cases {
        let header = credential_header(&challenge, payload.clone(), None);
        assert!(parse_authorization(&header).is_err(), "{payload}");
    }
    // Exactly the cap is fine (header size permitting is a separate bound).
    let at_cap = format!("0x76{}", "00".repeat(MAX_RAW_TX_BYTES - 1));
    assert_eq!(
        decode_prefixed_hex(&at_cap, MAX_RAW_TX_BYTES)
            .unwrap()
            .len(),
        MAX_RAW_TX_BYTES
    );

    // A `|` in a bound field would make the HMAC input ambiguous.
    let mut tampered = serde_json::json!({
        "challenge": {"id": "x", "realm": "a|b", "method": "tempo", "intent": "charge", "request": "e30"},
        "payload": {"type": "hash", "hash": format!("0x{}", "cd".repeat(32))},
    });
    let header = format!("Payment {}", b64url_encode(tampered.to_string().as_bytes()));
    assert!(parse_authorization(&header).is_err());
    tampered["challenge"]["realm"] = Value::String("a".into());
    tampered["challenge"]["id"] = Value::String(String::new());
    let header = format!("Payment {}", b64url_encode(tampered.to_string().as_bytes()));
    assert!(parse_authorization(&header).is_err());
}

#[test]
fn legacy_object_opaque_is_normalised_to_the_base64url_string() {
    let challenge = sample_challenge();
    let credential = serde_json::json!({
        "challenge": {
            "id": challenge.id, "realm": challenge.realm, "method": "tempo",
            "intent": "charge", "request": challenge.request,
            "opaque": {"route": "/v1/chat/completions"},
        },
        "payload": {"type": "hash", "hash": format!("0x{}", "cd".repeat(32))},
    });
    let header = format!(
        "Payment {}",
        b64url_encode(credential.to_string().as_bytes())
    );
    let parsed = parse_authorization(&header).unwrap();
    assert_eq!(
        parsed.challenge.opaque.as_deref(),
        Some(b64url_encode(br#"{"route":"/v1/chat/completions"}"#).as_str())
    );
}

/// Hex of a valid RLP list holding `payload_len` zero bytes (short or long form).
fn rlp_list_hex(payload_len: usize) -> String {
    let mut bytes = Vec::new();
    if payload_len <= 55 {
        bytes.push(0xc0 + payload_len as u8);
    } else {
        let length = payload_len.to_be_bytes();
        let significant: Vec<u8> = length.iter().copied().skip_while(|b| *b == 0).collect();
        bytes.push(0xf7 + significant.len() as u8);
        bytes.extend(significant);
    }
    bytes.extend(vec![0u8; payload_len]);
    hex::encode(bytes)
}

#[test]
fn rlp_check_requires_one_exact_list() {
    for len in [0usize, 1, 55, 56, 255, 256, 1000] {
        let bytes = hex::decode(rlp_list_hex(len)).unwrap();
        assert!(rlp::list_spans_exactly(&bytes), "len {len}");
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(!rlp::list_spans_exactly(&trailing), "trailing {len}");
        if len > 0 {
            let mut short = bytes.clone();
            short.pop();
            assert!(!rlp::list_spans_exactly(&short), "short {len}");
        }
    }
    assert!(!rlp::list_spans_exactly(&[]));
    assert!(!rlp::list_spans_exactly(&[0x80]), "a string is not a list");
    assert!(!rlp::list_spans_exactly(&[0x05]));
    assert!(!rlp::list_spans_exactly(&[0xf8]));
    // Long form used for a short payload, and a leading zero in the length.
    assert!(!rlp::list_spans_exactly(&[0xf8, 0x01, 0x00]));
    assert!(!rlp::list_spans_exactly(&[0xf9, 0x00, 0x38]));
    // A declared length near usize::MAX must not overflow.
    let mut huge = vec![0xff];
    huge.extend([0xff; 8]);
    assert!(!rlp::list_spans_exactly(&huge));
}

#[test]
fn scheme_detection() {
    assert!(is_payment_scheme("Payment abc"));
    assert!(is_payment_scheme("  payment\tabc"));
    assert!(is_payment_scheme("PAYMENT"));
    assert!(!is_payment_scheme("Bearer abc"));
    assert!(!is_payment_scheme("Payments abc"));
    assert!(!is_payment_scheme(""));
}

#[test]
fn source_did_must_be_canonical() {
    let ok = "did:pkh:eip155:42431:0xAbCdEf0123456789aBcDeF0123456789AbCdEf01";
    let (chain, address) = parse_source_did(ok).unwrap();
    assert_eq!(chain, 42431);
    assert_eq!(address[0], 0xab);
    for bad in [
        "did:pkh:eip155:042431:0xabcdef0123456789abcdef0123456789abcdef01",
        "did:pkh:eip155:42431:0xabcdef0123456789abcdef0123456789abcdef0",
        "did:pkh:eip155:42431:0xabcdef0123456789abcdef0123456789abcdef01:x",
        "did:pkh:eip155:-1:0xabcdef0123456789abcdef0123456789abcdef01",
        "did:pkh:solana:1:0xabcdef0123456789abcdef0123456789abcdef01",
        "did:pkh:eip155:42431",
        "",
    ] {
        assert!(parse_source_did(bad).is_err(), "{bad}");
    }
    assert_eq!(
        parse_source_did("did:pkh:eip155:0:0x0000000000000000000000000000000000000000")
            .unwrap()
            .0,
        0
    );
}

#[test]
fn receipt_is_base64url_of_sorted_compact_json() {
    let at = DateTime::parse_from_rfc3339("2026-11-02T10:11:12.345Z")
        .unwrap()
        .with_timezone(&Utc);
    let header = format_receipt("0xabc", at).unwrap();
    assert_eq!(
        b64url_decode(&header).unwrap(),
        br#"{"method":"tempo","reference":"0xabc","status":"success","timestamp":"2026-11-02T10:11:12.345Z"}"#
    );
}

#[test]
fn private_cache_control_merges() {
    assert_eq!(with_private_cache_control(None), "private");
    assert_eq!(with_private_cache_control(Some("  ")), "private");
    assert_eq!(
        with_private_cache_control(Some("no-store")),
        "no-store, private"
    );
    assert_eq!(
        with_private_cache_control(Some("max-age=1, Private")),
        "max-age=1, Private"
    );
}

#[test]
fn problem_bodies_use_the_paymentauth_type_uris() {
    let body: Value = serde_json::from_str(
        &ProblemCode::VerificationFailed.body(402, "Payment verification failed."),
    )
    .unwrap();
    assert_eq!(
        body["type"],
        "https://paymentauth.org/problems/verification-failed"
    );
    assert_eq!(body["status"], 402);
    assert_eq!(body["title"], "Payment Verification Failed");
    for (code, name) in [
        (ProblemCode::PaymentRequired, "payment-required"),
        (ProblemCode::MalformedCredential, "malformed-credential"),
        (ProblemCode::InvalidChallenge, "invalid-challenge"),
        (ProblemCode::PaymentExpired, "payment-expired"),
        (ProblemCode::MethodUnsupported, "method-unsupported"),
    ] {
        assert_eq!(code.as_str(), name);
        assert_eq!(
            code.type_uri(),
            format!("https://paymentauth.org/problems/{name}")
        );
    }
}
