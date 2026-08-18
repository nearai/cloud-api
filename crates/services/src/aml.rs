use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use config::AmlConfig;
use moka::future::Cache;
use serde::{Deserialize, Deserializer, Serialize};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmlReportPage {
    pub reports: Vec<AmlReport>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmlAllowlistEntry {
    pub account_id: String,
    pub address_type: String,
    pub reason: Option<String>,
    pub created_by_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewAmlAllowlistEntry {
    pub account_id: String,
    pub address_type: String,
    pub reason: Option<String>,
    pub created_by_user_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmlDecision {
    Allowed,
}

#[derive(Debug, thiserror::Error)]
pub enum AmlError {
    #[error("account blocked by AML policy")]
    AccountBlocked,
    #[error("invalid NEAR account id")]
    InvalidAccountId,
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

    async fn list_reports(&self, limit: i64, offset: i64) -> anyhow::Result<AmlReportPage>;

    async fn set_report_active(
        &self,
        report_id: Uuid,
        active: bool,
    ) -> anyhow::Result<Option<AmlReport>>;

    async fn is_allowlisted(&self, account_id: &str, address_type: &str) -> anyhow::Result<bool>;

    async fn list_allowlist_entries(&self) -> anyhow::Result<Vec<AmlAllowlistEntry>>;

    async fn upsert_allowlist_entry(
        &self,
        entry: NewAmlAllowlistEntry,
    ) -> anyhow::Result<AmlAllowlistEntry>;

    async fn remove_allowlist_entry(
        &self,
        account_id: &str,
        address_type: &str,
    ) -> anyhow::Result<bool>;
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
    score_block_threshold: Option<i32>,
}

impl LukkaAmlClient {
    pub fn new(config: &AmlConfig) -> anyhow::Result<Self> {
        let timeout = StdDuration::from_secs(config.request_timeout_seconds);
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
            base_url: config.lukka_base_url.trim_end_matches('/').to_string(),
            bearer_token: config.lukka_bearer_token.clone().unwrap_or_default(),
            score_block_threshold: config.score_block_threshold,
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
    #[serde(default, deserialize_with = "lenient_opt")]
    address: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt")]
    address_type: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt")]
    report_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt")]
    report_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "lenient_opt")]
    description: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LukkaCscore {
    #[serde(default, deserialize_with = "lenient_opt_i32")]
    cscore: Option<i32>,
    #[serde(default, deserialize_with = "lenient_opt")]
    risk_level: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

fn lenient_opt<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

fn lenient_opt_i32<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(number) = value.as_i64() {
        return Ok(i32::try_from(number).ok());
    }
    if let Some(number) = value.as_f64() {
        if number.is_finite() && number >= i32::MIN as f64 && number <= i32::MAX as f64 {
            return Ok(Some(number.round() as i32));
        }
    }
    Ok(serde_json::from_value(value).ok())
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
        Ok(normalize_lukka_report(
            user_id,
            flow,
            &normalized,
            value,
            self.score_block_threshold,
        ))
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

pub fn validate_near_account_id(account_id: &str) -> Result<String, AmlError> {
    let normalized = normalize_near_account_id(account_id);
    let Ok(_) = normalized.parse::<near_api::AccountId>() else {
        return Err(AmlError::InvalidAccountId);
    };
    Ok(normalized)
}

fn normalize_lukka_report(
    user_id: Option<Uuid>,
    flow: AmlFlow,
    requested_account_id: &str,
    value: serde_json::Value,
    score_block_threshold: Option<i32>,
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
        return provider_failure_report_with_result(
            user_id,
            flow,
            requested_account_id.to_string(),
            "address_mismatch".to_string(),
            value,
        );
    }

    if info
        .and_then(|info| info.address_type.as_deref())
        .filter(|address_type| address_type.eq_ignore_ascii_case(AML_ADDRESS_TYPE_NEAR))
        .is_none()
    {
        return provider_failure_report_with_result(
            user_id,
            flow,
            requested_account_id.to_string(),
            "address_type_mismatch".to_string(),
            value,
        );
    }

    let score = report
        .cscore_section
        .as_ref()
        .and_then(|section| section.cscore);
    let provider_risk_level = report
        .cscore_section
        .as_ref()
        .and_then(|section| section.risk_level.as_deref());
    let (risk_level, risk_reason) =
        normalize_provider_risk_level(provider_risk_level, score, score_block_threshold);
    let active = risk_level != AmlRiskLevel::Unknown;
    NewAmlReport {
        user_id,
        flow,
        provider: AML_PROVIDER_LUKKA.to_string(),
        account_id: requested_account_id.to_string(),
        address_type: AML_ADDRESS_TYPE_NEAR.to_string(),
        risk_level,
        score,
        report_id: info.and_then(|info| info.report_id.clone()),
        reason: risk_reason.or_else(|| info.and_then(|info| info.description.clone())),
        provider_report_time: info.and_then(|info| info.report_time),
        result_json: value,
        active,
    }
}

fn normalize_provider_risk_level(
    value: Option<&str>,
    score: Option<i32>,
    score_block_threshold: Option<i32>,
) -> (AmlRiskLevel, Option<String>) {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    match value.map(|value| value.to_ascii_uppercase()) {
        Some(value) if value == "LOW" => (AmlRiskLevel::Low, None),
        Some(value) if value == "MEDIUM" => (AmlRiskLevel::Medium, None),
        Some(value) if value == "HIGH" => (AmlRiskLevel::High, None),
        Some(_) => {
            let raw_value = value.unwrap_or_default();
            let reason = Some(format!("unrecognized_risk_level:{raw_value}"));
            if score_block_threshold
                .zip(score)
                .is_some_and(|(threshold, score)| score >= threshold)
            {
                (AmlRiskLevel::High, reason)
            } else {
                (AmlRiskLevel::Unknown, reason)
            }
        }
        None => (
            AmlRiskLevel::Unknown,
            Some("missing_risk_level".to_string()),
        ),
    }
}

fn provider_failure_report(
    user_id: Option<Uuid>,
    flow: AmlFlow,
    account_id: String,
    reason: String,
) -> NewAmlReport {
    provider_failure_report_with_result(
        user_id,
        flow,
        account_id,
        reason.clone(),
        serde_json::json!({ "error": reason }),
    )
}

fn provider_failure_report_with_result(
    user_id: Option<Uuid>,
    flow: AmlFlow,
    account_id: String,
    reason: String,
    result_json: serde_json::Value,
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
        result_json,
        active: false,
    }
}

#[derive(Clone)]
pub struct AmlService {
    repository: Arc<dyn AmlRepository>,
    provider: Arc<dyn AmlProviderClient>,
    config: AmlConfig,
    cache: Cache<String, AmlReport>,
    unknown_cache: Cache<String, AmlReport>,
    alert_dedupe_cache: Cache<String, ()>,
    slack_client: reqwest::Client,
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
        let unknown_cache = Cache::builder()
            .time_to_live(StdDuration::from_secs(config.unknown_cache_ttl_seconds))
            .max_capacity(10_000)
            .build();
        let alert_dedupe_cache = Cache::builder()
            .time_to_live(StdDuration::from_secs(
                config.high_risk_slack_dedupe_seconds,
            ))
            .max_capacity(10_000)
            .build();
        let slack_client = reqwest::Client::builder()
            .timeout(StdDuration::from_millis(config.high_risk_slack_timeout_ms))
            .build()
            .unwrap_or_else(|error| {
                tracing::warn!(
                    error = %error,
                    "failed to initialize AML Slack alert client; using default client"
                );
                reqwest::Client::new()
            });
        Self {
            repository,
            provider,
            config,
            cache,
            unknown_cache,
            alert_dedupe_cache,
            slack_client,
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

        if let Some(report) = self.unknown_cache.get(&account_id).await {
            if self.should_block_report(&report) {
                return Err(AmlError::AccountBlocked);
            }
            return self
                .decision_from_unknown_report(&account_id, &report, user_id, flow, false)
                .await;
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

        if report.risk_level != AmlRiskLevel::Unknown {
            self.cache.insert(account_id.clone(), report.clone()).await;
        }

        if report.risk_level == AmlRiskLevel::Unknown {
            self.unknown_cache
                .insert(account_id.clone(), report.clone())
                .await;
        }

        if self.should_block_report(&report) {
            self.enqueue_high_risk_slack_alert(&report, user_id, flow, "provider_report")
                .await;
            return Err(AmlError::AccountBlocked);
        }

        if report.risk_level == AmlRiskLevel::Unknown {
            return self
                .decision_from_unknown_report(&account_id, &report, user_id, flow, true)
                .await;
        }

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

    pub async fn list_reports(&self, limit: i64, offset: i64) -> Result<AmlReportPage, AmlError> {
        self.repository
            .list_reports(limit, offset)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))
    }

    pub async fn set_report_active(
        &self,
        report_id: Uuid,
        active: bool,
    ) -> Result<Option<AmlReport>, AmlError> {
        self.repository
            .set_report_active(report_id, active)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))
    }

    pub async fn list_allowlist_entries(&self) -> Result<Vec<AmlAllowlistEntry>, AmlError> {
        self.repository
            .list_allowlist_entries()
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))
    }

    pub async fn upsert_allowlist_entry(
        &self,
        account_id: &str,
        reason: Option<String>,
        created_by_user_id: Option<Uuid>,
    ) -> Result<AmlAllowlistEntry, AmlError> {
        let account_id = validate_near_account_id(account_id)?;
        self.repository
            .upsert_allowlist_entry(NewAmlAllowlistEntry {
                account_id,
                address_type: AML_ADDRESS_TYPE_NEAR.to_string(),
                reason,
                created_by_user_id,
            })
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))
    }

    pub async fn remove_allowlist_entry(&self, account_id: &str) -> Result<bool, AmlError> {
        let account_id = validate_near_account_id(account_id)?;
        self.repository
            .remove_allowlist_entry(&account_id, AML_ADDRESS_TYPE_NEAR)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))
    }

    fn decision_from_report(&self, report: &AmlReport) -> Result<AmlDecision, AmlError> {
        if self.should_block_report(report) {
            Err(AmlError::AccountBlocked)
        } else {
            Ok(AmlDecision::Allowed)
        }
    }

    fn should_block_report(&self, report: &AmlReport) -> bool {
        self.config.blocked_risk_levels.iter().any(|level| {
            report.risk_level != AmlRiskLevel::Unknown && level == report.risk_level.as_str()
        }) || self
            .config
            .score_block_threshold
            .zip(report.score)
            .is_some_and(|(threshold, score)| score >= threshold)
    }

    async fn decision_from_unknown_report(
        &self,
        account_id: &str,
        report: &AmlReport,
        user_id: Option<Uuid>,
        flow: AmlFlow,
        cache_unknown: bool,
    ) -> Result<AmlDecision, AmlError> {
        if cache_unknown {
            self.unknown_cache
                .insert(account_id.to_string(), report.clone())
                .await;
        }

        if let Some(stale_report) = self
            .repository
            .latest_active_report(account_id, AML_ADDRESS_TYPE_NEAR, AML_PROVIDER_LUKKA)
            .await
            .map_err(|error| AmlError::RepositoryFailure(error.to_string()))?
        {
            if self.should_block_report(&stale_report) {
                self.enqueue_high_risk_slack_alert(
                    &stale_report,
                    user_id,
                    flow,
                    "stale_report_fallback",
                )
                .await;
                return Err(AmlError::AccountBlocked);
            }
        }

        tracing::warn!(
            user_id = ?user_id,
            flow = flow.as_str(),
            "AML provider result was UNKNOWN; failing open"
        );
        Ok(AmlDecision::Allowed)
    }

    async fn enqueue_high_risk_slack_alert(
        &self,
        report: &AmlReport,
        user_id: Option<Uuid>,
        flow: AmlFlow,
        action: &'static str,
    ) {
        let Some(webhook_url) = self.reserve_high_risk_slack_alert(report, action).await else {
            return;
        };
        let payload = high_risk_slack_payload(report, user_id, flow, action);
        let client = self.slack_client.clone();
        tokio::spawn(async move {
            match client.post(webhook_url).json(&payload).send().await {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    tracing::warn!(
                        status = response.status().as_u16(),
                        "AML high-risk Slack alert returned non-success status"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to send AML high-risk Slack alert"
                    );
                }
            }
        });
    }

    async fn reserve_high_risk_slack_alert(
        &self,
        report: &AmlReport,
        action: &'static str,
    ) -> Option<String> {
        let webhook_url = self.config.high_risk_slack_webhook_url.clone()?;
        let dedupe_key = format!("{}:{action}:{}", report.provider, report.account_id);
        if self.alert_dedupe_cache.get(&dedupe_key).await.is_some() {
            return None;
        }
        self.alert_dedupe_cache.insert(dedupe_key, ()).await;
        Some(webhook_url)
    }
}

