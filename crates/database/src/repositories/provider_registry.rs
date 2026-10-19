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
        let row = tx.query_one(
            "INSERT INTO inference_providers (id,name,config,credential_env,models_path,archived,updated_by)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT(id) DO UPDATE SET name=$2,config=$3,credential_env=$4,models_path=$5,archived=$6,updated_by=$7,
             revision=inference_providers.revision+1,updated_at=now() RETURNING *",
            &[&id,&definition.name,&definition.config,&definition.credential_env,&definition.models_path,&definition.archived,&actor]).await?;
        let result = provider(row);
        tx.execute("INSERT INTO provider_registry_history(provider_id,changed_by,snapshot) VALUES($1,$2,$3)", &[&id,&actor,&serde_json::to_value(&result)?]).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn models(&self, id: Uuid) -> Result<Vec<ProviderModel>> {
        Ok(self.pool.get().await?.query("SELECT *, COALESCE(reasoning_efforts_override,discovered_reasoning_efforts) AS reasoning_efforts FROM provider_models WHERE provider_id=$1 ORDER BY upstream_model", &[&id]).await?.into_iter().map(|r| ProviderModel {
            upstream_model: r.get("upstream_model"), metadata: r.get("metadata"), available: r.get("available"), model_id: r.get("model_id"), approved: r.get("approved"), archived: r.get("archived"), reasoning_efforts: r.get("reasoning_efforts"),
        }).collect())
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
        tx.execute("INSERT INTO provider_registry_history(provider_id,upstream_model,changed_by,snapshot) VALUES($1,$2,$3,$4)", &[&id,&request.upstream_model,&actor,&serde_json::to_value(request)?]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn sync(&self, id: Uuid) -> Result<()> {
        let Some(current) = self.get(id).await? else {
            bail!("Provider not found");
        };
        if current.definition.archived || current.definition.models_path.is_none() {
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
        let ids: Vec<Uuid> = self.pool.get().await?.query("SELECT id FROM inference_providers WHERE NOT archived AND models_path IS NOT NULL AND next_sync_at <= now()", &[]).await?.into_iter().map(|row| row.get(0)).collect();
        for id in ids {
            if self.sync(id).await.is_err() {
                tracing::warn!(provider_id=%id,"Provider inventory refresh failed");
            }
        }
        Ok(())
    }

    pub async fn routes(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let rows = self.pool.get().await?.query("SELECT m.model_name,p.config,p.credential_env,pm.upstream_model,
            COALESCE(pm.reasoning_efforts_override,pm.discovered_reasoning_efforts) AS reasoning_efforts
            FROM provider_models pm JOIN inference_providers p ON p.id=pm.provider_id JOIN models m ON m.id=pm.model_id
            WHERE pm.approved AND NOT pm.archived AND pm.available AND NOT p.archived AND m.is_active
              AND m.provider_type='external' AND NOT m.verifiable AND NOT m.attestation_supported
            UNION ALL
            SELECT model_name,provider_config,NULL,NULL,NULL FROM models
            WHERE is_active AND provider_type='external' AND NOT provider_registry_managed AND provider_config IS NOT NULL
            ORDER BY model_name,upstream_model", &[]).await?;
        let mut routes = Vec::new();
        for row in rows {
            let mut config: serde_json::Value = row.get("config");
            let upstream: Option<String> = row.get("upstream_model");
            let Some(upstream) = upstream else {
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
