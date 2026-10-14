use crate::common::*;
use services::auth::{AuthError, AuthServiceTrait, OAuthManager, OAuthUserInfo, Session, User};
use std::sync::Arc;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const UA: &str = "Mozilla/5.0 LocalLinkTest/1.0";
const CALLBACK: &str = "https://cloud.near.ai/auth/link/callback";

fn info(provider: &str, email: &str) -> OAuthUserInfo {
    let id = if provider == "github" {
        (uuid::Uuid::new_v4().as_u128() as u64 & ((1 << 52) - 1)).to_string()
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    OAuthUserInfo {
        provider: provider.into(),
        provider_user_id: id,
        email: email.into(),
        email_verified: true,
        username: "link-test".into(),
        display_name: Some("Original profile".into()),
        avatar_url: None,
    }
}

async fn account(
    auth: &Arc<dyn AuthServiceTrait>,
    provider: &str,
) -> (User, OAuthUserInfo, String, Session) {
    let info = info(
        provider,
        &format!("link-{}@example.test", uuid::Uuid::new_v4()),
    );
    let user = auth.get_or_create_oauth_user(info.clone()).await.unwrap();
    let (token, session, _) = auth
        .create_session(
            user.id.clone(),
            None,
            UA.into(),
            test_config().auth.encoding_key,
            1,
            24,
        )
        .await
        .unwrap();
    (user, info, token, session)
}

async fn fixture() -> (
    axum_test::TestServer,
    Arc<database::Database>,
    Arc<dyn AuthServiceTrait>,
    Arc<database::OAuthStateRepository>,
    MockServer,
) {
    let (_, db) = setup_test_server_with_database().await;
    let provider = MockServer::start().await;
    let mut config = test_config();
    config.auth.mock = false;
    let oauth_config = config::OAuthProviderConfig {
        client_id: "local-test".into(),
        client_secret: "local-secret".into(),
        redirect_uri: "https://cloud-api.near.ai/v1/auth/callback".into(),
    };
    config.auth.github = Some(config::GitHubOAuthConfig {
        client_id: oauth_config.client_id.clone(),
        client_secret: oauth_config.client_secret.clone(),
        redirect_url: oauth_config.redirect_uri.clone(),
    });
    config.auth.google = Some(config::GoogleOAuthConfig {
        client_id: oauth_config.client_id.clone(),
        client_secret: oauth_config.client_secret.clone(),
        redirect_url: oauth_config.redirect_uri.clone(),
    });
    let mut components = api::init_auth_services(db.clone(), &config);
    components.oauth_manager = Arc::new(
        OAuthManager::new(Some(oauth_config.clone()), Some(oauth_config))
            .unwrap()
            .with_test_provider_endpoints("github", &provider.uri())
            .with_test_provider_endpoints("google", &provider.uri()),
    );
    components.auth_state_middleware.oauth_manager = components.oauth_manager.clone();
    let auth = components.auth_service.clone();
    let store = components.state_store.clone();
    let router = axum::Router::new().nest(
        "/v1/auth",
        api::build_auth_routes(
            Arc::new(components.clone()),
            &components.auth_state_middleware,
            Arc::new(config),
        ),
    );
    (
        axum_test::TestServer::new(router),
        db,
        auth,
        store,
        provider,
    )
}

async fn mock_identity(server: &MockServer, identity: &OAuthUserInfo) {
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"local-provider-token","token_type":"Bearer","expires_in":3600}))).mount(server).await;
    let user = if identity.provider == "github" {
        serde_json::json!({"id":identity.provider_user_id.parse::<i64>().unwrap(),"login":"linked-user","email":identity.email,"name":"Secondary profile"})
    } else {
        serde_json::json!({"id":identity.provider_user_id,"email":identity.email,"verified_email":identity.email_verified,"name":"Secondary profile"})
    };
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(user))
        .mount(server)
        .await;
    Mock::given(method("GET")).and(path("/emails")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{"email":identity.email,"primary":true,"verified":identity.email_verified}]))).mount(server).await;
}

async fn counts(db: &database::Database, user: &User) -> Vec<i64> {
    let client = db.pool().get().await.unwrap();
    let row=client.query_one("SELECT (SELECT count(*) FROM users WHERE id=$1), (SELECT count(*) FROM organization_members WHERE user_id=$1), (SELECT count(*) FROM workspaces WHERE created_by_user_id=$1)",&[&user.id.0]).await.unwrap();
    (0..3).map(|index| row.get(index)).collect()
}