fn high_risk_slack_payload(
    report: &AmlReport,
    user_id: Option<Uuid>,
    flow: AmlFlow,
    action: &str,
) -> serde_json::Value {
    let user_id = user_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let score = report
        .score
        .map(|score| score.to_string())
        .unwrap_or_else(|| "-".to_string());
    let report_id = report.report_id.as_deref().unwrap_or("-");
    let reason = report.reason.as_deref().unwrap_or("-");
    let provider_report_time = report
        .provider_report_time
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| "-".to_string());

    serde_json::json!({
        "text": format!(
            "High-risk Lukka AML account detected: {} ({})",
            report.account_id,
            report.risk_level.as_str()
        ),
        "attachments": [
            {
                "color": "danger",
                "fields": [
                    { "title": "Account", "value": report.account_id, "short": true },
                    { "title": "User ID", "value": user_id, "short": true },
                    { "title": "Flow", "value": flow.as_str(), "short": true },
                    { "title": "Action", "value": action, "short": true },
                    { "title": "Provider", "value": report.provider, "short": true },
                    { "title": "Address Type", "value": report.address_type, "short": true },
                    { "title": "Risk Level", "value": report.risk_level.as_str(), "short": true },
                    { "title": "Score", "value": score, "short": true },
                    { "title": "Report ID", "value": report_id, "short": false },
                    { "title": "Reason", "value": reason, "short": false },
                    { "title": "Provider Report Time", "value": provider_report_time, "short": false }
                ]
            }
        ]
    })
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
            Some(75),
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
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert_eq!(report.reason.as_deref(), Some("address_mismatch"));
        assert_eq!(
            report.result_json["report_info_section"]["address"].as_str(),
            Some("bob.near")
        );
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
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert!(!report.active);
    }

    #[test]
    fn lukka_missing_risk_level_preserves_score_for_policy() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "alice.near",
                    "address_type": "NEAR"
                },
                "cscore_section": {
                    "cscore": 99
                }
            }),
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert_eq!(report.score, Some(99));
        assert_eq!(report.reason.as_deref(), Some("missing_risk_level"));
        assert!(!report.active);
    }

    #[test]
    fn lukka_metadata_type_drift_does_not_discard_high_risk() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "alice.near",
                    "address_type": "NEAR",
                    "report_id": 12345,
                    "report_time": "not-rfc3339"
                },
                "cscore_section": {
                    "cscore": 99.5,
                    "risk_level": "HIGH"
                }
            }),
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::High);
        assert_eq!(report.score, Some(100));
        assert!(report.report_id.is_none());
        assert!(report.provider_report_time.is_none());
        assert!(report.active);
    }

    #[test]
    fn lukka_unrecognized_risk_level_uses_high_score_fallback() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "alice.near",
                    "address_type": "NEAR"
                },
                "cscore_section": {
                    "cscore": 99,
                    "risk_level": "SEVERE"
                }
            }),
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::High);
        assert_eq!(
            report.reason.as_deref(),
            Some("unrecognized_risk_level:SEVERE")
        );
        assert!(report.active);
    }

    #[test]
    fn lukka_unrecognized_risk_level_with_low_score_stays_inactive_unknown() {
        let report = normalize_lukka_report(
            None,
            AmlFlow::UserStatus,
            "alice.near",
            serde_json::json!({
                "report_info_section": {
                    "address": "alice.near",
                    "address_type": "NEAR"
                },
                "cscore_section": {
                    "cscore": 10,
                    "risk_level": "NEW_LOW_TIER"
                }
            }),
            Some(75),
        );

        assert_eq!(report.risk_level, AmlRiskLevel::Unknown);
        assert_eq!(
            report.reason.as_deref(),
            Some("unrecognized_risk_level:NEW_LOW_TIER")
        );
        assert!(!report.active);
    }

    #[derive(Default)]
    struct MockAmlRepository {
        latest_active: Mutex<Option<AmlReport>>,
        latest_fresh_active: Mutex<Option<AmlReport>>,
        allowlisted: Mutex<bool>,
        created: Mutex<Vec<NewAmlReport>>,
        reports: Mutex<Vec<AmlReport>>,
        allowlist_entries: Mutex<Vec<AmlAllowlistEntry>>,
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
            self.reports.lock().unwrap().push(stored.clone());
            Ok(stored)
        }

        async fn list_reports(&self, limit: i64, offset: i64) -> anyhow::Result<AmlReportPage> {
            let reports = self.reports.lock().unwrap();
            let total = reports.len() as i64;
            let page = reports
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect();
            Ok(AmlReportPage {
                reports: page,
                total,
                limit,
                offset,
            })
        }

        async fn set_report_active(
            &self,
            report_id: Uuid,
            active: bool,
        ) -> anyhow::Result<Option<AmlReport>> {
            let mut reports = self.reports.lock().unwrap();
            let Some(report) = reports.iter_mut().find(|report| report.id == report_id) else {
                return Ok(None);
            };
            report.active = active;
            report.updated_at = Utc::now();
            Ok(Some(report.clone()))
        }

        async fn is_allowlisted(
            &self,
            account_id: &str,
            address_type: &str,
        ) -> anyhow::Result<bool> {
            Ok(*self.allowlisted.lock().unwrap()
                || self.allowlist_entries.lock().unwrap().iter().any(|entry| {
                    entry.account_id == account_id && entry.address_type == address_type
                }))
        }

        async fn list_allowlist_entries(&self) -> anyhow::Result<Vec<AmlAllowlistEntry>> {
            Ok(self.allowlist_entries.lock().unwrap().clone())
        }

        async fn upsert_allowlist_entry(
            &self,
            entry: NewAmlAllowlistEntry,
        ) -> anyhow::Result<AmlAllowlistEntry> {
            let mut entries = self.allowlist_entries.lock().unwrap();
            let stored = AmlAllowlistEntry {
                account_id: entry.account_id,
                address_type: entry.address_type,
                reason: entry.reason,
                created_by_user_id: entry.created_by_user_id,
                created_at: Utc::now(),
            };
            if let Some(existing) = entries.iter_mut().find(|existing| {
                existing.account_id == stored.account_id
                    && existing.address_type == stored.address_type
            }) {
                existing.reason = stored.reason.clone();
                existing.created_by_user_id = stored.created_by_user_id;
                return Ok(existing.clone());
            }
            entries.push(stored.clone());
            Ok(stored)
        }

        async fn remove_allowlist_entry(
            &self,
            account_id: &str,
            address_type: &str,
        ) -> anyhow::Result<bool> {
            let mut entries = self.allowlist_entries.lock().unwrap();
            let before = entries.len();
            entries.retain(|entry| {
                entry.account_id != account_id || entry.address_type != address_type
            });
            Ok(entries.len() != before)
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
    async fn configured_risk_level_blocks_matching_report() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Medium)));
        let config = AmlConfig {
            blocked_risk_levels: vec!["MEDIUM".to_string()],
            score_block_threshold: None,
            ..enabled_config()
        };
        let service = AmlService::new(repo, provider, config);

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Err(AmlError::AccountBlocked)));
    }

    #[tokio::test]
    async fn configured_score_threshold_blocks_matching_report() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Low,
            score: Some(80),
            ..new_report(AmlRiskLevel::Low)
        }));
        let config = AmlConfig {
            blocked_risk_levels: vec![],
            score_block_threshold: Some(75),
            ..enabled_config()
        };
        let service = AmlService::new(repo, provider, config);

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Err(AmlError::AccountBlocked)));
    }

    #[tokio::test]
    async fn configured_score_threshold_blocks_unknown_report_with_matching_score() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Unknown,
            score: Some(80),
            active: false,
            reason: Some("missing_risk_level".to_string()),
            ..new_report(AmlRiskLevel::Unknown)
        }));
        let config = AmlConfig {
            blocked_risk_levels: vec![],
            score_block_threshold: Some(75),
            ..enabled_config()
        };
        let service = AmlService::new(repo.clone(), provider, config);

        let first = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(first, Err(AmlError::AccountBlocked)));

        let second = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(second, Err(AmlError::AccountBlocked)));
        assert_eq!(repo.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_risk_level_fails_open_even_if_policy_contains_unknown() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Unknown,
            score: None,
            active: false,
            reason: Some("missing_risk_level".to_string()),
            ..new_report(AmlRiskLevel::Unknown)
        }));
        let config = AmlConfig {
            blocked_risk_levels: vec!["UNKNOWN".to_string()],
            score_block_threshold: None,
            ..enabled_config()
        };
        let service = AmlService::new(repo, provider, config);

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Ok(AmlDecision::Allowed)));
    }

    #[tokio::test]
    async fn high_level_is_allowed_when_policy_excludes_it_and_score_disabled() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::High)));
        let config = AmlConfig {
            blocked_risk_levels: vec!["MEDIUM".to_string()],
            score_block_threshold: None,
            ..enabled_config()
        };
        let service = AmlService::new(repo, provider, config);

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Ok(AmlDecision::Allowed)));
    }

    #[test]
    fn high_risk_slack_payload_includes_audit_context() {
        let user_id = Uuid::new_v4();
        let report = report_to_stored(NewAmlReport {
            user_id: Some(user_id),
            flow: AmlFlow::StakingFarmSync,
            risk_level: AmlRiskLevel::High,
            score: Some(91),
            reason: Some("risk policy matched".to_string()),
            ..new_report(AmlRiskLevel::High)
        });

        let payload =
            high_risk_slack_payload(&report, Some(user_id), AmlFlow::StakingFarmSync, "test");

        assert_eq!(
            payload["text"].as_str(),
            Some("High-risk Lukka AML account detected: gregoshes.near (HIGH)")
        );
        let fields = payload["attachments"][0]["fields"]
            .as_array()
            .expect("Slack attachment should include fields");
        assert!(fields
            .iter()
            .any(|field| { field["title"] == "User ID" && field["value"] == user_id.to_string() }));
        assert!(fields
            .iter()
            .any(|field| field["title"] == "Score" && field["value"] == "91"));
        assert!(fields
            .iter()
            .any(|field| field["title"] == "Action" && field["value"] == "test"));
    }

    #[tokio::test]
    async fn unknown_provider_result_fails_open_without_stale_high() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Unknown,
            score: None,
            active: false,
            reason: Some("timeout".to_string()),
            ..new_report(AmlRiskLevel::Unknown)
        }));
        let service = AmlService::new(repo, provider, enabled_config());

        let result = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;

        assert!(matches!(result, Ok(AmlDecision::Allowed)));
    }

    #[tokio::test]
    async fn unknown_provider_result_is_short_cached_after_fail_open() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Unknown,
            score: None,
            active: false,
            reason: Some("lukka_http_404".to_string()),
            ..new_report(AmlRiskLevel::Unknown)
        }));
        let service = AmlService::new(repo.clone(), provider, enabled_config());

        let first = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(first, Ok(AmlDecision::Allowed)));

        let second = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(second, Ok(AmlDecision::Allowed)));
        assert_eq!(repo.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cached_unknown_result_keeps_stale_high_blocked() {
        let repo = Arc::new(MockAmlRepository::default());
        *repo.latest_active.lock().unwrap() =
            Some(report_to_stored(new_report(AmlRiskLevel::High)));
        let provider = Arc::new(StaticProvider(NewAmlReport {
            risk_level: AmlRiskLevel::Unknown,
            score: None,
            active: false,
            reason: Some("timeout".to_string()),
            ..new_report(AmlRiskLevel::Unknown)
        }));
        let service = AmlService::new(repo.clone(), provider, enabled_config());

        let first = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(first, Err(AmlError::AccountBlocked)));

        let second = service
            .check_near_account(None, "alice.near", AmlFlow::UserStatus)
            .await;
        assert!(matches!(second, Err(AmlError::AccountBlocked)));
        assert_eq!(repo.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_report_fallback_slack_alert_is_deduped() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::High)));
        let config = AmlConfig {
            high_risk_slack_webhook_url: Some("https://hooks.slack.test/token".to_string()),
            high_risk_slack_dedupe_seconds: 60,
            ..enabled_config()
        };
        let service = AmlService::new(repo, provider, config);
        let report = report_to_stored(new_report(AmlRiskLevel::High));

        let first = service
            .reserve_high_risk_slack_alert(&report, "stale_report_fallback")
            .await;
        let second = service
            .reserve_high_risk_slack_alert(&report, "stale_report_fallback")
            .await;
        let different_action = service
            .reserve_high_risk_slack_alert(&report, "provider_report")
            .await;

        assert_eq!(first.as_deref(), Some("https://hooks.slack.test/token"));
        assert!(second.is_none());
        assert_eq!(
            different_action.as_deref(),
            Some("https://hooks.slack.test/token")
        );
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

    #[tokio::test]
    async fn report_list_pagination_preserves_limit_and_offset() {
        let repo = Arc::new(MockAmlRepository::default());
        repo.reports.lock().unwrap().extend(
            [
                new_report(AmlRiskLevel::Low),
                new_report(AmlRiskLevel::High),
            ]
            .map(report_to_stored),
        );
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Low)));
        let service = AmlService::new(repo, provider, enabled_config());

        let page = service.list_reports(1, 1).await.unwrap();

        assert_eq!(page.total, 2);
        assert_eq!(page.limit, 1);
        assert_eq!(page.offset, 1);
        assert_eq!(page.reports.len(), 1);
    }

    #[tokio::test]
    async fn report_status_mutation_updates_active_and_returns_none_for_unknown_report() {
        let repo = Arc::new(MockAmlRepository::default());
        let stored = report_to_stored(new_report(AmlRiskLevel::High));
        let report_id = stored.id;
        repo.reports.lock().unwrap().push(stored);
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Low)));
        let service = AmlService::new(repo, provider, enabled_config());

        let updated = service
            .set_report_active(report_id, false)
            .await
            .unwrap()
            .expect("known report should update");
        let missing = service
            .set_report_active(Uuid::new_v4(), false)
            .await
            .unwrap();

        assert!(!updated.active);
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn allowlist_upsert_normalizes_and_remove_deletes_entry() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Low)));
        let service = AmlService::new(repo.clone(), provider, enabled_config());
        let user_id = Uuid::new_v4();

        let created = service
            .upsert_allowlist_entry(
                "  Alice.NEAR  ",
                Some("Compliance approval reference".to_string()),
                Some(user_id),
            )
            .await
            .unwrap();
        let updated = service
            .upsert_allowlist_entry("alice.near", Some("Updated".to_string()), Some(user_id))
            .await
            .unwrap();
        let removed = service.remove_allowlist_entry("ALICE.near").await.unwrap();
        let removed_again = service.remove_allowlist_entry("alice.near").await.unwrap();

        assert_eq!(created.account_id, "alice.near");
        assert_eq!(updated.reason.as_deref(), Some("Updated"));
        assert!(removed);
        assert!(!removed_again);
        assert!(repo.allowlist_entries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allowlist_upsert_rejects_invalid_near_account_id() {
        let repo = Arc::new(MockAmlRepository::default());
        let provider = Arc::new(StaticProvider(new_report(AmlRiskLevel::Low)));
        let service = AmlService::new(repo, provider, enabled_config());

        let result = service
            .upsert_allowlist_entry("not valid", None, Some(Uuid::new_v4()))
            .await;

        assert!(matches!(result, Err(AmlError::InvalidAccountId)));
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
