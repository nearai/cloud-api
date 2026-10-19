use crate::common::*;
use services::auth::{AuthServiceTrait, OAuthManager, OAuthUserInfo, User};
use std::sync::Arc;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const UA: &str = "Mozilla/5.0 LocalLinkTest/1.0";

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
        email_authoritative: true,
        username: "link-test".into(),
        display_name: Some("Original profile".into()),
        avatar_url: None,
    }
}

async fn account(auth: &Arc<dyn AuthServiceTrait>, provider: &str) -> (User, OAuthUserInfo) {
    let info = info(
        provider,
        &format!("link-{}@example.test", uuid::Uuid::new_v4()),
    );
    let user = auth.get_or_create_oauth_user(info.clone()).await.unwrap();
    (user, info)
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
        serde_json::json!({"id":identity.provider_user_id,"email":identity.email,"verified_email":identity.email_verified,"hd":identity.email_authoritative.then_some("example.test"),"name":"Secondary profile"})
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

async fn identity_count(db: &database::Database, user: &User) -> i64 {
    db.pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM user_oauth_identities WHERE user_id=$1",
            &[&user.id.0],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn oauth_non_authoritative_google_email_returns_conflict_without_identity_or_session() {
    for (original, incoming) in [("github", "google"), ("google", "github")] {
        let (server, db, auth, store, provider) = fixture().await;
        let (user, mut original_info) = account(&auth, original).await;
        if original == "google" {
            original_info.email_authoritative = false;
            auth.get_or_create_oauth_user(original_info.clone())
                .await
                .unwrap();
        }
        let mut incoming_info = info(incoming, &original_info.email);
        if incoming == "google" {
            incoming_info.email_authoritative = false;
        }
        mock_identity(&provider, &incoming_info).await;
        let before = counts(&db, &user).await;
        let sessions_before: i64 = db
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM refresh_tokens WHERE user_id=$1",
                &[&user.id.0],
            )
            .await
            .unwrap()
            .get(0);
        let state = uuid::Uuid::new_v4().to_string();
        store
            .create(
                state.clone(),
                incoming.into(),
                Some("local-verifier".into()),
                None,
            )
            .await
            .unwrap();
        let response = server
            .get(&format!("/v1/auth/callback?state={state}&code=local-code"))
            .add_header("User-Agent", UA)
            .await;
        assert_eq!(response.status_code(), 409);
        let body = response.json::<serde_json::Value>();
        assert_eq!(body["error"], "oauth_provider_conflict");
        assert_eq!(body["existing_provider"], original);
        assert!(body.get("access_token").is_none());
        assert!(body.get("refresh_token").is_none());
        assert_eq!(identity_count(&db, &user).await, 1);
        assert!(database::UserRepository::new(db.pool().clone())
            .get_by_provider(incoming, &incoming_info.provider_user_id)
            .await
            .unwrap()
            .is_none());
        let sessions_after: i64 = db
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM refresh_tokens WHERE user_id=$1",
                &[&user.id.0],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(sessions_after, sessions_before);
        assert_eq!(counts(&db, &user).await, before);
    }
}

#[tokio::test]
async fn oauth_verified_email_callback_auto_links_and_signs_into_original_account() {
    for verified in [false, true] {
        for (original, incoming) in [("github", "google"), ("google", "github")] {
            let (server, db, auth, store, provider) = fixture().await;
            let (user, original_info) = account(&auth, original).await;
            let mut incoming_info = info(incoming, &original_info.email);
            incoming_info.email_verified = verified;
            mock_identity(&provider, &incoming_info).await;
            let before = counts(&db, &user).await;
            let state = uuid::Uuid::new_v4().to_string();
            store
                .create(
                    state.clone(),
                    incoming.into(),
                    Some("local-verifier".into()),
                    None,
                )
                .await
                .unwrap();
            let response = server
                .get(&format!("/v1/auth/callback?state={state}&code=local-code"))
                .add_header("User-Agent", UA)
                .await;
            let body = response.json::<serde_json::Value>();
            if verified {
                assert_eq!(response.status_code(), 200);
                let token = body["access_token"].as_str().unwrap();
                let signed_in = auth
                    .validate_session_access(token.into(), test_config().auth.encoding_key)
                    .await
                    .unwrap();
                assert_eq!(signed_in.id, user.id);
            } else {
                assert_eq!(response.status_code(), 409);
                assert_eq!(body["error"], "oauth_provider_conflict");
                assert!(body.get("access_token").is_none());
            }
            assert_eq!(counts(&db, &user).await, before);
            assert_eq!(
                identity_count(&db, &user).await,
                if verified { 2 } else { 1 }
            );
            let stored = database::UserRepository::new(db.pool().clone())
                .get_by_id(user.id.0)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.auth_provider, original);
            assert_eq!(stored.email, user.email);
            assert_eq!(stored.display_name, user.display_name);
        }
    }
}

#[tokio::test]
async fn oauth_google_gmail_callback_auto_links_without_hosted_domain() {
    let (server, db, auth, store, provider) = fixture().await;
    let email = format!("link-{}@gmail.com", uuid::Uuid::new_v4());
    let original = info("github", &email);
    let user = auth.get_or_create_oauth_user(original).await.unwrap();
    let mut incoming = info("google", &email);
    incoming.email_authoritative = false; // The mock omits hd; Gmail itself supplies authority.
    mock_identity(&provider, &incoming).await;
    let state = uuid::Uuid::new_v4().to_string();
    store
        .create(
            state.clone(),
            "google".into(),
            Some("local-verifier".into()),
            None,
        )
        .await
        .unwrap();
    let response = server
        .get(&format!("/v1/auth/callback?state={state}&code=local-code"))
        .add_header("User-Agent", UA)
        .await;
    assert_eq!(response.status_code(), 200);
    let token = response.json::<serde_json::Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        auth.validate_session_access(token, test_config().auth.encoding_key)
            .await
            .unwrap()
            .id,
        user.id
    );
    let row = db.pool().get().await.unwrap().query_one(
        "SELECT email_verified,email_authoritative FROM user_oauth_identities WHERE provider='google' AND provider_user_id=$1",
        &[&incoming.provider_user_id],
    ).await.unwrap();
    assert!(row.get::<_, bool>("email_verified"));
    assert!(row.get::<_, bool>("email_authoritative"));
}
