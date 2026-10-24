//! Sends durable low-credit notifications from the same balance ports used by usage admission.

use std::sync::Arc;

use tracing::{error, info, warn};

use super::ports::{
    CreditEmailClaimResult, CreditEmailNotificationRepository, CreditEmailOrganization,
    OrganizationLimitsRepository,
};
use crate::email::{deliverable_email, CreditBalanceAlertEmail, EmailDeliveryOutcome, EmailSender};

const SCAN_INTERVAL_SECS: u64 = 300;
const MAX_SNAPSHOT_RETRIES: usize = 5;

pub struct CreditEmailScheduler {
    repository: Arc<dyn CreditEmailNotificationRepository>,
    limits_repository: Arc<dyn OrganizationLimitsRepository>,
    email_sender: Arc<dyn EmailSender>,
    thresholds: Vec<i16>,
    frontend_base_url: Option<String>,
    task_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CreditEmailScheduler {
    pub fn new(
        repository: Arc<dyn CreditEmailNotificationRepository>,
        limits_repository: Arc<dyn OrganizationLimitsRepository>,
        email_sender: Arc<dyn EmailSender>,
        mut thresholds: Vec<i16>,
        frontend_base_url: Option<String>,
    ) -> Self {
        thresholds.sort_unstable();
        thresholds.dedup();
        Self {
            repository,
            limits_repository,
            email_sender,
            thresholds,
            frontend_base_url,
            task_handle: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn start(self: Arc<Self>) {
        let handle = tokio::spawn({
            let scheduler = self.clone();
            async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(SCAN_INTERVAL_SECS));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if let Err(error) = scheduler.run_once().await {
                        error!(%error, "Failed to scan credit alert thresholds");
                    }
                }
            }
        });
        *self.task_handle.lock().await = Some(handle);
        info!(
            interval_secs = SCAN_INTERVAL_SECS,
            "credit email scheduler started"
        );
    }

