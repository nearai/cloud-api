use crate::common::db_setup::create_test_pool;
use api::database_encryption::{
    operational_migrate, operational_scan, operational_verify, DatabaseEncryptionState,
};
use uuid::Uuid;

#[tokio::test]
async fn operational_worker_encrypts_and_verifies_a_scoped_field() {
    let pool = create_test_pool().await;
    let key = [42_u8; 32];
    pool.set_encryption_key(key);
    let client = pool.get().await.expect("database connection");
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let api_key_id = Uuid::new_v4();
    let file_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO organizations(id,name) VALUES($1,$2)",
            &[&organization_id, &format!("worker-org-{organization_id}")],
        )
        .await
        .expect("organization");
    client
        .execute(
            "INSERT INTO users(id,email,username,auth_provider,provider_user_id) VALUES($1,$2,$3,'mock',$4)",
            &[&user_id, &format!("{user_id}@test.invalid"), &format!("worker-{user_id}"), &user_id.to_string()],
        )
        .await
        .expect("user");
    client
        .execute(
            "INSERT INTO workspaces(id,name,organization_id,created_by_user_id) VALUES($1,$2,$3,$4)",
            &[&workspace_id, &format!("worker-{workspace_id}"), &organization_id, &user_id],
        )
        .await
        .expect("workspace");
    client
        .execute(
            "INSERT INTO api_keys(id,key_hash,name,workspace_id,created_by_user_id,key_prefix) VALUES($1,$2,'worker',$3,$4,'sk-test')",
            &[
                &api_key_id,
                &format!("{:0>64}", api_key_id.simple()),
                &workspace_id,
                &user_id,
            ],
        )
        .await
        .expect("API key");
    client
        .execute(
            "INSERT INTO files(id,filename,bytes,content_type,purpose,storage_key,workspace_id,uploaded_by_api_key_id) \
             VALUES($1,'legacy secret.txt',1,'text/plain','assistants','legacy/key',$2,$3)",
            &[&file_id, &workspace_id, &api_key_id],
        )
        .await
        .expect("legacy plaintext file");
    drop(client);

    let state =
        DatabaseEncryptionState::new(pool.clone(), &hex::encode(key)).expect("worker state");
    let job_id = operational_migrate(
        &state,
        vec!["files.filename".to_string()],
        1,
        None,
        None,
        "e2e-test",
    )
    .await
    .expect("worker migration");

    let client = pool.get().await.expect("database connection");
    let row = client
        .query_one(
            "SELECT f.filename,j.status,j.operator FROM files f CROSS JOIN database_encryption_jobs j \
             WHERE f.id=$1 AND j.id=$2",
            &[&file_id, &job_id],
        )
        .await
        .expect("stored migration result");
    let stored: String = row.get(0);
    let status: String = row.get(1);
    let operator: String = row.get(2);
    assert!(stored.contains(database::field_encryption::MARKER));
    assert!(!stored.contains("legacy secret.txt"));
    assert_eq!(status, "completed");
    assert_eq!(operator, "e2e-test");

    let verification = operational_verify(&state, vec!["files.filename".to_string()])
        .await
        .expect("worker verification");
    assert_eq!(verification["pass"], true);
}

#[tokio::test]
async fn worker_inventory_classifies_the_live_schema() {
    let pool = create_test_pool().await;
    let key = [43_u8; 32];
    pool.set_encryption_key(key);
    let state = DatabaseEncryptionState::new(pool, &hex::encode(key)).expect("worker state");
    let scan = operational_scan(&state, vec!["responses.metadata".to_string()])
        .await
        .expect("classification scan");
    assert_eq!(scan["inventory"]["complete"], true);
    assert!(scan["inventory"]["fields"].as_array().unwrap().len() > 100);
}
