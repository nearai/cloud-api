use super::ProviderRegistryRepository;
use anyhow::{bail, Result};
use services::provider_registry::entities::{
    AttemptRepository, InferenceAttempt, ModelRoute, ProviderAccount, RouteCostRates,
};
use tokio_postgres::{GenericClient, Row};
use uuid::Uuid;

fn route(row: Row) -> ModelRoute {
    ModelRoute {
        id: row.get("id"),
        model_id: row.get("model_id"),
        inventory_id: row.get("inventory_id"),
        approved: row.get("approved"),
        enabled: row.get("enabled"),
        archived: row.get("archived"),
        reasoning_efforts: row.get("reasoning_efforts_override"),
        context_limit: row.get("context_limit"),
        output_limit: row.get("output_limit"),
        role: row.get("role"),
    }
}

fn validate_reason(reason: &str) -> Result<()> {
    if reason.trim().is_empty() || reason.len() > 1000 {
        bail!("A change reason of 1..1000 characters is required");
    }
    Ok(())
}

async fn audit(
    tx: &(impl GenericClient + Sync),
    kind: &str,
    id: Uuid,
    actor: Uuid,
    reason: &str,
    snapshot: serde_json::Value,
) -> Result<()> {
    tx.execute("INSERT INTO registry_entity_history(entity_kind,entity_id,changed_by,reason,snapshot) VALUES($1,$2,$3,$4,$5)", &[&kind,&id,&actor,&reason,&snapshot]).await?;
    Ok(())
}

impl ProviderRegistryRepository {
    pub async fn organization_tee_policy(&self, organization_id: Uuid) -> Result<bool> {
        Ok(self.pool.get().await?.query_one(
            "SELECT COALESCE(p.tee_required,false) FROM organizations o LEFT JOIN organization_inference_policies p ON p.organization_id=o.id WHERE o.id=$1",
            &[&organization_id],
        ).await?.get(0))
    }

