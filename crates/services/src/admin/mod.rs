pub mod analytics;
pub mod ports;

pub use analytics::{
    AnalyticsRepository, AnalyticsService, ApiKeyMetrics, BillingSourceBreakdown, BillingSummary,
    MetricsSummary, ModelConsumptionPoint, ModelConsumptionTimeseries,
    ModelConsumptionTimeseriesQuery, ModelMetrics, ModelRevenueEntry, ModelRevenueQuery,
    ModelRevenueReport, OrgRevenueEntry, OrgRevenueQuery, OrgRevenueReport, OrganizationMetrics,
    PerformancePoint, PerformanceTimeseries, PerformanceTimeseriesQuery, PlatformMetrics,
    PlatformTimeSeriesMetrics, PlatformTimeSeriesPoint, RevenueDensityModelRow,
    RevenueDensityQuery, RevenueDensityReport, RevenueSort, TimeSeriesMetrics, TimeSeriesPoint,
    TopModelMetrics, TopOrganizationMetrics, WorkspaceMetrics,
};
pub mod infra;
pub mod pricing_scheduler;
pub use infra::{InfraService, InfraSummary};
pub use ports::{PlatformServiceInfo, *};
pub use pricing_scheduler::ModelPricingScheduler;
use std::sync::Arc;

use crate::completions::CompletionServiceTrait;
use crate::email::{
    EmailDeliveryOutcome, EmailSender, ModelDeprecationEmail, PricingChangeEmail,
    PricingChangeEmailModel,
};
use crate::models::ModelsServiceTrait;

const MODEL_DEPRECATION_USAGE_WINDOW_DAYS: i64 = 30;
const MODEL_PRICING_CHANGE_USAGE_WINDOW_DAYS: i64 = 30;
/// Minimum lead time between confirming a pricing change and its effective
/// date, so recipients are notified before the new pricing lands.
const MIN_PRICING_CHANGE_LEAD_SECS: i64 = 3600;
/// Maximum number of models in one scheduled pricing change batch.
const MAX_PRICING_CHANGE_BATCH_SIZE: usize = 50;

pub struct AdminServiceImpl {
    repository: Arc<dyn AdminRepository>,
    /// Used solely to invalidate the public `/v1/model/list` cache after
    /// admin writes that mutate the `models` or `model_aliases` tables.
    models_service: Arc<dyn ModelsServiceTrait>,
    /// Used to invalidate the per-org concurrent-limit cache after a PATCH
    /// to `/v1/admin/organizations/{org_id}/concurrent-limit`, so admin
    /// changes take effect immediately instead of waiting for the 5-minute TTL.
    completion_service: Arc<dyn CompletionServiceTrait>,
    email_sender: Arc<dyn EmailSender>,
}

impl AdminServiceImpl {
    pub fn new(
        repository: Arc<dyn AdminRepository>,
        models_service: Arc<dyn ModelsServiceTrait>,
        completion_service: Arc<dyn CompletionServiceTrait>,
        email_sender: Arc<dyn EmailSender>,
    ) -> Self {
        Self {
            repository,
            models_service,
            completion_service,
            email_sender,
        }
    }

