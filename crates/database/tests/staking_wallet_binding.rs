use database::repositories::PostgresStakingBindingRepository;
use services::staking_farm::{
    binding::{
        BindingChallenge, BindingError, BindingPayload, BindingRepository, BINDING_TERMS_VERSION,
    },
    UpsertStakingFarmSourceRequest,
};
use uuid::Uuid;
#[path = "support/service_usage_reporting_pool.rs"]
mod service_usage_reporting_pool;

async fn fixture() -> anyhow::Result<(database::DbPool, Uuid, Uuid)> {
    let pool = service_usage_reporting_pool::test_pool().await?;
    let client = pool.get().await?;
    let actor = Uuid::new_v4();
    let org = Uuid::new_v4();
    client.execute("INSERT INTO users(id,email,username,auth_provider,provider_user_id) VALUES($1,$2,'binding-test','google',$2)", &[&actor,&format!("{actor}@example.test")]).await?;
    client
        .execute(
            "INSERT INTO organizations(id,name) VALUES($1,$2)",
            &[&org, &format!("binding-{org}")],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES($1,$2,'owner')",
            &[&org, &actor],
        )
        .await?;
    Ok((pool, org, actor))
}
fn challenge(org: Uuid, actor: Uuid, account: String) -> BindingChallenge {
    BindingChallenge {
        phase: "prepare".into(),
        challenge_id: Uuid::new_v4(),
        organization_id: org,
        actor_user_id: actor,
        near_account_id: account,
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        payload: BindingPayload {
            message: "Verified in service tests".into(),
            nonce: vec![1; 32],
            recipient: "cloud.example".into(),
        },
        binding_terms_version: BINDING_TERMS_VERSION.into(),
        network_id: "testnet".into(),
        contract_id: "stake.testnet".into(),
        farm_product_id: "cloud".into(),
        farm_price_id: None,
        credit_nano_usd_per_reward_unit: 1_000_000_000,
    }
}
fn request(c: &BindingChallenge) -> UpsertStakingFarmSourceRequest {
    UpsertStakingFarmSourceRequest {
        organization_id: c.organization_id,
        near_account_id: c.near_account_id.clone(),
        network_id: c.network_id.clone(),
        contract_id: c.contract_id.clone(),
        farm_product_id: c.farm_product_id.clone(),
        farm_price_id: None,
        credit_nano_usd_per_reward_unit: c.credit_nano_usd_per_reward_unit,
        created_by_user_id: Some(c.actor_user_id),
    }
}
fn account() -> String {
    format!("w{}.testnet", Uuid::new_v4().simple())
}

