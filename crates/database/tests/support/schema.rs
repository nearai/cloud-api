//! Applies the ledger schema through `scripts/apply-schema.sh`, which runs the pinned
//! cloud-platform migrator. Each test process runs it at most once; the script is idempotent.

use anyhow::{bail, Context, Result};
use tokio::sync::OnceCell;

static SCHEMA_READY: OnceCell<()> = OnceCell::const_new();

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Ensure `PGDATABASE` has the current schema, using the same libpq variables and defaults
/// as the test pools.
pub async fn ensure_schema() -> Result<()> {
    SCHEMA_READY
        .get_or_try_init(|| async { apply_schema().await })
        .await?;
    Ok(())
}

async fn apply_schema() -> Result<()> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apply-schema.sh");
    let status = tokio::process::Command::new(script)
        .env("PGHOST", env_or("PGHOST", "localhost"))
        .env("PGPORT", env_or("PGPORT", "5432"))
        .env("PGUSER", env_or("PGUSER", "postgres"))
        .env("PGPASSWORD", env_or("PGPASSWORD", "postgres"))
        .env("PGDATABASE", env_or("PGDATABASE", "platform_api"))
        .status()
        .await
        .context("run scripts/apply-schema.sh")?;
    if !status.success() {
        bail!("scripts/apply-schema.sh failed with {status}");
    }
    Ok(())
}
