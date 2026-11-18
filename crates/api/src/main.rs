use api::{
    build_app_with_config, init_auth_services, init_database, init_domain_services, telemetry,
};
use config::{ApiConfig, LoggingConfig};
use database::repositories::AdminCompositeRepository;
use database::{Database, ShutdownCoordinator, ShutdownStage};
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use services::admin::ModelPricingScheduler;
use services::inference_provider_pool::InferenceProviderPool;
use services::metrics::{MetricsServiceTrait, OtlpMetricsService};
use std::sync::Arc;
use std::time::Duration;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{
    layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

#[tokio::main]
async fn main() {
    // Install the process-wide rustls crypto provider unconditionally, as
    // early as possible. Both `aws-lc-rs` and `ring` are enabled on rustls
    // (needed for TLS support across the dependency graph), which removes
    // rustls's implicit default provider for the whole binary: any rustls
    // config built through that implicit default (today, `redis`'s
    // `rediss://` TLS setup) would otherwise panic unless something already
    // installed a provider first. `inference_providers::placement_io` also
    // calls this lazily inside `PlacementIo::start` (kept below, since it's
    // the one path that's guaranteed to run before that crate's own
    // `rediss://` client is built) — installing it here too makes the
    // default provider exist regardless of whether placement is configured.
    // `install_crypto_provider` is idempotent: a second call is a no-op.
    // The `database` crate separately installs `ring` as well; both calls
    // race for "install_default" and only the first one wins, which is fine
    // since they install the same provider.
    inference_providers::placement_io::install_crypto_provider();

    // Load configuration and initialize logging
    let config = load_configuration();

    // Get environment from env var (local, dev, staging, prod)
    let environment = std::env::var("ENVIRONMENT").unwrap_or_else(|_| "local".to_string());
    let resource =
        telemetry::build_telemetry_resource(&environment, config.otlp.instance_id.as_deref());

    // The tracer provider is built before the subscriber so the subscriber can
    // export spans through it. `None` when tracing is disabled.
    let tracer_provider = telemetry::build_tracer_provider(&config.otlp, resource.clone());
    init_tracing(&config.logging, tracer_provider.as_ref());
    tracing::info!(
        traces_enabled = tracer_provider.is_some(),
        traces_sample_ratio = config.otlp.traces_sample_ratio,
        "Distributed tracing configured"
    );

    // Initialize core services
    let database = init_database(&config.database).await;
    let auth_components = init_auth_services(database.clone(), &config);

    // Initialize OpenTelemetry pipeline
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(&config.otlp.endpoint)
        .build()
        .expect("Failed to build OTLP metrics exporter");

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource)
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

    // Start the scheduled-pricing-change apply task. Safe to run on every
    // instance: due rows are claimed atomically (FOR UPDATE SKIP LOCKED).
    let pricing_scheduler = Arc::new(ModelPricingScheduler::new(
        Arc::new(AdminCompositeRepository::new(database.pool().clone())),
        api::cloud_platform::invalidating_models_service(
            domain_services.models_service.clone(),
            domain_services.cloud_platform_catalog.clone(),
        ),
    ));
    pricing_scheduler
        .clone()
        .start(config.server.pricing_change_apply_interval_secs)
        .await;

    // Maintain the usage_hourly aggregate. Safe on every instance: recompute takes a
    // transaction-scoped try-lock, so one replica writes per tick.
    let usage_hourly_scheduler = Arc::new(services::usage::UsageHourlyScheduler::new(
        Arc::new(database::repositories::UsageHourlyRepositoryImpl::new(
            database.pool().clone(),
        )),
        domain_services.metrics_service.clone(),
    ));
    usage_hourly_scheduler
        .clone()
        .start(config.server.usage_hourly_interval_secs)
        .await;

    // A durable per-org claim prevents replica duplication. Keep two jobs in
    // flight so a slow final verification does not stall every other organization.
    let discount_repository = database::repositories::organization_usage_discount::OrganizationUsageDiscountRepository::new(database.pool().clone());
    let discount_worker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    while jobs.len() < 2 {
                        match discount_repository.claim_next().await {
                            Ok(Some(claim)) => {
                                let repository = discount_repository.clone();
                                jobs.spawn(async move {
                                    let organization_id = claim.organization_id;
                                    if let Err(error) = repository.run_claimed(claim).await {
                                        tracing::error!(%organization_id, %error, "Usage discount correction deferred for retry");
                                    }
                                });
                            }
                            Ok(None) => break,
                            Err(error) => {
                                tracing::error!(%error, "Failed to claim usage discount work");
                                break;
                            }
                        }
                    }
                }
                result = jobs.join_next(), if !jobs.is_empty() => {
                    if let Some(Err(error)) = result {
                        tracing::error!(%error, "Usage discount worker stopped; claim will expire");
                    }
                }
            }
        }
    });

    // Start server with graceful shutdown handling
    start_server(
        app,
        config,
        database,
        domain_services.inference_provider_pool,
        pricing_scheduler,
        usage_hourly_scheduler,
        discount_worker,
        tracer_provider,
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
#[allow(clippy::too_many_arguments)]
async fn start_server(
    app: axum::Router,
    config: Arc<ApiConfig>,
    database: Arc<Database>,
    inference_provider_pool: Arc<InferenceProviderPool>,
    pricing_scheduler: Arc<ModelPricingScheduler>,
    usage_hourly_scheduler: Arc<services::usage::UsageHourlyScheduler>,
    discount_worker: tokio::task::JoinHandle<()>,
    tracer_provider: Option<SdkTracerProvider>,
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

    let result = server.await;
    discount_worker.abort();
    let _ = discount_worker.await;
    match result {
        Ok(_) => {
            tracing::info!("Server shutdown successfully, initiating coordinated cleanup");
            perform_coordinated_shutdown(
                database,
                inference_provider_pool,
                pricing_scheduler,
                usage_hourly_scheduler,
            )
            .await;
            shutdown_tracer_provider(tracer_provider).await;
        }
        Err(e) => {
            tracing::error!("Server error: {}", e);
            perform_coordinated_shutdown(
                database,
                inference_provider_pool,
                pricing_scheduler,
                usage_hourly_scheduler,
            )
            .await;
            shutdown_tracer_provider(tracer_provider).await;
            std::process::exit(1);
        }
    }
}

/// Flush pending spans and stop the tracer provider. Logs only whether the
/// shutdown succeeded.
async fn shutdown_tracer_provider(tracer_provider: Option<SdkTracerProvider>) {
    let Some(provider) = tracer_provider else {
        return;
    };
    let result =
        tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(Duration::from_secs(5)))
            .await;
    let flushed = matches!(result, Ok(Ok(())));
    tracing::info!(flushed, "Tracer provider shut down");
}

