mod entities;
mod import;
use crate::pool::DbPool;
use anyhow::{bail, Result};
use services::provider_registry::{
    discover_models, ProviderDefinition, ProviderModel, ProviderModelApproval, RegisteredProvider,
};
use tokio_postgres::Row;
use uuid::Uuid;

#[derive(Clone)]
pub struct ProviderRegistryRepository {
    pool: DbPool,
}

fn provider(row: Row) -> RegisteredProvider {
    RegisteredProvider {
        id: row.get("id"),
        definition: ProviderDefinition {
            account_id: Some(row.get("account_id")),
            quota_group: row.get("quota_group"),
            enabled: row.get("enabled"),
            name: row.get("name"),
            config: row.get("config"),
            credential_env: row.get("credential_env"),
            models_path: row.get("models_path"),
            archived: row.get("archived"),
        },
        execution_security: row.get("execution_security"),
        revision: row.get("revision"),
        last_synced_at: row.get("last_synced_at"),
        sync_error: row.get("sync_error"),
    }
}

impl ProviderRegistryRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn routing_policies(
        &self,
    ) -> Result<std::collections::HashMap<String, services::provider_registry::ModelRoutingPolicy>>
    {
        let rows = self.pool.get().await?.query(
            "SELECT m.model_name,
                COALESCE(p.default_tee_requirement, CASE WHEN m.verifiable OR m.attestation_supported OR m.provider_type IN ('chutes','tinfoil') THEN 'required' ELSE 'any' END) AS default_tee_requirement,
                COALESCE(p.mixed_routing_enabled,false) AS mixed_routing_enabled,
                COALESCE(p.max_attempts,4) AS max_attempts, COALESCE(p.deadline_ms,120000) AS deadline_ms,
                p.max_attempt_cost_microusd,p.max_total_cost_microusd,COALESCE(p.allow_negative_margin,false) AS allow_negative_margin
             FROM models m LEFT JOIN model_routing_policies p ON p.model_id=m.id", &[]).await?;
        rows.into_iter()
            .map(|row| {
                let policy = services::provider_registry::ModelRoutingPolicy {
                    default_tee_requirement: serde_json::from_value(serde_json::Value::String(
                        row.get("default_tee_requirement"),
                    ))?,
                    mixed_routing_enabled: row.get("mixed_routing_enabled"),
                    max_attempts: row.get::<_, i32>("max_attempts") as u32,
                    deadline_ms: row.get::<_, i64>("deadline_ms") as u64,
                    max_attempt_cost_microusd: row.get("max_attempt_cost_microusd"),
                    max_total_cost_microusd: row.get("max_total_cost_microusd"),
                    allow_negative_margin: row.get("allow_negative_margin"),
                };
                policy.validate()?;
                Ok((row.get("model_name"), policy))
            })
            .collect()
    }

    pub async fn save_routing_policy(
        &self,
        model_name: &str,
        policy: &services::provider_registry::ModelRoutingPolicy,
        reason: &str,
        actor: Uuid,
    ) -> Result<()> {
        policy.validate()?;
        if reason.trim().is_empty() || reason.len() > 1000 {
            bail!("A change reason of 1..1000 characters is required");
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let id: Uuid = tx
            .query_one(
                "SELECT id FROM models WHERE model_name=$1 FOR UPDATE",
                &[&model_name],
            )
            .await?
            .get(0);
        let tee = match policy.default_tee_requirement {
            services::provider_registry::TeeRequirement::Required => "required",
            services::provider_registry::TeeRequirement::Any => "any",
        };
        tx.execute("INSERT INTO model_routing_policies(model_id,default_tee_requirement,mixed_routing_enabled,max_attempts,deadline_ms,max_attempt_cost_microusd,max_total_cost_microusd,allow_negative_margin,updated_by)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(model_id) DO UPDATE SET
            default_tee_requirement=$2,mixed_routing_enabled=$3,max_attempts=$4,deadline_ms=$5,max_attempt_cost_microusd=$6,max_total_cost_microusd=$7,allow_negative_margin=$8,updated_by=$9,updated_at=now(),revision=model_routing_policies.revision+1",
            &[&id,&tee,&policy.mixed_routing_enabled,&(policy.max_attempts as i32),&(policy.deadline_ms as i64),&policy.max_attempt_cost_microusd,&policy.max_total_cost_microusd,&policy.allow_negative_margin,&actor]).await?;
        tx.execute("INSERT INTO model_routing_policy_history(model_id,changed_by,reason,snapshot) VALUES($1,$2,$3,$4)", &[&id,&actor,&reason,&serde_json::to_value(policy)?]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<RegisteredProvider>> {
        Ok(self
            .pool
            .get()
            .await?
            .query("SELECT * FROM inference_providers ORDER BY name", &[])
            .await?
            .into_iter()
            .map(provider)
            .collect())
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<RegisteredProvider>> {
        Ok(self
            .pool
            .get()
            .await?
            .query_opt("SELECT * FROM inference_providers WHERE id=$1", &[&id])
            .await?
            .map(provider))
    }

    /// Full replacement. Revision guards prevent an older discovery from applying
    /// after credentials or endpoints are changed.
    pub async fn save(
        &self,
        id: Uuid,
        definition: &ProviderDefinition,
        actor: Uuid,
    ) -> Result<RegisteredProvider> {
        definition.validate()?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let account_id = definition.account_id.unwrap_or(id);
        if definition.account_id.is_none() {
            tx.execute("INSERT INTO provider_accounts(id,name,vendor,account_reference) VALUES($1,$2,'unspecified',$2) ON CONFLICT(id) DO NOTHING", &[&account_id,&definition.name]).await?;
        }
        let row = tx.query_one(
            "INSERT INTO inference_providers (id,name,config,credential_env,models_path,archived,updated_by,account_id,quota_group,enabled)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT(id) DO UPDATE SET name=$2,config=$3,credential_env=$4,models_path=$5,archived=$6,updated_by=$7,account_id=$8,quota_group=$9,enabled=$10,
             revision=inference_providers.revision+1,updated_at=now() RETURNING *",
            &[&id,&definition.name,&definition.config,&definition.credential_env,&definition.models_path,&definition.archived,&actor,&account_id,&definition.quota_group,&definition.enabled]).await?;
        let result = provider(row);
        tx.execute("INSERT INTO provider_registry_history(provider_id,changed_by,snapshot) VALUES($1,$2,$3)", &[&id,&actor,&serde_json::to_value(&result)?]).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn models(&self, id: Uuid) -> Result<Vec<ProviderModel>> {
        Ok(self.pool.get().await?.query("SELECT *, COALESCE(reasoning_efforts_override,discovered_reasoning_efforts) AS reasoning_efforts FROM provider_models WHERE provider_id=$1 ORDER BY upstream_model", &[&id]).await?.into_iter().map(|r| ProviderModel {
            id: Some(r.get("id")), upstream_model: r.get("upstream_model"), metadata: r.get("metadata"), available: r.get("available"), model_id: r.get("model_id"), approved: r.get("approved"), archived: r.get("archived"), reasoning_efforts: r.get("reasoning_efforts"),
        }).collect())
    }

    pub async fn save_inventory(
        &self,
        provider_id: Uuid,
        entry: &services::provider_registry::ManualInventoryEntry,
        actor: Uuid,
    ) -> Result<()> {
        entry.validate()?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.query_one(
            "SELECT id FROM inference_providers WHERE id=$1 FOR SHARE",
            &[&provider_id],
        )
        .await?;
        tx.execute("INSERT INTO provider_models(provider_id,upstream_model,available,archived,reasoning_efforts_override,manual_provenance,updated_by,provenance)
            VALUES($1,$2,$3,$4,$5,$6,$7,'manual') ON CONFLICT(provider_id,upstream_model) DO UPDATE SET
            available=$3,archived=$4,reasoning_efforts_override=$5,manual_provenance=$6,updated_by=$7,provenance='manual',updated_at=now()",
            &[&provider_id,&entry.upstream_model,&entry.available,&entry.archived,&entry.reasoning_efforts,&entry.provenance,&actor]).await?;
        tx.execute("INSERT INTO provider_registry_history(provider_id,upstream_model,changed_by,snapshot) VALUES($1,$2,$3,$4)",
            &[&provider_id,&entry.upstream_model,&actor,&serde_json::to_value(entry)?]).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Manual entries support providers without a discovery endpoint. Mapping
    /// preserves the catalog row and never activates an archived model.
    pub async fn approve(
        &self,
        id: Uuid,
        request: &ProviderModelApproval,
        actor: Uuid,
    ) -> Result<()> {
        services::provider_registry::validate_efforts(request.reasoning_efforts.as_deref())?;
        if request.upstream_model.trim().is_empty() || request.upstream_model.len() > 500 {
            bail!("Invalid upstream model name");
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.query_one(
            "SELECT id FROM inference_providers WHERE id=$1 FOR SHARE",
            &[&id],
        )
        .await?;
        // External registry adapters must not silently convert a TEE catalog model.
        let model = tx.query_opt("SELECT id FROM models WHERE model_name=$1 AND provider_type='external' AND NOT verifiable AND NOT attestation_supported FOR UPDATE", &[&request.model_name]).await?;
        let model_id: Uuid = model
            .ok_or_else(|| {
                anyhow::anyhow!("Mapping requires an existing non-TEE external catalog model")
            })?
            .get("id");
        tx.execute(
            "UPDATE models SET provider_registry_managed=true WHERE id=$1",
            &[&model_id],
        )
        .await?;
        tx.execute("INSERT INTO provider_models(provider_id,upstream_model,model_id,approved,archived,reasoning_efforts_override,updated_by)
            VALUES ($1,$2,$3,$4,$5,$6,$7)
            ON CONFLICT(provider_id,upstream_model) DO UPDATE SET model_id=$3,approved=$4,archived=$5,reasoning_efforts_override=$6,updated_by=$7,updated_at=now()",
            &[&id,&request.upstream_model,&model_id,&request.approved,&request.archived,&request.reasoning_efforts,&actor]).await?;
        let inventory_id: Uuid = tx
            .query_one(
                "SELECT id FROM provider_models WHERE provider_id=$1 AND upstream_model=$2",
                &[&id, &request.upstream_model],
            )
            .await?
            .get(0);
        // The legacy mapping API replaces its mapping; independent routes are
        // managed through the route API. Keep old route identities/history.
        tx.execute("UPDATE model_provider_routes SET approved=false,revision=revision+1,updated_at=now() WHERE inventory_id=$1 AND model_id<>$2 AND legacy_managed", &[&inventory_id,&model_id]).await?;
        tx.execute("INSERT INTO model_provider_routes(model_id,inventory_id,approved,archived,reasoning_efforts_override,updated_by,legacy_managed)
            VALUES($1,$2,$3,$4,$5,$6,true) ON CONFLICT(model_id,inventory_id) DO UPDATE SET approved=$3,archived=$4,reasoning_efforts_override=$5,updated_by=$6,revision=model_provider_routes.revision+1,updated_at=now()",
            &[&model_id,&inventory_id,&request.approved,&request.archived,&request.reasoning_efforts,&actor]).await?;
        tx.execute("INSERT INTO provider_registry_history(provider_id,upstream_model,changed_by,snapshot) VALUES($1,$2,$3,$4)", &[&id,&request.upstream_model,&actor,&serde_json::to_value(request)?]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn sync(&self, id: Uuid) -> Result<()> {
        let Some(current) = self.get(id).await? else {
            bail!("Provider not found");
        };
        if current.definition.archived
            || !current.definition.enabled
            || current.definition.models_path.is_none()
        {
            return Ok(());
        }
        // A DB advisory lock coordinates background and manual refresh across replicas.
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let locked: bool = tx
            .query_one(
                "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))",
                &[&format!("provider-discovery:{id}")],
            )
            .await?
            .get(0);
        if !locked {
            return Ok(());
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            discover_models(&current.definition),
        )
        .await;
        let models = match result {
            Ok(Ok(models)) => models,
            _ => {
                tx.execute("UPDATE inference_providers SET next_sync_at=now()+interval '10 minutes',sync_error='Model discovery failed; previous inventory retained' WHERE id=$1 AND revision=$2", &[&id,&current.revision]).await?;
                tx.commit().await?;
                bail!("Model discovery failed; previous inventory retained");
            }
        };
        let revision: i64 = tx
            .query_one(
                "SELECT revision FROM inference_providers WHERE id=$1 FOR UPDATE",
                &[&id],
            )
            .await?
            .get(0);
        if revision != current.revision {
            return Ok(());
        }
        tx.execute(
            "UPDATE provider_models SET available=false WHERE provider_id=$1 AND provenance='discovered' ",
            &[&id],
        )
        .await?;
        for model in models {
            tx.execute("INSERT INTO provider_models(provider_id,upstream_model,metadata,discovered_reasoning_efforts,last_seen_at,provenance)
                VALUES($1,$2,$3,$4,now(),'discovered') ON CONFLICT(provider_id,upstream_model) DO UPDATE SET
                metadata=$3,discovered_reasoning_efforts=$4,available=true,last_seen_at=now(),updated_at=now()",
                &[&id,&model.upstream_model,&model.metadata,&model.reasoning_efforts]).await?;
        }
        tx.execute(
            "UPDATE inference_providers SET last_synced_at=now(),next_sync_at=now()+interval '10 minutes',sync_error=NULL WHERE id=$1",
            &[&id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn sync_due(&self) -> Result<()> {
        let ids: Vec<Uuid> = self.pool.get().await?.query("SELECT id FROM inference_providers WHERE NOT archived AND enabled AND models_path IS NOT NULL AND next_sync_at <= now()", &[]).await?.into_iter().map(|row| row.get(0)).collect();
        for id in ids {
            if self.sync(id).await.is_err() {
                tracing::warn!(provider_id=%id,"Provider inventory refresh failed");
            }
        }
        Ok(())
    }

    pub async fn routes(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let rows = self.pool.get().await?.query("SELECT m.model_name,p.config,p.credential_env,pm.upstream_model,
            COALESCE(r.reasoning_efforts_override,pm.reasoning_efforts_override,pm.discovered_reasoning_efforts) AS reasoning_efforts,
            jsonb_build_object('account_id',p.account_id,'deployment_revision',p.revision,'route_id',r.id,'deployment_id',p.id,'quota_group',COALESCE(p.quota_group,p.credential_env,p.id::text),
                'revision',r.revision,'context_limit',r.context_limit,'output_limit',r.output_limit,'role',r.role,'cost_profile_id',c.id,
                'cost_rates',CASE WHEN c.id IS NULL THEN NULL ELSE jsonb_build_object('currency',c.currency,'billing_unit',c.billing_unit,
                'input_microunits',c.input_microunits,'output_microunits',c.output_microunits,'cache_read_microunits',c.cache_read_microunits,
                'cache_write_microunits',c.cache_write_microunits,'provenance',c.provenance) END) AS route_runtime
            FROM model_provider_routes r JOIN provider_models pm ON pm.id=r.inventory_id
            JOIN inference_providers p ON p.id=pm.provider_id JOIN provider_accounts a ON a.id=p.account_id
            JOIN models m ON m.id=r.model_id
            LEFT JOIN external_registry_imports imported ON imported.deployment_id=p.id
            LEFT JOIN model_routing_policies policy ON policy.model_id=m.id
            LEFT JOIN route_cost_profiles c ON c.route_id=r.id AND c.effective_from<=now() AND (c.effective_until IS NULL OR c.effective_until>now())
            WHERE m.provider_registry_managed AND r.approved AND r.enabled AND NOT r.archived AND NOT pm.archived AND pm.available
              AND (imported.state IS NULL OR imported.state='active')
              AND NOT p.archived AND p.enabled AND NOT a.archived AND m.is_active AND m.archived_at IS NULL
              AND ((m.provider_type='external' AND NOT m.verifiable AND NOT m.attestation_supported) OR policy.mixed_routing_enabled)
            UNION ALL
            SELECT model_name,provider_config,NULL,NULL,NULL,NULL FROM models
            WHERE is_active AND archived_at IS NULL AND provider_type='external' AND NOT provider_registry_managed AND provider_config IS NOT NULL
            ORDER BY model_name,upstream_model", &[]).await?;
        let mut routes = Vec::new();
        for row in rows {
            let mut config: serde_json::Value = row.get("config");
            let upstream: Option<String> = row.get("upstream_model");
            let Some(upstream) = upstream else {
                if let Some(reference) = config
                    .as_object_mut()
                    .and_then(|object| object.remove("credential_env"))
                {
                    let key = reference
                        .as_str()
                        .and_then(|name| std::env::var(name).ok())
                        .filter(|key| !key.is_empty());
                    let Some(key) = key else {
                        continue;
                    };
                    config["api_key"] = key.into();
                }
                routes.push((row.get("model_name"), config));
                continue;
            };
            let env: Option<String> = row.get("credential_env");
            // Resolve each route independently: a missing key must not remove
            // unrelated providers or cause fallback to a different account key.
            let key = if let Some(env) = env {
                match std::env::var(env).ok().filter(|v| !v.is_empty()) {
                    Some(key) => key,
                    None => {
                        tracing::warn!("Skipping provider route with unavailable credentials");
                        continue;
                    }
                }
            } else {
                String::new()
            };
            config["_registry_route"] = row.get::<_, serde_json::Value>("route_runtime");
            config["api_key"] = key.into();
            config["model_name"] = upstream.into();
            if let Some(efforts) = row.get::<_, Option<Vec<String>>>("reasoning_efforts") {
                config["reasoning_efforts"] = serde_json::to_value(efforts)?;
            }
            routes.push((row.get("model_name"), config));
        }
        Ok(routes)
    }
}
