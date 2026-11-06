//! Direct database seeding for e2e setup.
//!
//! Setup state (organizations, workspaces, API keys, credits, platform services and
//! catalog models) is written to the shared test database with plain SQL, so tests that
//! are not about those management routes do not depend on them. The SQL mirrors what the
//! management handlers wrote: same columns, same defaults, same ledger rows.
//!
//! Seeding is not the route. It skips request validation (a malformed setup call panics
//! instead of returning 400), admin authentication and the admin identity (history rows
//! are attributed to the session's user with the mock admin email), the side
//! effects of the handlers beyond the row writes. It keeps two: `seed_models` registers
//! and unregisters the providers of the models it writes, in the harness's provider pools
//! (the route's pool changes, without the pinned-role refresh or GitHub dispatch), and
//! cache invalidation: `seed_models` clears the models caches and `seed_allowed_models` the
//! organization's filter, on every models service built by the harness in this process,
//! so a read after a seed sees the seeded rows. A test whose subject is a route, its
//! validation, its pool effects or its audit trail must call the route (for the catalog,
//! `admin_batch_upsert_models`), not seed.
//!
//! Under nextest every test runs in its own process, so the registries above hold one
//! server and seeding touches only that one. Under plain `cargo test` the tests share a
//! process and seeding touches every server built so far, including other tests' servers.
//!
//! The pool is created once per process on first use. Seeded users must already exist in
//! `users` (see `setup_unique_test_session`); the mock auth resolves `rt_<uuid>` to that
//! user and anything else to `MOCK_USER_ID`.

use super::db_setup::{self, MOCK_USER_ID};
use super::usage_views;
use api::models::{
    AdminModelWithPricing, ApiKeyResponse, BatchUpdateModelApiRequest, Datacenter, DecimalPrice,
    ModelArchitecture, ModelMetadata, ModelWithPricing, OrganizationResponse, ServiceResponse,
    SpendLimit, UpdateOrganizationLimitsResponse,
};
use api::routes::workspaces::WorkspaceResponse;
use chrono::{DateTime, Utc};
use database::models::{AdminAccessTokenPermission, UpdateModelPricingRequest};
use database::repositories::admin_access_token::AdminAccessTokenRepository;
use database::repositories::credit_allocation::settle_unfunded_usage;
use database::repositories::model::ModelRepository;
use database::repositories::model_alias::ModelAliasRepository;
use database::repositories::CreditAllocationPolicy;
use database::Database;
use services::inference_provider_pool::InferenceProviderPool;
use services::models::{ModelsServiceImpl, ModelsServiceTrait};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::OnceCell;
use uuid::Uuid;

static SEED_DB: OnceCell<Arc<Database>> = OnceCell::const_new();

/// Models services of the servers built by the harness in this process. Their caches sit
/// in front of the rows seeding writes, and a `TestServer` does not expose its service.
static MODELS_SERVICES: Mutex<Vec<Weak<ModelsServiceImpl>>> = Mutex::new(Vec::new());

/// Inference provider pools of the servers built by the harness in this process, so
/// seeding can register the providers of seeded models the way the admin route did.
static PROVIDER_POOLS: Mutex<Vec<Weak<InferenceProviderPool>>> = Mutex::new(Vec::new());

/// Record a server's models service and provider pool so seeding can invalidate its caches
/// and register providers. Called by the harness for every app it builds.
pub fn register_models_service(
    models_service: &Arc<ModelsServiceImpl>,
    provider_pool: &Arc<InferenceProviderPool>,
) {
    let mut registered = MODELS_SERVICES.lock().expect("models service registry");
    registered.retain(|service| service.strong_count() > 0);
    registered.push(Arc::downgrade(models_service));
    let mut pools = PROVIDER_POOLS.lock().expect("provider pool registry");
    pools.retain(|pool| pool.strong_count() > 0);
    pools.push(Arc::downgrade(provider_pool));
}

fn registered_provider_pools() -> Vec<Arc<InferenceProviderPool>> {
    PROVIDER_POOLS
        .lock()
        .expect("provider pool registry")
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

fn registered_models_services() -> Vec<Arc<ModelsServiceImpl>> {
    MODELS_SERVICES
        .lock()
        .expect("models service registry")
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

/// Shared database handle for seeding. Created lazily, once per process.
pub async fn seed_database() -> Arc<Database> {
    SEED_DB
        .get_or_init(|| async { Arc::new(Database::new(db_setup::create_test_pool().await)) })
        .await
        .clone()
}

/// The user a mock session token resolves to.
pub fn user_id_for_session(session_id: &str) -> Uuid {
    session_id
        .strip_prefix("rt_")
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| Uuid::parse_str(MOCK_USER_ID).expect("fixed mock user ID must be valid"))
}

/// The email the mock auth reports for every session user.
const MOCK_ADMIN_EMAIL: &str = "admin@test.com";

/// Create an organization owned by the session's user, with the default workspace.
pub async fn seed_organization(session_id: &str) -> OrganizationResponse {
    let database = seed_database().await;
    let owner_id = user_id_for_session(session_id);
    let org_id = Uuid::new_v4();
    let name = Uuid::new_v4().to_string();
    let description = "A test organization".to_string();

    let mut client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let transaction = client.transaction().await.expect("begin transaction");

    let row = transaction
        .query_one(
            "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
             VALUES ($1, $2, $3, NOW(), NOW(), true)
             RETURNING created_at, updated_at, settings, is_active",
            &[&org_id, &name, &Some(description.clone())],
        )
        .await
        .expect("Failed to seed organization");
    let created_at: DateTime<Utc> = row.get("created_at");
    let updated_at: DateTime<Utc> = row.get("updated_at");
    let settings: Option<serde_json::Value> = row.get("settings");
    let is_active: bool = row.get("is_active");

    transaction
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role, joined_at)
             VALUES ($1, $2, 'owner', NOW())",
            &[&org_id, &owner_id],
        )
        .await
        .expect("Failed to seed organization owner");

    transaction
        .execute(
            "INSERT INTO workspaces (
                 id, name, description, organization_id,
                 created_by_user_id, created_at, updated_at, is_active
             ) VALUES ($1, 'default', $2, $3, $4, NOW(), NOW(), true)",
            &[
                &Uuid::new_v4(),
                &Some(format!("Default workspace for {name}")),
                &org_id,
                &owner_id,
            ],
        )
        .await
        .expect("Failed to seed default workspace");

    transaction
        .commit()
        .await
        .expect("commit seeded organization");

    OrganizationResponse {
        id: org_id.to_string(),
        name,
        description: Some(description),
        owner_id: owner_id.to_string(),
        settings: settings.unwrap_or_default(),
        role: api::models::MemberRole::Owner,
        is_active,
        created_at,
        updated_at,
    }
}

