use api::{build_app_with_config, init_auth_services, init_database, init_domain_services};
use config::{ApiConfig, LoggingConfig};
use database::{Database, ShutdownCoordinator, ShutdownStage};
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    metrics::{MeterProvider, PeriodicReader},
    runtime, Resource,
};
use services::inference_provider_pool::InferenceProviderPool;
use services::metrics::{MetricsServiceTrait, OtlpMetricsService};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Load configuration and initialize logging
    let config = load_configuration();
    init_tracing(&config.logging);
    tracing::debug!("Config: {:?}", config);

    // Initialize core services
    let database = init_database(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);

    // Initialize OpenTelemetry pipeline
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(&config.otlp.endpoint)
        .build_metrics_exporter(
            Box::new(opentelemetry_sdk::metrics::reader::DefaultAggregationSelector::new()),
            Box::new(opentelemetry_sdk::metrics::reader::DefaultTemporalitySelector::new()),
        )
        .expect("Failed to build OTLP metrics exporter");

    let reader = PeriodicReader::builder(exporter, runtime::Tokio).build();

    // Get environment from env var (local, dev, staging, prod)
    let environment = std::env::var("ENVIRONMENT").unwrap_or_else(|_| "local".to_string());

    let meter_provider = MeterProvider::builder()
        .with_reader(reader)
        .with_resource(Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", "cloud-api"),
            opentelemetry::KeyValue::new("environment", environment.clone()),
        ]))
        .build();

    tracing::info!(
        "OpenTelemetry metrics initialized for environment: {}",
        environment
    );

    global::set_meter_provider(meter_provider.clone());

    // Initialize metrics service
    let metrics_service =
        Arc::new(OtlpMetricsService::new(&meter_provider)) as Arc<dyn MetricsServiceTrait>;

    let domain_services = init_domain_services(
        database.clone(),
        &config,
        auth_components.organization_service.clone(),
        metrics_service,
    )
    .await;

    let config = Arc::new(config);

    // Build application router with config
    let app = build_app_with_config(
        database.clone(),
        auth_components,
        domain_services.clone(),
        config.clone(),
    );

    // Start server with graceful shutdown handling
    start_server(
        app,
        config,
        database,
        domain_services.inference_provider_pool,
    )
    .await;
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

/// Start the HTTP server with graceful shutdown on SIGTERM/SIGINT
async fn start_server(
    app: axum::Router,
    config: Arc<ApiConfig>,
    database: Arc<Database>,
    inference_provider_pool: Arc<InferenceProviderPool>,
) {
    let bind_address = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_address)
        .await
        .expect("Failed to bind to address");

    tracing::debug!(address = %bind_address, "Server started successfully");
    tracing::info!(
        "Authentication: {}",
        if config.auth.mock {
            "MOCK MODE"
        } else {
            "PRODUCTION MODE"
        }
    );

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    match server.await {
        Ok(_) => {
            tracing::info!("Server shutdown successfully, initiating coordinated cleanup");
            perform_coordinated_shutdown(database, inference_provider_pool).await;
        }
        Err(e) => {
            tracing::error!("Server error: {}", e);
            perform_coordinated_shutdown(database, inference_provider_pool).await;
            std::process::exit(1);
        }
    }
}

/// Perform coordinated shutdown with timeout protection
async fn perform_coordinated_shutdown(
    database: Arc<Database>,
    inference_provider_pool: Arc<InferenceProviderPool>,
) {
    let mut coordinator = ShutdownCoordinator::new(Duration::from_secs(30));
    coordinator.start();

    tracing::info!("=== SHUTDOWN PHASE: CANCEL BACKGROUND TASKS ===");
    tracing::info!("Cancelling all periodic background tasks");

    // Stage 1: Cancel background tasks (should be quick, 5-10 seconds)
    let (status, remaining) = coordinator
        .execute_stage(
            ShutdownStage {
                name: "Cancel Background Tasks",
                timeout: Duration::from_secs(10),
            },
            || async {
                tracing::info!("Step 1.1: Cancelling model discovery refresh task");
                inference_provider_pool.shutdown().await;
                tracing::debug!("All background tasks cancelled");
            },
        )
        .await;
    tracing::info!("PHASE 1 COMPLETE: {:?}", status);
    tracing::info!("  Time remaining: {:.2}s", remaining.as_secs_f32());

    if coordinator.has_exceeded_timeout() {
        tracing::warn!("Global timeout exceeded during Phase 1. Proceeding with Phase 2.");
    }

    tracing::info!("");
    tracing::info!("=== SHUTDOWN PHASE: CLOSE CONNECTIONS ===");
    tracing::info!("Closing database connections and resource pools");

    // Stage 2: Close connections (more time-intensive, 10-15 seconds)
    let (status, remaining) = coordinator
        .execute_stage(
            ShutdownStage {
                name: "Close Connections",
                timeout: Duration::from_secs(15),
            },
            || async {
                tracing::info!("Step 2.1: Draining active database connections");
                // Database::shutdown() drains connections with 15s timeout
                database.shutdown().await;
                tracing::info!("Step 2.2: Active connections drained and pools closed");

                tracing::debug!("Connection pool resources released");
            },
        )
        .await;
    tracing::info!("PHASE 2 COMPLETE: {:?}", status);
    tracing::info!("  Time remaining: {:.2}s", remaining.as_secs_f32());

    tracing::info!("");
    coordinator.finish();
    tracing::info!("=== SHUTDOWN COMPLETE ===");
}

async fn shutdown_signal() {
    use tokio::signal;

    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to setup SIGTERM handler")
            .recv()
            .await;
    };

    let sigint = async {
        signal::ctrl_c().await.ok();
    };

    tokio::select! {
        _ = sigterm => {
            tracing::info!("SIGTERM signal received");
            tracing::info!("=== SHUTDOWN PHASE 0: STOP ACCEPTING REQUESTS ===");
            tracing::info!("Server will stop accepting new requests immediately");
            tracing::info!("Draining in-flight requests...");
        }
        _ = sigint => {
            tracing::info!("SIGINT signal received");
            tracing::info!("=== SHUTDOWN PHASE 0: STOP ACCEPTING REQUESTS ===");
            tracing::info!("Server will stop accepting new requests immediately");
            tracing::info!("Draining in-flight requests...");
        }
    }
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
                .with_current_span(false)
                .with_span_list(false)
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(filter)
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .init();
        }
        "pretty" => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter)
                .init();
        }
        _ => {
            // Default to JSON format for containerized environments (Datadog friendly)
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .with_current_span(false)
                .with_span_list(false)
                .init();
        }
    }
}
