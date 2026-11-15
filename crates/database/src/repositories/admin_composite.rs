use crate::models::UpdateModelPricingRequest;
use crate::pool::DbPool;
use crate::repositories::lifecycle::{org_lifecycle_predicate, ORG_ALL_MEMBERS_ERASED_SQL};
use crate::repositories::{ModelAliasRepository, ModelRepository};
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::admin::{
    AdminOrganizationInfo, AdminRepository, DeprecateModelOutcome, ModelAccessGrant,
    ModelDeprecationDeliveryRecord, ModelDeprecationEmailStatus, ModelDeprecationRecipient,
    ModelPricing, ModelPricingSnapshot, OrganizationLifecycleFilter, PricingChangeDeliveryRecord,
    PricingChangeOpenConflictError, PricingChangeRecipientRow, ScheduledPricingChange,
    ScheduledPricingChangeInsert, ScheduledPricingChangeStatus, UpdateModelAdminRequest,
};
use services::lifecycle::OrganizationLifecycle;
use std::sync::Arc;
use uuid::Uuid;

/// The address a notification recipient row can be emailed at, resolved from the
/// user's `email`, `contact_email` and `contact_email_verified_at` columns. `None`
/// when the user has no deliverable address (a wallet login with no verified
/// contact email), in which case the row is not a recipient.
fn recipient_email(row: &tokio_postgres::Row) -> Option<String> {
    let login_email: String = row.get("email");
    let contact_email: Option<String> = row.get("contact_email");
    services::email::deliverable_email(
        &login_email,
        contact_email.as_deref(),
        row.get("contact_email_verified_at"),
    )
    .map(str::to_owned)
}

fn parse_text_pricing(
    value: Option<serde_json::Value>,
) -> Result<Option<services::usage::TextPricingProfile>> {
    value
        .map(services::usage::TextPricingProfile::from_json)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

/// Composite repository that implements AdminRepository for both model and organization operations
#[derive(Clone)]
pub struct AdminCompositeRepository {
    pool: DbPool,
    model_repo: Arc<ModelRepository>,
    alias_repo: Arc<ModelAliasRepository>,
}

impl AdminCompositeRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool: pool.clone(),
            model_repo: Arc::new(ModelRepository::new(pool.clone())),
            alias_repo: Arc::new(ModelAliasRepository::new(pool.clone())),
        }
    }
}

fn row_to_scheduled_pricing_change(
    row: &tokio_postgres::Row,
) -> Result<ScheduledPricingChange, anyhow::Error> {
    let status_str: String = row.get("status");
    let status = ScheduledPricingChangeStatus::parse(&status_str)
        .ok_or_else(|| anyhow::anyhow!("unknown scheduled pricing change status '{status_str}'"))?;
    Ok(ScheduledPricingChange {
        id: row.get("id"),
        batch_id: row.get("batch_id"),
        model_id: row.get("model_id"),
        model_name: row.get("model_name"),
        model_display_name: row.get("model_display_name"),
        new_input_cost_per_token: row.get("new_input_cost_per_token"),
        new_output_cost_per_token: row.get("new_output_cost_per_token"),
        new_cache_read_cost_per_token: row.get("new_cache_read_cost_per_token"),
        new_cost_per_image: row.get("new_cost_per_image"),
        old_input_cost_per_token: row.get("old_input_cost_per_token"),
        old_output_cost_per_token: row.get("old_output_cost_per_token"),
        old_cache_read_cost_per_token: row.get("old_cache_read_cost_per_token"),
        old_cost_per_image: row.get("old_cost_per_image"),
        old_text_pricing: parse_text_pricing(row.get("old_text_pricing"))?,
        new_text_pricing: parse_text_pricing(row.get("new_text_pricing"))?,
        effective_at: row.get("effective_at"),
        status,
        apply_attempts: row.get("apply_attempts"),
        applied_at: row.get("applied_at"),
        last_error: row.get("last_error"),
        created_by_user_id: row.get("created_by_user_id"),
        created_by_user_email: row.get("created_by_user_email"),
        change_reason: row.get("change_reason"),
        created_at: row.get("created_at"),
    })
}

/// Map a row from the admin organization SELECT (used by both the list and
/// single-org queries) to `AdminOrganizationInfo`. The row must select
/// `id, name, description, created_at, spend_limit, total_spent,
/// total_requests, total_tokens, is_active, all_members_erased`.
fn row_to_admin_org_info(row: &tokio_postgres::Row) -> AdminOrganizationInfo {
    AdminOrganizationInfo {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        spend_limit: row.get("spend_limit"),
        total_spent: row.get("total_spent"),
        total_requests: row.get("total_requests"),
        total_tokens: row.get("total_tokens"),
        created_at: row.get("created_at"),
        lifecycle: OrganizationLifecycle::from_columns(
            row.get("is_active"),
            row.get("all_members_erased"),
        ),
    }
}

/// One grant with its organization and the admin who made it. Only active
/// organizations: a grant held by an inactive one has no effect. Callers add
/// the `WHERE` clause.
const MODEL_ACCESS_GRANT_SELECT: &str = r#"
    SELECT
        a.organization_id,
        o.name AS organization_name,
        a.created_at,
        a.created_by_user_id,
        u.email AS created_by_user_email
    FROM organization_model_access a
    JOIN organizations o ON o.id = a.organization_id AND o.is_active = true
    LEFT JOIN users u ON u.id = a.created_by_user_id
