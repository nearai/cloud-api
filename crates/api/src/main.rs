use axum::{
    routing::{get, post},
    http::StatusCode,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use api::routes::{chat_completions, completions, models};
use domain::Domain;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Load configuration first to get logging settings
    let config = domain::providers::ApiConfig::load().unwrap_or_else(|e| {
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

    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        // `POST /users` goes to `create_user`
        .route("/users", post(create_user))
        // AI completion endpoints
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        // Models endpoint
        .route("/v1/models", get(models))
        // Share the domain service as application state
        .with_state(domain);

    // run our app with hyper, using configuration from domain
    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();
    tracing::info!(address = %bind_address, "Server started successfully");
    tracing::info!("Available endpoints:");
    tracing::info!("  - POST /v1/chat/completions (Chat Completions)");
    tracing::info!("  - POST /v1/completions (Text Completions)");
    tracing::info!("  - GET /v1/models (Available Models)");
    axum::serve(listener, app).await.unwrap();
}

fn init_tracing(logging_config: &domain::providers::LoggingConfig) {
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

// basic handler that responds with a static string
async fn root() -> &'static str {
    "Hello, World!"
}

async fn create_user(
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(payload): Json<CreateUser>,
) -> (StatusCode, Json<User>) {
    // insert your application logic here
    let user = User {
        id: 1337,
        username: payload.username,
    };

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    (StatusCode::CREATED, Json(user))
}

// the input to our `create_user` handler
#[derive(Deserialize)]
struct CreateUser {
    username: String,
}

// the output to our `create_user` handler
#[derive(Serialize)]
struct User {
    id: u64,
    username: String,
}