    pub async fn shutdown(&self) {
        if let Some(handle) = self.task_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
            info!("credit email scheduler task cancelled");
        }
    }

    pub async fn run_once(&self) -> anyhow::Result<()> {
        let mut failed_organizations = 0_u64;
        for organization in self.repository.active_organizations().await? {
            let organization_id = organization.id;
            if let Err(error) = self.process_organization(organization).await {
                failed_organizations += 1;
                warn!(%organization_id, %error, "Failed to process credit email alerts for organization");
            }
        }
        if failed_organizations > 0 {
            anyhow::bail!("credit email scan failed for {failed_organizations} organization(s)");
        }
        Ok(())
    }

    async fn process_organization(
        &self,
        organization: CreditEmailOrganization,
    ) -> anyhow::Result<()> {
        for attempt in 1..=MAX_SNAPSHOT_RETRIES {
            if self.process_organization_snapshot(&organization).await? {
                return Ok(());
            }
            info!(
                organization_id = %organization.id,
                attempt,
                "Credit balance changed during alert scan; retrying immediately"
            );
        }
        anyhow::bail!(
            "credit balance kept changing while claiming alerts after {MAX_SNAPSHOT_RETRIES} attempts"
        )
    }

    async fn process_organization_snapshot(
        &self,
        organization: &CreditEmailOrganization,
    ) -> anyhow::Result<bool> {
        let snapshot = self
            .limits_repository
            .get_current_credit_snapshot(organization.id)
            .await?;
        let Some(limit) = snapshot.limit else {
            return Ok(self
                .reconcile_claims(organization, &snapshot.version, None, &[])
                .await?
                .is_some());
        };
        let breakdown = snapshot.breakdown;
        let effective_capacity = breakdown
            .iter()
            .map(|credit| credit.effective_capacity)
            .fold(0_i64, i64::saturating_add);
        // Usage admission blocks while unresolved overage exists. Treat that state as exhausted
        // so the notification and request gate never disagree about whether credits are usable.
        let remaining = if limit.unfunded > 0 {
            0
        } else {
            limit.available
        };
        // A present limit with no effective capacity is exhausted, not replenished. Organizations
        // that have never had a limit are handled by the `snapshot.limit` branch above.
        let threshold = if effective_capacity <= 0 {
            self.thresholds
                .iter()
                .copied()
                .find(|threshold| *threshold == 0)
        } else {
            threshold_for_balance(remaining, effective_capacity, &self.thresholds)
        };
        let Some(threshold) = threshold else {
            return Ok(self
                .reconcile_claims(organization, &snapshot.version, None, &[])
                .await?
                .is_some());
        };

        let lower_thresholds = self
            .thresholds
            .iter()
            .copied()
            .filter(|candidate| *candidate < threshold)
            .collect::<Vec<_>>();
        let mut credit_types = breakdown
            .into_iter()
            .map(|credit| credit.credit_type)
            .collect::<Vec<_>>();
        credit_types.sort();
        credit_types.dedup();
        let credit_type = if credit_types.len() == 1 {
            credit_types.pop().unwrap_or_else(|| "combined".to_string())
        } else {
            format!("combined ({})", credit_types.join(", "))
        };
        let Some(recipients) = self
            .reconcile_claims(
                organization,
                &snapshot.version,
                Some(threshold),
                &lower_thresholds,
            )
            .await?
        else {
            return Ok(false);
        };
        for recipient in recipients {
            let Some(recipient_email) = deliverable_email(
                &recipient.email,
                recipient.contact_email.as_deref(),
                recipient.contact_email_verified_at,
            ) else {
                let _ = self
                    .repository
                    .release_claim(
                        organization.id,
                        threshold,
                        recipient.user_id,
                        recipient.claim_token,
                    )
                    .await;
                continue;
            };
            let email = CreditBalanceAlertEmail {
                recipient_email: recipient_email.to_string(),
                organization_name: organization.name.clone(),
                credit_type: credit_type.clone(),
                remaining,
                limit: effective_capacity,
                threshold_percent: threshold,
                credits_url: format!(
                    "{}/dashboard/organizations/{}/credits",
                    self.frontend_base_url
                        .as_deref()
                        .unwrap_or("")
                        .trim_end_matches('/'),
                    organization.id
                ),
            };
            if !self
                .repository
                .begin_dispatch(
                    organization.id,
                    threshold,
                    recipient.user_id,
                    recipient.claim_token,
                )
                .await?
            {
                continue;
            }
            match self.email_sender.send_credit_balance_alert(&email).await {
                Ok(EmailDeliveryOutcome::Sent { .. }) => {
                    if let Err(error) = self
                        .repository
                        .mark_sent(
                            organization.id,
                            threshold,
                            recipient.user_id,
                            recipient.claim_token,
                        )
                        .await
                    {
                        warn!(organization_id = %organization.id, error = %error, "Failed to mark credit balance alert as sent");
                    }
                }
                Ok(EmailDeliveryOutcome::Skipped) => {
                    let _ = self
                        .repository
                        .release_claim(
                            organization.id,
                            threshold,
                            recipient.user_id,
                            recipient.claim_token,
                        )
                        .await;
                }
                Err(error) => {
                    warn!(organization_id = %organization.id, error = %error.sanitized_message(), "Failed to send credit balance alert");
                    let _ = self
                        .repository
                        .release_claim(
                            organization.id,
                            threshold,
                            recipient.user_id,
                            recipient.claim_token,
                        )
                        .await;
                }
            }
        }
        Ok(true)
    }

    async fn reconcile_claims(
        &self,
        organization: &CreditEmailOrganization,
        snapshot_version: &str,
        threshold: Option<i16>,
        lower_thresholds: &[i16],
    ) -> anyhow::Result<Option<Vec<super::ports::CreditEmailRecipient>>> {
        match self
            .repository
            .reconcile_claims(
                organization.id,
                snapshot_version,
                threshold,
                lower_thresholds,
            )
            .await?
        {
            CreditEmailClaimResult::SnapshotChanged => Ok(None),
            CreditEmailClaimResult::Claimed(recipients) => Ok(Some(recipients)),
        }
    }
}

