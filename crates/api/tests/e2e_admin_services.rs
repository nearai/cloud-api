//! E2E tests for admin platform services CRUD (GET/POST/PATCH/DELETE /v1/admin/services).

mod common;

use api::models::{AdminServiceResponse, CreateServiceRequest, UpdateServiceRequest};
use common::*;
use services::service_usage::ports::ServiceUnit;

/// Create, get by id, update, get again, delete.
#[tokio::test]
async fn test_admin_services_crud() {
    let server = setup_test_server().await;
    let service_name = format!(
        "crud_service_{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    );

    // Create
    let create_req = CreateServiceRequest {
        service_name: service_name.clone(),
        display_name: "CRUD Test".to_string(),
        description: Some("Original".to_string()),
        unit: ServiceUnit::Request,
        cost_per_unit: 1_000_000,
    };
    let create_resp = server
        .post("/v1/admin/services")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&create_req)
        .await;
    assert_eq!(create_resp.status_code(), 200);
    let created: AdminServiceResponse =
        serde_json::from_str(&create_resp.text()).expect("Parse create response");
    let id = created.id;

    // Get by id
    let get_resp = server
        .get(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get_resp.status_code(), 200);
    let got: AdminServiceResponse =
        serde_json::from_str(&get_resp.text()).expect("Parse get response");
    assert_eq!(got.id, id);
    assert_eq!(got.display_name, "CRUD Test");
    assert_eq!(got.cost_per_unit, 1_000_000);

    // Update
    let update_req = UpdateServiceRequest {
        display_name: Some("CRUD Updated".to_string()),
        description: Some("Updated desc".to_string()),
        cost_per_unit: Some(3_000_000),
    };
    let patch_resp = server
        .patch(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&update_req)
        .await;
    assert_eq!(patch_resp.status_code(), 200);

    // Get again - verify update
    let get2_resp = server
        .get(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get2_resp.status_code(), 200);
    let got2: AdminServiceResponse =
        serde_json::from_str(&get2_resp.text()).expect("Parse get2 response");
    assert_eq!(got2.display_name, "CRUD Updated");
    assert_eq!(got2.description.as_deref(), Some("Updated desc"));
    assert_eq!(got2.cost_per_unit, 3_000_000);

    // Delete
    let del_resp = server
        .delete(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(del_resp.status_code(), 200);

    // Get after delete - 404
    let get3_resp = server
        .get(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get3_resp.status_code(), 404);
}
