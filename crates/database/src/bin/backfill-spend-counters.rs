use anyhow::{bail, Context, Result};
use database::spend_counters_backfill::{
    incomplete_organizations, PreparedSpendBackfill, SpendBackfillOutcome,
};
use database::Database;
use std::time::Duration;
use uuid::Uuid;

const BATCH_SIZE: i64 = 100;
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

#[derive(Debug)]
struct Options {
    organization_id: Option<Uuid>,
    statement_timeout: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = match parse_args(std::env::args().skip(1).collect())? {
        Some(options) => options,
        None => return Ok(()),
    };
    tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .init();

    let config = config::DatabaseConfig::from_env()
        .map_err(|error| anyhow::anyhow!("failed to load database configuration: {error}"))?;
    let database = Database::from_config(&config)
        .await
        .context("connecting to database")?;
    tracing::info!(
        statement_timeout_seconds = options.statement_timeout.as_secs(),
        "starting spend counter backfill; all writers must maintain counters and raw history must not be rewritten"
    );

    if let Some(organization_id) = options.organization_id {
        reconcile_one(&database, organization_id, options.statement_timeout).await?;
    } else {
        let mut after = None;
        loop {
            let ids = incomplete_organizations(database.pool(), after, BATCH_SIZE).await?;
            if ids.is_empty() {
                break;
            }
            after = ids.last().copied();
            for organization_id in ids {
                reconcile_one(&database, organization_id, options.statement_timeout).await?;
            }
        }
        database::ensure_spend_counters_ready(database.pool()).await?;
    }
    tracing::info!("spend counter backfill complete");
    Ok(())
}

async fn reconcile_one(
    database: &Database,
    organization_id: Uuid,
    statement_timeout: Duration,
) -> Result<()> {
    let Some(prepared) =
        PreparedSpendBackfill::prepare(database.pool(), organization_id, statement_timeout).await?
    else {
        tracing::info!(%organization_id, "spend counters already ready");
        return Ok(());
    };
    match prepared.apply().await? {
        SpendBackfillOutcome::Applied { key_count } => {
            tracing::info!(%organization_id, key_count, "reconciled spend counters");
        }
        SpendBackfillOutcome::AlreadyComplete => {
            tracing::info!(%organization_id, "spend counters completed by another worker");
        }
    }
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<Option<Options>> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(None);
    }
    let mut organization_id = None;
    let mut timeout_seconds = DEFAULT_TIMEOUT_SECONDS;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--organization" => {
                let value = args.next().context("--organization needs a UUID")?;
                organization_id = Some(
                    value
                        .parse()
                        .with_context(|| format!("invalid organization UUID: {value}"))?,
                );
            }
            "--statement-timeout-seconds" => {
                let value = args
                    .next()
                    .context("--statement-timeout-seconds needs an integer")?;
                timeout_seconds = value
                    .parse()
                    .with_context(|| format!("invalid timeout seconds: {value}"))?;
                if timeout_seconds == 0 {
                    bail!("--statement-timeout-seconds must be positive");
                }
            }
            unknown => bail!("unknown argument {unknown}; use --help"),
        }
    }
    Ok(Some(Options {
        organization_id,
        statement_timeout: Duration::from_secs(timeout_seconds),
    }))
}

fn print_help() {
    println!(
        "Usage: backfill-spend-counters [--organization UUID] [--statement-timeout-seconds N]\n\n\
Reconciles incomplete organization spend counters. Deploy counter writers (#1116)\n\
to every process and drain all older writers first. Do not rewrite or delete raw usage history\n\
while this process runs. The command does not run migrations."
    );
}