#[tokio::test]
async fn explicit_link_callback_confirms_original_session_and_both_providers_sign_in() {
    for (original, incoming) in [("github", "google"), ("google", "github")] {
        let (server, db, auth, store, provider) = fixture().await;
        let (user, original_info, token, session) = account(&auth, original).await;
        let incoming_info = info(incoming, &original_info.email);
        mock_identity(&provider, &incoming_info).await;
        let before = counts(&db, &user).await;
        let response = server
            .post("/v1/auth/identities/link")
            .add_header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"provider":incoming,"frontend_callback":CALLBACK}))
            .await;
        assert_eq!(response.status_code(), 200);
        let url = url::Url::parse(
            response.json::<serde_json::Value>()["auth_url"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        if incoming == "google" {
            assert_eq!(query["prompt"], "select_account");
        }
        let state = &query["state"];
        let response = server
            .get(&format!("/v1/auth/callback?state={state}&code=local-code"))
            .add_header("User-Agent", UA)
            .await;
        assert_eq!(response.status_code(), 307);
        assert!(!response.headers().contains_key("set-cookie"));
        let location = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        let fragment: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(location.fragment().unwrap().as_bytes())
                .into_owned()
                .collect();
        assert_eq!(fragment.len(), 1);
        let code = &fragment["link_code"];
        assert_eq!(code.len(), 64);
        let requests = provider.received_requests().await.unwrap();
        let exchange = requests
            .iter()
            .find(|request| request.url.path() == "/token")
            .unwrap();
        let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&exchange.body)
            .into_owned()
            .collect();
        assert!(!form["code_verifier"].is_empty());
        assert!(store.get_and_delete(state).await.unwrap().is_none());
        let users = database::UserRepository::new(db.pool().clone());
        assert!(
            users
                .get_by_provider(incoming, &incoming_info.provider_user_id)
                .await
                .unwrap()
                .is_none(),
            "callback must not bind before browser confirms"
        );
        let response = server
            .post("/v1/auth/identities/complete")
            .add_header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"code":code}))
            .await;
        assert_eq!(response.status_code(), 200);
        assert!(!response.headers().contains_key("set-cookie"));
        assert_eq!(counts(&db, &user).await, before);
        let stored = users.get_by_id(user.id.0).await.unwrap().unwrap();
        assert_eq!(stored.email, user.email);
        assert_eq!(stored.display_name, user.display_name);
        assert_eq!(stored.auth_provider, original);
        assert_eq!(
            auth.get_or_create_oauth_user(incoming_info.clone())
                .await
                .unwrap()
                .id,
            user.id
        );
        assert_eq!(
            auth.get_or_create_oauth_user(original_info)
                .await
                .unwrap()
                .id,
            user.id
        );
        assert_eq!(
            auth.list_oauth_identities(user.id.clone())
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(matches!(
            auth.complete_oauth_link(user.id.clone(), session.id, code)
                .await,
            Err(AuthError::OAuthLinkInvalid)
        ));
        // Exercise normal provider callback after binding, including JWT/session issuance.
        let state = uuid::Uuid::new_v4().to_string();
        store
            .create(
                state.clone(),
                incoming.into(),
                Some("normal-test-verifier".into()),
                None,
            )
            .await
            .unwrap();
        let response = server
            .get(&format!("/v1/auth/callback?state={state}&code=normal-code"))
            .add_header("User-Agent", UA)
            .await;
        assert_eq!(response.status_code(), 200);
        let access = response.json::<serde_json::Value>()["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            auth.validate_session_access(access, test_config().auth.encoding_key)
                .await
                .unwrap()
                .id,
            user.id
        );
        assert_eq!(counts(&db, &user).await, before);
    }
}

