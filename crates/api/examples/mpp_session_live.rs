// INTEGRATION: standalone harness using the existing services/test-mocks feature.
// Required environment:
// MPP_TEMPO_RPC_URL: Tempo JSON-RPC URL.
// MPP_TEMPO_SIGNER_KEY_FILE: path to the payee's 32-byte hex private key.
// MPP_TEMPO_CHAIN_ID: chain ID as an unsigned decimal integer.
// MPP_TEMPO_CURRENCY: TIP-20 token address as 0x-prefixed hex.
// MPP_MODEL_PRICES: JSON price table with at least one token_price.
// Run: cargo run -p api --features services/test-mocks --example mpp_session_live
// The stores and fake service key are local to this process and disappear on exit.

use api::middleware::{
    body_hash_middleware, mpp_auth_middleware, mpp_settle_middleware, AuthState, BodyHashLimit,
    MppAuthState, MppSettleState,
};
use async_trait::async_trait;
use axum::{extract::Request, middleware::from_fn_with_state, routing::post, Json, Router};
use chrono::{DateTime, Utc};
use config::{MppSessionSettings, MppSettings, MppTempoSettings};
use serde_json::{json, Value};
use services::common::RepositoryError;
use services::completions::usage_observer::{UsageObserverExt, UsageReport};
use services::mpp::{
    channel_memory::InMemoryChannelRepository, HttpTempoRpc, InMemoryClaimRepository, LocalSigner,
    MppService, SessionService, SessionServiceConfig, TempoSigner,
};
use services::organization::{Organization, OrganizationId};
use services::reporting_tokens::{
    CreateOrganizationReportingTokenRequest, CreatedOrganizationReportingToken,
    OrganizationReportingToken, OrganizationReportingTokenService,
    ValidatedOrganizationReportingToken,
};
use services::workspace::{
    ApiKey, ApiKeyId, ApiKeyOrderBy, ApiKeyOrderDirection, ApiKeyRepository, CreateApiKeyRequest,
    Workspace, WorkspaceId, WorkspaceOrderBy, WorkspaceOrderDirection, WorkspaceRepository,
};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

// Fixture credential accepted only by the in-process fake repository.
const SERVICE_KEY: &str = "sk-live-local-harness-service";
const ESCROW: [u8; 20] = [
    0x4d, 0x50, 0x50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn address_env(name: &str) -> [u8; 20] {
    let raw = std::env::var(name).unwrap();
    let parsed = config::mpp::parse_address(name, &raw).unwrap();
    hex::decode(&parsed[2..]).unwrap().try_into().unwrap()
}

async fn completion(request: Request) -> Json<Value> {
    if let Some(observer) = request.extensions().get::<UsageObserverExt>() {
        observer.0.on_finished(UsageReport {
            request_id: Uuid::new_v4(),
            prompt_tokens: 7,
            completion_tokens: 5,
            completed: true,
        });
    }
    Json(json!({
        "id": "chatcmpl-local",
        "object": "chat.completion",
        "created": 0,
        "model": "local",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 5, "total_tokens": 12}
    }))
}

