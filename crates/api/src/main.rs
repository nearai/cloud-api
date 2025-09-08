use axum::{
    routing::{get, post},
    Router,
};
use api::routes::{chat_completions, completions, models, quote};
use domain::Domain;
use config::{ApiConfig, LoggingConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Load configuration first to get logging settings
    let config = ApiConfig::load().unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {}", e);
        eprintln!("Application cannot start without a valid configuration file.");
        std::process::exit(1);
    });

    // Initialize tracing with configuration from config.yaml
    init_tracing(&config.logging);

    // Create the domain service from YAML configuration
    let domain = Arc::new(Domain::from_config().await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Failed to load domain configuration");
        tracing::error!("Application cannot start without a valid configuration. Exiting.");
        std::process::exit(1);
    }));

    // Get server config before moving domain into router state
    let server_config = domain.server_config().clone();
    let bind_address = format!("{}:{}", server_config.host, server_config.port);

    // build our application with routes
    let app = Router::new()
        // AI completion endpoints
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        // Models endpoint
        .route("/v1/models", get(models))
        // TDX attestation endpoint
        .route("/v1/quote", get(quote))
        // Share the domain service as application state
        .with_state(domain);

    // run our app with hyper, using configuration from domain
    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();
    tracing::info!(address = %bind_address, "Server started successfully");
    tracing::info!("Available endpoints:");
    tracing::info!("  - POST /v1/chat/completions (Chat Completions)");
    tracing::info!("  - POST /v1/completions (Text Completions)");
    tracing::info!("  - GET /v1/models (Available Models)");
    tracing::info!("  - GET /v1/quote (TDX Quote & Attestation)");
    axum::serve(listener, app).await.unwrap();
}

fn init_tracing(logging_config: &LoggingConfig) {
    // Build the filter string from the logging configuration
    let mut filter = logging_config.level.clone();
    
    for (module, level) in &logging_config.modules {
        filter.push_str(&format!(",{}={}", module, level));
    }
    
    // Initialize tracing based on the format specified in config
    match logging_config.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        },
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(filter)
                .init();
        },
        "pretty" | _ => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        },
    }
}