#[tokio::test]
async fn binding_provisions_admin_atomically_and_retry_does_not_restore_removed_access(
) -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account());
    let key = Uuid::new_v4();
    repo.prepare(&c).await?;
    let client = pool.get().await?;
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_staking_farm_sources WHERE organization_id=$1",
                &[&org]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM users WHERE auth_provider='near' AND provider_user_id=$1",
                &[&c.near_account_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    let result = repo.confirm(&c, request(&c), key, "key", "digest").await?;
    assert_eq!(result.role, "admin");
    assert_eq!(
        client
            .query_one(
                "SELECT role FROM organization_members WHERE organization_id=$1 AND user_id=$2",
                &[&org, &result.user_id]
            )
            .await?
            .get::<_, String>(0),
        "admin"
    );
    assert_eq!(
        repo.wallet_organization(
            &c.near_account_id,
            result.user_id,
            "testnet",
            "stake.testnet"
        )
        .await?,
        Some(org)
    );
    client
        .execute(
            "DELETE FROM organization_members WHERE organization_id=$1 AND user_id=$2",
            &[&org, &result.user_id],
        )
        .await?;
    assert!(repo
        .committed(c.challenge_id, org, actor, key, "other proof")
        .await
        .is_err());
    assert!(repo
        .committed(c.challenge_id, org, actor, key, "digest")
        .await?
        .is_some());
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_members WHERE organization_id=$1 AND user_id=$2",
                &[&org, &result.user_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        repo.wallet_organization(
            &c.near_account_id,
            result.user_id,
            "testnet",
            "stake.testnet"
        )
        .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn revoked_actor_and_conflicting_wallet_leave_no_partial_binding_or_membership(
) -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account());
    repo.prepare(&c).await?;
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE organization_members SET role='member' WHERE organization_id=$1 AND user_id=$2",
            &[&org, &actor],
        )
        .await?;
    let error = repo
        .confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<BindingError>().is_some());
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM users WHERE provider_user_id=$1",
                &[&c.near_account_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    client
        .execute(
            "UPDATE organization_members SET role='owner' WHERE organization_id=$1 AND user_id=$2",
            &[&org, &actor],
        )
        .await?;
    let first = repo
        .confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
        .await?;
    let d = challenge(org, actor, account());
    assert!(repo.prepare(&d).await.is_err());
    assert!(repo
        .confirm(&d, request(&d), Uuid::new_v4(), "key", "digest2")
        .await
        .is_err());
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM users WHERE provider_user_id=$1",
                &[&d.near_account_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_members WHERE organization_id=$1",
                &[&org]
            )
            .await?
            .get::<_, i64>(0),
        2
    );
    assert_eq!(first.role, "admin");
    Ok(())
}

#[tokio::test]
async fn existing_wallet_member_is_promoted_owner_is_preserved_and_disabled_identity_is_rejected(
) -> anyhow::Result<()> {
    for (existing, active) in [
        ("member", true),
        ("admin", true),
        ("owner", true),
        ("member", false),
    ] {
        let (pool, org, actor) = fixture().await?;
        let repo = PostgresStakingBindingRepository::new(pool.clone());
        let c = challenge(org, actor, account());
        let client = pool.get().await?;
        let user = Uuid::new_v4();
        client.execute("INSERT INTO users(id,email,username,auth_provider,provider_user_id,is_active) VALUES($1,$2,'wallet','near',$3,$4)", &[&user,&format!("{}@near",c.near_account_id),&c.near_account_id,&active]).await?;
        client
            .execute(
                "INSERT INTO organization_members(organization_id,user_id,role) VALUES($1,$2,$3)",
                &[&org, &user, &existing],
            )
            .await?;
        repo.prepare(&c).await?;
        let result = repo
            .confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
            .await;
        if active {
            let result = result?;
            assert_eq!(result.user_id, user);
            assert_eq!(
                result.role,
                if existing == "owner" {
                    "owner"
                } else {
                    "admin"
                }
            );
            if existing != "owner" {
                let first = client.query_one(
                    "SELECT role,organization_id FROM organization_members WHERE user_id=$1 ORDER BY joined_at,organization_id LIMIT 1",
                    &[&user],
                ).await?;
                assert_eq!(first.get::<_, String>(0), "owner");
                assert_ne!(first.get::<_, Uuid>(1), org);
            }
        } else {
            assert!(result.is_err());
            assert_eq!(client.query_one("SELECT count(*) FROM organization_staking_farm_sources WHERE organization_id=$1", &[&org]).await?.get::<_,i64>(0),0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_confirmations_choose_one_wallet_and_postpay_blocks_binding(
) -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let a = challenge(org, actor, account());
    let b = challenge(org, actor, account());
    repo.prepare(&a).await?;
    repo.prepare(&b).await?;
    let (a_result, b_result) = tokio::join!(
        repo.confirm(&a, request(&a), Uuid::new_v4(), "key", "a"),
        repo.confirm(&b, request(&b), Uuid::new_v4(), "key", "b")
    );
    assert_ne!(a_result.is_ok(), b_result.is_ok());
    let client = pool.get().await?;
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_staking_farm_sources WHERE organization_id=$1",
                &[&org]
            )
            .await?
            .get::<_, i64>(0),
        1
    );
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account());
    repo.prepare(&c).await?;
    let client = pool.get().await?;
    client.execute("INSERT INTO organization_limits_history(organization_id,spend_limit,credit_type) VALUES($1,100,'postpay')", &[&org]).await?;
    assert!(!repo.eligible(org).await?);
    assert!(repo
        .confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn configuration_changes_cannot_create_a_second_organization_source() -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let a = challenge(org, actor, account());
    let mut b = challenge(org, actor, account());
    b.network_id = "mainnet".into();
    b.contract_id = "new-staking.near".into();
    repo.prepare(&a).await?;
    repo.prepare(&b).await?;
    repo.confirm(&a, request(&a), Uuid::new_v4(), "key", "a")
        .await?;
    let error = repo
        .confirm(&b, request(&b), Uuid::new_v4(), "key", "b")
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<BindingError>(),
        Some(BindingError::Conflict)
    ));
    assert!(repo.prepare(&b).await.is_err());
    let client = pool.get().await?;
    let row = client.query_one("SELECT count(*), min(near_account_id) FROM organization_staking_farm_sources WHERE organization_id=$1", &[&org]).await?;
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, String>(1), a.near_account_id);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM users WHERE auth_provider='near' AND provider_user_id=$1",
                &[&b.near_account_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    Ok(())
}

