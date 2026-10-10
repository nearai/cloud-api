//! Unit tests for the email batch loops in `AdminServiceImpl`: the recipient
//! activity lookup must only run immediately before a real send.

use super::*;
use crate::admin::{AdminService, AdminServiceImpl};
use crate::completions::ports::MockCompletionServiceTrait;
use crate::email::{
    EmailDeliveryOutcome, EmailError, EmailSender, ModelDeprecationEmail, PricingChangeEmail,
};
use crate::models::ports::MockModelsServiceTrait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

fn test_model_pricing() -> ModelPricing {
    ModelPricing {
        model_display_name: "m".to_string(),
        model_description: String::new(),
        model_icon: None,
        input_cost_per_token: 1,
        output_cost_per_token: 1,
        cost_per_image: 0,
        cache_read_cost_per_token: None,
        text_pricing: None,
        context_length: 1,
        verifiable: false,
        is_active: true,
        aliases: vec![],
        owned_by: String::new(),
        provider_type: "vllm".to_string(),
        provider_config: None,
        attestation_supported: false,
        input_modalities: None,
        output_modalities: None,
        inference_url: None,
        hugging_face_id: None,
        quantization: None,
        max_output_length: None,
        supported_sampling_parameters: vec![],
        supported_features: vec![],
        datacenters: None,
        is_ready: None,
        deprecation_date: None,
        successor_model_name: None,
        openrouter_slug: None,
    }
}

#[derive(Default)]
struct CountingRepo {
    active_calls: AtomicUsize,
    user_active: bool,
    sent_keys: Vec<(Uuid, Uuid)>,
    deprecation_recipients: Vec<ModelDeprecationRecipient>,
    pricing_recipients: Vec<PricingChangeRecipientRow>,
    scheduled_rows: Vec<ScheduledPricingChange>,
}

#[allow(unused_variables)]
#[async_trait]
impl AdminRepository for CountingRepo {
    async fn upsert_model_pricing(
        &self,
        model_name: &str,
        request: UpdateModelAdminRequest,
    ) -> Result<ModelPricing, anyhow::Error> {
        Ok(test_model_pricing())
    }

