use api::{build_app_with_config, init_auth_services, init_database, init_domain_services};
use config::{ApiConfig, LoggingConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Load configuration and initialize logging
    let config = load_configuration();
    init_tracing(&config.logging);
    tracing::debug!("Config: {:?}", config);

    // Initialize core services
    let database = init_database(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);
    let domain_services = init_domain_services(
        database.clone(),
        &config,
        auth_components.organization_service.clone(),
    )
    .await;

    let config = Arc::new(config);

    // Build application router with config
    let app = build_app_with_config(database, auth_components, domain_services, config.clone());

    // Start server
    start_server(app, config).await;
}

/// Load and validate configuration
fn load_configuration() -> ApiConfig {
    ApiConfig::load().unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {e}");
        eprintln!("Application cannot start without valid configuration.");
        eprintln!("Please ensure environment variables are set or a .env file exists.");
        eprintln!("See env.template for a complete list of required environment variables.");
        std::process::exit(1);
    })
}

/// Start the HTTP server
async fn start_server(app: axum::Router, config: Arc<ApiConfig>) {
    let bind_address = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_address)
        .await
        .expect("Failed to bind to address");

    tracing::info!(address = %bind_address, "Server started successfully");
    tracing::info!(
        "Authentication: {}",
        if config.auth.mock {
            "MOCK MODE"
        } else {
            "PRODUCTION MODE"
        }
    );

    axum::serve(listener, app)
        .await
        .expect("Server failed to run");
}

/// Initialize tracing/logging based on configuration
fn init_tracing(logging_config: &LoggingConfig) {
    // Build the filter string from the logging configuration
    let mut filter = logging_config.level.clone();
    for (module, level) in &logging_config.modules {
        filter.push_str(&format!(",{module}={level}"));
    }

    // Initialize tracing based on the format specified in config
    match logging_config.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(filter)
                .init();
        }
        "pretty" => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
        _ => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
    }
}
