// INTEGRATION: registered by session_service::validation_tests; reuses its fixtures.

use serde_json::json;

use crate::mpp::error::{MppError, VerifyFailure};
use crate::mpp::pricing::Price;
use crate::mpp::protocol;
use crate::mpp::session_service::validation_tests::{
    cfg, edit_request, parse_envelope, resign, snapshot, voucher, Fixture, HIGH_PRICE, INITIAL,
    PRICE, ROUTE,
};
use crate::mpp::session_vectors::Vectors;
use crate::mpp::session_wire::hex_prefixed;

#[tokio::test]
async fn management_resolves_each_configured_token_price_from_signed_offer() {
    let f = Fixture::new();
    for price in [PRICE, HIGH_PRICE] {
        let challenge = f
            .service
            .challenge(ROUTE, &price, Some(snapshot()))
            .await
            .unwrap();
        let credential = parse_envelope(&challenge, voucher(INITIAL));
        let verified = f
            .service
            .verify_management_credential(&credential, ROUTE)
            .await
            .unwrap();
        assert_eq!(verified.challenge_id(), challenge.id);
        assert_eq!(verified.token_price, price.token_price.unwrap());
        assert_eq!(verified.min_voucher_delta, 100);
        assert_eq!(
            verified.payload().channel_id(),
            &Vectors::load().fixed::<32>("channelId")
        );
    }
    f.untouched();
}

#[tokio::test]
async fn management_rejects_authenticated_amount_without_configured_token_price() {
    let f = Fixture::new();
    let unmatched = Price {
        amount: 0,
        token_price: Some(99),
    };
    let challenge = f.service.challenge(ROUTE, &unmatched, None).await.unwrap();
    let credential = parse_envelope(&challenge, voucher(INITIAL));
    assert!(protocol::verify_challenge_id(
        &cfg().secret_key,
        &credential.challenge.id,
        &credential.challenge.bound_fields()
    ));
    assert_eq!(
        f.service
            .verify_management_credential(&credential, ROUTE)
            .await
            .err(),
        Some(MppError::InvalidChallenge("model_price"))
    );
    f.untouched();
}

#[tokio::test]
async fn management_authenticates_before_parsing_terms_or_resolving_price() {
    let f = Fixture::new();
    for malformed in [false, true] {
        let mut credential = f.credential(voucher(INITIAL)).await;
        if malformed {
            credential.challenge.request = "invalid terms".into();
        } else {
            edit_request(&mut credential, |request| request["amount"] = json!("99"));
        }
        credential.challenge.id = protocol::b64url_encode(&[0; 32]);
        assert_eq!(
            f.service
                .verify_management_credential(&credential, ROUTE)
                .await
                .err(),
            Some(MppError::InvalidChallenge("id"))
        );
    }
    f.untouched();
}

#[tokio::test]
async fn management_and_inference_share_challenge_source_and_payload_validation() {
    let f = Fixture::new();
    for case in 0..16 {
        let mut credential = f.credential(voucher(INITIAL)).await;
        let expected = match case {
            0 => {
                credential.challenge.intent = "charge".into();
                MppError::InvalidChallenge("intent")
            }
            1 => {
                credential.challenge.method = "foreign".into();
                MppError::MethodUnsupported
            }
            2 => {
                credential.challenge.realm = "foreign.example".into();
                MppError::InvalidChallenge("realm")
            }
            3 => {
                credential.challenge.digest = Some("unexpected".into());
                MppError::InvalidChallenge("digest")
            }
            4 => {
                credential.challenge.expires = Some("invalid".into());
                MppError::InvalidChallenge("expires")
            }
            5 => {
                credential.challenge.expires = Some("2000-01-01T00:00:00Z".into());
                MppError::Expired
            }
            6 => {
                credential.challenge.opaque = Some(
                    protocol::opaque_for("/v1/responses", &[0x31; protocol::OPAQUE_NONCE_BYTES])
                        .unwrap(),
                );
                MppError::InvalidChallenge("opaque")
            }
            7 => {
                credential.challenge.expires = None;
                MppError::InvalidChallenge("expires")
            }
            8 => {
                credential.challenge.opaque = None;
                MppError::InvalidChallenge("opaque")
            }
            9 => {
                credential.challenge.request = "invalid terms".into();
                MppError::InvalidChallenge("request")
            }
            10..=14 => {
                edit_request(&mut credential, |request| match case {
                    10 => request["currency"] = json!(hex_prefixed(&[0x42; 20])),
                    11 => request["recipient"] = json!(hex_prefixed(&[0x43; 20])),
                    12 => request["methodDetails"]["chainId"] = json!(0),
                    13 => request["suggestedDeposit"] = json!("6000000"),
                    _ => request["methodDetails"]["minVoucherDelta"] = json!("101"),
                });
                MppError::InvalidChallenge("request")
            }
            _ => {
                credential.source = Some(format!("did:pkh:eip155:0:{}", hex_prefixed(&[0x42; 20])));
                MppError::VerificationFailed(VerifyFailure::SourceChain)
            }
        };
        resign(&mut credential);
        assert_eq!(
            f.service
                .verify_management_credential(&credential, ROUTE)
                .await
                .err(),
            Some(expected.clone()),
            "management case {case}"
        );
        assert_eq!(
            f.service
                .verify_credential(&credential, ROUTE, &PRICE)
                .await
                .err(),
            Some(expected),
            "inference case {case}"
        );
    }
    for (source, payload) in [
        (Some("invalid source".into()), voucher(INITIAL)),
        (None, json!({"action":"unknown"})),
    ] {
        let mut credential = f.credential(payload).await;
        credential.source = source;
        let management = f
            .service
            .verify_management_credential(&credential, ROUTE)
            .await
            .err();
        assert!(matches!(management, Some(MppError::Malformed(_))));
        assert_eq!(
            management,
            f.service
                .verify_credential(&credential, ROUTE, &PRICE)
                .await
                .err()
        );
    }
    f.untouched();
}
