// E2E tests for the admin infra summary

use crate::common::*;
use services::admin::InfraSummary;

#[tokio::test]
async fn test_admin_platform_infra_summary_graceful() {
    let server = setup_test_server().await;

    // The machines endpoint is not reachable in tests; the handler must degrade
    // gracefully (200 + stale flag), never 500.
    let response = server
        .get("/v1/admin/platform/infra-summary")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let infra: InfraSummary = serde_json::from_str(&response.text()).expect("parse InfraSummary");
    assert!(infra.cost_per_host_usd_month >= 0.0);
    // Unconfigured/unreachable inventory in tests -> zero hosts, marked stale.
    assert!(
        infra.stale && infra.total_hosts == 0,
        "unconfigured fleet should be stale with 0 hosts"
    );
    assert!(
        infra.gpu_data_stale
            && infra.total_allocated_gpus == 0
            && infra.model_gpu_allocations.is_empty(),
        "unconfigured Prometheus should be stale with no claimed GPU allocation"
    );

    println!("✅ Platform infra summary degrades gracefully");
}
