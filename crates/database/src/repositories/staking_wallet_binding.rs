use crate::pool::DbPool;
use anyhow::Result;
use async_trait::async_trait;
use services::staking_farm::{
    binding::{BindingChallenge, BindingError, BindingRepository, WalletMembership},
    UpsertStakingFarmSourceRequest,
};
use tokio_postgres::Transaction;
use uuid::Uuid;

pub struct PostgresStakingBindingRepository {
    pool: DbPool,
}
impl PostgresStakingBindingRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// Use the same organization lock as deletion and role changes. Lock the actor's
/// membership too so removal cannot authorize a later binding commit.
async fn authorize(tx: &Transaction<'_>, org: Uuid, actor: Uuid, prepaid: bool) -> Result<()> {
    let active = tx
        .query_opt(
            "SELECT id FROM organizations WHERE id=$1 AND is_active=true FOR UPDATE",
            &[&org],
        )
        .await?;
    if active.is_none() {
        return Err(BindingError::Forbidden.into());
    }
    let role = tx.query_opt("SELECT role FROM organization_members WHERE organization_id=$1 AND user_id=$2 FOR UPDATE", &[&org, &actor]).await?;
    if !role.is_some_and(|row| matches!(row.get::<_, String>(0).as_str(), "owner" | "admin")) {
        return Err(BindingError::Forbidden.into());
    }
    if prepaid && tx.query_opt("SELECT id FROM organization_limits_history WHERE organization_id=$1 AND credit_type='postpay' AND effective_until IS NULL", &[&org]).await?.is_some() {
        return Err(BindingError::Unavailable.into());
    }
    Ok(())
}