    pub async fn save_organization_tee_policy(
        &self,
        organization_id: Uuid,
        required: bool,
        actor: Uuid,
        reason: &str,
    ) -> Result<()> {
        validate_reason(reason)?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.execute("INSERT INTO organization_inference_policies(organization_id,tee_required,updated_by) VALUES($1,$2,$3) ON CONFLICT(organization_id) DO UPDATE SET tee_required=$2,updated_by=$3,updated_at=now()", &[&organization_id,&required,&actor]).await?;
        tx.execute("INSERT INTO organization_inference_policy_history(organization_id,tee_required,changed_by,reason) VALUES($1,$2,$3,$4)", &[&organization_id,&required,&actor,&reason]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn attempts(&self, accepted_request_id: Uuid) -> Result<Vec<serde_json::Value>> {
        Ok(self.pool.get().await?.query(
            "SELECT to_jsonb(a) AS attempt FROM inference_attempts a WHERE accepted_request_id=$1 ORDER BY started_at,attempt_index LIMIT 1000",
            &[&accepted_request_id],
        ).await?.into_iter().map(|row| row.get("attempt")).collect())
    }

    /// Idempotent reconciliation; a conflicting second settlement is rejected.
    pub async fn settle_attempt(
        &self,
        id: Uuid,
        amount: i64,
        evidence: &str,
        actor: Uuid,
    ) -> Result<()> {
        validate_reason(evidence)?;
        if amount < 0 {
            bail!("Actual cost cannot be negative");
        }
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.query_one(
            "SELECT id FROM inference_attempts WHERE id=$1 FOR UPDATE",
            &[&id],
        )
        .await?;
        if let Some(row) = tx.query_opt("SELECT actual_cost_microusd,evidence FROM inference_attempt_settlements WHERE attempt_id=$1", &[&id]).await? {
            if row.get::<_,i64>("actual_cost_microusd") != amount || row.get::<_,String>("evidence") != evidence {
                bail!("Attempt already has a different settlement");
            }
        } else {
            tx.execute("INSERT INTO inference_attempt_settlements(attempt_id,actual_cost_microusd,evidence,changed_by) VALUES($1,$2,$3,$4)", &[&id,&amount,&evidence,&actor]).await?;
        }
        tx.execute("UPDATE inference_attempts SET actual_cost_microusd=$2,cost_status='settled' WHERE id=$1", &[&id,&amount]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn accounts(&self) -> Result<Vec<ProviderAccount>> {
        Ok(self
            .pool
            .get()
            .await?
            .query("SELECT * FROM provider_accounts ORDER BY name", &[])
            .await?
            .into_iter()
            .map(|row| ProviderAccount {
                id: row.get("id"),
                name: row.get("name"),
                vendor: row.get("vendor"),
                account_reference: row.get("account_reference"),
                archived: row.get("archived"),
            })
            .collect())
    }

    pub async fn save_account(
        &self,
        account: &ProviderAccount,
        actor: Uuid,
        reason: &str,
    ) -> Result<()> {
        account.validate()?;
        validate_reason(reason)?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.execute("INSERT INTO provider_accounts(id,name,vendor,account_reference,archived) VALUES($1,$2,$3,$4,$5)
            ON CONFLICT(id) DO UPDATE SET name=$2,vendor=$3,account_reference=$4,archived=$5,updated_at=now()",
            &[&account.id,&account.name,&account.vendor,&account.account_reference,&account.archived]).await?;
        audit(
            &*tx,
            "account",
            account.id,
            actor,
            reason,
            serde_json::to_value(account)?,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn model_routes(&self, model_name: &str) -> Result<Vec<ModelRoute>> {
        Ok(self.pool.get().await?.query("SELECT r.* FROM model_provider_routes r JOIN models m ON m.id=r.model_id WHERE m.model_name=$1 ORDER BY r.id", &[&model_name]).await?.into_iter().map(route).collect())
    }

    pub async fn save_route(&self, route: &ModelRoute, actor: Uuid, reason: &str) -> Result<()> {
        route.validate()?;
        validate_reason(reason)?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let model = tx.query_one("SELECT m.provider_type,m.verifiable,m.attestation_supported,COALESCE(p.mixed_routing_enabled,false) AS mixed FROM models m LEFT JOIN model_routing_policies p ON p.model_id=m.id WHERE m.id=$1 FOR UPDATE OF m", &[&route.model_id]).await?;
        if !model.get::<_, bool>("mixed")
            && (model.get::<_, String>("provider_type") != "external"
                || model.get::<_, bool>("verifiable")
                || model.get::<_, bool>("attestation_supported"))
        {
            bail!("Mixed routes require an explicitly enabled routing policy");
        }
        let changed = tx.execute("INSERT INTO model_provider_routes(id,model_id,inventory_id,approved,enabled,archived,reasoning_efforts_override,context_limit,output_limit,role,updated_by)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(id) DO UPDATE SET
            legacy_managed=false,approved=$4,enabled=$5,archived=$6,reasoning_efforts_override=$7,context_limit=$8,output_limit=$9,role=$10,updated_by=$11,revision=model_provider_routes.revision+1,updated_at=now()
            WHERE model_provider_routes.model_id=$2 AND model_provider_routes.inventory_id=$3",
            &[&route.id,&route.model_id,&route.inventory_id,&route.approved,&route.enabled,&route.archived,&route.reasoning_efforts,&route.context_limit,&route.output_limit,&route.role,&actor]).await?;
        if changed != 1 {
            bail!("Route identity is immutable");
        }
        tx.execute(
            "UPDATE models SET provider_registry_managed=true WHERE id=$1",
            &[&route.model_id],
        )
        .await?;
        audit(
            &*tx,
            "route",
            route.id,
            actor,
            reason,
            serde_json::to_value(route)?,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_cost_profile(
        &self,
        route_id: Uuid,
        rates: &RouteCostRates,
        actor: Uuid,
        reason: &str,
    ) -> Result<Uuid> {
        rates.validate()?;
        validate_reason(reason)?;
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        // Route lock serializes effective intervals without a catalog-wide lock.
        tx.query_one(
            "SELECT id FROM model_provider_routes WHERE id=$1 FOR UPDATE",
            &[&route_id],
        )
        .await?;
        let effective: chrono::DateTime<chrono::Utc> =
            tx.query_one("SELECT clock_timestamp()", &[]).await?.get(0);
        tx.execute("UPDATE route_cost_profiles SET effective_until=$2 WHERE route_id=$1 AND effective_until IS NULL", &[&route_id,&effective]).await?;
        let row = tx.query_one("INSERT INTO route_cost_profiles(route_id,version,currency,billing_unit,input_microunits,output_microunits,cache_read_microunits,cache_write_microunits,effective_from,provenance)
            SELECT $1,COALESCE(MAX(version),0)+1,$2,$3,$4,$5,$6,$7,$8,$9 FROM route_cost_profiles WHERE route_id=$1 RETURNING id",
            &[&route_id,&rates.currency,&rates.billing_unit,&rates.input_microunits,&rates.output_microunits,&rates.cache_read_microunits,&rates.cache_write_microunits,&effective,&rates.provenance]).await?;
        let id = row.get(0);
        audit(
            &*tx,
            "cost_profile",
            id,
            actor,
            reason,
            serde_json::to_value(rates)?,
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn cost_profiles(&self, route_id: Uuid) -> Result<Vec<serde_json::Value>> {
        Ok(self
            .pool
            .get()
            .await?
            .query(
                "SELECT to_jsonb(c) FROM route_cost_profiles c WHERE route_id=$1 ORDER BY version",
                &[&route_id],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }
}

#[async_trait::async_trait]
impl AttemptRepository for ProviderRegistryRepository {
    async fn claim_quota(&self, group: &str) -> Result<bool> {
        let client = self.pool.get().await?;
        if client
            .query_opt(
                "SELECT quota_group FROM provider_quota_health WHERE quota_group=$1",
                &[&group],
            )
            .await?
            .is_none()
        {
            return Ok(true);
        }
        Ok(client.execute("UPDATE provider_quota_health SET probe_until=clock_timestamp()+interval '30 seconds' WHERE quota_group=$1 AND cooldown_until<=clock_timestamp() AND (probe_until IS NULL OR probe_until<=clock_timestamp())", &[&group]).await? == 1)
    }

    async fn record_health(
        &self,
        group: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        cooldown_ms: Option<u64>,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        if let Some(delay) = cooldown_ms {
            let delay_ms = delay.clamp(1000, 300000) as i64;
            client.execute("INSERT INTO provider_quota_health(quota_group,last_failed_at,cooldown_until) VALUES($1,clock_timestamp(),clock_timestamp()+$2::bigint*interval '1 millisecond')
                ON CONFLICT(quota_group) DO UPDATE SET failures=provider_quota_health.failures+1,last_failed_at=clock_timestamp(),cooldown_until=GREATEST(provider_quota_health.cooldown_until,EXCLUDED.cooldown_until),probe_until=NULL", &[&group,&delay_ms]).await?;
        } else {
            client
                .execute(
                    "DELETE FROM provider_quota_health WHERE quota_group=$1 AND last_failed_at<=$2",
                    &[&group, &started_at],
                )
                .await?;
        }
        Ok(())
    }

    async fn start(&self, attempt: &InferenceAttempt) -> Result<()> {
        let tee = serde_json::to_value(attempt.effective_tee)?
            .as_str()
            .unwrap()
            .to_owned();
        self.pool.get().await?.execute("INSERT INTO inference_attempts(request_id,attempt_index,route_id,deployment_id,route_revision,cost_profile_id,effective_tee,reasoning_effort,outcome,fallback_reason,estimated_cost_microusd,cost_status,accepted_request_id,account_id,deployment_revision)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,'started',$9,$10,'unknown',$11,$12,$13)",
            &[&attempt.request_id,&attempt.attempt_index,&attempt.route.route_id,&attempt.route.deployment_id,&attempt.route.revision,&attempt.route.cost_profile_id,&tee,&attempt.reasoning_effort,&attempt.fallback_reason,&attempt.estimated_cost_microusd,&attempt.accepted_request_id,&attempt.route.account_id,&attempt.route.deployment_revision]).await?;
        Ok(())
    }

    async fn finish(&self, attempt: &InferenceAttempt) -> Result<()> {
        let usage_cost = attempt.route.cost_rates.as_ref().and_then(|rates| {
            if rates.billing_unit == "million_tokens" {
                attempt
                    .usage
                    .as_ref()
                    .and_then(|usage| rates.usage_cost_usd(usage))
            } else {
                attempt
                    .billable_quantity
                    .as_ref()
                    .and_then(|quantity| rates.quantity_cost_usd(quantity))
            }
        });
        let quantity = attempt
            .billable_quantity
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let input_tokens = attempt
            .usage
            .as_ref()
            .map(|usage| i64::from(usage.prompt_tokens));
        let output_tokens = attempt
            .usage
            .as_ref()
            .map(|usage| i64::from(usage.completion_tokens));
        self.pool.get().await?.execute("UPDATE inference_attempts SET outcome=$3,latency_ms=$4,finished_at=clock_timestamp(),usage_cost_microusd=$5,input_tokens=$6,output_tokens=$7,billable_quantity=$8,
            cost_status=CASE WHEN actual_cost_microusd IS NOT NULL THEN 'settled' WHEN $3='success' AND $5::bigint IS NOT NULL THEN 'estimated' ELSE 'unknown' END WHERE request_id=$1 AND attempt_index=$2",
            &[&attempt.request_id,&attempt.attempt_index,&attempt.outcome,&attempt.latency_ms,&usage_cost,&input_tokens,&output_tokens,&quantity]).await?;
        Ok(())
    }
}