#[tokio::test]
async fn synthetic_email_collision_returns_typed_conflict_and_rolls_back() -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account());
    let client = pool.get().await?;
    let existing = Uuid::new_v4();
    client.execute("INSERT INTO users(id,email,username,auth_provider,provider_user_id) VALUES($1,$2,'other-provider','google',$3)", &[&existing, &format!("{}@near", c.near_account_id), &existing.to_string()]).await?;
    repo.prepare(&c).await?;
    let error = repo
        .confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<BindingError>(),
        Some(BindingError::Conflict)
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_staking_farm_sources WHERE organization_id=$1",
                &[&org]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM users WHERE auth_provider='near' AND provider_user_id=$1",
                &[&c.near_account_id]
            )
            .await?
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_members WHERE organization_id=$1",
                &[&org]
            )
            .await?
            .get::<_, i64>(0),
        1
    );
    assert!(client
        .query_one(
            "SELECT consumed_at FROM staking_wallet_binding_challenges WHERE id=$1",
            &[&c.challenge_id]
        )
        .await?
        .get::<_, Option<chrono::DateTime<chrono::Utc>>>(0)
        .is_none());
    Ok(())
}

#[tokio::test]
async fn challenge_cleanup_preserves_consumed_audit_and_recent_rate_limit_history(
) -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let old = challenge(org, actor, account());
    let consumed = challenge(org, actor, account());
    let recent = challenge(org, actor, account());
    for c in [&old, &consumed, &recent] {
        repo.prepare(c).await?;
    }
    let client = pool.get().await?;
    client.execute("UPDATE staking_wallet_binding_challenges SET created_at=now()-interval '2 days', expires_at=now()-interval '1 day' WHERE id IN ($1,$2)", &[&old.challenge_id, &consumed.challenge_id]).await?;
    client.execute("UPDATE staking_wallet_binding_challenges SET consumed_at=now()-interval '1 day' WHERE id=$1", &[&consumed.challenge_id]).await?;
    client.execute("UPDATE staking_wallet_binding_challenges SET expires_at=now()-interval '1 minute' WHERE id=$1", &[&recent.challenge_id]).await?;
    repo.prepare(&challenge(org, actor, account())).await?;
    let rows = client
        .query(
            "SELECT id FROM staking_wallet_binding_challenges WHERE actor_user_id=$1",
            &[&actor],
        )
        .await?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.get(0)).collect();
    assert!(!ids.contains(&old.challenge_id));
    assert!(ids.contains(&consumed.challenge_id));
    assert!(ids.contains(&recent.challenge_id));
    Ok(())
}

