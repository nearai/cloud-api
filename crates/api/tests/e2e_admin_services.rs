//! E2E tests for admin platform services CRUD (POST/PATCH /v1/admin/services)
//! and public services (GET /v1/services, GET /v1/services/{service_name}).

mod common;

use api::models::{CreateServiceRequest, ServiceResponse, UpdateServiceRequest};
use common::*;
use services::service_usage::ports::ServiceUnit;

/// Create, get by name (public), update (display/cost), get again, disable (PATCH is_active false),
/// verify public GET returns 404 when disabled.
#[tokio::test]
async fn test_admin_services_crud() {
    let server = setup_test_server().await;
    let service_name = format!(
        "crud_service_{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    );

    // Create (admin)
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
    let created: ServiceResponse =
        serde_json::from_str(&create_resp.text()).expect("Parse create response");
    let id = created.id;

    // Get by name (public, no auth)
    let get_resp = server
        .get(format!("/v1/services/{}", service_name).as_str())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get_resp.status_code(), 200);
    let got: ServiceResponse = serde_json::from_str(&get_resp.text()).expect("Parse get response");
    assert_eq!(got.id, id);
    assert_eq!(got.display_name, "CRUD Test");
    assert_eq!(got.cost_per_unit, 1_000_000);

    // Update display_name, description, cost_per_unit
    let update_req = UpdateServiceRequest {
        display_name: Some("CRUD Updated".to_string()),
        description: Some("Updated desc".to_string()),
        cost_per_unit: Some(3_000_000),
        is_active: None,
    };
    let patch_resp = server
        .patch(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&update_req)
        .await;
    assert_eq!(patch_resp.status_code(), 200);

    // Get again - verify update (public)
    let get2_resp = server
        .get(format!("/v1/services/{}", service_name).as_str())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get2_resp.status_code(), 200);
    let got2: ServiceResponse =
        serde_json::from_str(&get2_resp.text()).expect("Parse get2 response");
    assert_eq!(got2.display_name, "CRUD Updated");
    assert_eq!(got2.description.as_deref(), Some("Updated desc"));
    assert_eq!(got2.cost_per_unit, 3_000_000);

    // Disable service (PATCH is_active = false)
    let disable_req = UpdateServiceRequest {
        display_name: None,
        description: None,
        cost_per_unit: None,
        is_active: Some(false),
    };
    let patch2_resp = server
        .patch(format!("/v1/admin/services/{}", id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&disable_req)
        .await;
    assert_eq!(patch2_resp.status_code(), 200);

    // Public GET returns 404 when service is disabled (get_active_by_name filters inactive)
    let get3_resp = server
        .get(format!("/v1/services/{}", service_name).as_str())
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(get3_resp.status_code(), 404);
}