#[tokio::main]
async fn main() {
    let rpc_url = config::mpp::parse_rpc_url(&std::env::var("MPP_TEMPO_RPC_URL").unwrap()).unwrap();
    let signer_key =
        std::fs::read_to_string(std::env::var("MPP_TEMPO_SIGNER_KEY_FILE").unwrap()).unwrap();
    let signer = Arc::new(LocalSigner::from_hex(&signer_key).unwrap());
    let chain_id = std::env::var("MPP_TEMPO_CHAIN_ID")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let currency = address_env("MPP_TEMPO_CURRENCY");
    let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let prices =
        config::mpp::parse_model_prices(&std::env::var("MPP_MODEL_PRICES").unwrap()).unwrap();
    assert!(prices.values().any(|price| price.token_price.is_some()));
    let session_settings = MppSessionSettings {
        signer_key: None,
        min_voucher_delta: None,
        suggested_deposit: config::mpp::DEFAULT_MPP_SESSION_SUGGESTED_DEPOSIT,
        settle_interval_secs: config::mpp::DEFAULT_MPP_SETTLE_INTERVAL_SECS,
        settle_amount: config::mpp::DEFAULT_MPP_SETTLE_AMOUNT,
        close_supervision_secs: config::mpp::DEFAULT_MPP_CLOSE_SUPERVISION_SECS,
    };
    let settings = MppSettings {
        realm: "localhost".into(),
        secret_key: secret.clone(),
        service_api_key: SERVICE_KEY.into(),
        challenge_ttl_secs: 600,
        tempo: MppTempoSettings {
            rpc_url: rpc_url.clone(),
            chain_id,
            currency: format!("0x{}", hex::encode(currency)),
            recipient: format!("0x{}", hex::encode(signer.address())),
            rpc_timeout_secs: 15,
        },
        model_prices: prices.clone(),
        session: Some(session_settings.clone()),
    };
    let rpc = Arc::new(HttpTempoRpc::new(&rpc_url, Duration::from_secs(15)).unwrap());
    let mpp = Arc::new(
        MppService::new(
            &settings,
            Arc::new(InMemoryClaimRepository::default()),
            rpc.clone(),
        )
        .unwrap(),
    );
    let session = Arc::new(
        SessionService::new(
            SessionServiceConfig {
                settings: session_settings,
                model_prices: prices,
                realm: settings.realm.clone(),
                secret_key: secret.as_bytes().to_vec(),
                challenge_ttl: Duration::from_secs(600),
                chain_id,
                escrow: ESCROW,
                currency,
            },
            Arc::new(InMemoryChannelRepository::new()),
            rpc,
            signer,
        )
        .unwrap(),
    );
    let app = Router::new()
        .route("/v1/chat/completions", post(completion))
        .layer(from_fn_with_state(
            MppSettleState {
                mpp: Some(mpp.clone()),
            },
            mpp_settle_middleware,
        ))
        .layer(from_fn_with_state(
            MppAuthState {
                auth: auth_state(),
                mpp: Some(mpp),
                session: Some(session),
            },
            mpp_auth_middleware,
        ))
        .layer(from_fn_with_state(
            BodyHashLimit(1 << 20),
            body_hash_middleware,
        ));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    println!("PORT={}", listener.local_addr().unwrap().port());
    axum::serve(listener, app).await.unwrap();
}

struct Keys;

fn api_key(key: &str) -> ApiKey {
    ApiKey {
        id: ApiKeyId(Uuid::from_u128(5).to_string()),
        key: None,
        key_prefix: key.chars().take(12).collect(),
        name: "k".to_string(),
        workspace_id: WorkspaceId(Uuid::from_u128(8)),
        created_by_user_id: services::auth::UserId(Uuid::nil()),
        created_at: Utc::now(),
        expires_at: None,
        last_used_at: None,
        is_active: true,
        deleted_at: None,
        spend_limit: None,
        usage: None,
    }
}

