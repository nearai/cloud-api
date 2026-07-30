use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use config::AmlConfig;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration as StdDuration};
use uuid::Uuid;

pub const AML_PROVIDER_LUKKA: &str = "lukka";
pub const AML_ADDRESS_TYPE_NEAR: &str = "NEAR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AmlRiskLevel {
    Low,
    Medium,
    High,
    Unknown,
}

impl AmlRiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
            Self::Unknown => "UNKNOWN",
        }
    }

    fn from_provider(value: Option<&str>) -> Self {
        match value
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase()
            .as_str()
        {
            "LOW" => Self::Low,
            "MEDIUM" => Self::Medium,
            "HIGH" => Self::High,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmlFlow {
    UserStatus,
    StakingFarmSync,
}

impl AmlFlow {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserStatus => "user_status",
            Self::StakingFarmSync => "staking_farm_sync",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmlReport {
    pub id: Uuid,
    pub user_id: Option<Uuid>,
    pub flow: String,
    pub provider: String,
    pub account_id: String,
    pub address_type: String,
    pub risk_level: AmlRiskLevel,
    pub score: Option<i32>,
    pub report_id: Option<String>,
    pub reason: Option<String>,
    pub provider_report_time: Option<DateTime<Utc>>,
    pub result_json: serde_json::Value,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewAmlReport {
    pub user_id: Option<Uuid>,
    pub flow: AmlFlow,
    pub provider: String,
    pub account_id: String,
    pub address_type: String,
    pub risk_level: AmlRiskLevel,
    pub score: Option<i32>,
    pub report_id: Option<String>,
    pub reason: Option<String>,
    pub provider_report_time: Option<DateTime<Utc>>,
    pub result_json: serde_json::Value,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmlDecision {
    Allowed,
    Blocked,
}

#[derive(Debug, thiserror::Error)]
pub enum AmlError {
    #[error("account blocked by AML policy")]
    AccountBlocked,
    #[error("AML provider request failed: {0}")]
    ProviderFailure(String),
    #[error("AML repository request failed: {0}")]
    RepositoryFailure(String),
}

#[async_trait]
pub trait AmlRepository: Send + Sync {
    async fn latest_active_report(
        &self,
        account_id: &str,
        address_type: &str,
        provider: &str,
    ) -> anyhow::Result<Option<AmlReport>>;

    async fn latest_fresh_active_report(
        &self,
        account_id: &str,
        address_type: &str,
        provider: &str,
        fresh_after: DateTime<Utc>,
    ) -> anyhow::Result<Option<AmlReport>>;

    async fn create_report(&self, report: NewAmlReport) -> anyhow::Result<AmlReport>;

    async fn is_allowlisted(&self, account_id: &str, address_type: &str) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait AmlProviderClient: Send + Sync {
    async fn score_near_account(
        &self,
        account_id: &str,
        user_id: Option<Uuid>,
        flow: AmlFlow,
    ) -> Result<NewAmlReport, AmlError>;
}

pub struct LukkaAmlClient {
    client: reqwest::Client,
    base_url: String,
    bearer_token: String,
}

impl LukkaAmlClient {
    pub fn new(config: &AmlConfig) -> anyhow::Result<Self> {
        let timeout = StdDuration::from_secs(config.request_timeout_seconds);
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
            base_url: config.lukka_base_url.trim_end_matches('/').to_string(),
            bearer_token: config.lukka_bearer_token.clone().unwrap_or_default(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct LukkaReport {
    report_info_section: Option<LukkaReportInfo>,
    cscore_section: Option<LukkaCscore>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LukkaReportInfo {
    address: Option<String>,
    address_type: Option<String>,
    report_id: Option<String>,
    report_time: Option<DateTime<Utc>>,
    description: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LukkaCscore {
    cscore: Option<i32>,
    risk_level: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[async_trait]
impl AmlProviderClient for LukkaAmlClient {
    async fn score_near_account(
        &self,
        account_id: &str,
        user_id: Option<Uuid>,
        flow: AmlFlow,
    ) -> Result<NewAmlReport, AmlError> {
        let normalized = normalize_near_account_id(account_id);
        let url = format!(
            "{}/v3/reports/aml/score/{}",
            self.base_url,
            urlencoding::encode(&normalized)
        );

        let response = match self
            .client
            .get(url)
            .bearer_auth(&self.bearer_token)
            .query(&[("address_type", AML_ADDRESS_TYPE_NEAR)])
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Ok(provider_failure_report(
                    user_id,
                    flow,
                    normalized,
                    provider_failure_reason(&error),
                ));
            }
        };

        if !response.status().is_success() {
            return Ok(provider_failure_report(
                user_id,
                flow,
                normalized,
                format!("lukka_http_{}", response.status().as_u16()),
            ));
        }

        let value = match response.json::<serde_json::Value>().await {
            Ok(value) => value,
            Err(_) => {
                return Ok(provider_failure_report(
                    user_id,
                    flow,
                    normalized,
                    "decode_failure".to_string(),
                ));
            }
        };
        Ok(normalize_lukka_report(user_id, flow, &normalized, value))
    }
}

fn provider_failure_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".to_string()
    } else if error.is_decode() {
        "decode_failure".to_string()
    } else {
        "network_failure".to_string()
    }
}

pub fn normalize_near_account_id(account_id: &str) -> String {
    account_id.trim().to_ascii_lowercase()
}

fn normalize_lukka_report(
    user_id: Option<Uuid>,
    flow: AmlFlow,
    requested_account_id: &str,
    value: serde_json::Value,
) -> NewAmlReport {
    let parsed = serde_json::from_value::<LukkaReport>(value.clone());
    let Ok(report) = parsed else {
        return provider_failure_report(
            user_id,
            flow,
            requested_account_id.to_string(),
            "decode_failure".to_string(),
        );
    };

    let info = report.report_info_section.as_ref();
    let returned_address = info.and_then(|info| info.address.as_deref());
    if returned_address
        .map(normalize_near_account_id)
        .filter(|address| address == requested_account_id)
        .is_none()
    {
        return provider_failure_report(
            user_id,
            flow,
            requested_account_id.to_string(),
            "address_mismatch".to_string(),
        );
    }

    if info
        .and_then(|info| info.address_type.as_deref())
        .filter(|address_type| address_type.eq_ignore_ascii_case(AML_ADDRESS_TYPE_NEAR))
        .is_none()
    {
        return provider_failure_report(
            user_id,
            flow,
            requested_account_id.to_string(),
            "address_type_mismatch".to_string(),
        );
    }

    let risk_level = AmlRiskLevel::from_provider(
        report
            .cscore_section
            .as_ref()
            .and_then(|section| section.risk_level.as_deref()),
    );
    let active = risk_level != AmlRiskLevel::Unknown;
    NewAmlReport {
        user_id,
        flow,
        provider: AML_PROVIDER_LUKKA.to_string(),
        account_id: requested_account_id.to_string(),
        address_type: AML_ADDRESS_TYPE_NEAR.to_string(),
        risk_level,
        score: report
            .cscore_section
            .as_ref()
            .and_then(|section| section.cscore),
        report_id: info.and_then(|info| info.report_id.clone()),
        reason: info.and_then(|info| info.description.clone()),
        provider_report_time: info.and_then(|info| info.report_time),
        result_json: value,
        active,
    }
}

fn provider_failure_report(
    user_id: Option<Uuid>,
    flow: AmlFlow,
    account_id: String,
    reason: String,
) -> NewAmlReport {
    NewAmlReport {
        user_id,
        flow,
        provider: AML_PROVIDER_LUKKA.to_string(),
        account_id,
        address_type: AML_ADDRESS_TYPE_NEAR.to_string(),
        risk_level: AmlRiskLevel::Unknown,
        score: None,
        report_id: None,
        reason: Some(reason.clone()),
        provider_report_time: None,
        result_json: serde_json::json!({ "error": reason }),
        active: false,
    }
}

#[derive(Clone)]
pub struct AmlService {
    repository: Arc<dyn AmlRepository>,
    provider: Arc<dyn AmlProviderClient>,
    config: AmlConfig,
    cache: Cache<String, AmlReport>,
}

impl AmlService {
    pub fn new(
        repository: Arc<dyn AmlRepository>,
        provider: Arc<dyn AmlProviderClient>,
        config: AmlConfig,
    ) -> Self {
        let cache = Cache::builder()
            .time_to_live(StdDuration::from_secs(config.memory_cache_ttl_seconds))
            .max_capacity(10_000)
            .build();
        Self {
            repository,
            provider,
            config,
            cache,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub async fn check_near_account(
        &self,
        user_id: Option<Uuid>,
        account_id: &str,
        flow: AmlFlow,
    ) -> Result<AmlDecision, AmlError> {
        if !self.config.enabled {
            return Ok(AmlDecision::Allowed);
        }

        let account_id = normalize_near_account_id(account_id);
        if account_id.is_empty() {
            return Ok(AmlDecision::Allowed);
        }
        if self
            .repository
            .is_allowlisted(&account_id, AML_ADDRESS_TYPE_NEAR)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))?
        {
            return Ok(AmlDecision::Allowed);
        }

        if let Some(cached) = self.cache.get(&account_id).await {
            return self.decision_from_report(&cached);
        }

        let fresh_after = Utc::now() - Duration::days(self.config.refresh_window_days);
        if let Some(report) = self
            .repository
            .latest_fresh_active_report(
                &account_id,
                AML_ADDRESS_TYPE_NEAR,
                AML_PROVIDER_LUKKA,
                fresh_after,
            )
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))?
        {
            if report.risk_level != AmlRiskLevel::Unknown {
                self.cache.insert(account_id, report.clone()).await;
            }
            return self.decision_from_report(&report);
        }

        let provider_result = self
            .provider
            .score_near_account(&account_id, user_id, flow)
            .await?;
        let report = self
            .repository
            .create_report(provider_result)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))?;

        if report.risk_level == AmlRiskLevel::Unknown {
            if let Some(stale_report) = self
                .repository
                .latest_active_report(&account_id, AML_ADDRESS_TYPE_NEAR, AML_PROVIDER_LUKKA)
                .await
                .map_err(|error| AmlError::RepositoryFailure(error.to_string()))?
            {
                if stale_report.risk_level == AmlRiskLevel::High {
                    return Err(AmlError::AccountBlocked);
                }
            }
            tracing::warn!(
                user_id = ?user_id,
                flow = flow.as_str(),
                "AML provider result was UNKNOWN; failing open"
            );
            return Ok(AmlDecision::Allowed);
        }

        self.cache.insert(account_id, report.clone()).await;
        self.decision_from_report(&report)
    }

    pub async fn check_authenticated_near_user(
        &self,
        user_id: Uuid,
        auth_provider: &str,
        provider_user_id: &str,
        flow: AmlFlow,
    ) -> Result<AmlDecision, AmlError> {
        if auth_provider != "near" || provider_user_id.is_empty() {
            return Ok(AmlDecision::Allowed);
        }
        self.check_near_account(Some(user_id), provider_user_id, flow)
            .await
    }

    fn decision_from_report(&self, report: &AmlReport) -> Result<AmlDecision, AmlError> {
        if report.risk_level == AmlRiskLevel::High {
            Err(AmlError::AccountBlocked)
        } else {
            Ok(AmlDecision::Allowed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn lukka_high_risk_fixture_normalizes_to_high() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "gregoshes.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "gregoshes.near",
                    "address_type": "NEAR",
                    "report_id": "4512815d6784a68a7101c72c8e0435e49c1652f6a9295639229bc980bc51dd49",
                    "report_time": "2026-07-08T19:08:43.545Z"
                },
                "cscore_section": {
                    "cscore": 99,
                    "risk_level": "HIGH"
                }
            }),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::High);
        assert_eq!(report.score, Some(99));
        assert_eq!(report.address_type, AML_ADDRESS_TYPE_NEAR);
    }

    #[test]
    fn lukka_address_mismatch_maps_to_unknown() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "bob.near",
                    "address_type": "NEAR"
                },
                "cscore_section": {
                    "cscore": 1,
                    "risk_level": "LOW"
                }
            }),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert_eq!(report.reason.as_deref(), Some("address_mismatch"));
        assert!(!report.active);
    }

    #[test]
    fn lukka_missing_risk_level_maps_to_inactive_unknown() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "alice.near",
                    "address_type": "NEAR"
                }
            }),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert!(!report.active);
    }

    #[derive(Default)]
    struct MockAmlRepository {
        latest_active: Mutex<Option<AmlReport>>,
        latest_fresh_active: Mutex<Option<AmlReport>>,
        allowlisted: Mutex<bool>,
        created: Mutex<Vec<NewAmlReport>>,
    }

    #[async_trait]
    impl AmlRepository for MockAmlRepository {
        async fn latest_active_report(
            &self,
            _account_id: &str,
            _address_type: &str,
            _provider: &str,
        ) -> anyhow::Result<Option<AmlReport>> {
            Ok(self.latest_active.lock().unwrap().clone())
        }

        async fn latest_fresh_active_report(
            &self,
            _account_id: &str,
            _address_type: &str,
            _provider: &str,
            _fresh_after: DateTime<Utc>,
        ) -> anyhow::Result<Option<AmlReport>> {
            Ok(self.latest_fresh_active.lock().unwrap().clone())
        }

        async fn create_report(&self, report: NewAmlReport) -> anyhow::Result<AmlReport> {
            self.created.lock().unwrap().push(report.clone());
            let stored = report_to_stored(report);
            if stored.active {
                *self.latest_active.lock().unwrap() = Some(stored.clone());
            }
            Ok(stored)
        }

        async fn is_allowlisted(
            &self,
            _account_id: &str,
            _address_type: &str,
        ) -> anyhow::Result<bool> {
            Ok(*self.allowlisted.lock().unwrap())
        }
    }

    struct StaticProvider(NewAmlReport);

    #[async_trait]
    impl AmlProviderClient for StaticProvider {
        async fn score_near_account(
            &self,
            _account_id: &str,
            _user_id: Option<Uuid>,
            _flow: AmlFlow,
        ) -> Result<NewAmlReport, AmlError> {
            Ok(self.0.clone())
        }
    }

    struct ConcurrentHighThenUnknownProvider {
        barrier: tokio::sync::Barrier,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl AmlProviderClient for ConcurrentHighThenUnknownProvider {
        async fn score_near_account(
            &self,
            _account_id: &str,
            _user_id: Option<Uuid>,
            _flow: AmlFlow,
        ) -> Result<NewAmlReport, AmlError> {
            let call_index = {
                let mut calls = self.calls.lock().unwrap();
                let call_index = *calls;
                *calls += 1;
                call_index
            };

            self.barrier.wait().await;
            if call_index == 0 {
                return Ok(new_report(AmlRiskLevel::High));
            }

            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(new_report(AmlRiskLevel::Unknown))
        }
    }

    #[tokio::test]
    async fn high_risk_blocks_unless_allowlisted() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::High)));
        let service = AmlService::new(repo.clone(), provider, enabled_config());

        let blocked = service
            .check_near_account(None, "Gregoshes.NEAR", AmlFlow::UserStatus)
            .await;
        assert!(matches!(blocked, Err(AmlError::AccountBlocked)));

        *repo.allowlisted.lock().unwrap() = true;
        let allowed = service
            .check_near_account(None, "gregoshes.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(allowed, Ok(AmlDecision::Allowed)));
    }

    #[tokio::test]
    async fn unknown_provider_result_fails_open_without_stale_high() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Unknown)));
        let service = AmlService::new(repo, provider, enabled_config());

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Ok(AmlDecision::Allowed)));
    }

    #[tokio::test]
    async fn unknown_provider_result_keeps_stale_high_blocked() {
        let repo = Arc::new(MockAmlRepository::default());
        *repo.latest_active.lock().unwrap() =
            Some(report_to_stored(new_report(AmlRiskLevel::High)));
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Unknown)));
        let service = AmlService::new(repo.clone(), provider, enabled_config());

        let first = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(first, Err(AmlError::AccountBlocked)));

        let second = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(second, Err(AmlError::AccountBlocked)));
        assert_eq!(repo.created.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn concurrent_unknown_result_rechecks_latest_active_high() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(ConcurrentHighThenUnknownProvider {
            barrier: tokio::sync::Barrier::new(2),
            calls: Mutex::new(0),
        });
        let service = AmlService::new(repo.clone(), provider, enabled_config());

        let (first, second) = tokio::join!(
            service.check_near_account(None, "alice.near", AmlFlow::UserStatus),
            service.check_near_account(None, "alice.near", AmlFlow::UserStatus)
        );

        assert!(matches!(first, Err(AmlError::AccountBlocked)));
        assert!(matches!(second, Err(AmlError::AccountBlocked)));
        assert_eq!(repo.created.lock().unwrap().len(), 2);
    }

    fn enabled_config() -> AmlConfig {
        AmlConfig {
            enabled: true,
            lukka_bearer_token: Some("token".to_string()),
            memory_cache_ttl_seconds: 60,
            ..AmlConfig::default()
        }
    }

    fn new_report(risk_level: AmlRiskLevel) -> NewAmlReport {
        let active = risk_level != AmlRiskLevel::Unknown;
        NewAmlReport {
            user_id: None,
            flow: AmlFlow::UserStatus,
            provider: AML_PROVIDER_LUKKA.to_string(),
            account_id: "gregoshes.near".to_string(),
            address_type: AML_ADDRESS_TYPE_NEAR.to_string(),
            risk_level,
            score: Some(99),
            report_id: Some("report".to_string()),
            reason: None,
            provider_report_time: None,
            result_json: serde_json::json!({}),
            active,
        }
    }

    fn report_to_stored(report: NewAmlReport) -> AmlReport {
        AmlReport {
            id: Uuid::new_v4(),
            user_id: report.user_id,
            flow: report.flow.as_str().to_string(),
            provider: report.provider,
            account_id: report.account_id,
            address_type: report.address_type,
            risk_level: report.risk_level,
            score: report.score,
            report_id: report.report_id,
            reason: report.reason,
            provider_report_time: report.provider_report_time,
            result_json: report.result_json,
            active: report.active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}
