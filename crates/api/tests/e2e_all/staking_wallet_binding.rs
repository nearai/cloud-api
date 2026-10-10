use super::common::{
    create_org, get_session_id, setup_test_server_with_config_and_database, MOCK_USER_AGENT,
};
use base64::Engine;
use near_api::signer::Signer;
use serde_json::{json, Value};
use services::staking_farm::binding::BindingChallenge;
use uuid::Uuid;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn binding_route_rejects_disabled_feature() {
    let (server, _) = setup_test_server_with_config_and_database(|config| {
        config.staking_farm.enabled = true;
        config.staking_farm.selected_org_binding_enabled = false;
    })
    .await;
    let org = create_org(&server).await;
    let state_path = format!("/v1/organizations/{}/staking/farm", org.id);
    let legacy = server
        .get(&state_path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(legacy.status_code(), 404);
    let envelope = server
        .get(&format!("{state_path}?include_binding_state=true"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(envelope.status_code(), 200);
    assert_eq!(envelope.json::<Value>()["binding_status"], "unbound");
    let response = server
        .post(&format!("/v1/organizations/{}/staking/farm/bind", org.id))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"phase":"prepare","near_account_id":"alice.testnet"}))
        .await;
    assert_eq!(response.status_code(), 409);
    assert_eq!(
        response.json::<Value>()["error"]["type"],
        "staking_binding_unavailable"
    );
}

#[tokio::test]
async fn binding_route_verifies_proof_authorization_retry_and_profile_responses() {
    let rpc = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "jsonrpc":"2.0","id":"dontcare","result":{
            "block_hash":"11111111111111111111111111111111","block_height":1,"nonce":0,"permission":"FullAccess"
        }
    }))).mount(&rpc).await;
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.auth.near.rpc_url = rpc.uri();
        config.auth.near.network_id = "testnet".into();
        config.auth.near.expected_recipient = "cloud.example".into();
        config.staking_farm.enabled = true;
        config.staking_farm.selected_org_binding_enabled = true;
        config.staking_farm.network_id = "testnet".into();
        config.staking_farm.contract_id = "stake.testnet".into();
        config.staking_farm.farm_product_id = "cloud".into();
        config.staking_farm.credit_nano_usd_per_reward_unit = 1_000_000_000;
    })
    .await;
    let org = create_org(&server).await;
    let path = format!("/v1/organizations/{}/staking/farm/bind", org.id);
    let account = format!("w{}.testnet", Uuid::new_v4().simple());
    let prepared = server
        .post(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"phase":"prepare","near_account_id":account}))
        .await;
    assert_eq!(prepared.status_code(), 200, "{}", prepared.text());
    let challenge = prepared.json::<BindingChallenge>();
    assert_eq!(challenge.organization_id.to_string(), org.id);
    let signer = Signer::from_seed_phrase(
        "fatal edge jacket cash hard pass gallery fabric whisper size rain biology",
        None,
    )
    .unwrap();
    let public_key = signer.get_public_key().await.unwrap();
    let signature = signer
        .sign_message_nep413(
            account.parse().unwrap(),
            public_key,
            &challenge.payload.nep413().unwrap(),
        )
        .await
        .unwrap();
    let signature = match signature {
        near_api::types::Signature::ED25519(signature) => {
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
        }
        _ => panic!("test signer must be ED25519"),
    };
    let confirm = json!({"phase":"confirm","challenge_id":challenge.challenge_id,
        "idempotency_key":Uuid::new_v4(),"acknowledged_terms_version":challenge.binding_terms_version,
        "signed_message":{"accountId":account,"publicKey":public_key.to_string(),"signature":signature}});
    let mut wrong_terms = confirm.clone();
    wrong_terms["acknowledged_terms_version"] = json!("different-version");
    let rejected = server
        .post(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&wrong_terms)
        .await;
    assert_eq!(rejected.status_code(), 400);
    assert_eq!(
        rejected.json::<Value>()["error"]["type"],
        "invalid_binding_proof"
    );
    let client = database.pool().get().await.unwrap();
    let replacement_owner = Uuid::new_v4();
    client.execute("INSERT INTO users(id,email,username,auth_provider,provider_user_id) VALUES($1,$2,'replacement-owner','google',$2)", &[&replacement_owner, &format!("{replacement_owner}@example.test")]).await.unwrap();
    client
        .execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES($1,$2,'owner')",
            &[&challenge.organization_id, &replacement_owner],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE organization_members SET role='member' WHERE organization_id=$1 AND user_id=$2",
            &[&challenge.organization_id, &challenge.actor_user_id],
        )
        .await
        .unwrap();
    for body in [
        &confirm,
        &json!({"phase":"prepare","near_account_id":account}),
    ] {
        let rejected = server
            .post(&path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(body)
            .await;
        assert_eq!(rejected.status_code(), 403);
    }
    client
        .execute(
            "UPDATE organization_members SET role='admin' WHERE organization_id=$1 AND user_id=$2",
            &[&challenge.organization_id, &challenge.actor_user_id],
        )
        .await
        .unwrap();
    let committed = server
        .post(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&confirm)
        .await;
    assert_eq!(committed.status_code(), 200, "{}", committed.text());
    let committed = committed.json::<Value>();
    assert_eq!(committed["phase"], "confirm");
    assert_eq!(committed["source"]["near_account_id"], account);
    assert_eq!(committed["wallet_membership"]["role"], "admin");
    assert!(committed.get("access_token").is_none());
    let retry = server
        .post(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&confirm)
        .await;
    assert_eq!(retry.status_code(), 200);
    assert_eq!(retry.json::<Value>(), committed);
    let already_bound = server
        .post(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"phase":"prepare","near_account_id":account}))
        .await;
    assert_eq!(already_bound.status_code(), 409);
    let wallet_user = committed["wallet_membership"]["user_id"].as_str().unwrap();
    let wallet_session = format!("Bearer rt_{wallet_user}");
    let profile = server
        .put("/v1/users/me/profile")
        .add_header("Authorization", wallet_session.clone())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({"display_name":"Updated wallet profile"}))
        .await;
    assert_eq!(profile.status_code(), 200, "{}", profile.text());
    assert_eq!(profile.json::<Value>()["staking_organization_id"], org.id);
    let me = server
        .get("/v1/users/me")
        .add_header("Authorization", wallet_session)
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(me.status_code(), 200);
    let me = me.json::<Value>();
    assert_eq!(me["staking_organization_id"], org.id);
    assert_eq!(me["organizations"][0]["role"], "owner");
    assert_ne!(me["organizations"][0]["id"], org.id);
    assert!(me["organizations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|membership| membership["id"] == org.id && membership["role"] == "admin"));
}
