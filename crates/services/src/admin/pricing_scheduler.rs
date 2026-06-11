use std::sync::Arc;

use tracing::{error, info, warn};

use super::ports::{AdminRepository, ScheduledPricingChange, UpdateModelAdminRequest};
use crate::models::ModelsServiceTrait;

/// A claimed change is retried until it has consumed this many attempts,
/// then parked in `failed` (visible in the admin UI) instead of retrying
/// forever.
const MAX_APPLY_ATTEMPTS: i32 = 5;
/// Rows stuck in `applying` longer than this (e.g. the claiming instance
/// crashed mid-apply) are recovered back to `pending`.
const STALE_APPLYING_AFTER_SECS: i64 = 600;
/// Max rows claimed per tick.
const CLAIM_BATCH_LIMIT: i64 = 25;

/// Background task that applies scheduled model pricing changes when their
/// effective date is reached.
///
/// Multi-instance safe: the claim query atomically moves due rows from
/// `pending` to `applying` with `FOR UPDATE SKIP LOCKED`, so instances
/// behind the load balancer partition the due set instead of double-applying.
pub struct ModelPricingScheduler {
    repository: Arc<dyn AdminRepository>,
    /// Used to invalidate the public `/v1/model/list` cache after applying.
    models_service: Arc<dyn ModelsServiceTrait>,
    task_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ModelPricingScheduler {
    pub fn new(
        repository: Arc<dyn AdminRepository>,
        models_service: Arc<dyn ModelsServiceTrait>,
    ) -> Self {
        Self {
            repository,
            models_service,
            task_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the periodic apply task. Unlike the provider refresh task, the
    /// first tick runs immediately so changes that came due during a deploy
    /// are applied promptly. If `interval_secs` is 0, this is a no-op
    /// (used by test servers, which drive `run_once` directly).
    pub async fn start(self: Arc<Self>, interval_secs: u64) {
        if interval_secs == 0 {
            info!("Pricing change scheduler disabled (interval is 0)");
            return;
        }

        let handle = tokio::spawn({
            let scheduler = self.clone();
            async move {
                let mut interval =
                    tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
                loop {
                    interval.tick().await;
                    if let Err(e) = scheduler.run_once().await {
                        error!(error = %e, "Pricing change scheduler tick failed");
                    }
                }
            }
        });

        let mut task_handle = self.task_handle.lock().await;
        *task_handle = Some(handle);
        info!(
            "Pricing change scheduler started with interval: {} seconds",
            interval_secs
        );
    }

    /// Cancel the background task.
    pub async fn shutdown(&self) {
        let mut task_handle = self.task_handle.lock().await;
        if let Some(handle) = task_handle.take() {
            handle.abort();
            info!("Pricing change scheduler task cancelled");
        }
    }

    /// One scheduler pass: recover stale claims, claim due changes, apply
    /// them. Public so tests (and operators) can drive it deterministically.
    pub async fn run_once(&self) -> anyhow::Result<()> {
        let recovered = self
            .repository
            .recover_stale_applying_pricing_changes(
                chrono::Duration::seconds(STALE_APPLYING_AFTER_SECS),
                MAX_APPLY_ATTEMPTS,
            )
            .await?;
        if recovered > 0 {
            warn!(
                count = recovered,
                "Recovered stale 'applying' pricing changes"
            );
        }

        let claimed = self
            .repository
            .claim_due_pricing_changes(CLAIM_BATCH_LIMIT)
            .await?;
        for change in claimed {
            self.apply_change(change).await;
        }
        Ok(())
    }

    async fn apply_change(&self, change: ScheduledPricingChange) {
        // Direct edits via PATCH /v1/admin/models are not blocked by a
        // pending schedule, so the live pricing can have drifted from the
        // snapshot captured (and announced by email) at confirm time. The
        // announced change is the source of truth — apply it regardless —
        // but surface the drift for operators.
        if let Ok(Some(current)) = self
            .repository
            .get_model_pricing_snapshot(&change.model_name)
            .await
        {
            let drifted = current.input_cost_per_token != change.old_input_cost_per_token
                || current.output_cost_per_token != change.old_output_cost_per_token
                || current.cache_read_cost_per_token != change.old_cache_read_cost_per_token
                || current.cost_per_image != change.old_cost_per_image;
            if drifted {
                warn!(
                    change_id = %change.id,
                    batch_id = %change.batch_id,
                    model_id = %change.model_id,
                    "Model pricing changed since the scheduled change was confirmed; applying the announced change anyway"
                );
            }

            // Value-idempotent re-apply: if a previous attempt already wrote
            // the new pricing but crashed before marking the row applied,
            // skip the upsert so the retry doesn't record a duplicate
            // model_history entry for identical values.
            let already_at_target = change
                .new_input_cost_per_token
                .is_none_or(|v| v == current.input_cost_per_token)
                && change
                    .new_output_cost_per_token
                    .is_none_or(|v| v == current.output_cost_per_token)
                && change
                    .new_cache_read_cost_per_token
                    .is_none_or(|v| v == current.cache_read_cost_per_token)
                && change
                    .new_cost_per_image
                    .is_none_or(|v| v == current.cost_per_image);
            if already_at_target {
                if let Err(e) = self.repository.mark_pricing_change_applied(change.id).await {
                    error!(
                        change_id = %change.id,
                        error = %e,
                        "Pricing already at target values but failed to mark change applied"
                    );
                    return;
                }
                self.models_service.invalidate_models_cache().await;
                info!(
                    change_id = %change.id,
                    batch_id = %change.batch_id,
                    model_id = %change.model_id,
                    "Model pricing already at the scheduled values; marked applied without re-updating"
                );
                return;
            }
        }

        let change_reason = match &change.change_reason {
            Some(reason) => format!(
                "Scheduled pricing change (batch {}): {reason}",
                change.batch_id
            ),
            None => format!("Scheduled pricing change (batch {})", change.batch_id),
        };
        let update = UpdateModelAdminRequest {
            input_cost_per_token: change.new_input_cost_per_token,
            output_cost_per_token: change.new_output_cost_per_token,
            cost_per_image: change.new_cost_per_image,
            cache_read_cost_per_token: change.new_cache_read_cost_per_token,
            model_display_name: None,
            model_description: None,
            model_icon: None,
            context_length: None,
            verifiable: None,
            is_active: None,
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
            deprecation_date: None,
            openrouter_slug: None,
            change_reason: Some(change_reason),
            changed_by_user_id: change.created_by_user_id,
            changed_by_user_email: change.created_by_user_email.clone(),
        };

        match self
            .repository
            .upsert_model_pricing(&change.model_name, update)
            .await
        {
            Ok(_) => {
                if let Err(e) = self.repository.mark_pricing_change_applied(change.id).await {
                    // The pricing is live but the row is still 'applying';
                    // the stale-claim recovery will retry the mark (the
                    // upsert is idempotent for the same values).
                    error!(
                        change_id = %change.id,
                        error = %e,
                        "Applied pricing change but failed to mark it applied"
                    );
                    return;
                }
                self.models_service.invalidate_models_cache().await;
                info!(
                    change_id = %change.id,
                    batch_id = %change.batch_id,
                    model_id = %change.model_id,
                    "Applied scheduled pricing change"
                );
            }
            Err(e) => {
                let retryable = change.apply_attempts < MAX_APPLY_ATTEMPTS;
                error!(
                    change_id = %change.id,
                    batch_id = %change.batch_id,
                    model_id = %change.model_id,
                    attempts = change.apply_attempts,
                    retryable,
                    error = %e,
                    "Failed to apply scheduled pricing change"
                );
                if let Err(mark_err) = self
                    .repository
                    .mark_pricing_change_failed(change.id, &e.to_string(), retryable)
                    .await
                {
                    error!(
                        change_id = %change.id,
                        error = %mark_err,
                        "Failed to record pricing change failure"
                    );
                }
            }
        }
    }
}