    async fn validate_and_load_model_deprecation(
        &self,
        model_name: &str,
        successor_model_name: &str,
        _deprecation_date: chrono::DateTime<chrono::Utc>,
    ) -> Result<
        (
            ModelDeprecationModel,
            ModelDeprecationModel,
            Vec<ModelDeprecationRecipient>,
        ),
        AdminError,
    > {
        let model = model_name.trim();
        let successor = successor_model_name.trim();
        if model.is_empty() || successor.is_empty() {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId are required".to_string(),
            ));
        }
        if model == successor {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId must differ".to_string(),
            ));
        }

        let model = self
            .repository
            .get_active_model_for_deprecation(model)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AdminError::ModelNotFound(format!("Model '{model_name}' not found or inactive"))
            })?;
        let successor = self
            .repository
            .get_active_model_for_deprecation(successor)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AdminError::ModelNotFound(format!(
                    "Successor model '{successor_model_name}' not found or inactive"
                ))
            })?;

        let since =
            chrono::Utc::now() - chrono::Duration::days(MODEL_DEPRECATION_USAGE_WINDOW_DAYS);
        let recipients = self
            .repository
            .list_model_deprecation_recipients(&model.model_name, since)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((model, successor, recipients))
    }

    fn deprecation_preview_from_recipients(
        recipients: &[ModelDeprecationRecipient],
    ) -> ModelDeprecationPreview {
        let recipient_count = recipients
            .iter()
            .map(|r| r.email.to_lowercase())
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;
        let organization_count = recipients
            .iter()
            .map(|r| r.organization_id)
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;

        ModelDeprecationPreview {
            recipient_count,
            organization_count,
            usage_window_days: MODEL_DEPRECATION_USAGE_WINDOW_DAYS,
        }
    }

    /// Validate a scheduled pricing change batch and load the affected
    /// models' current pricing snapshots plus the (user, org, model)
    /// recipient rows for the whole batch.
    /// Validate a NEW scheduled pricing change batch (shape, amounts, lead
    /// time) and load the affected models' current pricing snapshots.
    /// Retries of an already-persisted batch must NOT go through this — the
    /// lead-time check would reject a legitimate resume once the effective
    /// date is near (see `confirm_pricing_changes`).
    async fn validate_pricing_changes(
        &self,
        changes: &[PricingChangeInput],
    ) -> Result<Vec<(PricingChangeInput, ModelPricingSnapshot)>, AdminError> {
        if changes.is_empty() {
            return Err(AdminError::InvalidPricing(
                "At least one pricing change must be provided".to_string(),
            ));
        }
        if changes.len() > MAX_PRICING_CHANGE_BATCH_SIZE {
            return Err(AdminError::InvalidPricing(format!(
                "At most {MAX_PRICING_CHANGE_BATCH_SIZE} pricing changes are allowed per batch"
            )));
        }

        let mut seen = std::collections::HashSet::new();
        let min_effective_at =
            chrono::Utc::now() + chrono::Duration::seconds(MIN_PRICING_CHANGE_LEAD_SECS);
        let mut loaded = Vec::with_capacity(changes.len());
        for change in changes {
            let model_name = change.model_name.trim();
            if model_name.is_empty() {
                return Err(AdminError::InvalidPricing(
                    "modelId is required for every pricing change".to_string(),
                ));
            }
            if !seen.insert(model_name.to_string()) {
                return Err(AdminError::InvalidPricing(format!(
                    "model '{model_name}' appears more than once in the batch"
                )));
            }
            let new_amounts = [
                change.new_input_cost_per_token,
                change.new_output_cost_per_token,
                change.new_cache_read_cost_per_token,
                change.new_cost_per_image,
            ];
            if new_amounts.iter().all(Option::is_none) {
                return Err(AdminError::InvalidPricing(format!(
                    "model '{model_name}': at least one pricing field must be provided"
                )));
            }
            if new_amounts.iter().flatten().any(|amount| *amount < 0) {
                return Err(AdminError::InvalidPricing(format!(
                    "model '{model_name}': pricing amounts must be non-negative"
                )));
            }
            if change.effective_at < min_effective_at {
                return Err(AdminError::InvalidPricing(format!(
                    "model '{model_name}': effectiveAt must be at least {} minutes in the future",
                    MIN_PRICING_CHANGE_LEAD_SECS / 60
                )));
            }

            let snapshot = self
                .repository
                .get_model_pricing_snapshot(model_name)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?
                .ok_or_else(|| {
                    AdminError::ModelNotFound(format!("Model '{model_name}' not found or inactive"))
                })?;
            let mut change = change.clone();
            change.model_name = model_name.to_string();
            loaded.push((change, snapshot));
        }

        Ok(loaded)
    }

    /// Owner/admin recipients (one row per user/org/model) for a set of
    /// canonical model names, over the standard usage window.
    async fn load_pricing_change_recipients(
        &self,
        model_names: &[String],
    ) -> Result<Vec<PricingChangeRecipientRow>, AdminError> {
        let since =
            chrono::Utc::now() - chrono::Duration::days(MODEL_PRICING_CHANGE_USAGE_WINDOW_DAYS);
        self.repository
            .list_pricing_change_recipients(model_names, since)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    /// Reject a confirm whose request body does not describe the batch that
    /// is already persisted under its `batch_id`. Without this, a reused
    /// idempotency key would return the old schedule rows while the
    /// notification emails were built from the new, unscheduled request.
    fn verify_batch_matches(
        rows: &[ScheduledPricingChange],
        changes: &[PricingChangeInput],
    ) -> Result<(), AdminError> {
        let conflict = |detail: String| {
            AdminError::PricingChangeConflict(format!(
                "batchId was already used for a different set of changes ({detail}); use a new batchId"
            ))
        };
        if rows.len() != changes.len() {
            return Err(conflict(format!(
                "{} models persisted, {} requested",
                rows.len(),
                changes.len()
            )));
        }
        let by_name: std::collections::HashMap<&str, &ScheduledPricingChange> =
            rows.iter().map(|r| (r.model_name.as_str(), r)).collect();
        for change in changes {
            let model_name = change.model_name.trim();
            let row = by_name
                .get(model_name)
                .ok_or_else(|| conflict(format!("model '{model_name}' is not in the batch")))?;
            let matches = row.effective_at == change.effective_at
                && row.new_input_cost_per_token == change.new_input_cost_per_token
                && row.new_output_cost_per_token == change.new_output_cost_per_token
                && row.new_cache_read_cost_per_token == change.new_cache_read_cost_per_token
                && row.new_cost_per_image == change.new_cost_per_image;
            if !matches {
                return Err(conflict(format!(
                    "model '{model_name}' differs from the persisted change"
                )));
            }
        }
        Ok(())
    }

    fn pricing_change_preview_from_loaded(
        loaded: &[(PricingChangeInput, ModelPricingSnapshot)],
        recipients: &[PricingChangeRecipientRow],
    ) -> PricingChangePreview {
        let recipient_count = recipients
            .iter()
            .map(|r| r.email.to_lowercase())
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;
        let organization_count = recipients
            .iter()
            .map(|r| r.organization_id)
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;

        let models = loaded
            .iter()
            .map(|(change, snapshot)| {
                let model_rows = recipients
                    .iter()
                    .filter(|r| r.model_name == change.model_name)
                    .collect::<Vec<_>>();
                PricingChangeModelPreview {
                    model_name: change.model_name.clone(),
                    model_display_name: snapshot.model_display_name.clone(),
                    effective_at: change.effective_at,
                    recipient_count: model_rows
                        .iter()
                        .map(|r| r.email.to_lowercase())
                        .collect::<std::collections::HashSet<_>>()
                        .len() as i64,
                    organization_count: model_rows
                        .iter()
                        .map(|r| r.organization_id)
                        .collect::<std::collections::HashSet<_>>()
                        .len() as i64,
                    old_input_cost_per_token: snapshot.input_cost_per_token,
                    old_output_cost_per_token: snapshot.output_cost_per_token,
                    old_cache_read_cost_per_token: snapshot.cache_read_cost_per_token,
                    old_cost_per_image: snapshot.cost_per_image,
                    new_input_cost_per_token: change.new_input_cost_per_token,
                    new_output_cost_per_token: change.new_output_cost_per_token,
                    new_cache_read_cost_per_token: change.new_cache_read_cost_per_token,
                    new_cost_per_image: change.new_cost_per_image,
                }
            })
            .collect();

        PricingChangePreview {
            recipient_count,
            organization_count,
            usage_window_days: MODEL_PRICING_CHANGE_USAGE_WINDOW_DAYS,
            models,
        }
    }
}

