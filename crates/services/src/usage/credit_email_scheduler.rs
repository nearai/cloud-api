//! Sends durable low-credit notifications from the same balance ports used by usage admission.

use std::sync::Arc;

use tracing::{error, info, warn};

use super::ports::{
    CreditEmailNotificationRepository, CreditEmailOrganization, OrganizationLimitsRepository,
};
use crate::email::{deliverable_email, CreditBalanceAlertEmail, EmailDeliveryOutcome, EmailSender};

const SCAN_INTERVAL_SECS: u64 = 300;

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
        let Some(limit) = self
            .limits_repository
            .get_current_limits(organization.id)
            .await?
        else {
            self.repository.clear_claims(organization.id).await?;
            return Ok(());
        };
        let breakdown = self
            .limits_repository
            .get_current_limit_breakdown(organization.id)
            .await?;
        let effective_capacity = breakdown
            .iter()
            .map(|credit| credit.effective_capacity)
            .fold(0_i64, i64::saturating_add);
        if effective_capacity <= 0 {
            self.repository.clear_claims(organization.id).await?;
            return Ok(());
        }

        // Usage admission blocks while unresolved overage exists. Treat that state as exhausted
        // so the notification and request gate never disagree about whether credits are usable.
        let remaining = if limit.unfunded > 0 {
            0
        } else {
            limit.available
        };
        let Some(threshold) =
            threshold_for_balance(remaining, effective_capacity, &self.thresholds)
        else {
            self.repository.clear_claims(organization.id).await?;
            return Ok(());
        };

        let lower_thresholds = self
            .thresholds
            .iter()
            .copied()
            .filter(|candidate| *candidate < threshold)
            .collect::<Vec<_>>();
        if !lower_thresholds.is_empty() {
            self.repository
                .clear_threshold_claims(organization.id, &lower_thresholds)
                .await?;
        }

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
        let recipients = self
            .repository
            .owner_admin_recipients(organization.id)
            .await?;
        for recipient in recipients {
            let Some(recipient_email) = deliverable_email(
                &recipient.email,
                recipient.contact_email.as_deref(),
                recipient.contact_email_verified_at,
            ) else {
                continue;
            };
            if !self
                .repository
                .try_claim(organization.id, threshold, recipient_email)
                .await?
            {
                continue;
            }
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
            match self.email_sender.send_credit_balance_alert(&email).await {
                Ok(EmailDeliveryOutcome::Sent { .. }) => {
                    if let Err(error) = self
                        .repository
                        .mark_sent(organization.id, threshold, recipient_email)
                        .await
                    {
                        warn!(organization_id = %organization.id, error = %error, "Failed to mark credit balance alert as sent");
                    }
                }
                Ok(EmailDeliveryOutcome::Skipped) => {
                    let _ = self
                        .repository
                        .release_claim(organization.id, threshold, recipient_email)
                        .await;
                }
                Err(error) => {
                    warn!(organization_id = %organization.id, error = %error.sanitized_message(), "Failed to send credit balance alert");
                    let _ = self
                        .repository
                        .release_claim(organization.id, threshold, recipient_email)
                        .await;
                }
            }
        }
        Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::*;
    use crate::email::NoopEmailSender;
    use crate::usage::ports::{CreditEmailRecipient, OrganizationCreditLimit, OrganizationLimit};

    struct TestNotificationRepository {
        organizations: Vec<CreditEmailOrganization>,
        recipients: Vec<CreditEmailRecipient>,
        clear_all_calls: AtomicUsize,
        cleared_thresholds: Mutex<Vec<Vec<i16>>>,
        claimed_thresholds: Mutex<Vec<i16>>,
        claimed_organizations: Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl CreditEmailNotificationRepository for TestNotificationRepository {
        async fn active_organizations(&self) -> anyhow::Result<Vec<CreditEmailOrganization>> {
            Ok(self.organizations.clone())
        }

        async fn owner_admin_recipients(
            &self,
            _: Uuid,
        ) -> anyhow::Result<Vec<CreditEmailRecipient>> {
            Ok(self.recipients.clone())
        }

        async fn clear_claims(&self, _: Uuid) -> anyhow::Result<()> {
            self.clear_all_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn clear_threshold_claims(&self, _: Uuid, thresholds: &[i16]) -> anyhow::Result<()> {
            self.cleared_thresholds
                .lock()
                .await
                .push(thresholds.to_vec());
            Ok(())
        }

        async fn try_claim(
            &self,
            organization_id: Uuid,
            threshold: i16,
            _: &str,
        ) -> anyhow::Result<bool> {
            self.claimed_organizations
                .lock()
                .await
                .push(organization_id);
            self.claimed_thresholds.lock().await.push(threshold);
            Ok(false)
        }

        async fn release_claim(&self, _: Uuid, _: i16, _: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_sent(&self, _: Uuid, _: i16, _: &str) -> anyhow::Result<()> {
            Ok(())
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
    }

    fn repository(recipients: Vec<CreditEmailRecipient>) -> Arc<TestNotificationRepository> {
        Arc::new(TestNotificationRepository {
            organizations: vec![CreditEmailOrganization {
                id: Uuid::new_v4(),
                name: "Example".to_string(),
            }],
            recipients,
            clear_all_calls: AtomicUsize::new(0),
            cleared_thresholds: Mutex::new(Vec::new()),
            claimed_thresholds: Mutex::new(Vec::new()),
            claimed_organizations: Mutex::new(Vec::new()),
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

        assert_eq!(repository.clear_all_calls.load(Ordering::Relaxed), 1);
        assert!(repository.claimed_thresholds.lock().await.is_empty());
    }

    #[test]
    fn threshold_crossing_does_not_round_down() {
        assert_eq!(threshold_for_balance(201, 1_000, &[0, 10, 20]), None);
        assert_eq!(threshold_for_balance(200, 1_000, &[0, 10, 20]), Some(20));
        assert_eq!(threshold_for_balance(101, 1_000, &[0, 10, 20]), Some(20));
        assert_eq!(threshold_for_balance(100, 1_000, &[0, 10, 20]), Some(10));
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
                email: "owner@example.com".to_string(),
                contact_email: None,
                contact_email_verified_at: None,
            }],
            clear_all_calls: AtomicUsize::new(0),
            cleared_thresholds: Mutex::new(Vec::new()),
            claimed_thresholds: Mutex::new(Vec::new()),
            claimed_organizations: Mutex::new(Vec::new()),
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
            *repository.claimed_organizations.lock().await,
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

        assert_eq!(repository.clear_all_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            *repository.cleared_thresholds.lock().await,
            vec![vec![0, 10]]
        );
    }

    #[tokio::test]
    async fn placeholder_email_filter_is_shared_and_case_insensitive() {
        let repository = repository(vec![CreditEmailRecipient {
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

        assert!(repository.claimed_thresholds.lock().await.is_empty());
        assert_eq!(*repository.cleared_thresholds.lock().await, vec![vec![0]]);
    }

    #[tokio::test]
    async fn unresolved_overage_uses_exhausted_threshold() {
        let repository = repository(vec![CreditEmailRecipient {
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

        assert_eq!(*repository.claimed_thresholds.lock().await, vec![0]);
    }

    #[test]
    fn verified_contact_email_allows_placeholder_login() {
        let recipient = CreditEmailRecipient {
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
