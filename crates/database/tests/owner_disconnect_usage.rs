//! `has_client_disconnect_usage` answers only for usage rows of the given
//! organization and workspace, and only for a client-disconnect stop reason.

#[allow(dead_code)]
mod support;

use database::models::{RecordUsageRequest, StopReason};
use database::repositories::OrganizationUsageRepository;
use support::{cleanup_usage_fixtures, insert_model, insert_org_fixture, test_pool};
use uuid::Uuid;

fn usage(
    org_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model: &support::ModelFixture,
    provider_request_id: &str,
    stop_reason: StopReason,
) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: org_id,
        workspace_id,
        api_key_id,
        model_id: model.id,
        model_name: model.name.clone(),
        input_tokens: 1,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: 100,
        output_cost: 200,
        total_cost: 300,
        inference_type: "chat_completion".into(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: Some(provider_request_id.to_string()),
        stop_reason: Some(stop_reason),
        response_id: None,
        image_count: None,
        billing_details: None,
        service_tier: None,
        context_band: None,
        served_provider_tier: None,
        served_provider_type: None,
        served_via_fallback: false,
        dedupe_on_conflict: true,
    }
}

#[tokio::test]
async fn client_disconnect_usage_is_scoped_to_organization_and_workspace() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let mine = insert_org_fixture(&pool).await?;
    let theirs = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "owner-disconnect").await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();

    let own_id = format!("chatcmpl-own-{suffix}");
    let their_id = format!("chatcmpl-theirs-{suffix}");
    let sibling_id = format!("chatcmpl-sibling-{suffix}");
    let completed_id = format!("chatcmpl-completed-{suffix}");
    for request in [
        usage(
            mine.org_id,
            mine.workspace_a_id,
            mine.api_key_a_id,
            &model,
            &own_id,
            StopReason::ClientDisconnect,
        ),
        usage(
            theirs.org_id,
            theirs.workspace_a_id,
            theirs.api_key_a_id,
            &model,
            &their_id,
            StopReason::ClientDisconnect,
        ),
        usage(
            mine.org_id,
            mine.workspace_b_id,
            mine.api_key_b_id,
            &model,
            &sibling_id,
            StopReason::ClientDisconnect,
        ),
        usage(
            mine.org_id,
            mine.workspace_a_id,
            mine.api_key_a_id,
            &model,
            &completed_id,
            StopReason::Completed,
        ),
    ] {
        repository.record_usage(request).await?;
    }

    let check = |org_id: Uuid, workspace_id: Uuid, id: String| {
        let repository = repository.clone();
        async move {
            repository
                .has_client_disconnect_usage(org_id, workspace_id, &id)
                .await
        }
    };

    // The caller's own disconnected row.
    assert!(check(mine.org_id, mine.workspace_a_id, own_id.clone()).await?);
    // Another organization's row, read from either side.
    assert!(!check(mine.org_id, mine.workspace_a_id, their_id.clone()).await?);
    assert!(!check(theirs.org_id, theirs.workspace_a_id, own_id.clone()).await?);
    assert!(check(theirs.org_id, theirs.workspace_a_id, their_id).await?);
    // A sibling workspace of the same organization.
    assert!(!check(mine.org_id, mine.workspace_a_id, sibling_id.clone()).await?);
    assert!(!check(mine.org_id, mine.workspace_b_id, own_id.clone()).await?);
    assert!(check(mine.org_id, mine.workspace_b_id, sibling_id).await?);
    // The organization paired with a workspace it does not own.
    assert!(!check(mine.org_id, theirs.workspace_a_id, own_id).await?);
    // Another stop reason, and an unknown id.
    assert!(!check(mine.org_id, mine.workspace_a_id, completed_id).await?);
    assert!(
        !check(
            mine.org_id,
            mine.workspace_a_id,
            format!("chatcmpl-unknown-{suffix}")
        )
        .await?
    );

    // A `resp_<uuid>` id also matches the row's `response_id`.
    let response_id = Uuid::new_v4();
    pool.get().await?.execute(
        "INSERT INTO responses (id, model, status, workspace_id, api_key_id) VALUES ($1, $2, 'completed', $3, $4)",
        &[&response_id, &model.name, &mine.workspace_a_id, &mine.api_key_a_id],
    ).await?;
    let mut response_usage = usage(
        mine.org_id,
        mine.workspace_a_id,
        mine.api_key_a_id,
        &model,
        &format!("chatcmpl-inner-{suffix}"),
        StopReason::ClientDisconnect,
    );
    response_usage.response_id = Some(response_id.into());
    repository.record_usage(response_usage).await?;
    let resp_id = format!("resp_{response_id}");
    assert!(check(mine.org_id, mine.workspace_a_id, resp_id.clone()).await?);
    assert!(!check(mine.org_id, mine.workspace_b_id, resp_id.clone()).await?);
    assert!(!check(theirs.org_id, theirs.workspace_a_id, resp_id).await?);
    assert!(
        !check(
            mine.org_id,
            mine.workspace_a_id,
            format!("resp_{}", Uuid::new_v4())
        )
        .await?
    );
    assert!(!check(mine.org_id, mine.workspace_a_id, "resp_not-a-uuid".into()).await?);

    cleanup_usage_fixtures(&pool, &[mine.org_id, theirs.org_id], &[model.id]).await?;
    Ok(())
}