#[async_trait::async_trait]
impl AdminService for AdminServiceImpl {
    async fn batch_upsert_models(
        &self,
        models: BatchUpdateModelAdminRequest,
    ) -> Result<BatchUpdateModelAdminResponse, AdminError> {
        if models.is_empty() {
            return Err(AdminError::InvalidPricing(
                "At least one model must be provided".to_string(),
            ));
        }

        // Validate all models first
        for (model_name, request) in &models {
            Self::validate_model_request(model_name, request, Arc::clone(&self.repository)).await?;
        }

        // Upsert all models. Each row is committed independently, so we
        // invalidate the public `/v1/model/list` cache after EACH successful
        // write rather than only at the end of the loop. If a later row fails
        // and we bail out, the rows already committed must not stay hidden
        // behind a 30 s-stale cached response.
        //
        // The cache has capacity 1 (single "all" key), so per-row invalidation
        // is essentially free.
        let mut results = std::collections::HashMap::new();
        for (model_name, request) in models {
            let pricing = self
                .repository
                .upsert_model_pricing(&model_name, request)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
            results.insert(model_name, pricing);
            self.models_service.invalidate_models_cache().await;
        }

        Ok(results)
    }

    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64), AdminError> {
        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        let (history, total) = self
            .repository
            .get_model_history(model_name, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if total == 0 {
            return Err(AdminError::ModelNotFound(model_name.to_string()));
        }

        Ok((history, total))
    }

    async fn delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<(), AdminError> {
        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        let deleted = self
            .repository
            .soft_delete_model(
                model_name,
                change_reason,
                changed_by_user_id,
                changed_by_user_email,
            )
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if !deleted {
            return Err(AdminError::ModelNotFound(format!(
                "Model '{model_name}' not found"
            )));
        }

        // Invalidate the public `/v1/model/list` cache since a model row was
        // soft-deleted (is_active = false).
        self.models_service.invalidate_models_cache().await;

        Ok(())
    }

    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<DeprecateModelOutcome, AdminError> {
        let deprecated = deprecated_model_name.trim();
        let successor = successor_model_name.trim();

        if deprecated.is_empty() || successor.is_empty() {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId are required".to_string(),
            ));
        }
        if deprecated == successor {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId must differ".to_string(),
            ));
        }

        let outcome = self
            .repository
            .deprecate_model(
                deprecated,
                successor,
                change_reason,
                changed_by_user_id,
                changed_by_user_email,
            )
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AdminError::ModelNotFound(format!(
                    "Either '{deprecated}' or '{successor}' was not found, or the successor is not active"
                ))
            })?;

        // Invalidate the public `/v1/model/list` cache since deprecation
        // mutates both `models` (is_active) and `model_aliases` rows.
        self.models_service.invalidate_models_cache().await;

        Ok(outcome)
    }

    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, AdminError> {
        // Validate limits
        Self::validate_organization_limits(&limits)?;

        let updated_limits = self
            .repository
            .update_organization_limits(organization_id, limits)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("Organization not found") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{organization_id}' not found"
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })?;

        Ok(updated_limits)
    }

    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationLimitsHistoryEntry>, i64), AdminError> {
        let total = self
            .repository
            .count_organization_limits_history(organization_id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let history = self
            .repository
            .get_organization_limits_history(organization_id, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if history.is_empty() {
            return Err(AdminError::OrganizationNotFound(
                organization_id.to_string(),
            ));
        }

        Ok((history, total))
    }

    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
    ) -> Result<(Vec<UserInfo>, i64), AdminError> {
        let (users, total) = self
            .repository
            .list_users(limit, offset, search, is_active)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((users, total))
    }

    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), AdminError> {
        let (users_with_orgs, total) = self
            .repository
            .list_users_with_organizations(limit, offset, search, is_active, search_by_name)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((users_with_orgs, total))
    }

    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), AdminError> {
        let (models, total) = self
            .repository
            .list_models(include_inactive, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((models, total))
    }

    async fn preview_model_deprecation(
        &self,
        model_name: &str,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
    ) -> Result<ModelDeprecationPreview, AdminError> {
        let (model, successor, recipients) = self
            .validate_and_load_model_deprecation(model_name, successor_model_name, deprecation_date)
            .await?;
        drop(model);
        drop(successor);

        Ok(Self::deprecation_preview_from_recipients(&recipients))
    }

    async fn confirm_model_deprecation(
        &self,
        model_name: &str,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<ModelDeprecationConfirmResult, AdminError> {
        let (model, successor, recipients) = self
            .validate_and_load_model_deprecation(model_name, successor_model_name, deprecation_date)
            .await?;

        let update = UpdateModelAdminRequest {
            input_cost_per_token: None,
            output_cost_per_token: None,
            cost_per_image: None,
            cache_read_cost_per_token: None,
            model_display_name: None,
            model_description: None,
            model_icon: None,
            context_length: None,
            verifiable: None,
            is_active: None,
            allow_free: None,
            aliases: None,
            owned_by: None,
            provider_type: None,
            provider_config: None,
            attestation_supported: None,
            input_modalities: None,
            output_modalities: None,
            inference_url: None,
            hugging_face_id: None,
            quantization: None,
            max_output_length: None,
            supported_sampling_parameters: None,
            supported_features: None,
            datacenters: None,
            is_ready: None,
            deprecation_date: Some(Some(deprecation_date)),
            openrouter_slug: None,
            change_reason: change_reason.or_else(|| {
                Some(format!(
                    "Planned deprecation; recommended successor: {}",
                    successor.model_name
                ))
            }),
            changed_by_user_id,
            changed_by_user_email: changed_by_user_email.clone(),
        };

        self.repository
            .upsert_model_pricing(&model.model_name, update)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;
        self.models_service.invalidate_models_cache().await;

        let already_sent = self
            .repository
            .list_sent_model_deprecation_delivery_keys(
                model.id,
                &successor.model_name,
                deprecation_date,
            )
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;
        let already_sent: std::collections::HashSet<(uuid::Uuid, uuid::Uuid)> =
            already_sent.into_iter().collect();
        let already_sent_emails = recipients
            .iter()
            .filter(|recipient| {
                already_sent.contains(&(recipient.user_id, recipient.organization_id))
            })
            .map(|recipient| recipient.email.to_lowercase())
            .collect::<std::collections::HashSet<_>>();

        let mut sent_count = 0_i64;
        let mut failed_count = 0_i64;
        let mut skipped_count = 0_i64;
        let mut counted_emails = std::collections::HashSet::<String>::new();
        let mut email_results = std::collections::HashMap::<
            String,
            (ModelDeprecationEmailStatus, Option<String>, Option<String>),
        >::new();

        for recipient in &recipients {
            let email_key = recipient.email.to_lowercase();
            let already_sent_for_row =
                already_sent.contains(&(recipient.user_id, recipient.organization_id));
            let result = if already_sent_for_row || already_sent_emails.contains(&email_key) {
                (
                    ModelDeprecationEmailStatus::Skipped,
                    None,
                    Some("Already sent for this deprecation".to_string()),
                )
            } else if let Some(existing) = email_results.get(&email_key) {
                match existing.0 {
                    ModelDeprecationEmailStatus::Failed => (
                        ModelDeprecationEmailStatus::Failed,
                        None,
                        existing.2.clone(),
                    ),
                    _ => (
                        ModelDeprecationEmailStatus::Skipped,
                        existing.1.clone(),
                        Some(
                            "Deduplicated: email already sent to this recipient in this run"
                                .to_string(),
                        ),
                    ),
                }
            } else {
                let email = ModelDeprecationEmail {
                    recipient_email: recipient.email.clone(),
                    model_id: model.model_name.clone(),
                    model_display_name: model.model_display_name.clone(),
                    deprecation_date,
                    successor_model_id: successor.model_name.clone(),
                };
                let outcome = match self.email_sender.send_model_deprecation(&email).await {
                    Ok(EmailDeliveryOutcome::Sent { message_id }) => {
                        (ModelDeprecationEmailStatus::Sent, message_id, None)
                    }
                    Ok(EmailDeliveryOutcome::Skipped) => {
                        (ModelDeprecationEmailStatus::Skipped, None, None)
                    }
                    Err(e) => (
                        ModelDeprecationEmailStatus::Failed,
                        None,
                        Some(e.sanitized_message()),
                    ),
                };
                email_results.insert(email_key.clone(), outcome.clone());
                outcome
            };

            if counted_emails.insert(email_key) {
                match result.0 {
                    ModelDeprecationEmailStatus::Sent => sent_count += 1,
                    ModelDeprecationEmailStatus::Failed => failed_count += 1,
                    ModelDeprecationEmailStatus::Skipped => skipped_count += 1,
                }
            }

            if already_sent_for_row {
                continue;
            }

            self.repository
                .record_model_deprecation_delivery(ModelDeprecationDeliveryRecord {
                    model_id: model.id,
                    model_name: model.model_name.clone(),
                    model_display_name: model.model_display_name.clone(),
                    successor_model_name: successor.model_name.clone(),
                    deprecation_date,
                    recipient_user_id: recipient.user_id,
                    recipient_email: recipient.email.clone(),
                    organization_id: recipient.organization_id,
                    organization_name: recipient.organization_name.clone(),
                    status: result.0,
                    email_message_id: result.1,
                    email_last_error: result.2,
                    initiated_by_user_id: changed_by_user_id,
                    initiated_by_user_email: changed_by_user_email.clone(),
                })
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
        }

        let preview = Self::deprecation_preview_from_recipients(&recipients);
        Ok(ModelDeprecationConfirmResult {
            model_id: model.model_name,
            successor_model_id: successor.model_name,
            deprecation_date,
            recipient_count: preview.recipient_count,
            organization_count: preview.organization_count,
            sent_count,
            failed_count,
            skipped_count,
        })
    }

    async fn preview_pricing_changes(
        &self,
        changes: Vec<PricingChangeInput>,
    ) -> Result<PricingChangePreview, AdminError> {
        let loaded = self.validate_pricing_changes(&changes).await?;
        let model_names: Vec<String> = loaded
            .iter()
            .map(|(change, _)| change.model_name.clone())
            .collect();
        let recipients = self.load_pricing_change_recipients(&model_names).await?;
        Ok(Self::pricing_change_preview_from_loaded(
            &loaded,
            &recipients,
        ))
    }

    async fn confirm_pricing_changes(
        &self,
        batch_id: uuid::Uuid,
        changes: Vec<PricingChangeInput>,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<PricingChangeConfirmResult, AdminError> {
        // A retry of an already-persisted batch must skip schedule
        // validation: the lead-time check would reject a legitimate resume
        // (e.g. retrying after a timeout or partial email failure) once the
        // effective date is less than the minimum lead away.
        let existing = self
            .repository
            .list_scheduled_pricing_changes_by_batch(batch_id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let inserted = if existing.is_empty() {
            let loaded = self.validate_pricing_changes(&changes).await?;

            // Persist the schedule first: notifying users about a change
            // that failed to persist would be worse than the reverse (the
            // confirm is idempotent per batch_id, so a retry resumes the
            // email sending).
            let inserts = loaded
                .iter()
                .map(|(change, snapshot)| ScheduledPricingChangeInsert {
                    model_id: snapshot.id,
                    model_name: change.model_name.clone(),
                    model_display_name: snapshot.model_display_name.clone(),
                    new_input_cost_per_token: change.new_input_cost_per_token,
                    new_output_cost_per_token: change.new_output_cost_per_token,
                    new_cache_read_cost_per_token: change.new_cache_read_cost_per_token,
                    new_cost_per_image: change.new_cost_per_image,
                    old_input_cost_per_token: snapshot.input_cost_per_token,
                    old_output_cost_per_token: snapshot.output_cost_per_token,
                    old_cache_read_cost_per_token: snapshot.cache_read_cost_per_token,
                    old_cost_per_image: snapshot.cost_per_image,
                    effective_at: change.effective_at,
                })
                .collect();
            self.repository
                .insert_scheduled_pricing_changes(
                    batch_id,
                    inserts,
                    changed_by_user_id,
                    changed_by_user_email.clone(),
                    change_reason,
                )
                .await
                .map_err(
                    |e| match e.downcast_ref::<PricingChangeOpenConflictError>() {
                        Some(conflict) => AdminError::PricingChangeConflict(format!(
                            "A pending pricing change already exists for model '{}'; cancel it first",
                            conflict.model_name
                        )),
                        None => AdminError::InternalError(e.to_string()),
                    },
                )?
        } else {
            existing
        };

        // Whether freshly inserted or resumed, the request body must
        // describe the batch that is actually persisted — emails below are
        // built from the persisted rows, never from the request.
        Self::verify_batch_matches(&inserted, &changes)?;

        let model_names: Vec<String> = inserted.iter().map(|c| c.model_name.clone()).collect();
        let recipients = self.load_pricing_change_recipients(&model_names).await?;

        // Per-model email payload from the persisted rows, keyed by
        // canonical model name.
        let email_models: std::collections::HashMap<String, PricingChangeEmailModel> = inserted
            .iter()
            .map(|row| {
                (
                    row.model_name.clone(),
                    PricingChangeEmailModel {
                        model_id: row.model_name.clone(),
                        model_display_name: row.model_display_name.clone(),
                        effective_at: row.effective_at,
                        old_input_cost_per_token: row.old_input_cost_per_token,
                        new_input_cost_per_token: row.new_input_cost_per_token,
                        old_output_cost_per_token: row.old_output_cost_per_token,
                        new_output_cost_per_token: row.new_output_cost_per_token,
                        old_cache_read_cost_per_token: row.old_cache_read_cost_per_token,
                        new_cache_read_cost_per_token: row.new_cache_read_cost_per_token,
                        old_cost_per_image: row.old_cost_per_image,
                        new_cost_per_image: row.new_cost_per_image,
                    },
                )
            })
            .collect();

        // Consolidate: one email per distinct recipient address, listing
        // every batch model at least one of their org memberships used.
        struct RecipientAggregate<'a> {
            rows: Vec<&'a PricingChangeRecipientRow>,
            model_names: std::collections::BTreeSet<&'a str>,
        }
        let mut per_email: std::collections::BTreeMap<String, RecipientAggregate> =
            std::collections::BTreeMap::new();
        for row in &recipients {
            let aggregate = per_email
                .entry(row.email.to_lowercase())
                .or_insert_with(|| RecipientAggregate {
                    rows: Vec::new(),
                    model_names: std::collections::BTreeSet::new(),
                });
            aggregate.rows.push(row);
            aggregate.model_names.insert(row.model_name.as_str());
        }

        let already_sent: std::collections::HashSet<(uuid::Uuid, uuid::Uuid)> = self
            .repository
            .list_sent_pricing_change_delivery_keys(batch_id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .into_iter()
            .collect();

        let mut sent_count = 0_i64;
        let mut failed_count = 0_i64;
        let mut skipped_count = 0_i64;
        let mut organization_ids = std::collections::HashSet::new();

        for aggregate in per_email.values() {
            let model_names: Vec<String> = aggregate
                .model_names
                .iter()
                .map(|name| name.to_string())
                .collect();
            let any_row_sent = aggregate
                .rows
                .iter()
                .any(|row| already_sent.contains(&(row.user_id, row.organization_id)));
            let result = if any_row_sent {
                (
                    ModelDeprecationEmailStatus::Skipped,
                    None,
                    Some("Already sent for this batch".to_string()),
                )
            } else {
                let email = PricingChangeEmail {
                    recipient_email: aggregate.rows[0].email.clone(),
                    models: aggregate
                        .model_names
                        .iter()
                        .filter_map(|name| email_models.get(*name).cloned())
                        .collect(),
                };
                match self.email_sender.send_pricing_change(&email).await {
                    Ok(EmailDeliveryOutcome::Sent { message_id }) => {
                        (ModelDeprecationEmailStatus::Sent, message_id, None)
                    }
                    Ok(EmailDeliveryOutcome::Skipped) => {
                        (ModelDeprecationEmailStatus::Skipped, None, None)
                    }
                    Err(e) => (
                        ModelDeprecationEmailStatus::Failed,
                        None,
                        Some(e.sanitized_message()),
                    ),
                }
            };

            match result.0 {
                ModelDeprecationEmailStatus::Sent => sent_count += 1,
                ModelDeprecationEmailStatus::Failed => failed_count += 1,
                ModelDeprecationEmailStatus::Skipped => skipped_count += 1,
            }

            for row in &aggregate.rows {
                organization_ids.insert(row.organization_id);
                if already_sent.contains(&(row.user_id, row.organization_id)) {
                    continue;
                }
                self.repository
                    .record_pricing_change_delivery(PricingChangeDeliveryRecord {
                        batch_id,
                        recipient_user_id: row.user_id,
                        recipient_email: row.email.clone(),
                        organization_id: row.organization_id,
                        organization_name: row.organization_name.clone(),
                        model_names: model_names.clone(),
                        status: result.0,
                        email_message_id: result.1.clone(),
                        email_last_error: result.2.clone(),
                        initiated_by_user_id: changed_by_user_id,
                        initiated_by_user_email: changed_by_user_email.clone(),
                    })
                    .await
                    .map_err(|e| AdminError::InternalError(e.to_string()))?;
            }
        }

        Ok(PricingChangeConfirmResult {
            batch_id,
            recipient_count: per_email.len() as i64,
            organization_count: organization_ids.len() as i64,
            sent_count,
            failed_count,
            skipped_count,
            changes: inserted,
        })
    }

    async fn list_pricing_changes(
        &self,
        status: Option<ScheduledPricingChangeStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ScheduledPricingChange>, i64), AdminError> {
        if limit <= 0 || limit > 1000 {
            return Err(AdminError::InvalidPricing(
                "limit must be between 1 and 1000".to_string(),
            ));
        }
        if offset < 0 {
            return Err(AdminError::InvalidPricing(
                "offset must be non-negative".to_string(),
            ));
        }
        self.repository
            .list_scheduled_pricing_changes(status, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    async fn cancel_pricing_change(
        &self,
        id: uuid::Uuid,
        cancelled_by_user_id: Option<uuid::Uuid>,
        cancelled_by_user_email: Option<String>,
    ) -> Result<ScheduledPricingChange, AdminError> {
        self.repository
            .cancel_scheduled_pricing_change(id, cancelled_by_user_id, cancelled_by_user_email)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AdminError::PricingChangeNotFound(format!(
                    "Scheduled pricing change '{id}' not found or no longer pending"
                ))
            })
    }

    async fn update_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<(), AdminError> {
        // Validate limit if provided (u32 is already non-negative, just check for zero)
        if let Some(limit) = concurrent_limit {
            if limit == 0 {
                return Err(AdminError::InvalidLimits(
                    "Concurrent limit must be a positive integer".to_string(),
                ));
            }
        }

        self.repository
            .update_organization_concurrent_limit(organization_id, concurrent_limit)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("not found") || error_msg.contains("inactive") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{}' not found",
                        organization_id
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })?;

        // Drop the cached limit so the next request reads the freshly-written
        // value. Without this, admin PATCHes only take effect after the
        // 5-minute TTL expires.
        self.completion_service
            .invalidate_org_concurrent_limit(organization_id)
            .await;

        Ok(())
    }

    async fn get_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<u32>, AdminError> {
        self.repository
            .get_organization_concurrent_limit(organization_id)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("not found") || error_msg.contains("inactive") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{}' not found",
                        organization_id
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })
    }

    async fn list_organizations(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationInfo>, i64), AdminError> {
        // Execute both queries in parallel for better performance
        let (organizations_result, total_result) = tokio::join!(
            self.repository.list_all_organizations(limit, offset),
            self.repository.count_all_organizations()
        );

        let organizations =
            organizations_result.map_err(|e| AdminError::InternalError(e.to_string()))?;
        let total = total_result.map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((organizations, total))
    }

    async fn get_organization(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<AdminOrganizationInfo, AdminError> {
        self.repository
            .get_organization(organization_id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| AdminError::OrganizationNotFound(organization_id.to_string()))
    }

    async fn list_organization_members(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationMemberInfo>, i64), AdminError> {
        let (members_result, total_result) = tokio::join!(
            self.repository
                .list_organization_members(organization_id, limit, offset),
            self.repository.count_organization_members(organization_id)
        );

        let members = members_result.map_err(|e| AdminError::InternalError(e.to_string()))?;
        let total = total_result.map_err(|e| AdminError::InternalError(e.to_string()))?;

        // Distinguish "organization does not exist" (404) from "exists but has
        // no members" (200 with an empty list). Only pay for the existence
        // round-trip when the count is zero — in the common case (org exists,
        // has members) `total > 0` proves existence and we skip it.
        if total == 0 {
            let exists = self
                .repository
                .organization_exists(organization_id)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
            if !exists {
                return Err(AdminError::OrganizationNotFound(
                    organization_id.to_string(),
                ));
            }
        }

        Ok((members, total))
    }

    async fn list_services(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PlatformServiceInfo>, i64), AdminError> {
        self.repository
            .list_services(include_inactive, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    async fn get_service_by_id(&self, id: uuid::Uuid) -> Result<PlatformServiceInfo, AdminError> {
        self.repository
            .get_service_by_id(id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| AdminError::ServiceNotFound(format!("Service {id} not found")))
    }

    async fn create_service(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: crate::service_usage::ports::ServiceUnit,
        cost_per_unit: i64,
    ) -> Result<PlatformServiceInfo, AdminError> {
        let name = service_name.trim();
        if name.is_empty() {
            return Err(AdminError::InvalidPricing(
                "Service name cannot be empty".to_string(),
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(AdminError::InvalidPricing(
                "Service name must contain only lowercase letters, digits, and underscores (e.g. web_search)".to_string(),
            ));
        }
        if cost_per_unit < 0 {
            return Err(AdminError::InvalidPricing(
                "Cost per unit cannot be negative".to_string(),
            ));
        }
        self.repository
            .create_service(name, display_name, description, unit, cost_per_unit)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    async fn update_service(
        &self,
        id: uuid::Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<PlatformServiceInfo, AdminError> {
        if let Some(c) = cost_per_unit {
            if c < 0 {
                return Err(AdminError::InvalidPricing(
                    "Cost per unit cannot be negative".to_string(),
                ));
            }
        }
        self.repository
            .update_service(id, display_name, description, cost_per_unit, is_active)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| AdminError::ServiceNotFound(format!("Service {id} not found")))
    }
}

impl AdminServiceImpl {
    async fn validate_model_request(
        model_name: &str,
        request: &UpdateModelAdminRequest,
        repository: Arc<dyn AdminRepository>,
    ) -> Result<(), AdminError> {
        // All costs use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        // Validate display name if provided
        if let Some(ref display_name) = request.model_display_name {
            if display_name.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model display name cannot be empty".to_string(),
                ));
            }
        }

        // Validate description if provided
        if let Some(ref description) = request.model_description {
            if description.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model description cannot be empty".to_string(),
                ));
            }
        }

        // Activation pricing gate: reject requests that would result in an
        // active model with all-zero pricing unless allow_free is set.
        //
        // The gate fires when:
        //   - is_active is explicitly Some(true), OR
        //   - is_active is None AND the model does not exist yet (new model
        //     inserts default is_active to true, so omitting it on create is
        //     equivalent to passing is_active=true).
        //
        // The free-check covers all four cost fields: input, output, image,
        // and cache_read.  A model with non-zero cost on any field is treated
        // as priced and is not blocked.
        let existing = repository
            .get_model_costs(model_name)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let is_new_model = existing.is_none();

        // Evaluate whether this request will result in an active model.
        let (
            existing_input,
            existing_output,
            existing_image,
            existing_cache_read,
            existing_allow_free,
        ) = existing.unwrap_or((0, 0, 0, 0, false));

        let effective_is_active = request.is_active.unwrap_or(is_new_model);

        if effective_is_active {
            let effective_input = request.input_cost_per_token.unwrap_or(existing_input);
            let effective_output = request.output_cost_per_token.unwrap_or(existing_output);
            let effective_image = request.cost_per_image.unwrap_or(existing_image);
            let effective_cache_read = request
                .cache_read_cost_per_token
                .unwrap_or(existing_cache_read);
            let effective_allow_free = request.allow_free.unwrap_or(existing_allow_free);

            if effective_input == 0
                && effective_output == 0
                && effective_image == 0
                && effective_cache_read == 0
                && !effective_allow_free
            {
                return Err(AdminError::InvalidPricing(format!(
                    "Cannot activate model '{}' with zero pricing. \
                     Set allowFree=true to explicitly allow free serving, \
                     or set a non-zero cost on at least one of: \
                     inputCostPerToken, outputCostPerToken, costPerImage, \
                     cacheReadCostPerToken.",
                    model_name
                )));
            }
        }

        Ok(())
    }

    fn validate_organization_limits(limits: &OrganizationLimitsUpdate) -> Result<(), AdminError> {
        // All amounts use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

        // Validate amount is non-negative
        if limits.spend_limit < 0 {
            return Err(AdminError::InvalidLimits(
                "Spend limit cannot be negative".to_string(),
            ));
        }

        Ok(())
    }
}
