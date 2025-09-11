use axum::{
    middleware::from_fn_with_state,
    routing::{get, post},
    Router,
};
use api::{
    middleware::{auth_middleware, AuthState},
    routes::{
        api::build_api_router,
        chat_completions, completions, models, quote,
        auth::{github_login, google_login, oauth_callback, current_user, logout, auth_success, login_page, StateStore},
    },
};
use domain::{Domain, auth::OAuthManager};
use config::{ApiConfig, LoggingConfig};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

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

    // Create OAuth manager with configuration
    let oauth_manager = if config.auth.enabled {
        tracing::info!("Authentication enabled, setting up OAuth providers");
        
        let github_config = config.auth.github.clone()
            .map(|c| config::OAuthProviderConfig::from(c));
        let google_config = config.auth.google.clone()
            .map(|c| config::OAuthProviderConfig::from(c));
        
        let mut manager = OAuthManager::new(
            github_config,
            google_config,
        ).unwrap_or_else(|e| {
            tracing::error!("Failed to create OAuth manager: {}", e);
            std::process::exit(1);
        });
        
        // Set database if available
        if let Some(ref db) = domain.database {
            manager = manager.with_database(db.clone());
            tracing::info!("OAuth manager configured with database support");
        }
        
        if config.auth.github.is_some() {
            tracing::info!("GitHub OAuth configured");
        }
        if config.auth.google.is_some() {
            tracing::info!("Google OAuth configured");
        }
        
        Arc::new(manager)
    } else {
        tracing::info!("Authentication disabled");
        // Create dummy manager (won't be used when auth is disabled)
        Arc::new(OAuthManager::new(None, None).unwrap())
    };

    let _sessions = oauth_manager.sessions.clone();
    let state_store: StateStore = Arc::new(RwLock::new(HashMap::new()));
    
    // Create AuthState for middleware
    let auth_state_middleware = AuthState::new(
        oauth_manager.clone(),
        domain.database.clone(),
    );

    // Build authentication routes with combined state
    let auth_state = (oauth_manager.clone(), state_store.clone());
    let auth_routes = Router::new()
        .route("/login", get(login_page))
        .route("/github", get(github_login))
        .route("/google", get(google_login))
        .route("/callback", get(oauth_callback))
        .route("/user", get(current_user).layer(from_fn_with_state(auth_state_middleware.clone(), auth_middleware)))
        .route("/logout", post(logout))
        .route("/success", get(auth_success))
        .with_state(auth_state);

    // Build API routes for completions
    let completion_routes = if config.auth.enabled {
        Router::new()
            .route("/chat/completions", post(chat_completions))
            .route("/completions", post(completions))
            .route("/models", get(models))
            .route("/quote", get(quote))
            .layer(from_fn_with_state(auth_state_middleware.clone(), auth_middleware))
    } else {
        // No auth middleware when disabled
        Router::new()
            .route("/chat/completions", post(chat_completions))
            .route("/completions", post(completions))
            .route("/models", get(models))
            .route("/quote", get(quote))
    };
    
    // Build management API routes (orgs, teams, users)
    let management_routes = if let Some(ref db) = domain.database {
        Some(build_api_router(
            db.clone(),
            auth_state_middleware.clone(),
            config.auth.enabled,
        ))
    } else {
        None
    };
    
    // Build the final application
    let mut app = Router::new()
        .nest("/auth", auth_routes)
        .nest("/v1", completion_routes.with_state(domain.clone()));
    
    // Add management routes if database is available
    if let Some(mgmt_routes) = management_routes {
        app = app.nest("/api/v1", mgmt_routes);
    }

    // Start periodic session cleanup
    if config.auth.enabled {
        let oauth_cleanup = oauth_manager.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                oauth_cleanup.cleanup_sessions().await;
                tracing::debug!("Cleaned up expired sessions");
            }
        });
    }

    // run our app with hyper, using configuration from domain
    let listener = tokio::net::TcpListener::bind(&bind_address).await.unwrap();

    tracing::info!(address = %bind_address, "Server started successfully");
    tracing::info!("Authentication: {}", if config.auth.enabled { "ENABLED" } else { "DISABLED" });
    
    if config.auth.enabled {
        tracing::info!("OAuth Endpoints:");
        tracing::info!("  - GET /auth/login (Login page)");
        if config.auth.github.is_some() {
            tracing::info!("  - GET /auth/github (Redirect to GitHub OAuth)");
        }
        if config.auth.google.is_some() {
            tracing::info!("  - GET /auth/google (Redirect to Google OAuth)");
        }
        tracing::info!("  - GET /auth/callback (OAuth callback)");
        tracing::info!("  - GET /auth/user (Current user info)");
        tracing::info!("  - POST /auth/logout (Logout)");
    }
    
    tracing::info!("API Endpoints:");
    tracing::info!("  - POST /v1/chat/completions (Chat Completions)");
    tracing::info!("  - POST /v1/completions (Text Completions)");
    tracing::info!("  - GET /v1/models (Available Models)");
    tracing::info!("  - GET /v1/quote (TDX Quote & Attestation)");
    
    if domain.database.is_some() {
        tracing::info!("");
        tracing::info!("Management API Endpoints:");
        tracing::info!("  Organizations:");
        tracing::info!("    - GET/POST /api/v1/organizations");
        tracing::info!("    - GET/PUT/DELETE /api/v1/organizations/:id");
        tracing::info!("    - GET/POST /api/v1/organizations/:id/members");
        tracing::info!("    - GET/POST /api/v1/organizations/:id/teams");
        tracing::info!("    - GET/POST /api/v1/organizations/:id/api-keys");
        tracing::info!("  Teams:");
        tracing::info!("    - GET/PUT/DELETE /api/v1/teams/:id");
        tracing::info!("    - GET/POST /api/v1/teams/:id/members");
        tracing::info!("  Users:");
        tracing::info!("    - GET /api/v1/users/me (Current user)");
        tracing::info!("    - GET /api/v1/users/me/personal-org");
        tracing::info!("    - GET /api/v1/users/me/organizations");
        tracing::info!("    - GET /api/v1/users/:id/teams");
    }
    
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