use super::*;

pub(super) fn build_admin_routes_with_options(
    database: Arc<Database>,
    auth_state_middleware: &AuthState,
    config: Arc<ApiConfig>,
    build_options: AppBuildOptions,
    services: AdminRouteServices,
) -> Router {
    use crate::middleware::admin_middleware;
    use crate::routes::admin::{
        batch_upsert_models, cancel_model_pricing_change, confirm_model_deprecation,
        confirm_model_pricing_changes, create_admin_access_token, create_service,
        delete_admin_access_token, delete_aml_allowlist_entry, delete_model, deprecate_model,
        get_admin_organization_balance, get_billing_summary, get_infra_summary,
        get_model_consumption_timeseries, get_model_history, get_model_revenue, get_org_revenue,
        get_organization as get_admin_organization, get_organization_concurrent_limit,
        get_organization_fallback, get_organization_limits_history, get_organization_metrics,
        get_organization_priority, get_organization_timeseries, get_performance_timeseries,
        get_platform_metrics, get_platform_timeseries, get_revenue_density,
        list_admin_access_tokens, list_aml_allowlist, list_aml_reports,
        list_invitation_email_deliveries, list_model_pricing_changes,
        list_models as admin_list_models, list_organization_members, list_organizations,
        list_users, preview_model_deprecation, preview_model_pricing_changes,
        resend_invitation_email, update_aml_report_status, update_organization_concurrent_limit,
        update_organization_fallback, update_organization_limits, update_organization_member_role,
        update_organization_priority, update_service, upsert_aml_allowlist_entry, AdminAppState,
    };
    use crate::routes::staking_farm::{
        get_admin_organization_staking_farm, sync_admin_organization_staking_farm,
    };
    use database::repositories::{AdminAccessTokenRepository, AdminCompositeRepository};
    use services::admin::AdminServiceImpl;

    // Create composite admin repository (handles models, organization limits, and users)
    let admin_repository = Arc::new(AdminCompositeRepository::with_accounting_config(
        database.pool().clone(),
        &config.credit_allocation,
    ));

    // Create admin access token repository
    let admin_access_token_repository =
        Arc::new(AdminAccessTokenRepository::new(database.pool().clone()));
    // Create admin service with composite repository.
    //
    // The admin service holds a reference to the `models_service` so it can
    // invalidate the public `/v1/model/list` cache after admin writes
    // (`upsert`, `delete`, `deprecate`) that mutate the `models` or
    // `model_aliases` tables. It also holds the `completion_service` so it
    // can invalidate the per-org concurrent-limit cache after a PATCH to
    // `/v1/admin/organizations/{org_id}/concurrent-limit`.
    let admin_service = Arc::new(AdminServiceImpl::new(
        admin_repository as Arc<dyn services::admin::AdminRepository>,
        services.models_service as Arc<dyn services::models::ModelsServiceTrait>,
        services.completion_service.clone()
            as Arc<dyn services::completions::CompletionServiceTrait>,
        services::email::sender_from_config(&config.invitation_email)
            .expect("Failed to initialize admin email sender"),
        services.admission.clone(),
    )) as Arc<dyn services::admin::AdminService + Send + Sync>;

    let github_dispatcher =
        services::github_dispatch::dispatcher_from_config(&config.github_dispatch);

    let infra_service = Arc::new(services::admin::InfraService::new(
        config.infra.machines_url.clone(),
        config.infra.cost_per_host_usd_month,
        config.infra.prometheus_url.clone(),
        config.infra.prometheus_bearer_token.clone(),
        config.infra.prometheus_environment.clone(),
        config.infra.cost_per_gpu_hour_usd,
    ));

    let admin_app_state = AdminAppState {
        admin_service,
        analytics_service: services.analytics_service,
        organization_service: services.organization_service,
        auth_service: auth_state_middleware.auth_service.clone(),
        usage_service: services.usage_service,
        staking_farm_service: services.staking_farm_service,
        aml_service: services.aml_service,
        config: config.clone(),
        admin_access_token_repository,
        inference_provider_pool: services.inference_provider_pool,
        github_dispatcher,
        infra_service,
    };

    let database_encryption_state = crate::database_encryption::DatabaseEncryptionState::new(
        database.pool().clone(),
        &config.database_encryption_key,
        &config.database_encryption_key_id,
    )
    .map_err(|_| {
        tracing::error!(
            error_class = "invalid_database_encryption_key",
            "Database encryption admin routes are disabled"
        );
    })
    .ok();

    // Classify read operations in middleware::admin_policy when adding admin routes.
    let admin_routes = Router::new()
        .route(
            "/admin/models",
            axum::routing::get(admin_list_models).patch(batch_upsert_models),
        )
        .route(
            "/admin/models/deprecate",
            axum::routing::post(deprecate_model),
        )
        .route(
            "/admin/models/pricing-changes",
            axum::routing::get(list_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/preview",
            axum::routing::post(preview_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/confirm",
            axum::routing::post(confirm_model_pricing_changes),
        )
        .route(
            "/admin/models/pricing-changes/{id}",
            axum::routing::delete(cancel_model_pricing_change),
        )
        .route(
            "/admin/models/{model_name}",
            axum::routing::delete(delete_model),
        )
        .route(
            "/admin/models/{model_name}/history",
            axum::routing::get(get_model_history),
        )
        .route(
            "/admin/models/{model_name}/deprecation/preview",
            axum::routing::post(preview_model_deprecation),
        )
        .route(
            "/admin/models/{model_name}/deprecation/confirm",
            axum::routing::post(confirm_model_deprecation),
        )
        .route("/admin/services", axum::routing::post(create_service))
        .route("/admin/services/{id}", axum::routing::patch(update_service))
        .route(
            "/admin/organizations/{org_id}/limits",
            axum::routing::patch(update_organization_limits),
        )
        .route(
            "/admin/organizations/{org_id}/limits/history",
            axum::routing::get(get_organization_limits_history),
        )
        .route(
            "/admin/organizations/{org_id}/usage/balance",
            axum::routing::get(get_admin_organization_balance),
        )
        .route(
            "/admin/organizations/{org_id}/staking/farm",
            axum::routing::get(get_admin_organization_staking_farm),
        )
        .route(
            "/admin/organizations/{org_id}/staking/farm/sync",
            axum::routing::post(sync_admin_organization_staking_farm),
        )
        .route("/admin/aml/reports", axum::routing::get(list_aml_reports))
        .route(
            "/admin/aml/reports/{report_id}/status",
            axum::routing::patch(update_aml_report_status),
        )
        .route(
            "/admin/aml/allowlist",
            axum::routing::get(list_aml_allowlist).post(upsert_aml_allowlist_entry),
        )
        .route(
            "/admin/aml/allowlist/{account_id}",
            axum::routing::delete(delete_aml_allowlist_entry),
        )
        .route(
            "/admin/organizations/{org_id}/concurrent-limit",
            axum::routing::patch(update_organization_concurrent_limit)
                .get(get_organization_concurrent_limit),
        )
        .route(
            "/admin/organizations/{org_id}/priority",
            axum::routing::get(get_organization_priority).patch(update_organization_priority),
        )
        .route(
            "/admin/organizations/{org_id}/fallback",
            axum::routing::get(get_organization_fallback).patch(update_organization_fallback),
        )
        .route(
            "/admin/organizations/{org_id}/metrics",
            axum::routing::get(get_organization_metrics),
        )
        .route(
            "/admin/organizations/{org_id}/metrics/timeseries",
            axum::routing::get(get_organization_timeseries),
        )
        .route(
            "/admin/platform/metrics",
            axum::routing::get(get_platform_metrics),
        )
        .route(
            "/admin/platform/metrics/timeseries",
            axum::routing::get(get_platform_timeseries),
        )
        .route(
            "/admin/platform/billing-summary",
            axum::routing::get(get_billing_summary),
        )
        .route(
            "/admin/platform/model-revenue",
            axum::routing::get(get_model_revenue),
        )
        .route(
            "/admin/platform/org-revenue",
            axum::routing::get(get_org_revenue),
        )
        .route(
            "/admin/platform/infra-summary",
            axum::routing::get(get_infra_summary),
        )
        .route(
            "/admin/platform/model-consumption-timeseries",
            axum::routing::get(get_model_consumption_timeseries),
        )
        .route(
            "/admin/platform/performance-timeseries",
            axum::routing::get(get_performance_timeseries),
        )
        .route(
            "/admin/platform/revenue-density",
            axum::routing::get(get_revenue_density),
        )
        .route(
            "/admin/invitation-email-deliveries",
            axum::routing::get(list_invitation_email_deliveries),
        )
        .route(
            "/admin/invitation-email-deliveries/{invitation_id}/resend",
            axum::routing::post(resend_invitation_email),
        )
        .route("/admin/users", axum::routing::get(list_users))
        .route(
            "/admin/organizations",
            axum::routing::get(list_organizations),
        )
        .route(
            "/admin/organizations/{org_id}",
            axum::routing::get(get_admin_organization),
        )
        .route(
            "/admin/organizations/{org_id}/members",
            axum::routing::get(list_organization_members),
        )
        .route(
            "/admin/organizations/{org_id}/members/{user_id}",
            axum::routing::put(update_organization_member_role),
        )
        .route(
            "/admin/access-tokens",
            axum::routing::post(create_admin_access_token),
        )
        .route(
            "/admin/access-tokens",
            axum::routing::get(list_admin_access_tokens),
        )
        .route(
            "/admin/access-tokens/{token_id}",
            axum::routing::delete(delete_admin_access_token),
        )
        .with_state(admin_app_state);

    let admin_routes = if let Some(database_encryption_state) = database_encryption_state {
        if build_options.start_database_encryption_recovery {
            database_encryption_state.recover_jobs();
        }
        admin_routes.merge(
            Router::new()
                .route(
                    "/admin/database-encryption/scan",
                    axum::routing::post(crate::database_encryption::scan),
                )
                .route(
                    "/admin/database-encryption/jobs",
                    axum::routing::post(crate::database_encryption::create_job),
                )
                .route(
                    "/admin/database-encryption/jobs/{id}",
                    axum::routing::get(crate::database_encryption::get_job),
                )
                .route(
                    "/admin/database-encryption/jobs/{id}/cancel",
                    axum::routing::post(crate::database_encryption::cancel_job),
                )
                .route(
                    "/admin/database-encryption/verify",
                    axum::routing::post(crate::database_encryption::verify),
                )
                .layer(axum::Extension(database_encryption_state)),
        )
    } else {
        admin_routes
    };

    admin_routes
        // Admin middleware handles both authentication and authorization
        .layer(from_fn_with_state(
            auth_state_middleware.clone(),
            admin_middleware,
        ))
}