#[tokio::test]
async fn link_handoff_rejects_other_accounts_other_sessions_expiration_and_logout() {
    let (_, db, auth, _, _) = fixture().await;
    let (user, original, _, session) = account(&auth, "github").await;
    let (other, _, _, other_session) = account(&auth, "github").await;
    let incoming = info("google", &original.email);
    let code = auth
        .prepare_oauth_link(user.id.clone(), session.id.clone(), incoming.clone())
        .await
        .unwrap();
    assert!(matches!(
        auth.complete_oauth_link(other.id, other_session.id, &code)
            .await,
        Err(AuthError::OAuthLinkInvalid)
    ));
    let (_, new_session, _) = auth
        .create_session(
            user.id.clone(),
            None,
            UA.into(),
            test_config().auth.encoding_key,
            1,
            24,
        )
        .await
        .unwrap();
    assert!(matches!(
        auth.complete_oauth_link(user.id.clone(), new_session.id, &code)
            .await,
        Err(AuthError::OAuthLinkInvalid)
    ));
    // A failed confirmation by another account must not consume the owner's code.
    auth.complete_oauth_link(user.id.clone(), session.id.clone(), &code)
        .await
        .unwrap();
    let client = db.pool().get().await.unwrap();
    let code = auth
        .prepare_oauth_link(user.id.clone(), session.id.clone(), incoming.clone())
        .await
        .unwrap();
    client
        .execute(
            "UPDATE oauth_link_requests SET expires_at=NOW()-INTERVAL '1 minute' WHERE user_id=$1",
            &[&user.id.0],
        )
        .await
        .unwrap();
    assert!(matches!(
        auth.complete_oauth_link(user.id.clone(), session.id.clone(), &code)
            .await,
        Err(AuthError::OAuthLinkInvalid)
    ));
    let code = auth
        .prepare_oauth_link(user.id.clone(), session.id.clone(), incoming)
        .await
        .unwrap();
    auth.logout(session.id.clone()).await.unwrap();
    assert!(matches!(
        auth.complete_oauth_link(user.id, session.id, &code).await,
        Err(AuthError::OAuthLinkInvalid)
    ));
}