#[async_trait]
impl ApiKeyRepository for Keys {
    async fn validate(&self, key: String) -> Result<Option<ApiKey>, RepositoryError> {
        Ok((key == SERVICE_KEY).then(|| api_key(&key)))
    }
    async fn create(&self, _: CreateApiKeyRequest) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn get_by_id(&self, _: ApiKeyId) -> Result<Option<ApiKey>, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_workspace_paginated(
        &self,
        _: WorkspaceId,
        _: i64,
        _: i64,
        _: Option<ApiKeyOrderBy>,
        _: Option<ApiKeyOrderDirection>,
    ) -> Result<Vec<ApiKey>, RepositoryError> {
        unimplemented!()
    }
    async fn delete(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn update_last_used(&self, _: ApiKeyId) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn update_spend_limit(
        &self,
        _: ApiKeyId,
        _: Option<i64>,
    ) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn update(
        &self,
        _: ApiKeyId,
        _: Option<String>,
        _: Option<Option<DateTime<Utc>>>,
        _: Option<Option<i64>>,
        _: Option<bool>,
    ) -> Result<ApiKey, RepositoryError> {
        unimplemented!()
    }
    async fn count_by_workspace(&self, _: WorkspaceId) -> Result<i64, RepositoryError> {
        unimplemented!()
    }
    async fn check_name_duplication(
        &self,
        _: WorkspaceId,
        _: &str,
    ) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn revoke(&self, _: ApiKeyId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn get_all_active_key_hashes(&self) -> Result<Vec<String>, RepositoryError> {
        Ok(vec![])
    }
    async fn get_active_key_hashes_created_after(
        &self,
        _: DateTime<Utc>,
    ) -> Result<Vec<String>, RepositoryError> {
        Ok(vec![])
    }
}

struct Workspaces;

#[async_trait]
impl WorkspaceRepository for Workspaces {
    async fn get_by_id(&self, _: WorkspaceId) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_by_name(&self, _: Uuid, _: &str) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn get_workspace_with_organization(
        &self,
        id: WorkspaceId,
    ) -> Result<Option<(Workspace, Organization)>, RepositoryError> {
        let owner = services::auth::UserId(Uuid::nil());
        Ok(Some((
            Workspace {
                id,
                name: "w".to_string(),
                description: None,
                organization_id: OrganizationId(Uuid::from_u128(9)),
                created_by_user_id: owner.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                is_active: true,
                settings: None,
            },
            Organization {
                request_priority: 0,
                id: OrganizationId(Uuid::from_u128(9)),
                name: "o".to_string(),
                description: None,
                owner_id: owner,
                settings: json!({}),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )))
    }
    async fn list_by_organization(
        &self,
        _: OrganizationId,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_organization_paginated(
        &self,
        _: OrganizationId,
        _: i64,
        _: i64,
        _: Option<WorkspaceOrderBy>,
        _: Option<WorkspaceOrderDirection>,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn create(
        &self,
        _: String,
        _: Option<String>,
        _: OrganizationId,
        _: services::auth::UserId,
    ) -> Result<Workspace, RepositoryError> {
        unimplemented!()
    }
    async fn update(
        &self,
        _: WorkspaceId,
        _: Option<String>,
        _: Option<String>,
        _: Option<Value>,
    ) -> Result<Option<Workspace>, RepositoryError> {
        unimplemented!()
    }
    async fn delete(&self, _: WorkspaceId) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
    async fn count_by_organization(&self, _: OrganizationId) -> Result<i64, RepositoryError> {
        unimplemented!()
    }
    async fn list_by_user(
        &self,
        _: services::auth::UserId,
        _: i64,
    ) -> Result<Vec<Workspace>, RepositoryError> {
        unimplemented!()
    }
}

struct NoReportingTokens;

#[async_trait]
impl OrganizationReportingTokenService for NoReportingTokens {
    async fn create(
        &self,
        _: CreateOrganizationReportingTokenRequest,
    ) -> Result<CreatedOrganizationReportingToken, RepositoryError> {
        unimplemented!()
    }
    async fn validate(
        &self,
        _: &str,
    ) -> Result<Option<ValidatedOrganizationReportingToken>, RepositoryError> {
        Ok(None)
    }
    async fn get_by_id(
        &self,
        _: Uuid,
    ) -> Result<Option<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn list_active_by_organization(
        &self,
        _: Uuid,
    ) -> Result<Vec<OrganizationReportingToken>, RepositoryError> {
        unimplemented!()
    }
    async fn revoke(&self, _: Uuid, _: Uuid) -> Result<bool, RepositoryError> {
        unimplemented!()
    }
}

fn auth_state() -> AuthState {
    AuthState::new(
        Arc::new(services::auth::OAuthManager::new(None, None).unwrap()),
        Arc::new(services::auth::MockAuthService {
            apikey_repository: Arc::new(Keys),
        }),
        Arc::new(Workspaces),
        Arc::new(NoReportingTokens),
        Arc::new(database::repositories::AdminAccessTokenRepository::new(
            database::DbPool::uninitialized(),
        )),
        Vec::new(),
        String::new(),
    )
}