/// Replace the active limit of one credit type, as the admin limits update did:
/// close the active row, insert the new one, then settle any unfunded usage.
#[allow(clippy::too_many_arguments)]
pub async fn seed_credit_limit(
    session_id: &str,
    org_id: &str,
    credit_type: &str,
    source: Option<&str>,
    amount_nano_dollars: i64,
    currency: &str,
    changed_by: &str,
    change_reason: &str,
) -> UpdateOrganizationLimitsResponse {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let changed_by_user_id = user_id_for_session(session_id);
    let currency = currency.to_uppercase();
    let source = source.map(str::to_string);

    let mut client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let transaction = client.transaction().await.expect("begin transaction");

    let org = transaction
        .query_opt(
            "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true FOR UPDATE",
            &[&organization_id],
        )
        .await
        .expect("lock organization");
    assert!(org.is_some(), "Organization not found: {organization_id}");

    let now = Utc::now();
    transaction
        .execute(
            "UPDATE organization_limits_history
             SET effective_until = $1
             WHERE organization_id = $2 AND credit_type = $3 AND effective_until IS NULL",
            &[&now, &organization_id, &credit_type],
        )
        .await
        .expect("close previous limit");

    let row = transaction
        .query_one(
            "INSERT INTO organization_limits_history (
                 organization_id, spend_limit, credit_type, source, currency,
                 effective_from, changed_by, change_reason,
                 changed_by_user_id, changed_by_user_email
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING effective_from",
            &[
                &organization_id,
                &amount_nano_dollars,
                &credit_type,
                &source,
                &currency,
                &now,
                &Some(changed_by.to_string()),
                &Some(change_reason.to_string()),
                &Some(changed_by_user_id),
                &Some(MOCK_ADMIN_EMAIL.to_string()),
            ],
        )
        .await
        .expect("Failed to seed credit limit");
    let effective_from: DateTime<Utc> = row.get("effective_from");

    settle_unfunded_usage(
        &transaction,
        organization_id,
        &CreditAllocationPolicy::default(),
    )
    .await
    .expect("settle unfunded usage");

    transaction.commit().await.expect("commit credit limit");

    UpdateOrganizationLimitsResponse {
        organization_id: organization_id.to_string(),
        credit_type: credit_type
            .parse()
            .expect("credit type must be a known credit type"),
        source,
        spend_limit: SpendLimit {
            amount: amount_nano_dollars,
            scale: 9,
            currency,
        },
        updated_at: effective_from.to_rfc3339(),
    }
}