#[tokio::test]
async fn binding_creates_personal_default_before_admin_membership() -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account());
    repo.prepare(&c).await?;
    let key = Uuid::new_v4();
    let result = repo.confirm(&c, request(&c), key, "key", "digest").await?;
    let client = pool.get().await?;
    let memberships = client.query(
        "SELECT m.organization_id,m.role FROM organization_members m JOIN organizations o ON o.id=m.organization_id AND o.is_active WHERE m.user_id=$1 ORDER BY m.joined_at,m.organization_id",
        &[&result.user_id],
    ).await?;
    assert_eq!(memberships.len(), 2);
    let personal: Uuid = memberships[0].get(0);
    assert_ne!(personal, org);
    assert_eq!(memberships[0].get::<_, String>(1), "owner");
    assert_eq!(memberships[1].get::<_, Uuid>(0), org);
    assert_eq!(memberships[1].get::<_, String>(1), "admin");
    assert_eq!(client.query_one("SELECT count(*) FROM workspaces WHERE organization_id=$1 AND created_by_user_id=$2 AND name='default'", &[&personal,&result.user_id]).await?.get::<_,i64>(0),1);
    repo.confirm(&c, request(&c), key, "key", "digest").await?;
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_members WHERE user_id=$1",
                &[&result.user_id]
            )
            .await?
            .get::<_, i64>(0),
        2
    );
    Ok(())
}

#[tokio::test]
async fn first_login_and_binding_share_personal_organization_provisioning() -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let users = database::repositories::UserRepository::new(pool.clone());
    let account = account();
    let c = challenge(org, actor, account.clone());
    repo.prepare(&c).await?;
    let (login, binding) = tokio::join!(
        users.create_from_oauth(
            format!("{account}@near"),
            account.clone(),
            None,
            None,
            "near".into(),
            account.clone()
        ),
        repo.confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
    );
    let result = binding?;
    // If binding wins the identity insert, OAuth resolves the uniqueness error
    // through its existing provider-identity lookup.
    let user = users.get_by_provider("near", &account).await?.unwrap();
    assert_eq!(user.id, result.user_id);
    if let Ok(login) = login {
        assert_eq!(login.id, result.user_id);
    }
    let client = pool.get().await?;
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM organization_members WHERE user_id=$1 AND role='owner'",
                &[&result.user_id]
            )
            .await?
            .get::<_, i64>(0),
        1
    );
    assert_eq!(client.query_one("SELECT count(*) FROM organization_members WHERE organization_id=$1 AND user_id=$2 AND role='admin'", &[&org,&result.user_id]).await?.get::<_,i64>(0),1);
    Ok(())
}

#[tokio::test]
async fn binding_preserves_existing_personal_organization_and_workspace() -> anyhow::Result<()> {
    let (pool, org, actor) = fixture().await?;
    let users = database::repositories::UserRepository::new(pool.clone());
    let account = account();
    let user = users
        .create_from_oauth(
            format!("{account}@near"),
            account.clone(),
            None,
            None,
            "near".into(),
            account.clone(),
        )
        .await?;
    let client = pool.get().await?;
    let personal: Uuid = client
        .query_one(
            "SELECT organization_id FROM organization_members WHERE user_id=$1 AND role='owner'",
            &[&user.id],
        )
        .await?
        .get(0);
    let repo = PostgresStakingBindingRepository::new(pool.clone());
    let c = challenge(org, actor, account);
    repo.prepare(&c).await?;
    repo.confirm(&c, request(&c), Uuid::new_v4(), "key", "digest")
        .await?;
    assert_eq!(client.query_one("SELECT organization_id FROM organization_members WHERE user_id=$1 AND role='owner'", &[&user.id]).await?.get::<_,Uuid>(0),personal);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM workspaces WHERE created_by_user_id=$1",
                &[&user.id]
            )
            .await?
            .get::<_, i64>(0),
        1
    );
    Ok(())
}
