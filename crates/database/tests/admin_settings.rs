#[allow(dead_code)]
mod support;

use database::repositories::PostgresAdminSettingsRepository;
use serde_json::json;
use services::admin_settings::AdminSettingsRepository;
use support::test_pool;
use uuid::Uuid;

#[tokio::test]
async fn get_upsert_round_trip() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repo = PostgresAdminSettingsRepository::new(pool.clone());
    // The key is test-owned, so this never touches a real setting.
    let key = format!("test-{}", Uuid::new_v4());
    let admin = Uuid::new_v4();
    let admin2 = Uuid::new_v4();

    assert!(repo.get(&key).await?.is_none());

    let first = repo.upsert(&key, json!({"a": 1}), admin).await?;
    assert_eq!(first.key, key);
    assert_eq!(first.value, json!({"a": 1}));
    assert_eq!(first.updated_by_user_id, Some(admin));
    assert_eq!(repo.get(&key).await?, Some(first.clone()));

    // An upsert replaces the value and the audit columns.
    let second = repo.upsert(&key, json!({"b": 2.5}), admin2).await?;
    assert_eq!(second.value, json!({"b": 2.5}));
    assert_eq!(second.updated_by_user_id, Some(admin2));
    assert!(second.updated_at >= first.updated_at);

    let all = repo.get_all().await?;
    assert_eq!(all.iter().filter(|s| s.key == key).count(), 1);
    assert!(all.contains(&second));

    pool.get()
        .await?
        .execute("DELETE FROM admin_settings WHERE key = $1", &[&key])
        .await?;
    Ok(())
}
