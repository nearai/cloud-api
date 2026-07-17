use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use inference_providers::models::TopProviderInfo;
use inference_providers::ModelInfo;

struct OutputLimitCase {
    id: &'static str,
    top_level: Option<i32>,
    nested: Option<i32>,
    expected: Option<i32>,
    explicit_null: bool,
}

const SIX_MODEL_CASES: &[OutputLimitCase] = &[
    OutputLimitCase {
        id: "deepseek/deepseek-v3.2",
        top_level: Some(8_192),
        nested: None,
        expected: Some(8_192),
        explicit_null: true,
    },
    OutputLimitCase {
        id: "minimax/minimax-m2.5",
        top_level: None,
        nested: Some(4_096),
        expected: Some(4_096),
        explicit_null: true,
    },
    OutputLimitCase {
        id: "qwen/qwen3-32b",
        top_level: Some(16_384),
        nested: Some(32_768),
        expected: Some(32_768),
        explicit_null: true,
    },
    OutputLimitCase {
        id: "moonshotai/kimi-k2.5",
        top_level: None,
        nested: None,
        expected: None,
        explicit_null: false,
    },
    OutputLimitCase {
        id: "qwen/qwen3.5-397b-a17b",
        top_level: Some(0),
        nested: Some(0),
        expected: None,
        explicit_null: false,
    },
    OutputLimitCase {
        id: "z-ai/glm-5",
        top_level: Some(-1),
        nested: Some(-2),
        expected: None,
        explicit_null: false,
    },
];

fn encoded_path_segment(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes()).collect()
}

fn provider_model(id: &str, top_level: Option<i32>, nested: Option<i32>) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        object: "model".to_string(),
        created: 0,
        owned_by: "test".to_string(),
        context_length: Some(65_536),
        max_model_len: None,
        max_output_length: top_level,
        top_provider: nested.map(|max_completion_tokens| TopProviderInfo {
            context_length: Some(65_536),
            max_completion_tokens: Some(max_completion_tokens),
        }),
    }
}

fn insert_catalog_model(
    batch: &mut BatchUpdateModelApiRequest,
    id: &str,
    aliases: Vec<String>,
    explicit_null: bool,
) {
    let mut value = serde_json::json!({
        "inputCostPerToken": { "amount": 1_000_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
        "modelDisplayName": format!("Backend Output Limits {id}"),
        "modelDescription": "Synthetic active row for output-limit e2e",
        "contextLength": 4_096,
        "verifiable": false,
        "isActive": true,
        "aliases": aliases,
        "inputModalities": ["text"],
        "outputModalities": ["text"]
    });
    if explicit_null {
        value["maxOutputLength"] = serde_json::Value::Null;
    }
    batch.insert(id.to_string(), serde_json::from_value(value).unwrap());
}

fn find_by_field<'a>(
    rows: &'a [serde_json::Value],
    field: &str,
    id: &str,
) -> &'a serde_json::Value {
    rows.iter()
        .find(|row| row[field] == id)
        .unwrap_or_else(|| panic!("missing row {id} by {field}"))
}

fn assert_openrouter_output(row: &serde_json::Value, expected: Option<i32>) {
    match expected {
        Some(limit) => {
            assert_eq!(row["max_output_length"], limit);
            assert_eq!(row["top_provider"]["max_completion_tokens"], limit);
        }
        None => {
            assert!(row.get("max_output_length").is_none(), "{row}");
            assert!(
                row["top_provider"].get("max_completion_tokens").is_none(),
                "{row}"
            );
        }
    }
}

fn assert_detail_output(row: &serde_json::Value, expected: Option<i32>) {
    match expected {
        Some(limit) => assert_eq!(row["metadata"]["maxOutputLength"], limit),
        None => assert!(row["metadata"].get("maxOutputLength").is_none(), "{row}"),
    }
}

async fn get_json(server: &axum_test::TestServer, path: &str) -> serde_json::Value {
    let response = server.get(path).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}

#[tokio::test]
async fn backend_output_limits_six_model_regression() {
    let (server, pool, _, _) = setup_test_server_with_pool().await;
    let suffix = uuid::Uuid::new_v4();
    let alias = format!("backend-output-limits-alias-{suffix}");
    let aliased = format!("backend-output-limits/aliased-{suffix}");
    let exact = format!("backend-output-limits-exact-{suffix}");
    let no_slash = format!("backend-output-limits-noslash-{suffix}");

    let mut batch = BatchUpdateModelApiRequest::new();
    for case in SIX_MODEL_CASES {
        insert_catalog_model(&mut batch, case.id, Vec::new(), case.explicit_null);
    }
    insert_catalog_model(&mut batch, &no_slash, Vec::new(), true);
    insert_catalog_model(
        &mut batch,
        &aliased,
        vec![alias.clone(), exact.clone()],
        true,
    );
    insert_catalog_model(&mut batch, &exact, Vec::new(), true);
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let backend_models: Vec<ModelInfo> = SIX_MODEL_CASES
        .iter()
        .map(|case| provider_model(case.id, case.top_level, case.nested))
        .chain([
            provider_model(&no_slash, Some(7_777), None),
            provider_model(&aliased, Some(8_888), None),
            provider_model(&exact, Some(9_999), None),
        ])
        .collect();
    let provider = std::sync::Arc::new(inference_providers::mock::MockProvider::with_models(
        backend_models,
    ));
    for id in SIX_MODEL_CASES.iter().map(|case| case.id).chain([
        no_slash.as_str(),
        aliased.as_str(),
        exact.as_str(),
    ]) {
        pool.register_provider(id.to_string(), provider.clone())
            .await;
    }

    let models_json = get_json(&server, "/v1/models").await;
    let openrouter_rows = models_json["data"]
        .as_array()
        .expect("/v1/models data should be an array");
    for case in SIX_MODEL_CASES {
        let row = find_by_field(openrouter_rows, "id", case.id);
        assert_openrouter_output(row, case.expected);
    }

    let list_json = get_json(&server, "/v1/model/list?limit=500").await;
    let list_rows = list_json["models"]
        .as_array()
        .expect("/v1/model/list models should be an array");
    for case in SIX_MODEL_CASES {
        let row = find_by_field(list_rows, "modelId", case.id);
        assert_detail_output(row, case.expected);
    }

    for case in SIX_MODEL_CASES {
        let path = format!("/v1/model/{}", encoded_path_segment(case.id));
        let detail_json = get_json(&server, &path).await;
        assert_eq!(detail_json["modelId"], case.id);
        assert_detail_output(&detail_json, case.expected);
    }

    let no_slash_detail = get_json(&server, &format!("/v1/model/{no_slash}")).await;
    assert_detail_output(&no_slash_detail, Some(7_777));

    let alias_json = get_json(&server, &format!("/v1/model/{alias}")).await;
    assert_eq!(alias_json["modelId"], aliased);
    assert_detail_output(&alias_json, Some(8_888));

    let exact_json = get_json(&server, &format!("/v1/model/{exact}")).await;
    assert_eq!(exact_json["modelId"], exact);
    assert_detail_output(&exact_json, Some(9_999));

    assert!(get_json(&server, "/v1/model/list?limit=1").await["models"].is_array());

    let admin = server
        .get("/v1/admin/models?limit=1")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(admin.status_code(), 200, "{}", admin.text());

    let malformed = server.get("/v1/model/%ZZ").await;
    assert!(
        malformed.status_code().is_client_error(),
        "malformed percent encoding should produce a normal 4xx, got {}: {}",
        malformed.status_code(),
        malformed.text()
    );
}