"#;

fn row_to_model_access_grant(row: &tokio_postgres::Row) -> ModelAccessGrant {
    ModelAccessGrant {
        organization_id: row.get("organization_id"),
        organization_name: row.get("organization_name"),
        granted_at: row.get("created_at"),
        granted_by_user_id: row.get("created_by_user_id"),
        granted_by_user_email: row.get("created_by_user_email"),
    }
}

#[async_trait]
impl AdminRepository for AdminCompositeRepository {
    async fn upsert_model_pricing(
        &self,
        model_name: &str,
        request: UpdateModelAdminRequest,
    ) -> Result<ModelPricing> {
        let text_pricing = request
            .text_pricing
            .as_ref()
            .map(|profile| profile.as_ref().map(serde_json::to_value).transpose())
            .transpose()?;
        // Convert service request to database request
        let db_request = UpdateModelPricingRequest {
            input_cost_per_token: request.input_cost_per_token,
            output_cost_per_token: request.output_cost_per_token,
            cost_per_image: request.cost_per_image,
            cache_read_cost_per_token: request.cache_read_cost_per_token,
            text_pricing,
            model_display_name: request.model_display_name,
            model_description: request.model_description,
            model_icon: request.model_icon,
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
            allow_free: request.allow_free,
            is_listed: request.is_listed,
            aliases: request.aliases.clone(),
            owned_by: request.owned_by,
            provider_type: request.provider_type,
            provider_config: request.provider_config,
            expected_provider_config: request.expected_provider_config,
            attestation_supported: request.attestation_supported,
            input_modalities: request.input_modalities,
            output_modalities: request.output_modalities,
            inference_url: request.inference_url,
            hugging_face_id: request.hugging_face_id,
            quantization: request.quantization,
            max_output_length: request.max_output_length,
            supported_sampling_parameters: request.supported_sampling_parameters,
            supported_features: request.supported_features,
            datacenters: request.datacenters,
            is_ready: request.is_ready,
            deprecation_date: request.deprecation_date,
            successor_model_name: request.successor_model_name,
            openrouter_slug: request.openrouter_slug,
            change_reason: request.change_reason,
            changed_by_user_id: request.changed_by_user_id,
            changed_by_user_email: request.changed_by_user_email,
        };

        let model = self
            .model_repo
            .upsert_model_pricing(model_name, &db_request)
            .await?;

        // Handle aliases if provided
        if let Some(alias_names) = request.aliases {
            self.alias_repo
                .upsert_aliases_for_model(&model.id, &alias_names)
                .await?;
        }

        Ok(ModelPricing {
            model_display_name: model.model_display_name,
            model_description: model.model_description,
            model_icon: model.model_icon,
            input_cost_per_token: model.input_cost_per_token,
            output_cost_per_token: model.output_cost_per_token,
            cost_per_image: model.cost_per_image,
            cache_read_cost_per_token: model.cache_read_cost_per_token,
            text_pricing: parse_text_pricing(model.text_pricing)?,
            context_length: model.context_length,
            verifiable: model.verifiable,
            is_active: model.is_active,
            aliases: model.aliases,
            owned_by: model.owned_by,
            provider_type: model.provider_type,
            provider_config: model.provider_config,
            attestation_supported: model.attestation_supported,
            input_modalities: model.input_modalities,
            output_modalities: model.output_modalities,
            inference_url: model.inference_url,
            hugging_face_id: model.hugging_face_id,
            quantization: model.quantization,
            max_output_length: model.max_output_length,
            supported_sampling_parameters: model.supported_sampling_parameters,
            supported_features: model.supported_features,
            datacenters: model.datacenters,
            is_ready: model.is_ready,
            deprecation_date: model.deprecation_date,
            successor_model_name: model.successor_model_name,
            openrouter_slug: model.openrouter_slug,
            is_listed: model.is_listed,
        })
    }

    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<Option<DeprecateModelOutcome>> {
        // All writes happen in a single transaction so a partial failure can
        // not leave the catalog in a half-deprecated state (e.g., alias
        // added but model still active).
        let mut client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;
        let tx = client.transaction().await?;

        // Fetch both models. Successor must be active; deprecated may or may
        // not be (idempotent re-deprecation is acceptable).
        let deprecated_row = tx
            .query_opt(
                "SELECT id, is_active FROM models WHERE model_name = $1",
                &[&deprecated_model_name],
            )
            .await?;
        let successor_row = tx
            .query_opt(
                "SELECT id, is_active FROM models WHERE model_name = $1",
                &[&successor_model_name],
            )
            .await?;

        let (Some(d), Some(s)) = (deprecated_row, successor_row) else {
            return Ok(None);
        };
        let deprecated_id: Uuid = d.get("id");
        let successor_id: Uuid = s.get("id");
        let successor_active: bool = s.get("is_active");
        if !successor_active {
            // Treat inactive successor like "not found" for the caller — the
            // service layer surfaces this as ModelNotFound.
            return Ok(None);
        }

        // 1. Add deprecated model_name as an alias of successor (idempotent).
        tx.execute(
            r#"
            INSERT INTO model_aliases (alias_name, canonical_model_id, is_active)
            VALUES ($1, $2, true)
            ON CONFLICT (alias_name) DO UPDATE
            SET canonical_model_id = EXCLUDED.canonical_model_id,
                is_active = true,
                updated_at = NOW()
            "#,
            &[&deprecated_model_name, &successor_id],
        )
        .await
        .context("Failed to add deprecated model name as alias of successor")?;

        // 2. Re-point pre-existing **active** inbound aliases of the deprecated
        //    model at the successor, so historical aliases keep resolving.
        //    We deliberately leave inactive inbound aliases alone — they were
        //    already not resolving, so silently re-pointing without
        //    reactivating them would mask a no-op behind a misleading
        //    "carried" count, and reactivating them could surface alias
        //    names an operator had explicitly disabled.
        let aliases_carried = tx
            .execute(
                r#"
                UPDATE model_aliases
                SET canonical_model_id = $1, updated_at = NOW()
                WHERE canonical_model_id = $2
                  AND alias_name <> $3
                  AND is_active = true
                "#,
                &[&successor_id, &deprecated_id, &deprecated_model_name],
            )
            .await
            .context("Failed to repoint inbound aliases to successor")?;

        // 3. Mark the deprecated model inactive (and capture a row snapshot
        //    for the history entry).
        let updated = tx
            .query_opt(
                r#"
                UPDATE models
                SET is_active = false, successor_model_name = $2, updated_at = NOW()
                WHERE id = $1
                RETURNING id, model_name, model_display_name, model_description, model_icon,
                          input_cost_per_token, output_cost_per_token, cost_per_image,
                          cache_read_cost_per_token, text_pricing, context_length, verifiable, is_active,
                          owned_by, created_at, updated_at, provider_type, provider_config,
                          attestation_supported, input_modalities, output_modalities, inference_url,
                          datacenters, hugging_face_id, quantization, max_output_length,
                          supported_sampling_parameters, supported_features,
                          is_ready, deprecation_date, successor_model_name,
                          openrouter_slug, allow_free, is_listed
                "#,
                &[&deprecated_id, &successor_model_name],
            )
            .await
            .context("Failed to deactivate deprecated model")?;
        let Some(deprecated_row_after) = updated else {
            // The row vanished between fetch and update — bail.
            tx.rollback().await.ok();
            return Ok(None);
        };

        // 4. Record a history entry for the deprecation. Inline the writes
        //    rather than calling the `&Client`-typed helper on
        //    ModelRepository so they participate in this transaction.
        let reason = change_reason
            .clone()
            .or_else(|| Some(format!("Deprecated in favor of '{successor_model_name}'")));
        tx.execute(
            "UPDATE model_history SET effective_until = NOW() WHERE model_id = $1 AND effective_until IS NULL",
            &[&deprecated_id],
        )
        .await
        .context("Failed to close previous history record")?;
        tx.execute(
            r#"
            INSERT INTO model_history (
                model_id, input_cost_per_token, output_cost_per_token, cost_per_image,
                cache_read_cost_per_token, context_length, model_name, model_display_name,
                model_description, model_icon, verifiable, is_active, owned_by, provider_type,
                provider_config, attestation_supported, input_modalities, output_modalities,
                inference_url, datacenters, hugging_face_id, quantization, max_output_length,
                supported_sampling_parameters, supported_features, is_ready, deprecation_date,
                openrouter_slug, allow_free,
                effective_from, effective_until, changed_by_user_id,
                changed_by_user_email, change_reason, created_at, text_pricing,
                successor_model_name, is_listed
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19,
                $20, $21, $22, $23,
                COALESCE($24, ARRAY[]::TEXT[]),
                COALESCE($25, ARRAY[]::TEXT[]),
                $26, $27, $28, $29,
                NOW(), NULL, $30, $31, $32, NOW(), $33, $34, $35
            )
            "#,
            &[
                &deprecated_id,
                &deprecated_row_after.get::<_, i64>("input_cost_per_token"),
                &deprecated_row_after.get::<_, i64>("output_cost_per_token"),
                &deprecated_row_after.get::<_, i64>("cost_per_image"),
                &deprecated_row_after.get::<_, Option<i64>>("cache_read_cost_per_token"),
                &deprecated_row_after.get::<_, i32>("context_length"),
                &deprecated_row_after.get::<_, String>("model_name"),
                &deprecated_row_after.get::<_, String>("model_display_name"),
                &deprecated_row_after.get::<_, String>("model_description"),
                &deprecated_row_after.get::<_, Option<String>>("model_icon"),
                &deprecated_row_after.get::<_, bool>("verifiable"),
                &deprecated_row_after.get::<_, bool>("is_active"),
                &deprecated_row_after.get::<_, String>("owned_by"),
                &deprecated_row_after.try_get::<_, String>("provider_type").ok(),
                &deprecated_row_after
                    .try_get::<_, serde_json::Value>("provider_config")
                    .ok(),
                &deprecated_row_after
                    .try_get::<_, bool>("attestation_supported")
                    .ok(),
                &deprecated_row_after
                    .try_get::<_, Option<serde_json::Value>>("input_modalities")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<serde_json::Value>>("output_modalities")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<String>>("inference_url")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<Vec<String>>>("datacenters")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<String>>("hugging_face_id")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<String>>("quantization")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<i32>>("max_output_length")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<Vec<String>>>("supported_sampling_parameters")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<Vec<String>>>("supported_features")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<bool>>("is_ready")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>("deprecation_date")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<String>>("openrouter_slug")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, bool>("allow_free")
                    .unwrap_or(false),
                &changed_by_user_id,
                &changed_by_user_email,
                &reason,
                &deprecated_row_after
                    .try_get::<_, Option<serde_json::Value>>("text_pricing")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, Option<String>>("successor_model_name")
                    .ok()
                    .flatten(),
                &deprecated_row_after
                    .try_get::<_, bool>("is_listed")
                    .unwrap_or(true),
            ],
        )
        .await
        .context("Failed to insert history record for deprecation")?;

        // 5. Read both models' post-write state (with merged alias lists)
        //    INSIDE the transaction. If we did this after `tx.commit()`, a
        //    transient connection pool failure would surface as a 500 even
        //    though the deprecation was already committed — and a retry
        //    would write a second `model_history` entry. Reading inside the
        //    txn keeps the response build atomic with the writes: either
        //    everything succeeds and the caller sees both models, or
        //    nothing is committed and the caller can safely retry.
        let read_with_aliases = |row: &tokio_postgres::Row| -> Result<ModelPricing> {
            Ok(ModelPricing {
                model_display_name: row.get("model_display_name"),
                model_description: row.get("model_description"),
                model_icon: row.get("model_icon"),
                input_cost_per_token: row.get("input_cost_per_token"),
                output_cost_per_token: row.get("output_cost_per_token"),
                cost_per_image: row.get("cost_per_image"),
                cache_read_cost_per_token: row.get("cache_read_cost_per_token"),
                text_pricing: parse_text_pricing(row.try_get("text_pricing").ok().flatten())?,
                context_length: row.get("context_length"),
                verifiable: row.get("verifiable"),
                is_active: row.get("is_active"),
                aliases: row
                    .try_get::<_, Option<Vec<String>>>("aliases")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                owned_by: row.get("owned_by"),
                provider_type: row
                    .try_get::<_, String>("provider_type")
                    .unwrap_or_else(|_| "vllm".to_string()),
                provider_config: row.try_get("provider_config").ok().flatten(),
                attestation_supported: row.try_get("attestation_supported").unwrap_or(true),
                input_modalities: row
                    .try_get::<_, Option<serde_json::Value>>("input_modalities")
                    .ok()
                    .flatten()
                    .and_then(|v| serde_json::from_value(v).ok()),
                output_modalities: row
                    .try_get::<_, Option<serde_json::Value>>("output_modalities")
                    .ok()
                    .flatten()
                    .and_then(|v| serde_json::from_value(v).ok()),
                inference_url: row.try_get("inference_url").ok().flatten(),
                hugging_face_id: row.try_get("hugging_face_id").ok().flatten(),
                quantization: row.try_get("quantization").ok().flatten(),
                max_output_length: row.try_get("max_output_length").ok().flatten(),
                supported_sampling_parameters: row
                    .try_get("supported_sampling_parameters")
                    .unwrap_or_default(),
                supported_features: row.try_get("supported_features").unwrap_or_default(),
                datacenters: row.try_get("datacenters").ok().flatten(),
                is_ready: row.try_get("is_ready").ok().flatten(),
                deprecation_date: row.try_get("deprecation_date").ok().flatten(),
                successor_model_name: row.try_get("successor_model_name").ok().flatten(),
                openrouter_slug: row.try_get("openrouter_slug").ok().flatten(),
                is_listed: row.try_get("is_listed").unwrap_or(true),
            })
        };

        let select_with_aliases_sql = r#"
            SELECT
                m.model_display_name, m.model_description, m.model_icon,
                m.input_cost_per_token, m.output_cost_per_token, m.cost_per_image,
                m.cache_read_cost_per_token, m.text_pricing, m.context_length, m.verifiable,
                m.is_active, m.owned_by, m.provider_type, m.provider_config,
                m.attestation_supported, m.input_modalities, m.output_modalities,
                m.inference_url,
                m.hugging_face_id, m.quantization, m.max_output_length,
                m.supported_sampling_parameters, m.supported_features, m.datacenters,
                m.is_ready, m.deprecation_date, m.successor_model_name, m.openrouter_slug,
                m.is_listed,
                COALESCE(
                    array_agg(ma.alias_name) FILTER (WHERE ma.alias_name IS NOT NULL),
                    '{}'
                ) AS aliases
            FROM models m
            LEFT JOIN model_aliases ma
                ON ma.canonical_model_id = m.id AND ma.is_active = true
            WHERE m.model_name = $1
            GROUP BY m.id
        "#;

        // The rows must exist — we just wrote to them in this same
        // transaction. A `None` here would indicate driver corruption, not
        // a routine race; bubble up as an error so the txn rolls back.
        let deprecated_row_full = tx
            .query_opt(select_with_aliases_sql, &[&deprecated_model_name])
            .await?
            .context("deprecated model row missing in same transaction (driver bug?)")?;
        let successor_row_full = tx
            .query_opt(select_with_aliases_sql, &[&successor_model_name])
            .await?
            .context("successor model row missing in same transaction (driver bug?)")?;
        let outcome = DeprecateModelOutcome {
            deprecated: read_with_aliases(&deprecated_row_full)?,
            successor: read_with_aliases(&successor_row_full)?,
            // `aliases_carried` is u64 from `tx.execute`. The number of
            // inbound aliases on a single model can never realistically
            // exceed u32::MAX; saturate via `min` rather than `try_from` so
            // the behavior is explicitly "cap at u32::MAX" and not the
            // default "discard the value on overflow."
            aliases_carried: aliases_carried.min(u32::MAX as u64) as u32,
        };

        tx.commit().await?;
        Ok(Some(outcome))
    }