/// Active workspaces of an organization, oldest first.
pub async fn seed_list_workspaces(org_id: &str) -> Vec<WorkspaceResponse> {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let rows = client
        .query(
            "SELECT id, name, description, organization_id, created_by_user_id,
                    created_at, updated_at, is_active, settings
             FROM workspaces
             WHERE organization_id = $1 AND is_active = true
             ORDER BY created_at ASC",
            &[&organization_id],
        )
        .await
        .expect("Failed to list workspaces");
    rows.iter()
        .map(|row| WorkspaceResponse {
            id: row.get::<_, Uuid>("id").to_string(),
            name: row.get("name"),
            description: row.get("description"),
            organization_id: row.get::<_, Uuid>("organization_id").to_string(),
            created_by_user_id: row.get::<_, Uuid>("created_by_user_id").to_string(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            is_active: row.get("is_active"),
            settings: row.get("settings"),
        })
        .collect()
}

/// Create an API key in a workspace, hashed and prefixed by `ApiKeyRepository`.
pub async fn seed_api_key(workspace_id: &str, name: String, session_id: &str) -> ApiKeyResponse {
    let database = seed_database().await;
    let created_by = user_id_for_session(session_id);
    let (key, api_key) = database
        .api_keys
        .create(services::workspace::CreateApiKeyRequest {
            name,
            workspace_id: services::workspace::WorkspaceId(
                Uuid::parse_str(workspace_id).expect("workspace id must be a UUID"),
            ),
            created_by_user_id: services::auth::ports::UserId(created_by),
            expires_at: Some(Utc::now() + chrono::Duration::days(90)),
            spend_limit: None,
        })
        .await
        .expect("Failed to seed API key");

    let formatted_prefix = if api_key.key_prefix.len() > 6 {
        format!("{}****", &api_key.key_prefix[..6])
    } else {
        format!("{}****", api_key.key_prefix)
    };
    let price = |amount| DecimalPrice {
        amount,
        scale: 9,
        currency: "USD".to_string(),
    };
    ApiKeyResponse {
        id: api_key.id.to_string(),
        name: Some(api_key.name.clone()),
        key: Some(key),
        key_prefix: formatted_prefix,
        workspace_id: api_key.workspace_id.to_string(),
        created_by_user_id: api_key.created_by_user_id.to_string(),
        created_at: api_key.created_at,
        last_used_at: api_key.last_used_at,
        expires_at: api_key.expires_at,
        spend_limit: api_key.spend_limit.map(price),
        is_active: api_key.is_active,
        deleted_at: api_key.deleted_at,
        usage: Some(price(api_key.usage)),
    }
}

fn usd_price(amount: i64) -> DecimalPrice {
    DecimalPrice {
        amount,
        scale: 9,
        currency: "USD".to_string(),
    }
}

/// Whole-hour UTC instant, or a bare date that means 13:00 UTC on that day.
fn parse_deprecation_date(value: &str) -> Option<DateTime<Utc>> {
    use chrono::Timelike;
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        let utc = parsed.with_timezone(&Utc);
        if parsed.offset().local_minus_utc() != 0
            || utc.minute() != 0
            || utc.second() != 0
            || utc.nanosecond() != 0
        {
            return None;
        }
        return Some(utc);
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(13, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Merge a providerConfig write into the stored config: keys sent replace stored keys and
/// `null` removes a key. On a new model the write is the config.
fn merge_provider_config(
    stored: Option<serde_json::Value>,
    patch: serde_json::Value,
) -> serde_json::Value {
    match (stored, patch) {
        (Some(serde_json::Value::Object(mut merged)), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    merged.remove(&key);
                } else {
                    merged.insert(key, value);
                }
            }
            serde_json::Value::Object(merged)
        }
        (_, patch) => patch,
    }
}

const MODEL_COLUMNS: &str = "m.id, m.model_name, m.model_display_name, m.model_description,
    m.model_icon, m.input_cost_per_token, m.output_cost_per_token, m.cost_per_image,
    m.cache_read_cost_per_token, m.text_pricing, m.context_length, m.verifiable, m.is_active,
    m.owned_by, m.created_at, m.updated_at, m.provider_type, m.provider_config,
    m.attestation_supported, m.input_modalities, m.output_modalities, m.inference_url,
    m.hugging_face_id, m.quantization, m.max_output_length, m.supported_sampling_parameters,
    m.supported_features, m.datacenters, m.is_ready, m.deprecation_date,
    m.successor_model_name, m.openrouter_slug, m.allow_free, m.is_listed";

fn modalities(row: &tokio_postgres::Row, column: &str) -> Option<Vec<String>> {
    row.get::<_, Option<serde_json::Value>>(column)
        .and_then(|value| serde_json::from_value(value).ok())
}

fn model_from_row(row: &tokio_postgres::Row, aliases: Vec<String>) -> database::models::Model {
    database::models::Model {
        id: row.get("id"),
        model_name: row.get("model_name"),
        model_display_name: row.get("model_display_name"),
        model_description: row.get("model_description"),
        model_icon: row.get("model_icon"),
        input_cost_per_token: row.get("input_cost_per_token"),
        output_cost_per_token: row.get("output_cost_per_token"),
        cost_per_image: row.get("cost_per_image"),
        cache_read_cost_per_token: row.get("cache_read_cost_per_token"),
        text_pricing: row.get("text_pricing"),
        context_length: row.get("context_length"),
        verifiable: row.get("verifiable"),
        aliases,
        provider_type: row.get("provider_type"),
        provider_config: row.get("provider_config"),
        attestation_supported: row.get("attestation_supported"),
        input_modalities: modalities(row, "input_modalities"),
        output_modalities: modalities(row, "output_modalities"),
        inference_url: row.get("inference_url"),
        hugging_face_id: row.get("hugging_face_id"),
        quantization: row.get("quantization"),
        max_output_length: row.get("max_output_length"),
        supported_sampling_parameters: row.get("supported_sampling_parameters"),
        supported_features: row.get("supported_features"),
        datacenters: row.get("datacenters"),
        is_ready: row.get("is_ready"),
        deprecation_date: row.get("deprecation_date"),
        successor_model_name: row.get("successor_model_name"),
        openrouter_slug: row.get("openrouter_slug"),
        is_active: row.get("is_active"),
        allow_free: row.get("allow_free"),
        is_listed: row.get("is_listed"),
        owned_by: row.get("owned_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn model_metadata(model: database::models::Model) -> ModelMetadata {
    ModelMetadata {
        verifiable: model.verifiable,
        context_length: model.context_length,
        model_display_name: model.model_display_name,
        model_description: model.model_description,
        model_icon: model.model_icon,
        owned_by: model.owned_by,
        aliases: model.aliases,
        provider_type: model.provider_type,
        provider_config: api::routes::common::redact_provider_config(model.provider_config),
        attestation_supported: model.attestation_supported,
        architecture: ModelArchitecture::from_options(
            model.input_modalities,
            model.output_modalities,
        ),
        inference_url: model.inference_url,
        hugging_face_id: model.hugging_face_id,
        quantization: model.quantization,
        max_output_length: model.max_output_length,
        supported_sampling_parameters: model.supported_sampling_parameters,
        supported_features: model.supported_features,
        datacenters: Datacenter::from_codes(model.datacenters),
        is_ready: model.is_ready,
        deprecation_date: model
            .deprecation_date
            .map(|date| date.format("%Y-%m-%dT%H:00:00Z").to_string()),
        successor_model_id: model.successor_model_name,
        openrouter_slug: model.openrouter_slug,
        is_listed: Some(model.is_listed),
    }
}

/// Create or update catalog models (and their aliases and history rows) the way the admin
/// batch update did, without validation: setup calls are expected to be well formed.
///
/// Providers are registered as the route did, from the merged row: a model that changed
/// provider type, inference URL, provider config or activity is unregistered (unless
/// pinned) and an active one is registered again, as an inference-URL or an external
/// provider, in every pool the harness built. The models caches are invalidated once the
/// rows are written.
pub async fn seed_models(
    models: BatchUpdateModelApiRequest,
    session_id: &str,
) -> Vec<ModelWithPricing> {
    let database = seed_database().await;
    let model_repo = ModelRepository::new(database.pool().clone());
    let alias_repo = ModelAliasRepository::new(database.pool().clone());
    let changed_by = user_id_for_session(session_id);
    let mut out = Vec::new();
    let mut inference_url_models = Vec::new();
    let mut external_models = Vec::new();
    let mut unregister = Vec::new();

    for (model_name, request) in models {
        let text_pricing = request.text_pricing.as_ref().map(|inner| {
            inner.as_ref().map(|value| {
                services::usage::TextPricingProfile::from_json(value.clone())
                    .expect("textPricing must be a valid profile")
            })
        });
        let projection = text_pricing
            .as_ref()
            .and_then(|profile| profile.as_ref())
            .map(|profile| profile.legacy_projection().expect("textPricing projection"));

        let provider_config = match request.provider_config.clone() {
            Some(patch) => {
                let stored = database
                    .pool()
                    .get()
                    .await
                    .expect("Failed to get database connection")
                    .query_opt(
                        "SELECT provider_config FROM models WHERE model_name = $1",
                        &[&model_name],
                    )
                    .await
                    .expect("read stored provider config")
                    .and_then(|row| row.get::<_, Option<serde_json::Value>>(0));
                Some(merge_provider_config(stored, patch))
            }
            None => None,
        };

        let db_request = UpdateModelPricingRequest {
            input_cost_per_token: projection
                .map(|(input, _, _)| input)
                .or_else(|| request.input_cost_per_token.as_ref().map(|p| p.amount)),
            output_cost_per_token: projection
                .map(|(_, _, output)| output)
                .or_else(|| request.output_cost_per_token.as_ref().map(|p| p.amount)),
            cost_per_image: request.cost_per_image.as_ref().map(|p| p.amount),
            cache_read_cost_per_token: projection.map(|(_, cached, _)| Some(cached)).or_else(
                || {
                    request
                        .cache_read_cost_per_token
                        .as_ref()
                        .map(|inner| inner.as_ref().map(|p| p.amount))
                },
            ),
            text_pricing: text_pricing.map(|inner| {
                inner.map(|profile| serde_json::to_value(profile).expect("text pricing serializes"))
            }),
            model_display_name: request.model_display_name.clone(),
            model_description: request.model_description.clone(),
            model_icon: request.model_icon.clone(),
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
            allow_free: request.allow_free,
            is_listed: request.is_listed,
            aliases: request.aliases.clone(),
            owned_by: request.owned_by.clone(),
            provider_type: request.provider_type.clone(),
            provider_config,
            expected_provider_config: None,
            attestation_supported: request.attestation_supported,
            input_modalities: request.input_modalities.clone(),
            output_modalities: request.output_modalities.clone(),
            inference_url: request.inference_url.clone(),
            hugging_face_id: request.hugging_face_id.clone(),
            quantization: request.quantization.clone(),
            max_output_length: request.max_output_length,
            supported_sampling_parameters: request.supported_sampling_parameters.clone(),
            supported_features: request.supported_features.clone(),
            datacenters: Datacenter::to_codes(request.datacenters.clone()),
            is_ready: request.is_ready,
            deprecation_date: request.deprecation_date.as_ref().map(|inner| {
                inner
                    .as_deref()
                    .map(|value| parse_deprecation_date(value).expect("valid deprecationDate"))
            }),
            successor_model_name: request
                .successor_model_id
                .as_ref()
                .map(|inner| inner.as_ref().map(|name| name.trim().to_string())),
            openrouter_slug: request.openrouter_slug.clone(),
            change_reason: request.change_reason.clone(),
            changed_by_user_id: Some(changed_by),
            changed_by_user_email: Some(MOCK_ADMIN_EMAIL.to_string()),
        };

        let model = model_repo
            .upsert_model_pricing(&model_name, &db_request)
            .await
            .expect("Failed to seed model");
        if let Some(alias_names) = &request.aliases {
            alias_repo
                .upsert_aliases_for_model(&model.id, alias_names)
                .await
                .expect("Failed to seed model aliases");
        }

        // The route's two registration triggers (admin.rs, `inference_url_models` and
        // `external_models`): the inference-URL one also reacts to `context_length`, the
        // external one does not look at `inference_url`.
        let touches_url_registration = request.provider_type.is_some()
            || request.inference_url.is_some()
            || request.provider_config.is_some()
            || request.is_active.is_some()
            || request.context_length.is_some();
        let touches_external_registration = request.provider_type.is_some()
            || request.provider_config.is_some()
            || request.is_active.is_some();
        if request.is_active == Some(false)
            || request.provider_type.is_some()
            || request.inference_url.is_some()
        {
            unregister.push(model_name.clone());
        }
        if model.is_active {
            if model.provider_type == "external" {
                if touches_external_registration {
                    if let Some(config) = model.provider_config.clone() {
                        external_models.push((model_name.clone(), config));
                    }
                }
            } else if let (true, Some(url)) =
                (touches_url_registration, model.inference_url.clone())
            {
                inference_url_models.push(
                    services::inference_provider_pool::expand_inference_endpoints(
                        &model_name,
                        &url,
                        u32::try_from(model.context_length).ok(),
                        model.provider_config.as_ref(),
                    ),
                );
            }
        }

        out.push(ModelWithPricing {
            upcoming_pricing: None,
            model_id: model_name,
            input_cost_per_token: usd_price(model.input_cost_per_token),
            output_cost_per_token: usd_price(model.output_cost_per_token),
            cost_per_image: usd_price(model.cost_per_image),
            cache_read_cost_per_token: model.cache_read_cost_per_token.map(usd_price),
            text_pricing: model.text_pricing.clone(),
            metadata: model_metadata(model),
        });
    }
    for pool in registered_provider_pools() {
        if !(unregister.is_empty() && inference_url_models.is_empty() && external_models.is_empty())
        {
            pool.invalidate_periodic_provider_refreshes().await;
        }
        for model_name in &unregister {
            if !pool.is_pinned(model_name) {
                pool.unregister_provider(model_name).await;
            }
        }
        let url_models: Vec<_> = inference_url_models.iter().flatten().cloned().collect();
        if !url_models.is_empty() {
            pool.load_inference_url_models(url_models, true).await;
        }
        if !external_models.is_empty() {
            // The route only warned: a model without a usable key (the e2e config has
            // none) is simply not registered. A config that carries its own api_key is
            // seeded because a test needs the provider, so its failed load panics.
            if let Err(error) = pool.load_external_providers(external_models.clone()).await {
                assert!(
                    !external_models
                        .iter()
                        .any(|(_, config)| config.get("api_key").is_some()),
                    "Failed to register a seeded external provider with an api_key: {error}"
                );
                tracing::warn!(%error, "Failed to register some seeded external providers");
            }
        }
    }
    for models_service in registered_models_services() {
        models_service.invalidate_models_cache().await;
    }
    out
}

/// One page of the admin model listing, ordered by name, with the active aliases of each.
pub async fn seed_list_models_page(
    include_inactive: bool,
    limit: i64,
    offset: i64,
) -> (Vec<AdminModelWithPricing>, i64) {
    let database = seed_database().await;
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let total: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM models WHERE ($1 OR is_active = true)",
            &[&include_inactive],
        )
        .await
        .expect("count models")
        .get(0);
    let rows = client
        .query(
            &format!(
                "SELECT {MODEL_COLUMNS},
                        COALESCE(array_agg(a.alias_name) FILTER (WHERE a.alias_name IS NOT NULL), '{{}}') AS aliases
                 FROM models m
                 LEFT JOIN model_aliases a ON a.canonical_model_id = m.id AND a.is_active = true
                 WHERE ($1 OR m.is_active = true)
                 GROUP BY m.id
                 ORDER BY m.model_name ASC
                 LIMIT $2 OFFSET $3"
            ),
            &[&include_inactive, &limit, &offset],
        )
        .await
        .expect("list models");
    let models = rows
        .iter()
        .map(|row| {
            let model = model_from_row(row, row.get("aliases"));
            AdminModelWithPricing {
                model_id: model.model_name.clone(),
                input_cost_per_token: usd_price(model.input_cost_per_token),
                output_cost_per_token: usd_price(model.output_cost_per_token),
                cost_per_image: usd_price(model.cost_per_image),
                cache_read_cost_per_token: model.cache_read_cost_per_token.map(usd_price),
                text_pricing: model.text_pricing.clone(),
                is_active: model.is_active,
                created_at: model.created_at,
                updated_at: model.updated_at,
                metadata: model_metadata(model),
            }
        })
        .collect();
    (models, total)
}

/// The `web_search` platform service. Created with `cost_per_unit` = 1_000_000 nano-USD per
/// request when absent; an existing row is never overwritten.
pub async fn seed_web_search_service() -> ServiceResponse {
    let database = seed_database().await;
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let service_name = services::service_usage::ports::SERVICE_NAME_WEB_SEARCH;
    client
        .execute(
            "INSERT INTO services (service_name, display_name, description, unit, cost_per_unit)
             VALUES ($1, 'Web Search', 'Brave web search API', 'request', 1000000)
             ON CONFLICT (service_name) DO NOTHING",
            &[&service_name],
        )
        .await
        .expect("Failed to seed web_search service");
    let row = client
        .query_one(
            "SELECT id, service_name, display_name, description, unit, cost_per_unit,
                    is_active, created_at, updated_at
             FROM services WHERE service_name = $1",
            &[&service_name],
        )
        .await
        .expect("web_search service must exist after seeding");
    ServiceResponse {
        id: row.get("id"),
        service_name: row.get("service_name"),
        display_name: row.get("display_name"),
        description: row.get("description"),
        unit: serde_json::from_value(serde_json::Value::String(row.get("unit")))
            .expect("service unit must be a known unit"),
        cost_per_unit: row.get("cost_per_unit"),
        is_active: row.get("is_active"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// An `adm_` admin access token for the user with this email, which must be on the test
/// `AUTH_ADMIN_EMAILS`. The user is created when absent. The token row is written by
/// `AdminAccessTokenRepository`, the code that hashes and validates admin tokens.
///
/// The shared mock user already holds `admin@test.com`, the default allowlisted email, so
/// that email reuses it. The token has no User-Agent binding and read-write permission.
pub async fn seed_admin_access_token(email: &str) -> String {
    let database = seed_database().await;
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let user_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO users (id, email, username, display_name, auth_provider,
                                provider_user_id, created_at, updated_at)
             VALUES ($1, $2, $3, $4, 'mock', $5, NOW(), NOW())
             ON CONFLICT (email) DO NOTHING",
            &[
                &user_id,
                &email,
                &format!("admin-{user_id}"),
                &Some("Test Admin".to_string()),
                &format!("mock_admin-{user_id}"),
            ],
        )
        .await
        .expect("Failed to seed admin user");
    let created_by: Uuid = client
        .query_one("SELECT id FROM users WHERE email = $1", &[&email])
        .await
        .expect("admin user must exist after seeding")
        .get(0);

    let (_row, token) = AdminAccessTokenRepository::new(database.pool().clone())
        .create(
            created_by,
            format!("e2e-{}", Uuid::new_v4()),
            "e2e test".to_string(),
            Utc::now() + chrono::Duration::days(1),
            None,
            AdminAccessTokenPermission::ReadWrite,
        )
        .await
        .expect("Failed to seed admin access token");
    token
}

/// Set or clear the organization's `allowed_models` setting. It is a key of the
/// `organizations.settings` JSON; `None` removes it, which allows every model. Names are
/// stored as given, not validated or canonicalized as the route does, so pass canonical
/// model names. The organization's cached filter is invalidated after the write.
pub async fn seed_allowed_models(org_id: &str, allowed_models: Option<&[String]>) {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let allowed = allowed_models.map(|names| serde_json::json!(names));
    let updated = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection")
        .execute(
            "UPDATE organizations
             SET settings = CASE
                     WHEN $2::jsonb IS NULL THEN COALESCE(settings, '{}'::jsonb) - 'allowed_models'
                     ELSE jsonb_set(COALESCE(settings, '{}'::jsonb), '{allowed_models}', $2::jsonb, true)
                 END,
                 updated_at = NOW()
             WHERE id = $1 AND is_active = true",
            &[&organization_id, &allowed],
        )
        .await
        .expect("Failed to seed allowed_models");
    assert_eq!(updated, 1, "Organization not found: {organization_id}");
    for models_service in registered_models_services() {
        models_service
            .invalidate_organization_model_filter(organization_id)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Reads of what the usage console showed
// ---------------------------------------------------------------------------

/// Credit allocations of one charge, as the usage history showed them: `None` for legacy
/// usage whose funding was never captured.
fn parse_credit_allocations(
    value: Option<serde_json::Value>,
) -> Option<Vec<services::usage::CreditAllocation>> {
    value.map(|value| serde_json::from_value(value).expect("credit allocations should parse"))
}

/// What `GET /v1/organizations/{id}/usage/balance` returned: the balance row, the active
/// credit limits and their lifetime consumption. Panics when the organization has neither a
/// balance row nor a limit, where the route answered 404.
pub async fn organization_balance(org_id: &str) -> usage_views::OrganizationBalanceResponse {
    try_organization_balance(org_id)
        .await
        .unwrap_or_else(|| panic!("No usage data or limit found for organization {org_id}"))
}

/// Like `organization_balance`, but `None` where the route answered 404.
pub async fn try_organization_balance(
    org_id: &str,
) -> Option<usage_views::OrganizationBalanceResponse> {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let balance = client
        .query_opt(
            "SELECT total_spent, legacy_unattributed_amount, last_usage_at, total_requests,
                    total_tokens, updated_at
             FROM organization_balance WHERE organization_id = $1",
            &[&organization_id],
        )
        .await
        .expect("Failed to read organization balance");
    let rows = client
        .query(
            "SELECT olh.spend_limit, olh.credit_type, olh.source, olh.currency,
                    olh.transferred_out, olh.effective_capacity,
                    COALESCE(consumed.amount, 0)::BIGINT AS consumed,
                    COALESCE(balance.unresolved_unfunded_amount, 0)::BIGINT AS unfunded,
                    COALESCE(balance.legacy_unattributed_amount, 0)::BIGINT AS unattributed
             FROM organization_credit_capacities olh
             LEFT JOIN organization_credit_consumption consumed
               ON consumed.organization_id = olh.organization_id
              AND consumed.credit_type = olh.credit_type
             LEFT JOIN organization_balance balance
               ON balance.organization_id = olh.organization_id
             WHERE olh.organization_id = $1 AND olh.effective_until IS NULL
             ORDER BY olh.credit_type, olh.effective_from DESC",
            &[&organization_id],
        )
        .await
        .expect("Failed to read organization credit limits");

    let credit_limits: Vec<usage_views::CreditLimitBreakdownResponse> = rows
        .iter()
        .map(|row| {
            let consumed: i64 = row.get("consumed");
            let effective_capacity: i64 = row.get("effective_capacity");
            usage_views::CreditLimitBreakdownResponse {
                credit_type: row.get("credit_type"),
                source: row.get("source"),
                amount: row.get("spend_limit"),
                transferred_out: row.get("transferred_out"),
                effective_capacity,
                consumed,
                available: effective_capacity.saturating_sub(consumed).max(0),
                currency: row.get("currency"),
            }
        })
        .collect();
    // Totals over every active type, minus legacy spend that has no type.
    let limit = rows.first().map(|first| {
        let spend_limit = rows
            .iter()
            .map(|row| row.get::<_, i64>("spend_limit"))
            .fold(0_i64, i64::saturating_add);
        let available = credit_limits
            .iter()
            .map(|limit| limit.available)
            .fold(0_i64, i64::saturating_add)
            .saturating_sub(first.get::<_, i64>("unattributed"))
            .max(0);
        (spend_limit, available, first.get::<_, i64>("unfunded"))
    });
    let (spend_limit, remaining) = match limit {
        Some((spend_limit, available, _)) => (Some(spend_limit), Some(available)),
        None => (None, None),
    };
    let unfunded_amount = limit.map_or(0, |(_, _, unfunded)| unfunded);
    let display = |amount: Option<i64>| amount.map(api::routes::common::format_amount);

    Some(match balance {
        Some(row) => {
            let total_spent: i64 = row.get("total_spent");
            let last_usage_at: Option<DateTime<Utc>> = row.get("last_usage_at");
            let updated_at: DateTime<Utc> = row.get("updated_at");
            usage_views::OrganizationBalanceResponse {
                organization_id: organization_id.to_string(),
                legacy_unattributed_amount: row.get("legacy_unattributed_amount"),
                total_spent,
                total_spent_display: api::routes::common::format_amount(total_spent),
                spend_limit,
                spend_limit_display: display(spend_limit),
                remaining,
                remaining_display: display(remaining),
                last_usage_at: last_usage_at.map(|at| at.to_rfc3339()),
                total_requests: row.get("total_requests"),
                total_tokens: row.get("total_tokens"),
                updated_at: updated_at.to_rfc3339(),
                credit_limits,
                unfunded_amount,
            }
        }
        None => {
            limit?;
            usage_views::OrganizationBalanceResponse {
                organization_id: organization_id.to_string(),
                legacy_unattributed_amount: 0,
                total_spent: 0,
                total_spent_display: api::routes::common::format_amount(0),
                spend_limit,
                spend_limit_display: display(spend_limit),
                remaining,
                remaining_display: display(remaining),
                last_usage_at: None,
                total_requests: 0,
                total_tokens: 0,
                updated_at: Utc::now().to_rfc3339(),
                credit_limits,
                unfunded_amount,
            }
        }
    })
}

/// What `GET /v1/organizations/{id}/usage/history?limit=&offset=` returned without filters:
/// one page of inference usage, newest first, with the balance's request counter as total.
pub async fn usage_history(
    org_id: &str,
    limit: i64,
    offset: i64,
) -> usage_views::UsageHistoryResponse {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let rows = client
        .query(
            "SELECT ul.id, ul.workspace_id, ul.api_key_id, ul.model_name, ul.input_tokens,
                    ul.output_tokens, ul.cache_read_tokens, ul.cache_write_tokens,
                    ul.service_tier, ul.context_band, ul.billing_details, ul.total_tokens,
                    ul.total_cost, ul.inference_type, ul.created_at, ul.stop_reason,
                    ul.response_id, ul.provider_request_id, ul.inference_id, ul.image_count,
                    ul.funded_amount, ul.allocation_policy_version,
                    CASE WHEN ul.funded_amount IS NULL THEN NULL ELSE
                        COALESCE((SELECT jsonb_agg(jsonb_build_object(
                            'type', a.credit_type, 'amount', a.amount, 'source', a.source,
                            'organization_limit_id', a.organization_limit_id,
                            'policy_version', a.policy_version
                        ) ORDER BY a.created_at, a.priority_position, a.id)
                        FROM effective_usage_credit_allocations a
                        WHERE a.inference_usage_id = ul.id), '[]'::jsonb)
                    END AS credit_allocations
             FROM organization_usage_log ul
             WHERE ul.organization_id = $1
             ORDER BY ul.created_at DESC
             LIMIT $2 OFFSET $3",
            &[&organization_id, &limit, &offset],
        )
        .await
        .expect("Failed to read usage history");
    let total: i64 = client
        .query_one(
            "SELECT COALESCE((SELECT total_requests FROM organization_balance
                              WHERE organization_id = $1), 0)::BIGINT",
            &[&organization_id],
        )
        .await
        .expect("Failed to read usage total")
        .get(0);

    let data = rows
        .iter()
        .map(|row| {
            let total_cost: i64 = row.get("total_cost");
            let created_at: DateTime<Utc> = row.get("created_at");
            let credit_allocations = parse_credit_allocations(row.get("credit_allocations"));
            let funded_amount: Option<i64> = row.get("funded_amount");
            // Funded and unfunded amounts are reported only for usage with captured funding.
            let (funded_amount, unfunded_amount) = match funded_amount {
                Some(_) => {
                    let funded: i64 = credit_allocations
                        .iter()
                        .flatten()
                        .map(|allocation| allocation.amount)
                        .sum();
                    (Some(funded), Some(total_cost - funded))
                }
                None => (None, None),
            };
            usage_views::UsageHistoryEntryResponse {
                id: row.get::<_, Uuid>("id").to_string(),
                workspace_id: row.get::<_, Uuid>("workspace_id").to_string(),
                api_key_id: row.get::<_, Uuid>("api_key_id").to_string(),
                model: row.get("model_name"),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                cache_read_tokens: row.get("cache_read_tokens"),
                cache_write_tokens: row.get("cache_write_tokens"),
                service_tier: row.get("service_tier"),
                context_band: row.get("context_band"),
                billing_details: row.get("billing_details"),
                total_tokens: row.get("total_tokens"),
                total_cost,
                total_cost_display: api::routes::common::format_amount(total_cost),
                inference_type: row.get("inference_type"),
                created_at: created_at.to_rfc3339(),
                stop_reason: row.get("stop_reason"),
                response_id: row
                    .get::<_, Option<Uuid>>("response_id")
                    .map(|id| id.to_string()),
                provider_request_id: row.get("provider_request_id"),
                inference_id: row
                    .get::<_, Option<Uuid>>("inference_id")
                    .map(|id| id.to_string()),
                image_count: row.get("image_count"),
                credit_allocations,
                funded_amount,
                unfunded_amount,
                allocation_policy_version: row.get("allocation_policy_version"),
            }
        })
        .collect();
    usage_views::UsageHistoryResponse {
        data,
        total: total as usize,
        limit,
        offset,
    }
}

/// What `GET /v1/organizations/{id}/service-usage/history?limit=&serviceName=` returned: one
/// page of platform-service usage, newest first.
pub async fn service_usage_history(
    org_id: &str,
    service_name: &str,
    limit: i64,
) -> usage_views::ServiceUsageHistoryResponse {
    let database = seed_database().await;
    let organization_id = Uuid::parse_str(org_id).expect("organization id must be a UUID");
    let client = database
        .pool()
        .get()
        .await
        .expect("Failed to get database connection");
    let rows = client
        .query(
            "SELECT usage_log.id, usage_log.workspace_id, usage_log.api_key_id,
                    usage_log.service_id, usage_log.quantity, usage_log.total_cost,
                    usage_log.created_at, usage_log.funded_amount,
                    CASE WHEN usage_log.funded_amount IS NULL THEN NULL ELSE
                        COALESCE((SELECT jsonb_agg(jsonb_build_object(
                            'type', a.credit_type, 'amount', a.amount, 'source', a.source,
                            'organization_limit_id', a.organization_limit_id,
                            'policy_version', a.policy_version
                        ) ORDER BY a.created_at, a.priority_position, a.id)
                        FROM effective_usage_credit_allocations a
                        WHERE a.service_usage_id = usage_log.id), '[]'::jsonb)
                    END AS credit_allocations
             FROM organization_service_usage_log usage_log
             JOIN services ON services.id = usage_log.service_id
             WHERE usage_log.organization_id = $1 AND services.service_name = $2
             ORDER BY usage_log.created_at DESC
             LIMIT $3",
            &[&organization_id, &service_name, &limit],
        )
        .await
        .expect("Failed to read service usage history");
    let total: i64 = client
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM organization_service_usage_log usage_log
             JOIN services ON services.id = usage_log.service_id
             WHERE usage_log.organization_id = $1 AND services.service_name = $2",
            &[&organization_id, &service_name],
        )
        .await
        .expect("Failed to count service usage")
        .get(0);

    let data = rows
        .iter()
        .map(|row| {
            let total_cost: i64 = row.get("total_cost");
            let created_at: DateTime<Utc> = row.get("created_at");
            let credit_allocations = parse_credit_allocations(row.get("credit_allocations"));
            let funded_amount: Option<i64> = row.get("funded_amount");
            let (funded_amount, unfunded_amount) = match funded_amount {
                Some(_) => {
                    let funded: i64 = credit_allocations
                        .iter()
                        .flatten()
                        .map(|allocation| allocation.amount)
                        .sum();
                    (Some(funded), Some(total_cost - funded))
                }
                None => (None, None),
            };
            usage_views::ServiceUsageEntryResponse {
                id: row.get::<_, Uuid>("id").to_string(),
                organization_id: organization_id.to_string(),
                workspace_id: row.get::<_, Uuid>("workspace_id").to_string(),
                api_key_id: row.get::<_, Uuid>("api_key_id").to_string(),
                service_id: row.get::<_, Uuid>("service_id").to_string(),
                quantity: row.get("quantity"),
                total_cost,
                total_cost_display: api::routes::common::format_amount(total_cost),
                created_at: created_at.to_rfc3339(),
                credit_allocations,
                funded_amount,
                unfunded_amount,
            }
        })
        .collect();
    usage_views::ServiceUsageHistoryResponse {
        data,
        total: total as usize,
        limit,
        offset: 0,
    }
}
