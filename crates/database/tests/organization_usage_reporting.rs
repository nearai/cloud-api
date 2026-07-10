mod support;

use database::repositories::OrganizationUsageRepository;
use services::usage::ports::{InferenceUsageReportCursor, InferenceUsageReportQuery};
use support::{
    cleanup_usage_fixtures, insert_model, insert_org_fixture, insert_usage, test_pool, ts,
    UsageSeed,
};
use uuid::Uuid;

#[tokio::test]
async fn organization_usage_reporting_filters_and_cursor() -> anyhow::Result<()> {
    // Given: two org-scoped rows sharing a timestamp plus rows excluded by filters.
    let pool = test_pool().await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let other_org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "report-model-a").await?;
    let other_model = insert_model(&pool, "report-model-b").await?;
    let model_id = model.id;
    let other_model_id = other_model.id;
    let response_id = Uuid::new_v4();
    let shared_time = ts(2026, 1, 2, 0);
    let mut tied_ids = [Uuid::new_v4(), Uuid::new_v4()];
    tied_ids.sort();
    let second_id = tied_ids[0];
    let first_id = tied_ids[1];

    for seed in [
        UsageSeed {
            id: first_id,
            org_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model: model.clone(),
            created_at: shared_time,
            inference_type: "chat_completion",
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            input_cost: 100,
            output_cost: 200,
            total_cost: 300,
            response_id: Some(response_id),
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-secret-first",
        },
        UsageSeed {
            id: second_id,
            org_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model: model.clone(),
            created_at: shared_time,
            inference_type: "chat_completion",
            input_tokens: 11,
            output_tokens: 6,
            cache_read_tokens: 3,
            input_cost: 110,
            output_cost: 220,
            total_cost: 330,
            response_id: None,
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-secret-second",
        },
        UsageSeed {
            id: Uuid::new_v4(),
            org_id: org.org_id,
            workspace_id: org.workspace_b_id,
            api_key_id: org.api_key_b_id,
            model: other_model,
            created_at: shared_time,
            inference_type: "embedding",
            input_tokens: 9,
            output_tokens: 0,
            cache_read_tokens: 0,
            input_cost: 90,
            output_cost: 0,
            total_cost: 90,
            response_id: None,
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-filtered",
        },
        UsageSeed {
            id: Uuid::new_v4(),
            org_id: other_org.org_id,
            workspace_id: other_org.workspace_a_id,
            api_key_id: other_org.api_key_a_id,
            model: model.clone(),
            created_at: shared_time,
            inference_type: "chat_completion",
            input_tokens: 99,
            output_tokens: 99,
            cache_read_tokens: 0,
            input_cost: 990,
            output_cost: 990,
            total_cost: 1980,
            response_id: None,
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-other-org",
        },
    ] {
        insert_usage(&pool, &seed).await?;
    }

    // When: a filtered report is fetched with a one-row keyset page.
    let first_page = repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            organization_id: org.org_id,
            start_time: Some(ts(2026, 1, 1, 0)),
            end_time: Some(ts(2026, 1, 3, 0)),
            workspace_id: Some(org.workspace_a_id),
            api_key_id: Some(org.api_key_a_id),
            model: Some(model.name.clone()),
            inference_type: Some("chat_completion".to_string()),
            limit: 1,
            cursor: None,
        })
        .await?;

    // Then: ordering uses created_at DESC, id DESC and exposes reporting-safe fields only.
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].id, first_id);
    assert_eq!(first_page[0].response_id, Some(response_id));
    assert_eq!(first_page[0].input_tokens, 10);
    assert_eq!(first_page[0].output_tokens, 5);
    assert_eq!(first_page[0].cache_read_tokens, 2);
    assert_eq!(first_page[0].total_tokens, 15);
    assert_eq!(first_page[0].input_cost_nano_usd, 100);
    assert_eq!(first_page[0].output_cost_nano_usd, 200);
    assert_eq!(first_page[0].total_cost_nano_usd, 300);
    let first_page_json = serde_json::to_string(&first_page)?;
    assert!(!first_page_json.contains("provider_request_id"));
    println!("first_page_json={first_page_json}");

    // When: the next page starts after the first row's cursor tuple.
    let second_page = repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            organization_id: org.org_id,
            start_time: Some(ts(2026, 1, 1, 0)),
            end_time: Some(ts(2026, 1, 3, 0)),
            workspace_id: Some(org.workspace_a_id),
            api_key_id: Some(org.api_key_a_id),
            model: Some(model.name.clone()),
            inference_type: Some("chat_completion".to_string()),
            limit: 1,
            cursor: Some(InferenceUsageReportCursor {
                created_at: first_page[0].created_at,
                id: first_page[0].id,
            }),
        })
        .await?;

    // Then: cursor continuation returns the tied row with the lower id.
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0].id, second_id);
    assert_eq!(second_page[0].organization_id, org.org_id);
    assert_eq!(second_page[0].workspace_id, org.workspace_a_id);
    assert_eq!(second_page[0].api_key_id, org.api_key_a_id);
    println!(
        "cursor_continuation={} -> {}",
        first_page[0].id, second_page[0].id
    );
    cleanup_usage_fixtures(
        &pool,
        &[org.org_id, other_org.org_id],
        &[model_id, other_model_id],
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn organization_usage_reporting_excludes_other_orgs() -> anyhow::Result<()> {
    // Given: one row in each of two organizations.
    let pool = test_pool().await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let other_org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "report-isolation-model").await?;
    let model_id = model.id;
    insert_usage(
        &pool,
        &UsageSeed {
            id: Uuid::new_v4(),
            org_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            model: model.clone(),
            created_at: ts(2026, 2, 1, 0),
            inference_type: "chat_completion",
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            input_cost: 10,
            output_cost: 20,
            total_cost: 30,
            response_id: None,
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-owned",
        },
    )
    .await?;
    insert_usage(
        &pool,
        &UsageSeed {
            id: Uuid::new_v4(),
            org_id: other_org.org_id,
            workspace_id: other_org.workspace_a_id,
            api_key_id: other_org.api_key_a_id,
            model,
            created_at: ts(2026, 2, 1, 1),
            inference_type: "chat_completion",
            input_tokens: 2,
            output_tokens: 2,
            cache_read_tokens: 0,
            input_cost: 20,
            output_cost: 40,
            total_cost: 60,
            response_id: None,
            inference_id: Uuid::new_v4(),
            provider_request_id: "provider-other",
        },
    )
    .await?;

    // When: the first organization fetches reporting rows, including impossible filters.
    let rows = repository
        .list_inference_usage_report(InferenceUsageReportQuery::for_organization(org.org_id))
        .await?;
    let bad_workspace_rows = repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            workspace_id: Some(other_org.workspace_a_id),
            ..InferenceUsageReportQuery::for_organization(org.org_id)
        })
        .await?;

    // Then: only rows belonging to the requested organization are returned.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].organization_id, org.org_id);
    assert!(bad_workspace_rows.is_empty());
    println!("org_rows_json={}", serde_json::to_string(&rows)?);
    println!("bad_workspace_rows={}", bad_workspace_rows.len());
    cleanup_usage_fixtures(&pool, &[org.org_id, other_org.org_id], &[model_id]).await?;

    Ok(())
}

#[tokio::test]
async fn organization_usage_reporting_rejects_invalid_date_range() -> anyhow::Result<()> {
    // Given: a repository query with end_time before start_time.
    let pool = test_pool().await?;
    let repository = OrganizationUsageRepository::new(pool);
    let query = InferenceUsageReportQuery {
        start_time: Some(ts(2026, 3, 2, 0)),
        end_time: Some(ts(2026, 3, 1, 0)),
        ..InferenceUsageReportQuery::for_organization(Uuid::new_v4())
    };

    // When: the malformed range is submitted.
    let result = repository.list_inference_usage_report(query).await;

    // Then: repository validation rejects it before SQL execution.
    assert!(result.is_err());

    Ok(())
}
