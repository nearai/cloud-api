use anyhow::{Context, Result};
use api::database_encryption::{
    operational_migrate, operational_scan, operational_verify, DatabaseEncryptionState,
};
use clap::{Parser, Subcommand};
use database::Database;
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "One-off database encryption backfill worker")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Scan {
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
    },
    Migrate {
        #[arg(long, required = true, value_delimiter = ',')]
        scope: Vec<String>,
        #[arg(long, default_value_t = 500)]
        batch_size: i64,
        #[arg(long)]
        max_rows: Option<i64>,
        #[arg(long)]
        resume: Option<Uuid>,
        #[arg(long)]
        operator: String,
    },
    Verify {
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    if run().await.is_err() {
        println!(
            "DATABASE_ENCRYPTION_WORKER_RESULT {}",
            serde_json::json!({"status":"failed","error_class":"worker_failed"})
        );
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let database_config = config::DatabaseConfig::from_env()
        .map_err(anyhow::Error::msg)
        .context("invalid database configuration")?;
    let key = read_encryption_key()?;
    let database = Database::from_config(&database_config).await?;
    let state = DatabaseEncryptionState::new(database.pool().clone(), &key)?;

    match cli.command {
        Command::Scan { scope } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&operational_scan(&state, scope).await?)?
            );
            print_success(None);
        }
        Command::Verify { scope } => {
            let report = operational_verify(&state, scope).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report["pass"] != true {
                anyhow::bail!("verification found plaintext or invalid envelopes");
            }
            print_success(None);
        }
        Command::Migrate {
            scope,
            batch_size,
            max_rows,
            resume,
            operator,
        } => {
            let id =
                operational_migrate(&state, scope, batch_size, max_rows, resume, &operator).await?;
            print_success(Some(id));
        }
    }
    Ok(())
}

fn print_success(job_id: Option<Uuid>) {
    println!(
        "DATABASE_ENCRYPTION_WORKER_RESULT {}",
        serde_json::json!({"status":"completed","job_id":job_id})
    );
}

fn read_encryption_key() -> Result<String> {
    if let Ok(key) = std::env::var("S3_ENCRYPTION_KEY") {
        return Ok(key);
    }
    let path = std::env::var("S3_ENCRYPTION_KEY_FILE")
        .context("S3_ENCRYPTION_KEY or S3_ENCRYPTION_KEY_FILE is required")?;
    std::fs::read_to_string(path)
        .context("failed to read S3_ENCRYPTION_KEY_FILE")
        .map(|key| key.trim().to_string())
}