#[async_trait]
impl BindingRepository for PostgresStakingBindingRepository {
    async fn eligible(&self, org: Uuid) -> Result<bool> {
        let client = self.pool.get().await?;
        Ok(client.query_opt("SELECT id FROM organizations WHERE id=$1 AND is_active=true AND NOT EXISTS(SELECT 1 FROM organization_limits_history WHERE organization_id=$1 AND credit_type='postpay' AND effective_until IS NULL)", &[&org]).await?.is_some())
    }
    async fn wallet_organization(
        &self,
        account: &str,
        actor: Uuid,
        network: &str,
        contract: &str,
    ) -> Result<Option<Uuid>> {
        let client = self.pool.get().await?;
        Ok(client.query_opt("SELECT s.organization_id FROM organization_staking_farm_sources s JOIN organizations o ON o.id=s.organization_id AND o.is_active=true JOIN organization_members m ON m.organization_id=s.organization_id AND m.user_id=$2 WHERE s.near_account_id=$1 AND s.network_id=$3 AND s.contract_id=$4", &[&account,&actor,&network,&contract]).await?.map(|r|r.get(0)))
    }
    async fn begin_attempt(&self, id: Uuid, actor: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        if client.execute("UPDATE staking_wallet_binding_challenges SET verification_attempts=verification_attempts+1 WHERE id=$1 AND actor_user_id=$2 AND consumed_at IS NULL AND expires_at>now() AND verification_attempts<5", &[&id,&actor]).await? == 0 { return Err(BindingError::RateLimited.into()); }
        Ok(())
    }
    async fn prepare(&self, c: &BindingChallenge) -> Result<()> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        authorize(&tx, c.organization_id, c.actor_user_id, true).await?;
        if tx
            .query_opt(
                "SELECT id FROM organization_staking_farm_sources WHERE organization_id=$1",
                &[&c.organization_id],
            )
            .await?
            .is_some()
        {
            return Err(BindingError::Conflict.into());
        }
        // Durable per-actor rate limit across replicas, including different orgs.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 88))",
            &[&c.actor_user_id.to_string()],
        )
        .await?;
        // Retain consumed audit records; expired unused challenges are short-lived.
        tx.execute("DELETE FROM staking_wallet_binding_challenges WHERE actor_user_id=$1 AND consumed_at IS NULL AND expires_at<now() AND created_at<now()-interval '1 day'", &[&c.actor_user_id]).await?;
        let count: i64 = tx.query_one("SELECT count(*) FROM staking_wallet_binding_challenges WHERE actor_user_id=$1 AND created_at > now()-interval '10 minutes'", &[&c.actor_user_id]).await?.get(0);
        if count >= 10 {
            return Err(BindingError::RateLimited.into());
        }
        tx.execute("INSERT INTO staking_wallet_binding_challenges(id, organization_id, actor_user_id, challenge, expires_at) VALUES($1,$2,$3,$4,$5)",
            &[&c.challenge_id, &c.organization_id, &c.actor_user_id, &serde_json::to_value(c)?, &c.expires_at]).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn get_challenge(&self, id: Uuid, org: Uuid, actor: Uuid) -> Result<BindingChallenge> {
        let client = self.pool.get().await?;
        let row = client.query_opt("SELECT challenge FROM staking_wallet_binding_challenges WHERE id=$1 AND organization_id=$2 AND actor_user_id=$3", &[&id,&org,&actor]).await?.ok_or(BindingError::InvalidChallenge)?;
        Ok(serde_json::from_value(row.get(0))?)
    }
    async fn committed(
        &self,
        id: Uuid,
        org: Uuid,
        actor: Uuid,
        key: Uuid,
        digest: &str,
    ) -> Result<Option<WalletMembership>> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        authorize(&tx, org, actor, false).await?;
        let row = tx.query_opt("SELECT idempotency_key, proof_digest, result FROM staking_wallet_binding_challenges WHERE id=$1 AND organization_id=$2 AND actor_user_id=$3 AND consumed_at IS NOT NULL", &[&id,&org,&actor]).await?;
        let result = if let Some(row) = row {
            if row.get::<_, Option<Uuid>>(0) != Some(key)
                || row.get::<_, Option<String>>(1).as_deref() != Some(digest)
            {
                return Err(BindingError::InvalidChallenge.into());
            }
            Some(serde_json::from_value(row.get(2))?)
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }
    async fn confirm(
        &self,
        c: &BindingChallenge,
        r: UpsertStakingFarmSourceRequest,
        key: Uuid,
        public_key: &str,
        digest: &str,
    ) -> Result<WalletMembership> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        authorize(&tx, c.organization_id, c.actor_user_id, true).await?;
        let row = tx.query_opt("SELECT consumed_at, idempotency_key, proof_digest, result FROM staking_wallet_binding_challenges WHERE id=$1 AND organization_id=$2 AND actor_user_id=$3 AND expires_at>now() FOR UPDATE",
            &[&c.challenge_id,&c.organization_id,&c.actor_user_id]).await?.ok_or(BindingError::InvalidChallenge)?;
        if row
            .get::<_, Option<chrono::DateTime<chrono::Utc>>>(0)
            .is_some()
        {
            if row.get::<_, Option<Uuid>>(1) != Some(key)
                || row.get::<_, Option<String>>(2).as_deref() != Some(digest)
            {
                return Err(BindingError::InvalidChallenge.into());
            }
            return Ok(serde_json::from_value(row.get(3))?);
        }
        // Global wallet lock handles concurrent destinations and provisioning races.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 89))",
            &[&format!(
                "{}:{}:{}",
                r.network_id, r.contract_id, r.near_account_id
            )],
        )
        .await?;
        let existing_sources = tx.query("SELECT organization_id, near_account_id, network_id, contract_id FROM organization_staking_farm_sources WHERE (near_account_id=$1 AND network_id=$2 AND contract_id=$3) OR organization_id=$4",
            &[&r.near_account_id,&r.network_id,&r.contract_id,&r.organization_id]).await?;
        if existing_sources.iter().any(|existing| {
            existing.get::<_, Uuid>(0) != r.organization_id
                || existing.get::<_, String>(1) != r.near_account_id
                || existing.get::<_, String>(2) != r.network_id
                || existing.get::<_, String>(3) != r.contract_id
        }) {
            return Err(BindingError::Conflict.into());
        }
        let email = format!("{}@near", r.near_account_id);
        let user = tx.query_one("INSERT INTO users(email,username,display_name,auth_provider,provider_user_id) VALUES($1,$2,$2,'near',$2) ON CONFLICT(auth_provider,provider_user_id) DO UPDATE SET provider_user_id=EXCLUDED.provider_user_id RETURNING id,is_active", &[&email,&r.near_account_id]).await.map_err(|error| {
            if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) {
                anyhow::Error::from(BindingError::Conflict)
            } else { error.into() }
        })?;
        if !user.get::<_, bool>(1) {
            return Err(BindingError::AccountUnavailable.into());
        }
        let wallet_user_id: Uuid = user.get(0);
        let previous = tx.query_opt("SELECT role FROM organization_members WHERE organization_id=$1 AND user_id=$2 FOR UPDATE", &[&r.organization_id,&wallet_user_id]).await?.map(|row| row.get::<_, String>(0));
        // A source already established by this flow must not re-grant access.
        if tx.query_opt("SELECT id FROM staking_wallet_binding_challenges WHERE organization_id=$1 AND wallet_user_id=$2 AND consumed_at IS NOT NULL", &[&r.organization_id,&wallet_user_id]).await?.is_some() {
            return Err(BindingError::Conflict.into());
        }
        let role = tx.query_one("INSERT INTO organization_members(organization_id,user_id,role,invited_by) VALUES($1,$2,'admin',$3) ON CONFLICT(organization_id,user_id) DO UPDATE SET role=CASE WHEN organization_members.role='owner' THEN 'owner' ELSE 'admin' END RETURNING role",
            &[&r.organization_id,&wallet_user_id,&c.actor_user_id]).await?.get::<_, String>(0);
        tx.execute("INSERT INTO organization_staking_farm_sources(organization_id,near_account_id,network_id,contract_id,farm_product_id,farm_price_id,credit_nano_usd_per_reward_unit,status,created_by_user_id) VALUES($1,$2,$3,$4,$5,$6,$7,'active',$8) ON CONFLICT(organization_id,network_id,contract_id) DO NOTHING",
            &[&r.organization_id,&r.near_account_id,&r.network_id,&r.contract_id,&r.farm_product_id,&r.farm_price_id,&r.credit_nano_usd_per_reward_unit,&c.actor_user_id]).await?;
        let result = WalletMembership {
            user_id: wallet_user_id,
            role,
            status: "active".into(),
        };
        tx.execute("UPDATE staking_wallet_binding_challenges SET consumed_at=now(),idempotency_key=$2,proof_digest=$3,public_key=$4,wallet_user_id=$5,previous_role=$6,result=$7 WHERE id=$1",
            &[&c.challenge_id,&key,&digest,&public_key,&wallet_user_id,&previous,&serde_json::to_value(&result)?]).await.map_err(|error| {
                if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) { anyhow::Error::from(BindingError::InvalidChallenge) } else { error.into() }
            })?;
        tx.commit().await?;
        Ok(result)
    }
}
