use super::{
    ReportingInferenceUsage, ReportingUsageCursor, ReportingUsageDetails,
    ReportingUsageExportResponse, ReportingUsageExportRow, ReportingUsageQuery,
    ReportingUsageQueryError, ReportingUsageQueryParams, ReportingUsageRowSource,
    ReportingUsageSource, ReportingUsageSummaryResponse, ReportingUsageTotals,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use utoipa::OpenApi as _;
use uuid::Uuid;

fn ts(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn organization_id() -> Uuid {
    Uuid::parse_str("018f7e4c-1111-7abc-9234-123456789abc").unwrap()
}

#[test]
fn reporting_usage_query_defaults_and_cursor_roundtrip() {
    // Given: no optional filters and a stable inference cursor tuple.
    let params = ReportingUsageQueryParams::default();
    let created_at = ts("2026-07-01T12:34:56Z");
    let id = Uuid::parse_str("018f7e4c-1234-7abc-9234-123456789abc").unwrap();

    // When: the boundary DTO is parsed and the cursor is encoded/decoded.
    let query = ReportingUsageQuery::try_from(params).unwrap();
    let cursor = ReportingUsageCursor::for_query(
        organization_id(),
        created_at,
        ReportingUsageRowSource::Inference,
        id,
        &query,
    )
    .unwrap();
    let encoded = cursor.encode().unwrap();
    let decoded = ReportingUsageCursor::decode(&encoded).unwrap();

    // Then: defaults match the public contract and the cursor is stable and URL-safe.
    assert_eq!(query.source, ReportingUsageSource::All);
    assert_eq!(query.limit.value(), 100);
    assert!(query.start_time.is_some());
    assert!(query.end_time.is_some());
    assert_eq!(decoded, cursor);
    assert!(decoded.validate_organization(organization_id()).is_ok());
    assert_eq!(
        decoded.validate_organization(Uuid::new_v4()),
        Err(ReportingUsageQueryError::InvalidCursor)
    );
    assert_eq!(cursor.encode().unwrap(), encoded);
    assert!(!encoded.contains('+'));
    assert!(!encoded.contains('/'));
    assert!(!encoded.contains('='));
}

#[test]
fn reporting_usage_cursor_restores_omitted_context_and_rejects_conflicts() {
    // Given: a first-page query with an explicit window and every supported filter.
    let workspace_id = Uuid::parse_str("018f7e4c-2222-7abc-9234-123456789abc").unwrap();
    let api_key_id = Uuid::parse_str("018f7e4c-3333-7abc-9234-123456789abc").unwrap();
    let first_page = ReportingUsageQuery::try_from(ReportingUsageQueryParams {
        start_time: Some("2026-07-01T00:00:00Z".to_string()),
        end_time: Some("2026-07-02T00:00:00Z".to_string()),
        source: Some("inference".to_string()),
        workspace_id: Some(workspace_id),
        api_key_id: Some(api_key_id),
        model: Some("test-model".to_string()),
        inference_type: Some("chat_completion".to_string()),
        service_name: Some("web_search".to_string()),
        limit: Some(1),
        cursor: None,
    })
    .unwrap();
    let cursor = ReportingUsageCursor::for_query(
        organization_id(),
        ts("2026-07-01T12:34:56Z"),
        ReportingUsageRowSource::Inference,
        Uuid::parse_str("018f7e4c-1234-7abc-9234-123456789abc").unwrap(),
        &first_page,
    )
    .unwrap()
    .encode()
    .unwrap();

    // When: page two supplies only the opaque cursor.
    let continuation = ReportingUsageQuery::try_from(ReportingUsageQueryParams {
        cursor: Some(cursor.clone()),
        ..ReportingUsageQueryParams::default()
    })
    .unwrap();

    // Then: the cursor restores the exact effective window and filter context.
    assert_eq!(continuation.start_time, first_page.start_time);
    assert_eq!(continuation.end_time, first_page.end_time);
    assert_eq!(continuation.source, first_page.source);
    assert_eq!(continuation.workspace_id, first_page.workspace_id);
    assert_eq!(continuation.api_key_id, first_page.api_key_id);
    assert_eq!(continuation.model, first_page.model);
    assert_eq!(continuation.inference_type, first_page.inference_type);
    assert_eq!(continuation.service_name, first_page.service_name);

    // When/Then: explicitly changing any bound context invalidates the cursor.
    let conflicting_source = ReportingUsageQueryParams {
        source: Some("service".to_string()),
        cursor: Some(cursor.clone()),
        ..ReportingUsageQueryParams::default()
    };
    let conflicting_start = ReportingUsageQueryParams {
        start_time: Some("2026-07-01T00:00:01Z".to_string()),
        cursor: Some(cursor.clone()),
        ..ReportingUsageQueryParams::default()
    };
    let conflicting_workspace = ReportingUsageQueryParams {
        workspace_id: Some(Uuid::new_v4()),
        cursor: Some(cursor),
        ..ReportingUsageQueryParams::default()
    };
    for params in [conflicting_source, conflicting_start, conflicting_workspace] {
        assert!(matches!(
            ReportingUsageQuery::try_from(params),
            Err(ReportingUsageQueryError::InvalidCursor)
        ));
    }
}

#[test]
fn reporting_usage_query_normalizes_open_ended_ranges() {
    // Given: open-ended reporting queries that would otherwise be unbounded.
    let before_default = Utc::now();
    let default_query =
        ReportingUsageQuery::try_from(ReportingUsageQueryParams::default()).unwrap();
    let after_default = Utc::now();
    let explicit_end = ts("2026-07-01T00:00:00Z");
    let end_only = ReportingUsageQueryParams {
        end_time: Some(explicit_end.to_rfc3339()),
        ..ReportingUsageQueryParams::default()
    };
    let recent_start = Utc::now() - Duration::days(1);
    let start_only = ReportingUsageQueryParams {
        start_time: Some(recent_start.to_rfc3339()),
        ..ReportingUsageQueryParams::default()
    };
    let stale_start = ReportingUsageQueryParams {
        start_time: Some((Utc::now() - Duration::days(367)).to_rfc3339()),
        ..ReportingUsageQueryParams::default()
    };

    // When: the boundary DTOs are parsed.
    let end_only_query = ReportingUsageQuery::try_from(end_only).unwrap();
    let start_only_query = ReportingUsageQuery::try_from(start_only).unwrap();
    let stale_result = ReportingUsageQuery::try_from(stale_start);

    // Then: every accepted query has an effective range no wider than 366 days.
    let default_start = default_query.start_time.unwrap();
    let default_end = default_query.end_time.unwrap();
    assert!(default_end >= before_default);
    assert!(default_end <= after_default);
    assert_eq!(default_end - default_start, Duration::days(366));
    assert_eq!(end_only_query.end_time, Some(explicit_end));
    assert_eq!(
        end_only_query.start_time,
        Some(explicit_end - Duration::days(366))
    );
    assert_eq!(start_only_query.start_time, Some(recent_start));
    assert!(start_only_query.end_time.unwrap() >= recent_start);
    assert!(matches!(
        stale_result,
        Err(ReportingUsageQueryError::TimeRangeTooLarge { max_days: 366 })
    ));
}

#[test]
fn reporting_usage_query_rejects_invalid_range_source_and_cursor() {
    // Given: malformed query values for each boundary validation class.
    let bad_order = ReportingUsageQueryParams {
        start_time: Some("2026-07-02T00:00:00Z".to_string()),
        end_time: Some("2026-07-01T00:00:00Z".to_string()),
        ..ReportingUsageQueryParams::default()
    };
    let too_wide = ReportingUsageQueryParams {
        start_time: Some("2026-01-01T00:00:00Z".to_string()),
        end_time: Some("2027-01-03T00:00:00Z".to_string()),
        ..ReportingUsageQueryParams::default()
    };
    let bad_source = ReportingUsageQueryParams {
        source: Some("database".to_string()),
        ..ReportingUsageQueryParams::default()
    };
    let excessive_limit = ReportingUsageQueryParams {
        limit: Some(1001),
        ..ReportingUsageQueryParams::default()
    };
    let malformed_cursor = ReportingUsageQueryParams {
        cursor: Some("not%base64url".to_string()),
        ..ReportingUsageQueryParams::default()
    };
    let wrong_cursor_schema = ReportingUsageQueryParams {
        cursor: Some(
            ReportingUsageCursor::encode_raw_for_tests(&json!({
                "created_at": "2026-07-01T00:00:00Z"
            }))
            .unwrap(),
        ),
        ..ReportingUsageQueryParams::default()
    };

    // When/Then: each invalid shape is rejected with the intended typed error.
    assert!(matches!(
        ReportingUsageQuery::try_from(bad_order),
        Err(ReportingUsageQueryError::InvalidTimeRange)
    ));
    assert!(matches!(
        ReportingUsageQuery::try_from(too_wide),
        Err(ReportingUsageQueryError::TimeRangeTooLarge { max_days: 366 })
    ));
    assert!(matches!(
        ReportingUsageQuery::try_from(bad_source),
        Err(ReportingUsageQueryError::InvalidSource(_))
    ));
    assert!(matches!(
        ReportingUsageQuery::try_from(excessive_limit),
        Err(ReportingUsageQueryError::LimitTooLarge { max: 1000 })
    ));
    assert!(matches!(
        ReportingUsageQuery::try_from(malformed_cursor),
        Err(ReportingUsageQueryError::InvalidCursor)
    ));
    assert!(matches!(
        ReportingUsageQuery::try_from(wrong_cursor_schema),
        Err(ReportingUsageQueryError::InvalidCursor)
    ));
}

#[test]
fn reporting_usage_query_manual_codec_serializes_response_and_cursor() {
    // Given: a valid query, export row, summary body, and stable cursor tuple.
    let created_at = ts("2026-07-01T12:34:56Z");
    let row_id = Uuid::parse_str("018f7e4c-1234-7abc-9234-123456789abc").unwrap();
    let workspace_id = Uuid::parse_str("018f7e4c-2222-7abc-9234-123456789abc").unwrap();
    let api_key_id = Uuid::parse_str("018f7e4c-3333-7abc-9234-123456789abc").unwrap();
    let query = ReportingUsageQuery::try_from(ReportingUsageQueryParams {
        start_time: Some("2026-07-01T00:00:00Z".to_string()),
        end_time: Some("2026-07-02T00:00:00Z".to_string()),
        source: Some("all".to_string()),
        limit: Some(1),
        ..ReportingUsageQueryParams::default()
    })
    .unwrap();
    let cursor = ReportingUsageCursor::for_query(
        organization_id(),
        created_at,
        ReportingUsageRowSource::Inference,
        row_id,
        &query,
    )
    .unwrap();

    let export = ReportingUsageExportResponse {
        data: vec![ReportingUsageExportRow {
            id: row_id,
            created_at,
            workspace_id,
            api_key_id,
            total_cost_nano_usd: 42,
            total_cost_usd: Some("$0.000000042".to_string()),
            usage: ReportingUsageDetails::Inference {
                inference: ReportingInferenceUsage {
                    model: "test-model".to_string(),
                    inference_type: "chat_completion".to_string(),
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 3,
                    total_tokens: 33,
                    input_cost_nano_usd: 10,
                    output_cost_nano_usd: 29,
                    cache_read_cost_nano_usd: None,
                    total_cost_nano_usd: 42,
                    response_id: None,
                    inference_id: Some(row_id),
                    image_count: None,
                },
            },
        }],
        next_cursor: Some(cursor.encode().unwrap()),
    };
    let summary = ReportingUsageSummaryResponse {
        source: ReportingUsageSource::All,
        start_time: query.start_time.unwrap(),
        end_time: query.end_time.unwrap(),
        totals: ReportingUsageTotals {
            request_count: 1,
            service_usage_count: 0,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 3,
            total_tokens: 33,
            inference_cost_nano_usd: 42,
            service_cost_nano_usd: 0,
            total_cost_nano_usd: 42,
            total_cost_usd: Some("$0.000000042".to_string()),
        },
        by_workspace: Vec::new(),
        by_api_key: Vec::new(),
        by_model: Vec::new(),
        by_service: Vec::new(),
        by_day: Vec::new(),
    };

    // When: response bodies serialize and the cursor is decoded.
    let encoded = cursor.encode().unwrap();
    let decoded = ReportingUsageCursor::decode(&encoded).unwrap();
    let export_json = serde_json::to_string(&export).unwrap();
    let summary_json = serde_json::to_string(&summary).unwrap();

    // Then: the cursor tuple and response JSON are exact enough for CLI/data QA.
    assert_eq!(decoded.created_at, created_at);
    assert_eq!(decoded.source, ReportingUsageRowSource::Inference);
    assert_eq!(decoded.id, row_id);
    assert!(export_json.contains("\"total_cost_nano_usd\":42"));
    assert!(export_json.contains("\"cache_read_tokens\":3"));
    assert!(!export_json.contains("cache_read_cost_nano_usd"));
    assert!(summary_json.contains("\"inference_cost_nano_usd\":42"));
    let mut invalid_row = serde_json::to_value(&export.data[0]).unwrap();
    invalid_row
        .as_object_mut()
        .unwrap()
        .insert("service".to_string(), json!({}));
    assert!(serde_json::from_value::<ReportingUsageExportRow>(invalid_row).is_err());
    println!("cursor={encoded}");
    println!(
        "cursor_tuple={}|{}|{}",
        decoded.created_at.to_rfc3339(),
        decoded.source.as_str(),
        decoded.id
    );
    println!("export_json={export_json}");
    println!("summary_json={summary_json}");
}

#[test]
fn reporting_usage_export_details_schema_requires_exactly_one_variant() {
    let spec = serde_json::to_value(crate::openapi::ApiDoc::openapi()).unwrap();
    let details = &spec["components"]["schemas"]["ReportingUsageDetails"];

    assert_eq!(details["oneOf"].as_array().map(Vec::len), Some(2));
}
