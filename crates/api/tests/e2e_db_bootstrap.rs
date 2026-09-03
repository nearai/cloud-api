#[allow(dead_code)]
#[path = "common/db_setup.rs"]
mod db_setup;

use std::fs::OpenOptions;
use std::io::Write;

/// Prepared by nextest's setup script before any e2e test process is launched.
/// It remains ignored so ordinary `cargo test` runs do not execute infrastructure
/// setup as a standalone test target.
#[tokio::test]
#[ignore = "invoked by the nextest e2e setup script"]
async fn bootstrap_e2e_database() {
    db_setup::bootstrap_test_database().await;

    let Some(nextest_env_path) = std::env::var_os("NEXTEST_ENV") else {
        return;
    };
    let nextest_env_path = std::path::PathBuf::from(nextest_env_path);
    assert!(
        nextest_env_path.is_absolute(),
        "NEXTEST_ENV must be an absolute path"
    );

    let mut nextest_env = OpenOptions::new()
        .append(true)
        .open(&nextest_env_path)
        .expect("open nextest's environment file");
    let marker = db_setup::nextest_bootstrap_marker()
        .expect("the e2e setup target must be invoked by nextest");
    assert!(
        !marker.contains('\r') && !marker.contains('\n'),
        "the e2e bootstrap marker cannot contain a line break"
    );
    writeln!(
        nextest_env,
        "{}={marker}",
        db_setup::E2E_DATABASE_BOOTSTRAPPED_ENV,
    )
    .expect("record completed e2e database bootstrap for test processes");
}
