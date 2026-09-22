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
                let timeout = Duration::from_secs(timeout_seconds);
                if timeout.as_millis() == 0 || timeout.as_millis() > i32::MAX as u128 {
                    bail!(
                        "--statement-timeout-seconds must produce a timeout between 1ms and {}ms",
                        i32::MAX
                    );
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
while this process runs. The statement timeout applies to the repeatable-read snapshot;\n\
apply and accounting-lock statements remain capped at 5 seconds. The command does not run migrations."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults() {
        let options = parse_args(Vec::new()).unwrap().unwrap();
        assert_eq!(options.organization_id, None);
        assert_eq!(options.statement_timeout, Duration::from_secs(300));
    }

    #[test]
    fn parse_args_accepts_organization_uuid() {
        let organization_id = Uuid::new_v4();
        let options = parse_args(vec![
            "--organization".to_string(),
            organization_id.to_string(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(options.organization_id, Some(organization_id));
    }

    #[test]
    fn parse_args_rejects_missing_organization_uuid() {
        let error = parse_args(vec!["--organization".to_string()]).unwrap_err();
        assert!(error.to_string().contains("--organization needs a UUID"));
    }

    #[test]
    fn parse_args_rejects_invalid_organization_uuid() {
        let error =
            parse_args(vec!["--organization".to_string(), "not-a-uuid".to_string()]).unwrap_err();
        assert!(error.to_string().contains("invalid organization UUID"));
    }

    #[test]
    fn parse_args_rejects_missing_statement_timeout() {
        let error = parse_args(vec!["--statement-timeout-seconds".to_string()]).unwrap_err();
        assert!(error
            .to_string()
            .contains("--statement-timeout-seconds needs an integer"));
    }

    #[test]
    fn parse_args_rejects_invalid_statement_timeout() {
        let error = parse_args(vec![
            "--statement-timeout-seconds".to_string(),
            "not-a-number".to_string(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("invalid timeout seconds"));
    }

    #[test]
    fn parse_args_rejects_zero_statement_timeout() {
        let error = parse_args(vec![
            "--statement-timeout-seconds".to_string(),
            "0".to_string(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("must produce a timeout"));
    }

    #[test]
    fn parse_args_rejects_oversized_statement_timeout() {
        let seconds = (i32::MAX as u64 / 1000) + 1;
        let error = parse_args(vec![
            "--statement-timeout-seconds".to_string(),
            seconds.to_string(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("must produce a timeout"));
    }

    #[test]
    fn parse_args_help_returns_none() {
        assert!(parse_args(vec!["--help".to_string()]).unwrap().is_none());
        assert!(parse_args(vec!["-h".to_string()]).unwrap().is_none());
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let error = parse_args(vec!["--unknown".to_string()]).unwrap_err();
        assert!(error.to_string().contains("unknown argument --unknown"));
    }
}
