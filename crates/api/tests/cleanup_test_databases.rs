// Utility to clean up orphaned test databases.
// Run with: cargo test --test cleanup_test_databases -- --ignored --nocapture

mod common;

#[tokio::test]
#[ignore] // Don't run during normal test suite to avoid interfering with parallel tests
async fn cleanup_all_test_databases() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::level_filters::LevelFilter::INFO)
        .try_init();

    let config = common::test_config();

    match common::db_setup::drop_all_test_databases(&config.database).await {
        Ok(count) => {
            println!("Successfully cleaned up {} test database(s)", count);
        }
        Err(e) => {
            println!("Failed to clean up test databases: {}", e);
        }
    }
}
