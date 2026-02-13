mod common;
use common::*;

#[tokio::test]
async fn test_list_workspaces_invalid_limit() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Test limit <= 0
    let response = server
        .get(format!("/v1/organizations/{}/workspaces?limit=0", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Limit must be positive");
    assert_eq!(err.error.r#type, "invalid_parameter");

    // Test limit > 1000
    let response = server
        .get(format!("/v1/organizations/{}/workspaces?limit=1001", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Limit cannot exceed 1000");
    assert_eq!(err.error.r#type, "invalid_parameter");
}

#[tokio::test]
async fn test_list_workspaces_invalid_offset() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Test offset < 0
    let response = server
        .get(format!("/v1/organizations/{}/workspaces?offset=-1", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Offset must be non-negative");
    assert_eq!(err.error.r#type, "invalid_parameter");
}

#[tokio::test]
async fn test_list_api_keys_invalid_limit() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Test limit <= 0
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys?limit=0", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Limit must be positive");
    assert_eq!(err.error.r#type, "invalid_parameter");

    // Test limit > 1000
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys?limit=1001", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Limit cannot exceed 1000");
    assert_eq!(err.error.r#type, "invalid_parameter");
}

#[tokio::test]
async fn test_list_api_keys_invalid_offset() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Test offset < 0
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys?offset=-1", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 400);
    let err = response.json::<api::models::ErrorResponse>();
    assert_eq!(err.error.message, "Offset must be non-negative");
    assert_eq!(err.error.r#type, "invalid_parameter");
}