#[tokio::test]
async fn linking_never_merges_accounts_or_overwrites_an_existing_provider() {
    let (_, db, auth, _, _) = fixture().await;
    let (user, original, _, session) = account(&auth, "github").await;
    let (other, other_info, _, _) = account(&auth, "google").await;
    let before = counts(&db, &other).await;
    let code = auth
        .prepare_oauth_link(user.id.clone(), session.id.clone(), other_info.clone())
        .await
        .unwrap();
    assert!(matches!(
        auth.complete_oauth_link(user.id.clone(), session.id.clone(), &code)
            .await,
        Err(AuthError::OAuthLinkConflict)
    ));
    assert_eq!(counts(&db, &other).await, before);
    assert_eq!(
        auth.get_or_create_oauth_user(other_info).await.unwrap().id,
        other.id
    );
    let code = auth
        .prepare_oauth_link(
            user.id.clone(),
            session.id.clone(),
            info("github", &original.email),
        )
        .await
        .unwrap();
    assert!(matches!(
        auth.complete_oauth_link(user.id.clone(), session.id.clone(), &code)
            .await,
        Err(AuthError::OAuthLinkConflict)
    ));
    let mut unverified = info("google", &original.email);
    unverified.email_verified = false;
    assert!(matches!(
        auth.prepare_oauth_link(user.id.clone(), session.id, unverified)
            .await,
        Err(AuthError::OAuthEmailUnverified)
    ));
    assert_eq!(auth.list_oauth_identities(user.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn linked_provider_login_does_not_change_primary_email_or_profile() {
    let (_, db, auth, _, _) = fixture().await;
    let (user, _, _, session) = account(&auth, "github").await;
    let incoming = info(
        "google",
        &format!("different-{}@example.test", uuid::Uuid::new_v4()),
    );
    let code = auth
        .prepare_oauth_link(user.id.clone(), session.id.clone(), incoming.clone())
        .await
        .unwrap();
    auth.complete_oauth_link(user.id.clone(), session.id, &code)
        .await
        .unwrap();
    let mut changed = incoming;
    changed.email = "changed@example.test".into();
    changed.display_name = Some("New secondary profile".into());
    assert_eq!(
        auth.get_or_create_oauth_user(changed).await.unwrap().id,
        user.id
    );
    let stored = database::UserRepository::new(db.pool().clone())
        .get_by_id(user.id.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.email, user.email);
    assert_eq!(stored.display_name, user.display_name);
}

#[tokio::test]
async fn concurrent_link_requests_cannot_assign_one_identity_to_two_accounts() {
    let (_, db, auth, _, _) = fixture().await;
    let (first, _, _, first_session) = account(&auth, "github").await;
    let (second, _, _, second_session) = account(&auth, "github").await;
    let incoming = info(
        "google",
        &format!("shared-link-{}@example.test", uuid::Uuid::new_v4()),
    );
    let first_code = auth
        .prepare_oauth_link(first.id.clone(), first_session.id.clone(), incoming.clone())
        .await
        .unwrap();
    let second_code = auth
        .prepare_oauth_link(
            second.id.clone(),
            second_session.id.clone(),
            incoming.clone(),
        )
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        auth.complete_oauth_link(first.id.clone(), first_session.id, &first_code),
        auth.complete_oauth_link(second.id.clone(), second_session.id, &second_code)
    );
    let winner = match (a, b) {
        (Ok(_), Err(AuthError::OAuthLinkConflict)) => first.id,
        (Err(AuthError::OAuthLinkConflict), Ok(_)) => second.id,
        results => panic!("expected one bind and one conflict: {results:?}"),
    };
    assert_eq!(
        database::UserRepository::new(db.pool().clone())
            .get_by_provider("google", &incoming.provider_user_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        winner.0
    );
}

#[tokio::test]
async fn oauth_link_initiation_and_cancel_validate_session_provider_and_callback() {
    let (server, _, auth, store, _) = fixture().await;
    let (user, _, token, _) = account(&auth, "github").await;
    for payload in [
        serde_json::json!({"provider":"near","frontend_callback":CALLBACK}),
        serde_json::json!({"provider":"google","frontend_callback":"https://evil.example/auth/link/callback"}),
        serde_json::json!({"provider":"github","frontend_callback":CALLBACK}),
        serde_json::json!({"provider":"google","frontend_callback":"https://cloud.near.ai/other"}),
    ] {
        let response = server
            .post("/v1/auth/identities/link")
            .add_header("Authorization", format!("Bearer {token}"))
            .json(&payload)
            .await;
        assert!(response.status_code().is_client_error());
    }
    let response = server
        .post("/v1/auth/identities/link")
        .json(&serde_json::json!({"provider":"google","frontend_callback":CALLBACK}))
        .await;
    assert_eq!(response.status_code(), 401);
    let legacy = auth
        .create_session_access_token(user.id, None, test_config().auth.encoding_key, 1)
        .unwrap();
    let response = server
        .post("/v1/auth/identities/link")
        .add_header("Authorization", format!("Bearer {legacy}"))
        .json(&serde_json::json!({"provider":"google","frontend_callback":CALLBACK}))
        .await;
    assert_eq!(response.status_code(), 401);
    let response = server
        .post("/v1/auth/identities/link")
        .add_header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"provider":"google","frontend_callback":CALLBACK}))
        .await;
    let url = url::Url::parse(
        response.json::<serde_json::Value>()["auth_url"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .to_string();
    let response = server
        .get(&format!(
            "/v1/auth/callback?state={state}&error=access_denied"
        ))
        .add_header("User-Agent", UA)
        .await;
    assert_eq!(response.status_code(), 307);
    assert!(response.headers()["location"]
        .to_str()
        .unwrap()
        .ends_with("#link_error=oauth_link_canceled"));
    assert!(store.get_and_delete(&state).await.unwrap().is_none());
}

#[tokio::test]
async fn provider_callback_rejects_unverified_email_and_revoked_original_session() {
    for incoming in ["google", "github"] {
        let (server, db, auth, store, provider) = fixture().await;
        let original = if incoming == "google" {
            "github"
        } else {
            "google"
        };
        let (user, original_info, token, session) = account(&auth, original).await;
        let mut incoming_info = info(incoming, &original_info.email);
        incoming_info.email_verified = false;
        mock_identity(&provider, &incoming_info).await;
        let response = server
            .post("/v1/auth/identities/link")
            .add_header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"provider":incoming,"frontend_callback":CALLBACK}))
            .await;
        let url = url::Url::parse(
            response.json::<serde_json::Value>()["auth_url"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .to_string();
        let response = server
            .get(&format!("/v1/auth/callback?state={state}&code=test-code"))
            .add_header("User-Agent", UA)
            .await;
        assert_eq!(response.status_code(), 307);
        assert!(response.headers()["location"]
            .to_str()
            .unwrap()
            .ends_with("#link_error=oauth_email_unverified"));
        assert_eq!(
            auth.list_oauth_identities(user.id.clone())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store.get_and_delete(&state).await.unwrap().is_none());

        incoming_info.email_verified = true;
        let code = auth
            .prepare_oauth_link(user.id.clone(), session.id.clone(), incoming_info)
            .await
            .unwrap();
        db.pool()
            .get()
            .await
            .unwrap()
            .execute(
                "UPDATE users SET tokens_revoked_at=NOW() WHERE id=$1",
                &[&user.id.0],
            )
            .await
            .unwrap();
        assert!(matches!(
            auth.complete_oauth_link(user.id.clone(), session.id, &code)
                .await,
            Err(AuthError::OAuthLinkInvalid)
        ));
        assert_eq!(auth.list_oauth_identities(user.id).await.unwrap().len(), 1);
    }
}
