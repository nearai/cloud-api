//! Real PostgreSQL sessions exercise proof privilege and revocation boundaries.
use crate::common::{setup_test_server_with_config_and_database, test_config};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use services::auth::AuthServiceTrait;

const UA: &str = "TraceCommonsAdmissionTests/1.0";

fn proof_config() -> config::AdmissionProofConfig {
    // Synthetic test seeds, never operator credentials.
    config::AdmissionProofConfig {
        issuer: "https://cloud-api.near.ai".into(),
        audiences: vec!["trace-commons".into()],
        subject_key: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        signing_seed: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        retained_public_keys: Vec::new(),
    }
}

fn request() -> Value {
    json!({"audience":"trace-commons", "nonce":URL_SAFE_NO_PAD.encode([3;32]),
        "device_public_key":URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[4;32]).verifying_key().to_bytes())})
}

#[tokio::test]
async fn valid_session_issues_verifiable_proof_and_revocation_stops_issuance() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.auth.mock = false;
        config.auth.require_session_bound_access_tokens = false;
        config.auth.admission_proof = Some(proof_config());
    })
    .await;
    let id = uuid::Uuid::new_v4();
    let users = database::repositories::UserRepository::new(database.pool().clone());
    let user = users
        .create_from_oauth(
            format!("admission-{id}@test.invalid"),
            format!("admission-{id}"),
            None,
            None,
            "near".into(),
            format!("admission-{id}.near"),
        )
        .await
        .unwrap();
    let sessions = database::repositories::SessionRepository::new(database.pool().clone());
    let (_, refresh) = sessions.create(user.id, None, UA.into(), 1).await.unwrap();
    let tokens = server
        .post("/v1/users/me/access-tokens")
        .add_header("Authorization", format!("Bearer {refresh}"))
        .add_header("User-Agent", UA)
        .await;
    tokens.assert_status_ok();
    let tokens = tokens.json::<api::models::AccessAndRefreshTokenResponse>();
    let minter = services::auth::MockAuthService {
        apikey_repository: std::sync::Arc::new(database::repositories::ApiKeyRepository::new(
            database.pool().clone(),
        )),
    };
    // Only the synthetic signing fixture uses the mock; the HTTP server and
    // session validation above use the real AuthService and PostgreSQL.
    let legacy = minter
        .create_session_access_token(
            services::auth::UserId(user.id),
            None,
            test_config().auth.encoding_key,
            1,
        )
        .unwrap();

    let keys = server.get("/v1/auth/admission-proof/jwks").await;
    keys.assert_status_ok();
    let jwks = keys.json::<Value>();
    assert!(jwks["keys"][0].get("d").is_none());
    let response = server
        .post("/v1/auth/admission-proof")
        .add_header("Authorization", format!("Bearer {}", tokens.access_token))
        .json(&request())
        .await;
    response.assert_status_ok();
    assert_eq!(response.header("cache-control"), "no-store");
    let response = response.json::<Value>();
    let assertion = response["assertion"].as_str().unwrap();
    let parts = assertion.split('.').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "EdDSA");
    assert_eq!(header["typ"], "tc-admission+jwt");
    assert_eq!(header["kid"], jwks["keys"][0]["kid"]);
    let key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(jwks["keys"][0]["x"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
    VerifyingKey::from_bytes(&key)
        .unwrap()
        .verify_strict(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], "trace-commons");
    assert_eq!(claims["auth_provider"], "near");
    assert_eq!(claims["nonce"], request()["nonce"]);
    assert_eq!(claims["device_public_key"], request()["device_public_key"]);
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        120
    );
    assert!(!claims.to_string().contains(&user.id.to_string()));
    assert!(!claims.to_string().contains(&user.provider_user_id));

    for forbidden in [
        &tokens.refresh_token,
        legacy.as_str(),
        assertion,
        "sk-not-a-session",
        "invalid",
    ] {
        server
            .post("/v1/auth/admission-proof")
            .add_header("Authorization", format!("Bearer {forbidden}"))
            .json(&request())
            .await
            .assert_status_unauthorized();
    }
    server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {assertion}"))
        .await
        .assert_status_unauthorized();

    let mut bad = request();
    bad["audience"] = json!("another-service");
    server
        .post("/v1/auth/admission-proof")
        .add_header("Authorization", format!("Bearer {}", tokens.access_token))
        .json(&bad)
        .await
        .assert_status_bad_request();
    bad["nonce"] = json!("x".repeat(4096));
    server
        .post("/v1/auth/admission-proof")
        .add_header("Authorization", format!("Bearer {}", tokens.access_token))
        .json(&bad)
        .await
        .assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);

    server
        .post("/v1/auth/logout")
        .add_header("Authorization", format!("Bearer {}", tokens.refresh_token))
        .add_header("User-Agent", UA)
        .await
        .assert_status_ok();
    server
        .post("/v1/auth/admission-proof")
        .add_header("Authorization", format!("Bearer {}", tokens.access_token))
        .json(&request())
        .await
        .assert_status_unauthorized();
    // Ordinary API compatibility stays enabled; admission enforces its own
    // live-session requirement even after this user's session is logged out.
    server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {legacy}"))
        .await
        .assert_status_ok();
    server
        .post("/v1/auth/admission-proof")
        .add_header("Authorization", format!("Bearer {legacy}"))
        .json(&request())
        .await
        .assert_status_unauthorized();
    database
        .pool()
        .get()
        .await
        .unwrap()
        .execute("DELETE FROM users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
}

#[tokio::test]
async fn unconfigured_issuer_remains_unavailable() {
    let (server, _) = setup_test_server_with_config_and_database(|config| {
        config.auth.admission_proof = None;
    })
    .await;
    server
        .get("/v1/auth/admission-proof/jwks")
        .await
        .assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    server
        .post("/v1/auth/admission-proof")
        .json(&request())
        .await
        .assert_status_unauthorized();
    let spec = serde_json::to_value(<api::openapi::ApiDoc as utoipa::OpenApi>::openapi()).unwrap();
    assert_eq!(
        spec["paths"]["/v1/auth/admission-proof"]["post"]["security"],
        json!([{"session_token":[]} ])
    );
    assert!(test_config().auth.admission_proof.is_none());
}
