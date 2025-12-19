// Utility test to clean up orphaned test databases
// Run with: cargo test --test cleanup_test_databases -- --nocapture

mod common;

/// Clean up ALL test databases (databases starting with 'test_')
/// This is useful for cleaning up orphaned databases from previous test runs.
///
/// Run with: cargo test --test cleanup_test_databases -- --nocapture
#[tokio::test]
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
