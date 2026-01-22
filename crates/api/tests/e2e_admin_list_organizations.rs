mod common;

use api::models::ListOrganizationsAdminResponse;
use common::*;

#[tokio::test]
async fn test_admin_list_organizations_response_structure() {
    let (server, _guard) = setup_test_server().await;

    let response = server
        .get("/v1/admin/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully list organizations"
    );

    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(list_response.limit > 0, "Limit should be positive");
    assert!(list_response.offset >= 0, "Offset should be non-negative");
    assert!(list_response.total >= 0, "Total should be non-negative");
    assert!(
        list_response.organizations.len() as i64 <= list_response.limit,
        "Organizations count should not exceed limit"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_with_data() {
    let (server, _guard) = setup_test_server().await;

    let org1 = create_org(&server).await;
    let org2 = create_org(&server).await;

    let response = server
        .get("/v1/admin/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let found_org1 = list_response
        .organizations
        .iter()
        .any(|o| o.id == org1.id);
    let found_org2 = list_response
        .organizations
        .iter()
        .any(|o| o.id == org2.id);

    assert!(found_org1, "Should find first organization");
    assert!(found_org2, "Should find second organization");
    assert!(
        list_response.total >= 2,
        "Total should include at least 2 organizations"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_sort_by_name_asc() {
    let (server, _guard) = setup_test_server().await;

    let org_names = vec!["zebra-org", "alpha-org", "delta-org"];
    for name in &org_names {
        let request = api::models::CreateOrganizationRequest {
            name: format!("test-{}-{}", name, uuid::Uuid::new_v4()),
            description: Some(format!("Test organization {}", name)),
        };
        let response = server
            .post("/v1/organizations")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!(request))
            .await;
        assert_eq!(response.status_code(), 200);
    }

    let response = server
        .get("/v1/admin/organizations?sort_by=name&sort_order=asc")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let names: Vec<String> = list_response
        .organizations
        .iter()
        .map(|o| o.name.clone())
        .collect();

    let mut sorted_names = names.clone();
    sorted_names.sort();
    assert_eq!(
        names, sorted_names,
        "Organizations should be sorted by name ascending"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_sort_by_name_desc() {
    let (server, _guard) = setup_test_server().await;

    for i in 0..3 {
        let request = api::models::CreateOrganizationRequest {
            name: format!("sort-test-{}-{}", i, uuid::Uuid::new_v4()),
            description: Some(format!("Test organization {}", i)),
        };
        let response = server
            .post("/v1/organizations")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!(request))
            .await;
        assert_eq!(response.status_code(), 200);
    }

    let response = server
        .get("/v1/admin/organizations?sort_by=name&sort_order=desc")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let names: Vec<String> = list_response
        .organizations
        .iter()
        .map(|o| o.name.clone())
        .collect();

    let mut sorted_names = names.clone();
    sorted_names.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        names, sorted_names,
        "Organizations should be sorted by name descending"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_sort_by_created_at() {
    let (server, _guard) = setup_test_server().await;

    let mut org_ids = Vec::new();
    for _i in 0..3 {
        let org = create_org(&server).await;
        org_ids.push(org.id.clone());
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    let response = server
        .get("/v1/admin/organizations?sort_by=created_at&sort_order=desc")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = list_response
        .organizations
        .iter()
        .map(|o| o.created_at)
        .collect();

    for i in 0..timestamps.len().saturating_sub(1) {
        assert!(
            timestamps[i] >= timestamps[i + 1],
            "Timestamps should be in descending order"
        );
    }
}

#[tokio::test]
async fn test_admin_list_organizations_sort_by_spend_limit() {
    let (server, _guard) = setup_test_server().await;

    let limits = vec![1000000, 500000, 2000000];
    let mut org_ids = Vec::new();

    for limit in limits {
        let org = setup_org_with_credits(&server, limit).await;
        org_ids.push(org.id);
    }

    let response = server
        .get("/v1/admin/organizations?sort_by=spend_limit&sort_order=asc")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let our_orgs: Vec<_> = list_response
        .organizations
        .iter()
        .filter(|o| org_ids.contains(&o.id.to_string()))
        .collect();

    let spend_limits: Vec<i64> = our_orgs
        .iter()
        .filter_map(|o| o.spend_limit.as_ref().map(|l| l.amount))
        .collect();

    for i in 0..spend_limits.len().saturating_sub(1) {
        assert!(
            spend_limits[i] <= spend_limits[i + 1],
            "Spend limits should be in ascending order"
        );
    }
}

#[tokio::test]
async fn test_admin_list_organizations_sort_by_current_usage() {
    let (server, _guard) = setup_test_server().await;

    let _org1 = setup_org_with_credits(&server, 1000000).await;
    let _org2 = setup_org_with_credits(&server, 2000000).await;

    let response = server
        .get("/v1/admin/organizations?sort_by=current_usage&sort_order=asc")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(list_response.organizations.len() >= 2);
}

#[tokio::test]
async fn test_admin_list_organizations_search_by_name() {
    let (server, _guard) = setup_test_server().await;

    let unique_prefix = format!("search-test-{}", uuid::Uuid::new_v4());
    let request = api::models::CreateOrganizationRequest {
        name: format!("{}-findme", unique_prefix),
        description: Some("Test organization for search".to_string()),
    };
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    let created_org: api::models::OrganizationResponse = response.json();

    let search_query = "findme";
    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}",
            urlencoding::encode(search_query)
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let found = list_response
        .organizations
        .iter()
        .any(|o| o.id == created_org.id);
    assert!(found, "Should find organization by name search");

    for org in &list_response.organizations {
        assert!(
            org.name
                .to_lowercase()
                .contains(&search_query.to_lowercase()),
            "Organization name '{}' should contain search term '{}'",
            org.name,
            search_query
        );
    }
}

#[tokio::test]
async fn test_admin_list_organizations_search_by_id() {
    let (server, _guard) = setup_test_server().await;

    let org = create_org(&server).await;

    let search_query = &org.id[0..8];
    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}",
            urlencoding::encode(search_query)
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let found = list_response
        .organizations
        .iter()
        .any(|o| o.id == org.id);
    assert!(found, "Should find organization by ID search");
}

#[tokio::test]
async fn test_admin_list_organizations_search_case_insensitive() {
    let (server, _guard) = setup_test_server().await;

    let unique_name = format!("TestCaseInsensitive-{}", uuid::Uuid::new_v4());
    let request = api::models::CreateOrganizationRequest {
        name: unique_name.clone(),
        description: Some("Test for case-insensitive search".to_string()),
    };
    let response = server
        .post("/v1/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!(request))
        .await;
    assert_eq!(response.status_code(), 200);
    let created_org: api::models::OrganizationResponse = response.json();

    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}",
            urlencoding::encode("testcaseinsensitive")
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let found = list_response
        .organizations
        .iter()
        .any(|o| o.id == created_org.id);
    assert!(
        found,
        "Should find organization with case-insensitive search"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_search_no_results() {
    let (server, _guard) = setup_test_server().await;

    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}",
            urlencoding::encode("definitely-does-not-exist-99999")
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(
        list_response.organizations.len(),
        0,
        "Should return no results for non-existent search"
    );
    assert_eq!(
        list_response.total, 0,
        "Total should be 0 for non-existent search"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_search_and_sort() {
    let (server, _guard) = setup_test_server().await;

    let prefix = format!("combined-test-{}", uuid::Uuid::new_v4());
    let names = vec![
        format!("{}-zebra", prefix),
        format!("{}-alpha", prefix),
        format!("{}-delta", prefix),
    ];

    for name in &names {
        let request = api::models::CreateOrganizationRequest {
            name: name.clone(),
            description: Some("Combined test".to_string()),
        };
        let response = server
            .post("/v1/organizations")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!(request))
            .await;
        assert_eq!(response.status_code(), 200);
    }

    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}&sort_by=name&sort_order=asc",
            urlencoding::encode("combined-test")
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    for org in &list_response.organizations {
        assert!(
            org.name.contains("combined-test"),
            "Organization should match search term"
        );
    }

    let returned_names: Vec<String> = list_response
        .organizations
        .iter()
        .map(|o| o.name.clone())
        .collect();
    let mut sorted_names = returned_names.clone();
    sorted_names.sort();
    assert_eq!(
        returned_names, sorted_names,
        "Results should be sorted by name ascending"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_pagination() {
    let (server, _guard) = setup_test_server().await;

    for _i in 0..5 {
        create_org(&server).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    let response = server
        .get("/v1/admin/organizations?limit=2")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(list_response.limit, 2, "Limit should be 2");
    assert!(
        list_response.organizations.len() <= 2,
        "Should return at most 2 organizations"
    );

    let response = server
        .get("/v1/admin/organizations?limit=2&offset=2")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(list_response.limit, 2, "Limit should be 2");
    assert_eq!(list_response.offset, 2, "Offset should be 2");
}

#[tokio::test]
async fn test_admin_list_organizations_pagination_with_search() {
    let (server, _guard) = setup_test_server().await;

    let prefix = format!("paginated-{}", uuid::Uuid::new_v4());
    for i in 0..5 {
        let request = api::models::CreateOrganizationRequest {
            name: format!("{}-org-{}", prefix, i),
            description: Some("Pagination test".to_string()),
        };
        let response = server
            .post("/v1/organizations")
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!(request))
            .await;
        assert_eq!(response.status_code(), 200);
    }

    let response = server
        .get(&format!(
            "/v1/admin/organizations?search={}&limit=2",
            urlencoding::encode(&prefix)
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(
        list_response.total >= 5,
        "Should find at least 5 matching organizations"
    );
    assert!(
        list_response.organizations.len() <= 2,
        "Should return at most 2 organizations per page"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_unauthorized() {
    let (server, _guard) = setup_test_server().await;

    let response = server.get("/v1/admin/organizations").await;

    assert_eq!(
        response.status_code(),
        401,
        "Should return 401 without auth"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_invalid_pagination() {
    let (server, _guard) = setup_test_server().await;

    let response = server
        .get("/v1/admin/organizations?offset=-1")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for negative offset"
    );

    let response = server
        .get("/v1/admin/organizations?limit=-1")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for negative limit"
    );
}

#[tokio::test]
async fn test_admin_list_organizations_default_sort() {
    let (server, _guard) = setup_test_server().await;

    create_org(&server).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    create_org(&server).await;

    let response = server
        .get("/v1/admin/organizations")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: ListOrganizationsAdminResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let timestamps: Vec<chrono::DateTime<chrono::Utc>> = list_response
        .organizations
        .iter()
        .map(|o| o.created_at)
        .collect();

    for i in 0..timestamps.len().saturating_sub(1) {
        assert!(
            timestamps[i] >= timestamps[i + 1],
            "Default sort should be by created_at descending"
        );
    }
}
