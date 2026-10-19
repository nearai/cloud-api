//! Staged imports preserve catalog identity and sanitize the rollback path.
use super::ProviderRegistryRepository;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use services::provider_registry::{LegacyExternalImport, ProviderDefinition};
use uuid::Uuid;

impl ProviderRegistryRepository {
    pub async fn import_external(
        &self,
        request: &LegacyExternalImport,
        actor: Uuid,
    ) -> Result<Value> {
        if !matches!(
            request.phase.as_str(),
            "plan" | "shadow" | "cutover" | "rollback"
        ) || request.change_reason.trim().is_empty()
            || request.change_reason.len() > 1000
        {
            bail!("Invalid import phase or change reason");
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let model = tx.query_one("SELECT id,provider_config,provider_registry_managed FROM models WHERE model_name=$1 AND provider_type='external' AND NOT verifiable AND NOT attestation_supported FOR UPDATE", &[&request.model_name]).await?;
        let id: Uuid = model.get("id");
        let prior = tx
            .query_opt(
                "SELECT * FROM external_registry_imports WHERE model_id=$1",
                &[&id],
            )
            .await?;
        if model.get::<_, bool>("provider_registry_managed") && prior.is_none() {
            bail!("Model already has independently managed registry routes");
        }
        let original: Value = model
            .get::<_, Option<Value>>("provider_config")
            .ok_or_else(|| anyhow::anyhow!("Legacy external configuration is missing"))?;
        let mut definition_config = original.clone();
        let object = definition_config
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Invalid legacy configuration"))?;
        let upstream = object
            .remove("model_name")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| request.model_name.clone());
        let inline_key = object.remove("api_key").filter(|value| !value.is_null());
        let existing_ref = object
            .remove("credential_env")
            .and_then(|value| value.as_str().map(str::to_owned));
        let credential_env = request.credential_env.clone().or(existing_ref);
        let definition = ProviderDefinition {
            account_id: Some(id),
            quota_group: None,
            enabled: true,
            name: format!("legacy-{id}"),
            config: definition_config.clone(),
            credential_env: credential_env.clone(),
            models_path: None,
            archived: false,
        };
        definition.validate()?;
        // Every imported authenticated deployment must bind to a provisioned
        // reference. Do not guess a global adapter key or emit the old secret.
        let resolved = credential_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
        if inline_key.is_some()
            && (resolved.is_none()
                || inline_key.as_ref().and_then(Value::as_str) != resolved.as_deref())
        {
            bail!("Provision a credential reference matching the legacy credential before import");
        }
        if credential_env.is_some() && resolved.is_none() {
            bail!("Credential reference unavailable");
        }
        if inline_key.is_none() && credential_env.is_none() {
            bail!("An explicit credential reference is required for legacy global-key imports");
        }
        let mut sanitized = definition_config.clone();
        sanitized["model_name"] = json!(upstream);
        sanitized["credential_env"] = json!(credential_env);
        let configuration_matches = if prior.is_some() {
            let deployment = tx
                .query_one(
                    "SELECT config,credential_env FROM inference_providers WHERE id=$1 FOR SHARE",
                    &[&id],
                )
                .await?;
            deployment.get::<_, Value>("config") == definition_config
                && deployment.get::<_, Option<String>>("credential_env") == credential_env
        } else {
            true
        };
        let snapshot_matches = prior
            .as_ref()
            .is_none_or(|prior| prior.get::<_, Value>("legacy_config") == sanitized);
        let report = json!({"model_id":id,"model_name":request.model_name,"deployment_id":id,"upstream_model":upstream,"phase":request.phase,"credential_env":credential_env,"catalog_identity_preserved":true,
            "shadow_configuration_matches":configuration_matches,"legacy_snapshot_matches":snapshot_matches});
        if request.phase == "plan" {
            return Ok(report);
        }
        if request.phase == "cutover" && !configuration_matches {
            bail!("Deployment changed after the shadow comparison");
        }
        if let Some(prior) = &prior {
            if prior.get::<_, Value>("legacy_config") != sanitized {
                bail!("Legacy configuration changed after the import snapshot");
            }
        } else if request.phase != "shadow" {
            bail!("Create and review a shadow import before cutover or rollback");
        }
        if request.phase == "shadow" && prior.is_none() {
            tx.execute("INSERT INTO provider_accounts(id,name,vendor,account_reference) VALUES($1,$2,'legacy',$2)", &[&id,&definition.name]).await?;
            tx.execute("INSERT INTO inference_providers(id,name,config,credential_env,models_path,updated_by,account_id) VALUES($1,$2,$3,$4,NULL,$5,$1)", &[&id,&definition.name,&definition_config,&credential_env,&actor]).await?;
            let inventory: Uuid = tx.query_one("INSERT INTO provider_models(provider_id,upstream_model,manual_provenance) VALUES($1,$2,'legacy catalog import') RETURNING id", &[&id,&upstream]).await?.get(0);
            tx.execute("INSERT INTO model_provider_routes(model_id,inventory_id,approved,updated_by) VALUES($1,$2,true,$3)", &[&id,&inventory,&actor]).await?;
            tx.execute("INSERT INTO external_registry_imports(model_id,deployment_id,legacy_config,state,updated_by) VALUES($1,$1,$2,'shadow',$3)", &[&id,&sanitized,&actor]).await?;
        }
        let new_state = match request.phase.as_str() {
            "cutover" => Some("active"),
            "rollback" => Some("rollback"),
            _ => None,
        };
        if let Some(state) = new_state {
            let managed = state == "active";
            tx.execute(
                "UPDATE models SET provider_registry_managed=$2,provider_config=$3 WHERE id=$1",
                &[&id, &managed, &sanitized],
            )
            .await?;
            tx.execute("UPDATE external_registry_imports SET state=$2,updated_by=$3,updated_at=now() WHERE model_id=$1", &[&id,&state,&actor]).await?;
        }
        let changed = prior.is_none()
            || new_state
                .is_some_and(|state| prior.as_ref().unwrap().get::<_, String>("state") != state);
        if changed {
            tx.execute("INSERT INTO registry_entity_history(entity_kind,entity_id,changed_by,reason,snapshot) VALUES('import',$1,$2,$3,$4)", &[&id,&actor,&request.change_reason,&report]).await?;
        }
        tx.commit().await?;
        Ok(report)
    }
}