    async fn get_model_validation_state(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelValidationState>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn soft_delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<bool, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<Option<DeprecateModelOutcome>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_current_organization_limits(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Vec<OrganizationLimits>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn count_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<i64, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
    ) -> Result<(Vec<UserInfo>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search: Option<String>,
        is_active: Option<bool>,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_active_model_for_deprecation(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelDeprecationModel>, anyhow::Error> {
        Ok(Some(ModelDeprecationModel {
            id: Uuid::new_v4(),
            model_name: model_name.to_string(),
            model_display_name: model_name.to_string(),
        }))
    }

    async fn list_model_deprecation_recipients(
        &self,
        model_name: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ModelDeprecationRecipient>, anyhow::Error> {
        Ok(self.deprecation_recipients.clone())
    }

    async fn list_sent_model_deprecation_delivery_keys(
        &self,
        model_id: uuid::Uuid,
        successor_model_name: &str,
        deprecation_date: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid)>, anyhow::Error> {
        Ok(self.sent_keys.clone())
    }

    async fn is_user_active(&self, user_id: uuid::Uuid) -> Result<bool, anyhow::Error> {
        self.active_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.user_active)
    }

    async fn record_model_deprecation_delivery(
        &self,
        record: ModelDeprecationDeliveryRecord,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn get_model_pricing_snapshot(
        &self,
        model_name: &str,
    ) -> Result<Option<ModelPricingSnapshot>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_pricing_change_recipients(
        &self,
        model_names: &[String],
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<PricingChangeRecipientRow>, anyhow::Error> {
        Ok(self.pricing_recipients.clone())
    }

    async fn insert_scheduled_pricing_changes(
        &self,
        batch_id: uuid::Uuid,
        changes: Vec<ScheduledPricingChangeInsert>,
        created_by_user_id: Option<uuid::Uuid>,
        created_by_user_email: Option<String>,
        change_reason: Option<String>,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_scheduled_pricing_changes_by_batch(
        &self,
        batch_id: uuid::Uuid,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error> {
        Ok(self.scheduled_rows.clone())
    }

    async fn list_scheduled_pricing_changes(
        &self,
        status: Option<ScheduledPricingChangeStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ScheduledPricingChange>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn cancel_scheduled_pricing_change(
        &self,
        id: uuid::Uuid,
        cancelled_by_user_id: Option<uuid::Uuid>,
        cancelled_by_user_email: Option<String>,
    ) -> Result<Option<ScheduledPricingChange>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn claim_due_pricing_changes(
        &self,
        limit: i64,
    ) -> Result<Vec<ScheduledPricingChange>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn mark_pricing_change_applied(&self, id: uuid::Uuid) -> Result<(), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn mark_pricing_change_failed(
        &self,
        id: uuid::Uuid,
        error: &str,
        retryable: bool,
    ) -> Result<(), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn recover_stale_applying_pricing_changes(
        &self,
        stale_after: chrono::Duration,
        max_attempts: i32,
    ) -> Result<u64, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_sent_pricing_change_delivery_keys(
        &self,
        batch_id: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid)>, anyhow::Error> {
        Ok(self.sent_keys.clone())
    }

    async fn record_pricing_change_delivery(
        &self,
        record: PricingChangeDeliveryRecord,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn update_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<(), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<u32>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_all_organizations(
        &self,
        limit: i64,
        offset: i64,
        lifecycle: OrganizationLifecycleFilter,
    ) -> Result<Vec<AdminOrganizationInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn count_all_organizations(
        &self,
        lifecycle: OrganizationLifecycleFilter,
    ) -> Result<i64, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_all_api_keys(
        &self,
        filters: &AdminApiKeyFilters,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminApiKeyInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn count_all_api_keys(&self, filters: &AdminApiKeyFilters) -> Result<i64, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_organization(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<AdminOrganizationInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_organization_members(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminOrganizationMemberInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn count_organization_members(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<i64, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn organization_exists(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<bool, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn list_services(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PlatformServiceInfo>, i64), anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn get_service_by_id(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<PlatformServiceInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn create_service(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: ServiceUnit,
        cost_per_unit: i64,
    ) -> Result<PlatformServiceInfo, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }

    async fn update_service(
        &self,
        id: uuid::Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<Option<PlatformServiceInfo>, anyhow::Error> {
        unimplemented!("not used by the email loop tests")
    }
}

struct CountingEmailSender {
    sends: AtomicUsize,
}

#[async_trait]
impl EmailSender for CountingEmailSender {
    async fn send_invitation(
        &self,
        _email: &crate::email::InvitationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        unimplemented!()
    }

    async fn send_model_deprecation(
        &self,
        _email: &ModelDeprecationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(EmailDeliveryOutcome::Sent { message_id: None })
    }

    async fn send_pricing_change(
        &self,
        _email: &PricingChangeEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(EmailDeliveryOutcome::Sent { message_id: None })
    }
}

fn service(repo: Arc<CountingRepo>) -> (AdminServiceImpl, Arc<CountingEmailSender>) {
    let mut models = MockModelsServiceTrait::new();
    models.expect_invalidate_models_cache().returning(|| ());
    let sender = Arc::new(CountingEmailSender {
        sends: AtomicUsize::new(0),
    });
    let service = AdminServiceImpl::new(
        repo,
        Arc::new(models),
        Arc::new(MockCompletionServiceTrait::new()),
        sender.clone(),
    );
    (service, sender)
}

fn deprecation_recipient(user_id: Uuid, org_id: Uuid) -> ModelDeprecationRecipient {
    ModelDeprecationRecipient {
        user_id,
        email: format!("{user_id}@example.test"),
        organization_id: org_id,
        organization_name: "org".to_string(),
    }
}

async fn confirm_deprecation(service: &AdminServiceImpl) {
    service
        .confirm_model_deprecation(
            "old-model",
            "new-model",
            chrono::Utc::now() + chrono::Duration::days(30),
            None,
            None,
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn deprecation_loop_skips_activity_lookup_when_every_row_already_sent() {
    let user = Uuid::new_v4();
    let (org_a, org_b) = (Uuid::new_v4(), Uuid::new_v4());
    let repo = Arc::new(CountingRepo {
        user_active: true,
        sent_keys: vec![(user, org_a), (user, org_b)],
        deprecation_recipients: vec![
            deprecation_recipient(user, org_a),
            deprecation_recipient(user, org_b),
        ],
        ..Default::default()
    });
    let (service, sender) = service(repo.clone());

    confirm_deprecation(&service).await;

    assert_eq!(repo.active_calls.load(Ordering::SeqCst), 0);
    assert_eq!(sender.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn deprecation_loop_looks_up_activity_once_per_recipient_with_two_org_rows() {
    let user = Uuid::new_v4();
    let repo = Arc::new(CountingRepo {
        user_active: true,
        deprecation_recipients: vec![
            deprecation_recipient(user, Uuid::new_v4()),
            deprecation_recipient(user, Uuid::new_v4()),
        ],
        ..Default::default()
    });
    let (service, sender) = service(repo.clone());

    confirm_deprecation(&service).await;

    assert_eq!(repo.active_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sender.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn deprecation_loop_looks_up_inactive_recipient_once_and_never_sends() {
    let user = Uuid::new_v4();
    let repo = Arc::new(CountingRepo {
        user_active: false,
        deprecation_recipients: vec![
            deprecation_recipient(user, Uuid::new_v4()),
            deprecation_recipient(user, Uuid::new_v4()),
        ],
        ..Default::default()
    });
    let (service, sender) = service(repo.clone());

    confirm_deprecation(&service).await;

    assert_eq!(repo.active_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sender.sends.load(Ordering::SeqCst), 0);
}

fn pricing_fixture(
    batch_id: Uuid,
    user: Uuid,
    org: Uuid,
    effective_at: chrono::DateTime<chrono::Utc>,
) -> (ScheduledPricingChange, PricingChangeRecipientRow) {
    let row = ScheduledPricingChange {
        id: Uuid::new_v4(),
        batch_id,
        model_id: Uuid::new_v4(),
        model_name: "priced-model".to_string(),
        model_display_name: "Priced".to_string(),
        new_input_cost_per_token: Some(2),
        new_output_cost_per_token: None,
        new_cache_read_cost_per_token: None,
        new_cost_per_image: None,
        old_input_cost_per_token: 1,
        old_output_cost_per_token: 1,
        old_cache_read_cost_per_token: None,
        old_cost_per_image: 0,
        old_text_pricing: None,
        new_text_pricing: None,
        effective_at,
        status: ScheduledPricingChangeStatus::Pending,
        apply_attempts: 0,
        applied_at: None,
        last_error: None,
        created_by_user_id: None,
        created_by_user_email: None,
        change_reason: None,
        created_at: chrono::Utc::now(),
    };
    let recipient = PricingChangeRecipientRow {
        user_id: user,
        email: format!("{user}@example.test"),
        organization_id: org,
        organization_name: "org".to_string(),
        model_name: "priced-model".to_string(),
    };
    (row, recipient)
}

async fn confirm_pricing(
    service: &AdminServiceImpl,
    batch_id: Uuid,
    effective_at: chrono::DateTime<chrono::Utc>,
) {
    service
        .confirm_pricing_changes(
            batch_id,
            vec![PricingChangeInput {
                model_name: "priced-model".to_string(),
                effective_at,
                new_input_cost_per_token: Some(2),
                new_output_cost_per_token: None,
                new_cache_read_cost_per_token: None,
                new_cost_per_image: None,
                new_text_pricing: None,
            }],
            None,
            None,
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pricing_loop_skips_activity_lookup_when_every_row_already_sent() {
    let (batch_id, user, org) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let effective_at = chrono::Utc::now() + chrono::Duration::days(7);
    let (row, recipient) = pricing_fixture(batch_id, user, org, effective_at);
    let repo = Arc::new(CountingRepo {
        user_active: true,
        sent_keys: vec![(user, org)],
        scheduled_rows: vec![row],
        pricing_recipients: vec![recipient],
        ..Default::default()
    });
    let (service, sender) = service(repo.clone());

    confirm_pricing(&service, batch_id, effective_at).await;

    assert_eq!(repo.active_calls.load(Ordering::SeqCst), 0);
    assert_eq!(sender.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pricing_loop_looks_up_activity_once_before_a_real_send() {
    let (batch_id, user, org) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let effective_at = chrono::Utc::now() + chrono::Duration::days(7);
    let (row, recipient) = pricing_fixture(batch_id, user, org, effective_at);
    let repo = Arc::new(CountingRepo {
        user_active: true,
        scheduled_rows: vec![row],
        pricing_recipients: vec![recipient],
        ..Default::default()
    });
    let (service, sender) = service(repo.clone());

    confirm_pricing(&service, batch_id, effective_at).await;

    assert_eq!(repo.active_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sender.sends.load(Ordering::SeqCst), 1);
}