/// Perform coordinated shutdown with timeout protection
async fn perform_coordinated_shutdown(
    database: Arc<Database>,
    inference_provider_pool: Arc<InferenceProviderPool>,
    pricing_scheduler: Arc<ModelPricingScheduler>,
    usage_hourly_scheduler: Arc<services::usage::UsageHourlyScheduler>,
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
                tracing::info!("Step 1.2: Cancelling pricing change scheduler task");
                pricing_scheduler.shutdown().await;
                tracing::info!("Step 1.3: Cancelling usage_hourly scheduler task");
                usage_hourly_scheduler.shutdown().await;
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

/// Dependencies whose debug output carries query parameters or request
/// credentials. When the default level is more verbose than `info`, they are
/// capped at `info`, so a debug-level deployment does not write those values
/// to logs.
const CAPPED_DEPENDENCY_TARGETS: &[&str] = &["tokio_postgres", "aws_smithy_runtime", "aws_sigv4"];

/// Build the `EnvFilter` directive string from the logging configuration.
///
/// The caps go first. `EnvFilter` replaces an earlier directive with a later
/// one for the same target and prefers more specific targets, so a directive
/// the operator sets for one of these crates or its submodules still applies.
/// A stricter default level (`warn`, `error`, `off`) already silences them, so
/// no cap is added and that level is kept.
fn build_log_filter(logging_config: &LoggingConfig) -> String {
    let mut directives = Vec::new();
    if default_level(&logging_config.level).is_some_and(|level| level > LevelFilter::INFO) {
        directives.extend(
            CAPPED_DEPENDENCY_TARGETS
                .iter()
                .map(|target| format!("{target}=info")),
        );
    }
    if !logging_config.level.trim().is_empty() {
        directives.push(logging_config.level.clone());
    }
    for (module, level) in &logging_config.modules {
        directives.push(format!("{module}={level}"));
    }
    directives.join(",")
}

/// The default level a filter string sets: its last bare level directive.
fn default_level(filter: &str) -> Option<LevelFilter> {
    filter
        .rsplit(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .find_map(|directive| directive.parse::<LevelFilter>().ok())
}

/// Crate targets whose spans are exported. Dependency crates are never
/// exported, whatever their level.
const EXPORTED_SPAN_TARGETS: &[&str] = &["api", "services", "inference_providers", "database"];

/// Whether `metadata` describes an INFO-or-more-severe span from one of our
/// own crates. Events never qualify, so log messages cannot become span
/// events.
fn is_exported_span(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.is_span()
        && *metadata.level() <= tracing::Level::INFO
        && EXPORTED_SPAN_TARGETS.iter().any(|prefix| {
            metadata
                .target()
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
}

/// The span-export filter. The decision depends only on static metadata, so
/// it is cached per callsite and carries an INFO level hint: callsites it
/// rejects (dependency crates, events, verbose levels) cost nothing per call.
fn exported_span_filter<S>() -> impl tracing_subscriber::layer::Filter<S> {
    tracing_subscriber::filter::dynamic_filter_fn(|metadata, _| is_exported_span(metadata))
        .with_callsite_filter(|metadata| {
            if is_exported_span(metadata) {
                tracing::subscriber::Interest::always()
            } else {
                tracing::subscriber::Interest::never()
            }
        })
        .with_max_level_hint(LevelFilter::INFO)
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Build the log output layer for the configured format. The log filter is
/// attached to this layer only, so it never affects span export.
fn build_fmt_layer(logging_config: &LoggingConfig) -> BoxedLayer {
    let filter = EnvFilter::new(build_log_filter(logging_config));
    match logging_config.format.as_str() {
        "compact" => tracing_subscriber::fmt::layer()
            .compact()
            .with_target(false)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_filter(filter)
            .boxed(),
        "pretty" => tracing_subscriber::fmt::layer()
            .pretty()
            .with_filter(filter)
            .boxed(),
        // "json" and the default for containerized environments.
        _ => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_filter(filter)
            .boxed(),
    }
}

/// Initialize tracing/logging based on configuration. When a tracer provider
/// is given, spans from our own crates are also exported through it.
fn init_tracing(logging_config: &LoggingConfig, tracer_provider: Option<&SdkTracerProvider>) {
    let mut layers: Vec<BoxedLayer> = vec![build_fmt_layer(logging_config)];
    if let Some(provider) = tracer_provider {
        layers.push(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("cloud-api"))
                .with_filter(exported_span_filter())
                .boxed(),
        );
    }
    tracing_subscriber::registry().with(layers).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn only_info_or_higher_spans_from_our_crates_are_exported() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};

        let exporter = InMemorySpanExporter::default();
        let provider = telemetry::tracer_provider_with_processor(
            1.0,
            SimpleSpanProcessor::new(exporter.clone()),
        );
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_filter(exported_span_filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let _kept = tracing::info_span!(target: "api::routes", "kept").entered();
            tracing::info!(target: "api::routes", "an event is never a span event");
            drop(_kept);
            tracing::info_span!(target: "services", "kept_exact").in_scope(|| {});
            tracing::debug_span!(target: "api::routes", "too_verbose").in_scope(|| {});
            tracing::info_span!(target: "tokio_postgres::query", "dependency").in_scope(|| {});
            tracing::info_span!(target: "apiary::other", "similar_prefix").in_scope(|| {});
        });
        let spans = exporter.get_finished_spans().unwrap();
        let mut names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["kept", "kept_exact"]);
        assert!(spans.iter().all(|span| span.events.is_empty()));
    }

    fn logging(level: &str, modules: &[(&str, &str)]) -> LoggingConfig {
        LoggingConfig {
            level: level.to_string(),
            format: "json".to_string(),
            modules: modules
                .iter()
                .map(|(module, level)| (module.to_string(), level.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured logs mutex should not poison")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Emit one debug event per target under `filter` and return what was written.
    fn emitted_under(filter: &str) -> String {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "tokio_postgres::query", "dependency-postgres-debug");
            tracing::debug!(
                target: "aws_smithy_runtime::client::orchestrator::endpoints",
                "dependency-aws-runtime-debug"
            );
            tracing::debug!(target: "aws_sigv4::http_request", "dependency-aws-sigv4-debug");
            tracing::info!(target: "tokio_postgres::query", "dependency-postgres-info");
            tracing::debug!(target: "api::routes", "application-debug");
        });
        let bytes = captured
            .0
            .lock()
            .expect("captured logs mutex should not poison")
            .clone();
        String::from_utf8(bytes).expect("logs should be UTF-8")
    }

    const CAPS: &str = "tokio_postgres=info,aws_smithy_runtime=info,aws_sigv4=info";

    #[test]
    fn debug_default_gets_caps_before_configured_directives() {
        assert_eq!(
            build_log_filter(&logging("debug", &[])),
            format!("{CAPS},debug")
        );
        assert_eq!(
            build_log_filter(&logging("trace", &[("api", "debug")])),
            format!("{CAPS},trace,api=debug")
        );
    }

    #[test]
    fn info_or_stricter_default_is_left_unchanged() {
        for level in [
            "info",
            "warn",
            "error",
            "off",
            "info,api=debug",
            "api=debug",
            "",
        ] {
            assert_eq!(build_log_filter(&logging(level, &[])), level, "{level}");
        }
    }

    #[test]
    fn debug_level_keeps_app_debug_and_drops_dependency_debug() {
        let filter = build_log_filter(&logging("debug", &[("api", "debug")]));
        let output = emitted_under(&filter);
        assert!(output.contains("application-debug"), "{output}");
        assert!(output.contains("dependency-postgres-info"), "{output}");
        assert!(!output.contains("dependency-postgres-debug"), "{output}");
        assert!(!output.contains("dependency-aws-runtime-debug"), "{output}");
        assert!(!output.contains("dependency-aws-sigv4-debug"), "{output}");
    }

    #[test]
    fn stricter_default_still_silences_dependency_info() {
        let output = emitted_under(&build_log_filter(&logging("warn", &[])));
        assert!(!output.contains("dependency-postgres-info"), "{output}");
        let output = emitted_under(&build_log_filter(&logging("off", &[])));
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn explicit_directive_for_a_dependency_is_respected() {
        for level in [
            "debug,tokio_postgres=debug",
            "debug,tokio_postgres::query=debug",
            // A bare target enables every level for it.
            "debug,tokio_postgres",
        ] {
            let output = emitted_under(&build_log_filter(&logging(level, &[])));
            assert!(
                output.contains("dependency-postgres-debug"),
                "{level}: {output}"
            );
            assert!(
                !output.contains("dependency-aws-runtime-debug"),
                "{level}: {output}"
            );
        }
    }

    #[test]
    fn scoped_or_invalid_dependency_directive_keeps_the_cap() {
        for level in [
            // Covers another submodule, not the query logger.
            "debug,tokio_postgres::connection=debug",
            // EnvFilter drops a directive with an unknown level.
            "debug,tokio_postgres=verbose",
        ] {
            let output = emitted_under(&build_log_filter(&logging(level, &[])));
            assert!(
                !output.contains("dependency-postgres-debug"),
                "{level}: {output}"
            );
        }
    }

    #[test]
    fn default_level_is_the_last_bare_level() {
        assert_eq!(default_level("debug"), Some(LevelFilter::DEBUG));
        assert_eq!(default_level("TRACE"), Some(LevelFilter::TRACE));
        assert_eq!(default_level("info,api=debug"), Some(LevelFilter::INFO));
        assert_eq!(default_level("debug,warn"), Some(LevelFilter::WARN));
        assert_eq!(default_level("debug, "), Some(LevelFilter::DEBUG));
        assert_eq!(default_level("api=debug"), None);
        assert_eq!(default_level(""), None);
    }
}