fn threshold_for_balance(remaining: i64, capacity: i64, thresholds: &[i16]) -> Option<i16> {
    if capacity <= 0 {
        return None;
    }
    let remaining_percent_numerator = i128::from(remaining.max(0)) * 100;
    thresholds.iter().copied().find(|threshold| {
        remaining_percent_numerator <= i128::from(capacity) * i128::from(*threshold)
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;
    use crate::email::{
        ContactEmailVerificationEmail, EmailError, InvitationEmail, ModelDeprecationEmail,
        NoopEmailSender, PricingChangeEmail,
    };
    use crate::usage::ports::{
        CreditEmailRecipient, OrganizationCreditLimit, OrganizationCreditSnapshot,
        OrganizationLimit,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ReconcileCall {
        organization_id: Uuid,
        snapshot_version: String,
        threshold: Option<i16>,
        lower_thresholds: Vec<i16>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ClaimMutation {
        threshold: i16,
        user_id: Uuid,
        claim_token: Uuid,
    }

    struct TestNotificationRepository {
        organizations: Vec<CreditEmailOrganization>,
        recipients: Vec<CreditEmailRecipient>,
        dispatch_allowed: bool,
        reconciliations: Mutex<Vec<ReconcileCall>>,
        begun_claims: Mutex<Vec<ClaimMutation>>,
        released_claims: Mutex<Vec<ClaimMutation>>,
        sent_claims: Mutex<Vec<ClaimMutation>>,
        snapshot_changes_remaining: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl CreditEmailNotificationRepository for TestNotificationRepository {
        async fn active_organizations(&self) -> anyhow::Result<Vec<CreditEmailOrganization>> {
            Ok(self.organizations.clone())
        }

        async fn reconcile_claims(
            &self,
            organization_id: Uuid,
            snapshot_version: &str,
            threshold: Option<i16>,
            lower_thresholds: &[i16],
        ) -> anyhow::Result<CreditEmailClaimResult> {
            self.reconciliations.lock().await.push(ReconcileCall {
                organization_id,
                snapshot_version: snapshot_version.to_string(),
                threshold,
                lower_thresholds: lower_thresholds.to_vec(),
            });
            let mut snapshot_changes_remaining = self.snapshot_changes_remaining.lock().await;
            if *snapshot_changes_remaining > 0 {
                *snapshot_changes_remaining -= 1;
                return Ok(CreditEmailClaimResult::SnapshotChanged);
            }
            Ok(CreditEmailClaimResult::Claimed(
                threshold.map_or_else(Vec::new, |_| self.recipients.clone()),
            ))
        }

        async fn begin_dispatch(
            &self,
            _: Uuid,
            threshold: i16,
            user_id: Uuid,
            claim_token: Uuid,
        ) -> anyhow::Result<bool> {
            self.begun_claims.lock().await.push(ClaimMutation {
                threshold,
                user_id,
                claim_token,
            });
            Ok(self.dispatch_allowed)
        }

        async fn release_claim(
            &self,
            _: Uuid,
            threshold: i16,
            user_id: Uuid,
            claim_token: Uuid,
        ) -> anyhow::Result<()> {
            self.released_claims.lock().await.push(ClaimMutation {
                threshold,
                user_id,
                claim_token,
            });
            Ok(())
        }

        async fn mark_sent(
            &self,
            _: Uuid,
            threshold: i16,
            user_id: Uuid,
            claim_token: Uuid,
        ) -> anyhow::Result<()> {
            self.sent_claims.lock().await.push(ClaimMutation {
                threshold,
                user_id,
                claim_token,
            });
            Ok(())
        }
    }

    struct RecordingEmailSender {
        result: Result<EmailDeliveryOutcome, EmailError>,
        emails: Mutex<Vec<CreditBalanceAlertEmail>>,
    }

    impl RecordingEmailSender {
        fn new(result: Result<EmailDeliveryOutcome, EmailError>) -> Self {
            Self {
                result,
                emails: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl EmailSender for RecordingEmailSender {
        async fn send_invitation(
            &self,
            _: &InvitationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            Ok(EmailDeliveryOutcome::Skipped)
        }

        async fn send_model_deprecation(
            &self,
            _: &ModelDeprecationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            Ok(EmailDeliveryOutcome::Skipped)
        }

        async fn send_pricing_change(
            &self,
            _: &PricingChangeEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            Ok(EmailDeliveryOutcome::Skipped)
        }

        async fn send_contact_email_verification(
            &self,
            _: &ContactEmailVerificationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            Ok(EmailDeliveryOutcome::Skipped)
        }

        async fn send_credit_balance_alert(
            &self,
            email: &CreditBalanceAlertEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            self.emails.lock().await.push(email.clone());
            self.result.clone()
        }
    }

    struct FixedLimitsRepository {
        limit: OrganizationLimit,
        breakdown: Vec<OrganizationCreditLimit>,
    }

    #[async_trait::async_trait]
    impl OrganizationLimitsRepository for FixedLimitsRepository {
        async fn get_current_limits(&self, _: Uuid) -> anyhow::Result<Option<OrganizationLimit>> {
            Ok(Some(self.limit.clone()))
        }

        async fn get_current_limit_breakdown(
            &self,
            _: Uuid,
        ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
            Ok(self.breakdown.clone())
        }

        async fn get_current_credit_snapshot(
            &self,
            _: Uuid,
        ) -> anyhow::Result<OrganizationCreditSnapshot> {
            Ok(OrganizationCreditSnapshot {
                limit: Some(self.limit.clone()),
                breakdown: self.breakdown.clone(),
                version: "snapshot-v1".to_string(),
            })
        }
    }

    struct PartiallyFailingLimitsRepository {
        failed_organization_id: Uuid,
        limit: OrganizationLimit,
        breakdown: Vec<OrganizationCreditLimit>,
    }

    #[async_trait::async_trait]
    impl OrganizationLimitsRepository for PartiallyFailingLimitsRepository {
        async fn get_current_limits(
            &self,
            organization_id: Uuid,
        ) -> anyhow::Result<Option<OrganizationLimit>> {
            if organization_id == self.failed_organization_id {
                anyhow::bail!("simulated organization read failure");
            }
            Ok(Some(self.limit.clone()))
        }

        async fn get_current_limit_breakdown(
            &self,
            _: Uuid,
        ) -> anyhow::Result<Vec<OrganizationCreditLimit>> {
            Ok(self.breakdown.clone())
        }

        async fn get_current_credit_snapshot(
            &self,
            organization_id: Uuid,
        ) -> anyhow::Result<OrganizationCreditSnapshot> {
            if organization_id == self.failed_organization_id {
                anyhow::bail!("simulated organization read failure");
            }
            Ok(OrganizationCreditSnapshot {
                limit: Some(self.limit.clone()),
                breakdown: self.breakdown.clone(),
                version: "snapshot-v1".to_string(),
            })
        }
    }

    fn repository(recipients: Vec<CreditEmailRecipient>) -> Arc<TestNotificationRepository> {
        repository_with_dispatch_fence(recipients, true)
    }

    fn repository_with_dispatch_fence(
        recipients: Vec<CreditEmailRecipient>,
        dispatch_allowed: bool,
    ) -> Arc<TestNotificationRepository> {
        Arc::new(TestNotificationRepository {
            organizations: vec![CreditEmailOrganization {
                id: Uuid::new_v4(),
                name: "Example".to_string(),
            }],
            recipients,
            dispatch_allowed,
            reconciliations: Mutex::new(Vec::new()),
            begun_claims: Mutex::new(Vec::new()),
            released_claims: Mutex::new(Vec::new()),
            sent_claims: Mutex::new(Vec::new()),
            snapshot_changes_remaining: Mutex::new(0),
        })
    }

    fn limits(available: i64) -> Arc<FixedLimitsRepository> {
        Arc::new(FixedLimitsRepository {
            limit: OrganizationLimit {
                spend_limit: 100,
                available,
                unfunded: 0,
            },
            breakdown: vec![OrganizationCreditLimit {
                credit_type: "payment".to_string(),
                source: None,
                amount: 100,
                transferred_out: 0,
                effective_capacity: 100,
                consumed: 100 - available,
                available,
                currency: "USD".to_string(),
            }],
        })
    }

    #[tokio::test]
    async fn replenished_balance_clears_previous_threshold_claims() {
        let repository = repository(Vec::new());
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(100),
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        let calls = repository.reconciliations.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].threshold, None);
        assert!(calls[0].lower_thresholds.is_empty());
    }

    #[tokio::test]
    async fn balance_change_retries_without_waiting_for_the_next_scan() {
        let repository = repository(Vec::new());
        *repository.snapshot_changes_remaining.lock().await = 1;
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(10),
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        let calls = repository.reconciliations.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].threshold, Some(10));
        assert_eq!(calls[1].threshold, Some(10));
    }

    #[tokio::test]
    async fn successful_send_marks_the_exact_claim_as_sent() {
        let user_id = Uuid::new_v4();
        let claim_token = Uuid::new_v4();
        let repository = repository(vec![CreditEmailRecipient {
            user_id,
            claim_token,
            email: "owner@example.com".to_string(),
            contact_email: None,
            contact_email_verified_at: None,
        }]);
        let sender = Arc::new(RecordingEmailSender::new(Ok(EmailDeliveryOutcome::Sent {
            message_id: Some("message-1".to_string()),
        })));
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(10),
            sender.clone(),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(sender.emails.lock().await.len(), 1);
        assert_eq!(
            *repository.begun_claims.lock().await,
            vec![ClaimMutation {
                threshold: 10,
                user_id,
                claim_token,
            }]
        );
        assert_eq!(
            *repository.sent_claims.lock().await,
            vec![ClaimMutation {
                threshold: 10,
                user_id,
                claim_token,
            }]
        );
        assert!(repository.released_claims.lock().await.is_empty());
    }

    #[tokio::test]
    async fn failed_send_releases_the_exact_claim() {
        let user_id = Uuid::new_v4();
        let claim_token = Uuid::new_v4();
        let repository = repository(vec![CreditEmailRecipient {
            user_id,
            claim_token,
            email: "owner@example.com".to_string(),
            contact_email: None,
            contact_email_verified_at: None,
        }]);
        let sender = Arc::new(RecordingEmailSender::new(Err(EmailError::new(
            "simulated send failure",
        ))));
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(10),
            sender.clone(),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(sender.emails.lock().await.len(), 1);
        assert_eq!(
            *repository.released_claims.lock().await,
            vec![ClaimMutation {
                threshold: 10,
                user_id,
                claim_token,
            }]
        );
        assert!(repository.sent_claims.lock().await.is_empty());
    }

    #[tokio::test]
    async fn replaced_claim_is_not_dispatched_or_mutated() {
        let user_id = Uuid::new_v4();
        let claim_token = Uuid::new_v4();
        let repository = repository_with_dispatch_fence(
            vec![CreditEmailRecipient {
                user_id,
                claim_token,
                email: "owner@example.com".to_string(),
                contact_email: None,
                contact_email_verified_at: None,
            }],
            false,
        );
        let sender = Arc::new(RecordingEmailSender::new(Ok(EmailDeliveryOutcome::Sent {
            message_id: Some("message-1".to_string()),
        })));
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(10),
            sender.clone(),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(repository.begun_claims.lock().await.len(), 1);
        assert!(sender.emails.lock().await.is_empty());
        assert!(repository.sent_claims.lock().await.is_empty());
        assert!(repository.released_claims.lock().await.is_empty());
    }

    #[test]
    fn threshold_crossing_does_not_round_down() {
        assert_eq!(threshold_for_balance(201, 1_000, &[0, 10, 20]), None);
        assert_eq!(threshold_for_balance(200, 1_000, &[0, 10, 20]), Some(20));
        assert_eq!(threshold_for_balance(101, 1_000, &[0, 10, 20]), Some(20));
        assert_eq!(threshold_for_balance(100, 1_000, &[0, 10, 20]), Some(10));
    }

    #[test]
    fn threshold_crossing_handles_supported_large_limits_without_overflow() {
        let billion_dollars = 1_000_000_000_i64 * 1_000_000_000;
        assert_eq!(
            threshold_for_balance(billion_dollars, billion_dollars, &[0, 10, 20]),
            None
        );
        assert_eq!(
            threshold_for_balance(billion_dollars / 10, billion_dollars, &[0, 10, 20]),
            Some(10)
        );
    }

    #[tokio::test]
    async fn one_organization_failure_does_not_block_later_organizations() {
        let failed_organization_id = Uuid::new_v4();
        let healthy_organization_id = Uuid::new_v4();
        let repository = Arc::new(TestNotificationRepository {
            organizations: vec![
                CreditEmailOrganization {
                    id: failed_organization_id,
                    name: "Failed".to_string(),
                },
                CreditEmailOrganization {
                    id: healthy_organization_id,
                    name: "Healthy".to_string(),
                },
            ],
            recipients: vec![CreditEmailRecipient {
                user_id: Uuid::new_v4(),
                claim_token: Uuid::new_v4(),
                email: "owner@example.com".to_string(),
                contact_email: None,
                contact_email_verified_at: None,
            }],
            dispatch_allowed: true,
            reconciliations: Mutex::new(Vec::new()),
            begun_claims: Mutex::new(Vec::new()),
            released_claims: Mutex::new(Vec::new()),
            sent_claims: Mutex::new(Vec::new()),
            snapshot_changes_remaining: Mutex::new(0),
        });
        let limits = Arc::new(PartiallyFailingLimitsRepository {
            failed_organization_id,
            limit: OrganizationLimit {
                spend_limit: 100,
                available: 10,
                unfunded: 0,
            },
            breakdown: vec![OrganizationCreditLimit {
                credit_type: "payment".to_string(),
                source: None,
                amount: 100,
                transferred_out: 0,
                effective_capacity: 100,
                consumed: 90,
                available: 10,
                currency: "USD".to_string(),
            }],
        });
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits,
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        let error = scheduler.run_once().await.unwrap_err();

        assert!(error.to_string().contains("1 organization"));
        assert_eq!(
            repository
                .reconciliations
                .lock()
                .await
                .iter()
                .map(|call| call.organization_id)
                .collect::<Vec<_>>(),
            vec![healthy_organization_id]
        );
    }

    #[tokio::test]
    async fn replenishment_above_lower_threshold_rearms_lower_alerts() {
        let repository = repository(Vec::new());
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(15),
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        let calls = repository.reconciliations.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].threshold, Some(20));
        assert_eq!(calls[0].lower_thresholds, vec![0, 10]);
    }

    #[tokio::test]
    async fn placeholder_email_filter_is_shared_and_case_insensitive() {
        let placeholder_user_id = Uuid::new_v4();
        let repository = repository(vec![CreditEmailRecipient {
            user_id: placeholder_user_id,
            claim_token: Uuid::new_v4(),
            email: "alice.near@NEAR".to_string(),
            contact_email: None,
            contact_email_verified_at: None,
        }]);
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits(10),
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(
            repository.released_claims.lock().await[0].user_id,
            placeholder_user_id
        );
        let calls = repository.reconciliations.lock().await;
        assert_eq!(calls[0].threshold, Some(10));
        assert_eq!(calls[0].lower_thresholds, vec![0]);
    }

    #[tokio::test]
    async fn unresolved_overage_uses_exhausted_threshold() {
        let repository = repository(vec![CreditEmailRecipient {
            user_id: Uuid::new_v4(),
            claim_token: Uuid::new_v4(),
            email: "owner@example.com".to_string(),
            contact_email: None,
            contact_email_verified_at: None,
        }]);
        let limits = Arc::new(FixedLimitsRepository {
            limit: OrganizationLimit {
                spend_limit: 100,
                available: 25,
                unfunded: 1,
            },
            breakdown: vec![OrganizationCreditLimit {
                credit_type: "postpay".to_string(),
                source: None,
                amount: 100,
                transferred_out: 0,
                effective_capacity: 100,
                consumed: 75,
                available: 25,
                currency: "USD".to_string(),
            }],
        });
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits,
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(
            repository.reconciliations.lock().await[0].threshold,
            Some(0)
        );
    }

    #[tokio::test]
    async fn active_zero_capacity_uses_exhausted_threshold() {
        let repository = repository(Vec::new());
        let limits = Arc::new(FixedLimitsRepository {
            limit: OrganizationLimit {
                spend_limit: 0,
                available: 0,
                unfunded: 0,
            },
            breakdown: vec![OrganizationCreditLimit {
                credit_type: "payment".to_string(),
                source: None,
                amount: 0,
                transferred_out: 0,
                effective_capacity: 0,
                consumed: 0,
                available: 0,
                currency: "USD".to_string(),
            }],
        });
        let scheduler = CreditEmailScheduler::new(
            repository.clone(),
            limits,
            Arc::new(NoopEmailSender),
            vec![0, 10, 20],
            None,
        );

        scheduler.run_once().await.unwrap();

        assert_eq!(
            repository.reconciliations.lock().await[0].threshold,
            Some(0)
        );
    }

    #[test]
    fn verified_contact_email_allows_placeholder_login() {
        let recipient = CreditEmailRecipient {
            user_id: Uuid::new_v4(),
            claim_token: Uuid::new_v4(),
            email: "alice.near@NEAR".to_string(),
            contact_email: Some("alice@example.com".to_string()),
            contact_email_verified_at: Some(Utc::now()),
        };
        assert_eq!(
            deliverable_email(
                &recipient.email,
                recipient.contact_email.as_deref(),
                recipient.contact_email_verified_at,
            ),
            Some("alice@example.com")
        );
    }
}