    async fn list_model_deprecation_recipients(
        &self,
        model_name: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ModelDeprecationRecipient>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                r#"
                SELECT DISTINCT
                    u.id AS user_id,
                    u.email AS email,
                    u.contact_email AS contact_email,
                    u.contact_email_verified_at AS contact_email_verified_at,
                    lower(u.email) AS email_sort,
                    o.id AS organization_id,
                    o.name AS organization_name
                FROM organization_usage_log ul
                JOIN organizations o ON o.id = ul.organization_id
                JOIN organization_members om ON om.organization_id = o.id
                JOIN users u ON u.id = om.user_id
                WHERE ul.model_name IN (
                    SELECT model_name
                    FROM models
                    WHERE model_name = $1
                    UNION
                    SELECT ma.alias_name
                    FROM model_aliases ma
                    JOIN models m ON m.id = ma.canonical_model_id
                    WHERE m.model_name = $1
                      AND ma.is_active = true
                )
                  AND ul.created_at >= $2
                  AND o.is_active = true
                  AND u.is_active = true
                  AND om.role IN ('owner', 'admin')
                ORDER BY email_sort, organization_name
                "#,
                &[&model_name, &since],
            )
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(ModelDeprecationRecipient {
                    user_id: row.get("user_id"),
                    email: recipient_email(&row)?,
                    organization_id: row.get("organization_id"),
                    organization_name: row.get("organization_name"),
                })
            })
            .collect())
    }

    async fn is_user_active(&self, user_id: Uuid) -> Result<bool> {
        let client = self.pool.get().await?;
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND is_active = true)",
                &[&user_id],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn record_model_deprecation_delivery(
        &self,
        record: ModelDeprecationDeliveryRecord,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let status = record.status.as_str();
        let email_sent_at = if record.status == ModelDeprecationEmailStatus::Sent {
            Some(chrono::Utc::now())
        } else {
            None
        };

        client
            .execute(
                r#"
                INSERT INTO model_deprecation_email_deliveries (
                    model_id, model_name, model_display_name, successor_model_name,
                    deprecation_date, recipient_user_id, recipient_email,
                    organization_id, organization_name, status, email_sent_at,
                    email_message_id, email_last_error, initiated_by_user_id,
                    initiated_by_user_email
                ) SELECT
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                    $14, $15
                WHERE EXISTS (
                    -- FOR SHARE blocks behind erasure's FOR UPDATE on the user
                    -- row (same pattern as the refresh_tokens insert in session.rs).
                    SELECT 1 FROM users WHERE id = $6 AND is_active = true FOR SHARE
                )
                ON CONFLICT (
                    model_id, successor_model_name, deprecation_date,
                    recipient_user_id, organization_id
                ) DO UPDATE SET
                    model_name = EXCLUDED.model_name,
                    model_display_name = EXCLUDED.model_display_name,
                    recipient_email = EXCLUDED.recipient_email,
                    organization_name = EXCLUDED.organization_name,
                    status = EXCLUDED.status,
                    email_sent_at = EXCLUDED.email_sent_at,
                    email_message_id = EXCLUDED.email_message_id,
                    email_last_error = EXCLUDED.email_last_error,
                    initiated_by_user_id = EXCLUDED.initiated_by_user_id,
                    initiated_by_user_email = EXCLUDED.initiated_by_user_email,
                    updated_at = NOW()
                "#,
                &[
                    &record.model_id,
                    &record.model_name,
                    &record.model_display_name,
                    &record.successor_model_name,
                    &record.deprecation_date,
                    &record.recipient_user_id,
                    &record.recipient_email,
                    &record.organization_id,
                    &record.organization_name,
                    &status,
                    &email_sent_at,
                    &record.email_message_id,
                    &record.email_last_error,
                    &record.initiated_by_user_id,
                    &record.initiated_by_user_email,
                ],
            )
            .await?;

        Ok(())
    }

    async fn get_model_pricing_snapshot(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelPricingSnapshot>> {
        let client = self.pool.get().await?;
        let row = client
            .query_opt(
                r#"
                SELECT id, model_name, model_display_name,
                       input_cost_per_token, output_cost_per_token,
                       cache_read_cost_per_token, cost_per_image, text_pricing
                FROM models
                WHERE model_name = $1 AND is_active = true
                "#,
                &[&model_name],
            )
            .await?;

        row.map(|row| {
            Ok(ModelPricingSnapshot {
                id: row.get("id"),
                model_name: row.get("model_name"),
                model_display_name: row.get("model_display_name"),
                input_cost_per_token: row.get("input_cost_per_token"),
                output_cost_per_token: row.get("output_cost_per_token"),
                cache_read_cost_per_token: row.get("cache_read_cost_per_token"),
                cost_per_image: row.get("cost_per_image"),
                text_pricing: parse_text_pricing(row.get("text_pricing"))?,
            })
        })
        .transpose()
    }

    async fn list_pricing_change_recipients(
        &self,
        model_names: &[String],
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<PricingChangeRecipientRow>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                r#"
                SELECT DISTINCT
                    u.id AS user_id,
                    u.email AS email,
                    u.contact_email AS contact_email,
                    u.contact_email_verified_at AS contact_email_verified_at,
                    o.id AS organization_id,
                    o.name AS organization_name,
                    names.canonical_name AS model_name
                FROM (
                    SELECT m.model_name AS canonical_name, m.model_name AS usage_name
                    FROM models m
                    WHERE m.model_name = ANY($1)
                    UNION
                    SELECT m.model_name AS canonical_name, ma.alias_name AS usage_name
                    FROM model_aliases ma
                    JOIN models m ON m.id = ma.canonical_model_id
                    WHERE m.model_name = ANY($1)
                      AND ma.is_active = true
                ) names
                JOIN organization_usage_log ul ON ul.model_name = names.usage_name
                JOIN organizations o ON o.id = ul.organization_id
                JOIN organization_members om ON om.organization_id = o.id
                JOIN users u ON u.id = om.user_id
                WHERE ul.created_at >= $2
                  AND o.is_active = true
                  AND u.is_active = true
                  AND om.role IN ('owner', 'admin')
                ORDER BY email, organization_name, model_name
                "#,
                &[&model_names, &since],
            )
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(PricingChangeRecipientRow {
                    user_id: row.get("user_id"),
                    email: recipient_email(&row)?,
                    organization_id: row.get("organization_id"),
                    organization_name: row.get("organization_name"),
                    model_name: row.get("model_name"),
                })
            })
            .collect())
    }

    async fn insert_scheduled_pricing_changes(
        &self,
        batch_id: Uuid,
        changes: Vec<ScheduledPricingChangeInsert>,
        created_by_user_id: Option<Uuid>,
        created_by_user_email: Option<String>,
        change_reason: Option<String>,
    ) -> Result<Vec<ScheduledPricingChange>> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;

        // Serialize concurrent confirms of the same batch so the idempotency
        // check below sees the winner's committed rows instead of racing it
        // into the open-change unique index (which would surface a spurious
        // 409 to an idempotent retry). The lock is transaction-scoped and
        // keyed on batch_id, so unrelated batches are unaffected.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
            &[&batch_id.to_string()],
        )
        .await?;

        // Idempotent confirm retry: if this batch was already persisted,
        // return the existing rows instead of inserting (the partial unique
        // index would otherwise reject the batch as a conflict with itself).
        let existing = tx
            .query(
                r#"
                SELECT * FROM scheduled_model_pricing_changes
                WHERE batch_id = $1
                ORDER BY model_name
                "#,
                &[&batch_id],
            )
            .await?;
        if !existing.is_empty() {
            return existing
                .iter()
                .map(row_to_scheduled_pricing_change)
                .collect();
        }
        let mut inserted = Vec::with_capacity(changes.len());
        for change in changes {
            let old_text_pricing = change
                .old_text_pricing
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?;
            let new_text_pricing = change
                .new_text_pricing
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?;
            let row = tx
                .query_one(
                    r#"
                    INSERT INTO scheduled_model_pricing_changes (
                        batch_id, model_id, model_name, model_display_name,
                        new_input_cost_per_token, new_output_cost_per_token,
                        new_cache_read_cost_per_token, new_cost_per_image,
                        old_input_cost_per_token, old_output_cost_per_token,
                        old_cache_read_cost_per_token, old_cost_per_image,
                        effective_at, created_by_user_id, created_by_user_email,
                        change_reason, old_text_pricing, new_text_pricing
                    ) VALUES (
                        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                        $13, $14, $15, $16, $17, $18
                    )
                    RETURNING *
                    "#,
                    &[
                        &batch_id,
                        &change.model_id,
                        &change.model_name,
                        &change.model_display_name,
                        &change.new_input_cost_per_token,
                        &change.new_output_cost_per_token,
                        &change.new_cache_read_cost_per_token,
                        &change.new_cost_per_image,
                        &change.old_input_cost_per_token,
                        &change.old_output_cost_per_token,
                        &change.old_cache_read_cost_per_token,
                        &change.old_cost_per_image,
                        &change.effective_at,
                        &created_by_user_id,
                        &created_by_user_email,
                        &change_reason,
                        &old_text_pricing,
                        &new_text_pricing,
                    ],
                )
                .await
                .map_err(|e| {
                    let is_open_conflict = e
                        .as_db_error()
                        .map(|db| {
                            db.code() == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
                                && db.constraint()
                                    == Some("uq_scheduled_pricing_change_open_per_model")
                        })
                        .unwrap_or(false);
                    if is_open_conflict {
                        anyhow::Error::new(PricingChangeOpenConflictError {
                            model_name: change.model_name.clone(),
                        })
                    } else {
                        anyhow::Error::new(e)
                    }
                })?;
            inserted.push(row_to_scheduled_pricing_change(&row)?);
        }
        tx.commit().await?;

        Ok(inserted)
    }

    async fn claim_due_pricing_changes(&self, limit: i64) -> Result<Vec<ScheduledPricingChange>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                r#"
                UPDATE scheduled_model_pricing_changes
                SET status = 'applying',
                    apply_attempts = apply_attempts + 1,
                    updated_at = NOW()
                WHERE id IN (
                    SELECT id FROM scheduled_model_pricing_changes
                    WHERE status = 'pending' AND effective_at <= NOW()
                    ORDER BY effective_at
                    FOR UPDATE SKIP LOCKED
                    LIMIT $1
                )
                RETURNING *
                "#,
                &[&limit],
            )
            .await?;

        rows.iter().map(row_to_scheduled_pricing_change).collect()
    }

    async fn mark_pricing_change_applied(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                r#"
                UPDATE scheduled_model_pricing_changes
                SET status = 'applied', applied_at = NOW(), last_error = NULL,
                    updated_at = NOW()
                WHERE id = $1 AND status = 'applying'
                "#,
                &[&id],
            )
            .await?;
        Ok(())
    }

    async fn mark_pricing_change_failed(
        &self,
        id: Uuid,
        error: &str,
        retryable: bool,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                r#"
                UPDATE scheduled_model_pricing_changes
                SET status = CASE WHEN $3 THEN 'pending' ELSE 'failed' END,
                    last_error = $2,
                    updated_at = NOW()
                WHERE id = $1 AND status = 'applying'
                "#,
                &[&id, &error, &retryable],
            )
            .await?;
        Ok(())
    }

    async fn recover_stale_applying_pricing_changes(
        &self,
        stale_after: chrono::Duration,
        max_attempts: i32,
    ) -> Result<u64> {
        let client = self.pool.get().await?;
        let stale_secs = stale_after.num_seconds() as f64;
        let count = client
            .execute(
                r#"
                UPDATE scheduled_model_pricing_changes
                SET status = CASE WHEN apply_attempts >= $2 THEN 'failed' ELSE 'pending' END,
                    last_error = CASE
                        WHEN apply_attempts >= $2
                            THEN COALESCE(last_error, 'apply timed out')
                        ELSE last_error
                    END,
                    updated_at = NOW()
                WHERE status = 'applying'
                  AND updated_at < NOW() - make_interval(secs => $1)
                "#,
                &[&stale_secs, &max_attempts],
            )
            .await?;
        Ok(count)
    }

    async fn record_pricing_change_delivery(
        &self,
        record: PricingChangeDeliveryRecord,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let status = record.status.as_str();
        let email_sent_at = if record.status == ModelDeprecationEmailStatus::Sent {
            Some(chrono::Utc::now())
        } else {
            None
        };

        client
            .execute(
                r#"
                INSERT INTO model_pricing_change_email_deliveries (
                    batch_id, recipient_user_id, recipient_email,
                    organization_id, organization_name, model_names, status,
                    email_sent_at, email_message_id, email_last_error,
                    initiated_by_user_id, initiated_by_user_email
                ) SELECT
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                WHERE EXISTS (
                    -- FOR SHARE blocks behind erasure's FOR UPDATE on the user
                    -- row (same pattern as the refresh_tokens insert in session.rs).
                    SELECT 1 FROM users WHERE id = $2 AND is_active = true FOR SHARE
                )
                ON CONFLICT (batch_id, recipient_user_id, organization_id)
                DO UPDATE SET
                    recipient_email = EXCLUDED.recipient_email,
                    organization_name = EXCLUDED.organization_name,
                    model_names = EXCLUDED.model_names,
                    status = EXCLUDED.status,
                    email_sent_at = EXCLUDED.email_sent_at,
                    email_message_id = EXCLUDED.email_message_id,
                    email_last_error = EXCLUDED.email_last_error,
                    initiated_by_user_id = EXCLUDED.initiated_by_user_id,
                    initiated_by_user_email = EXCLUDED.initiated_by_user_email,
                    updated_at = NOW()
                "#,
                &[
                    &record.batch_id,
                    &record.recipient_user_id,
                    &record.recipient_email,
                    &record.organization_id,
                    &record.organization_name,
                    &record.model_names,
                    &status,
                    &email_sent_at,
                    &record.email_message_id,
                    &record.email_last_error,
                    &record.initiated_by_user_id,
                    &record.initiated_by_user_email,
                ],
            )
            .await?;

        Ok(())
    }

    async fn list_all_organizations(
        &self,
        limit: i64,
        offset: i64,
        lifecycle: OrganizationLifecycleFilter,
    ) -> Result<Vec<AdminOrganizationInfo>> {
        let client = self.pool.get().await?;

        let sql = format!(
            r#"
                SELECT
                    o.id,
                    o.name,
                    o.description,
                    o.created_at,
                    olh.spend_limit,
                    ob.total_spent,
                    ob.total_requests,
                    ob.total_tokens,
                    o.is_active,
                    {all_erased} AS all_members_erased
                FROM organizations o
                LEFT JOIN LATERAL (
                    SELECT CASE
                        WHEN COUNT(spend_limit) = 0 THEN NULL
                        ELSE LEAST(SUM(spend_limit), 9223372036854775807)::BIGINT
                    END AS spend_limit
                    FROM organization_limits_history
                    WHERE organization_id = o.id
                      AND effective_until IS NULL
                ) olh ON true
                LEFT JOIN organization_balance ob ON o.id = ob.organization_id
                WHERE {predicate}
                ORDER BY o.created_at DESC
                LIMIT $1 OFFSET $2
                "#,
            all_erased = ORG_ALL_MEMBERS_ERASED_SQL,
            predicate = org_lifecycle_predicate(lifecycle),
        );
        let rows = client.query(sql.as_str(), &[&limit, &offset]).await?;

        let organizations = rows.iter().map(row_to_admin_org_info).collect();

        Ok(organizations)
    }

    async fn get_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<AdminOrganizationInfo>> {
        let client = self.pool.get().await?;

        let sql = format!(
            r#"
                SELECT
                    o.id,
                    o.name,
                    o.description,
                    o.created_at,
                    olh.spend_limit,
                    ob.total_spent,
                    ob.total_requests,
                    ob.total_tokens,
                    o.is_active,
                    {all_erased} AS all_members_erased
                FROM organizations o
                LEFT JOIN LATERAL (
                    SELECT CASE
                        WHEN COUNT(spend_limit) = 0 THEN NULL
                        ELSE LEAST(SUM(spend_limit), 9223372036854775807)::BIGINT
                    END AS spend_limit
                    FROM organization_limits_history
                    WHERE organization_id = o.id
                      AND effective_until IS NULL
                ) olh ON true
                LEFT JOIN organization_balance ob ON o.id = ob.organization_id
                WHERE o.id = $1
                  AND o.is_active = true
                "#,
            all_erased = ORG_ALL_MEMBERS_ERASED_SQL,
        );
        let row = client.query_opt(sql.as_str(), &[&organization_id]).await?;

        Ok(row.as_ref().map(row_to_admin_org_info))
    }

    async fn grant_model_access(
        &self,
        model_id: Uuid,
        organization_id: Uuid,
        granted_by_user_id: Option<Uuid>,
    ) -> Result<Option<(ModelAccessGrant, bool)>> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        // Hold the organization row until the grant is written. Deleting an
        // organization locks the same row to deactivate it and then removes
        // its grants in that transaction, so the two cannot interleave: either
        // the deletion waits and then removes this grant too, or this waits
        // for the deletion and finds the organization inactive. Without the
        // lock a grant could land after the deletion had removed the others.
        let active = transaction
            .query_opt(
                "SELECT 1 FROM organizations WHERE id = $1 AND is_active = true FOR SHARE",
                &[&organization_id],
            )
            .await?
            .is_some();
        if !active {
            return Ok(None);
        }
        // One statement, so the grant and its audit row are written together
        // or not at all. An existing grant keeps its date and its author, and
        // then nothing is written to the audit log either.
        let created = transaction
            .execute(
                r#"
                WITH granted AS (
                    INSERT INTO organization_model_access (organization_id, model_id, created_by_user_id)
                    VALUES ($1, $2, $3)
                    ON CONFLICT (organization_id, model_id) DO NOTHING
                    RETURNING organization_id, model_id, created_by_user_id
                )
                INSERT INTO organization_model_access_log (organization_id, model_id, action, acting_user_id)
                SELECT organization_id, model_id, 'grant', created_by_user_id FROM granted
                "#,
                &[&organization_id, &model_id, &granted_by_user_id],
            )
            .await?
            > 0;
        let row = transaction
            .query_opt(
                &format!(
                    "{MODEL_ACCESS_GRANT_SELECT} WHERE a.model_id = $1 AND a.organization_id = $2"
                ),
                &[&model_id, &organization_id],
            )
            .await?
            .context("the grant is gone: revoked meanwhile")?;
        transaction.commit().await?;

        Ok(Some((row_to_model_access_grant(&row), created)))
    }

    async fn revoke_model_access(
        &self,
        model_id: Uuid,
        organization_id: Uuid,
        revoked_by_user_id: Option<Uuid>,
    ) -> Result<bool> {
        let client = self.pool.get().await?;
        // One statement, so the grant goes and its audit row is written
        // together or not at all. No grant, no audit row.
        let revoked = client
            .execute(
                r#"
                WITH revoked AS (
                    DELETE FROM organization_model_access
                    WHERE organization_id = $1 AND model_id = $2
                    RETURNING organization_id, model_id
                )
                INSERT INTO organization_model_access_log (organization_id, model_id, action, acting_user_id)
                SELECT organization_id, model_id, 'revoke', $3::UUID FROM revoked
                "#,
                &[&organization_id, &model_id, &revoked_by_user_id],
            )
            .await?;

        Ok(revoked > 0)
    }
}
