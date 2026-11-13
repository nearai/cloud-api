//! Applies the ledger schema through `scripts/apply-schema.sh`, which runs the pinned
//! cloud-platform migrator. Each test process runs it at most once; the script is idempotent.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::OnceCell;

static SCHEMA_READY: OnceCell<()> = OnceCell::const_new();

/// Bounds an image pull plus the migrator's own lock wait, so a hang fails instead of stalling.
const SCHEMA_APPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// An empty variable counts as unset, matching the test pools' fallbacks.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
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
    let mut command = tokio::process::Command::new(script);
    command
        .env("PGHOST", env_or("PGHOST", "localhost"))
        .env("PGPORT", env_or("PGPORT", "5432"))
        .env("PGUSER", env_or("PGUSER", "postgres"))
        .env("PGPASSWORD", env_or("PGPASSWORD", "postgres"))
        .env("PGDATABASE", env_or("PGDATABASE", "platform_api"))
        .kill_on_drop(true);
    let status = tokio::time::timeout(SCHEMA_APPLY_TIMEOUT, command.status())
        .await
        .context("scripts/apply-schema.sh timed out")?
        .context("run scripts/apply-schema.sh")?;
    if !status.success() {
        bail!("scripts/apply-schema.sh failed with {status}");
    }
    let (client, connection) = tokio_postgres::Config::new()
        .host(env_or("PGHOST", "localhost"))
        .port(env_or("PGPORT", "5432").parse().context("parse PGPORT")?)
        .user(env_or("PGUSER", "postgres"))
        .password(env_or("PGPASSWORD", "postgres"))
        .dbname(env_or("PGDATABASE", "platform_api"))
        .connect(tokio_postgres::NoTls)
        .await
        .context("connect for the test schema bridge")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(include_str!(
            "../../../../scripts/test-chat-signature-owner.sql"
        ))
        .await
        .context("apply the test schema bridge")?;
    Ok(())
}
