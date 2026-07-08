use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use config::KytConfig;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use utoipa::ToSchema;

const ADDRESS_TYPE_NEAR: &str = "NEAR";
const PROVIDER_LUKKA: &str = "lukka";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KytRiskLevel {
    Low,
    Medium,
    High,
    Unknown,
}

impl KytRiskLevel {
    fn from_provider(value: Option<&str>) -> Self {
        match value.unwrap_or_default().to_ascii_uppercase().as_str() {
            "LOW" => Self::Low,
            "MEDIUM" => Self::Medium,
            "HIGH" => Self::High,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum KytRiskStatus {
    Completed,
    Unavailable,
    Disabled,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KytRisk {
    pub provider: String,
    pub level: KytRiskLevel,
    pub score: Option<i64>,
    pub report_id: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: KytRiskStatus,
    pub error_category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KytCheckResponse {
    pub account_id: String,
    pub address_type: String,
    pub risk: KytRisk,
    pub warning_required: bool,
}

#[derive(Debug, Clone)]
pub struct KytProviderResult {
    pub risk_level: KytRiskLevel,
    pub score: Option<i64>,
    pub report_id: Option<String>,
    pub checked_at: DateTime<Utc>,
}

#[async_trait]
pub trait KytProvider: Send + Sync {
    async fn score_near_account(&self, near_account_id: &str) -> Result<KytProviderResult>;
}

#[derive(Clone)]
pub struct LukkaKytClient {
    client: reqwest::Client,
    base_url: String,
    bearer_token: String,
    retries: u32,
}

impl LukkaKytClient {
    pub fn new(config: &KytConfig) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_seconds.max(1));
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .context("failed to build Lukka KYT HTTP client")?,
            base_url: config.lukka_base_url.clone(),
            bearer_token: config
                .lukka_bearer_token
                .clone()
                .context("Lukka KYT bearer token is required when KYT is enabled")?,
            retries: config.retries,
        })
    }

    pub fn score_url(base_url: &str, near_account_id: &str) -> Result<Url> {
        let mut url =
            Url::parse(base_url).with_context(|| format!("invalid Lukka base URL: {base_url}"))?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Lukka base URL cannot be a base"))?;
            segments.pop_if_empty();
            segments.extend(["v3", "reports", "aml", "score", near_account_id]);
        }
        url.query_pairs_mut()
            .append_pair("address_type", ADDRESS_TYPE_NEAR);
        Ok(url)
    }
}

#[async_trait]
impl KytProvider for LukkaKytClient {
    async fn score_near_account(&self, near_account_id: &str) -> Result<KytProviderResult> {
        let url = Self::score_url(&self.base_url, near_account_id)?;
        let mut last_error = None;
        for attempt in 0..=self.retries {
            let started = std::time::Instant::now();
            let response = self
                .client
                .get(url.clone())
                .bearer_auth(&self.bearer_token)
                .send()
                .await;

            match response {
                Ok(response) if response.status().is_success() => {
                    let provider_response = response
                        .json::<LukkaAmlScoreResponse>()
                        .await
                        .context("failed to decode Lukka AML score response")?;
                    debug!(
                        provider = PROVIDER_LUKKA,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "KYT provider score completed"
                    );
                    return Ok(provider_response.into_result());
                }
                Ok(response) => {
                    last_error = Some(anyhow::anyhow!(
                        "Lukka AML score request failed with status {}",
                        response.status()
                    ));
                }
                Err(error) => {
                    last_error = Some(error.into());
                }
            }

            warn!(
                provider = PROVIDER_LUKKA,
                attempt,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "KYT provider score attempt failed"
            );
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Lukka AML score request failed")))
    }
}

#[derive(Debug, Deserialize)]
struct LukkaAmlScoreResponse {
    report_info_section: Option<LukkaReportInfoSection>,
    cscore_section: Option<LukkaCscoreSection>,
}

#[derive(Debug, Deserialize)]
struct LukkaReportInfoSection {
    report_id: Option<String>,
    report_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct LukkaCscoreSection {
    cscore: Option<i64>,
    risk_level: Option<String>,
}

impl LukkaAmlScoreResponse {
    fn into_result(self) -> KytProviderResult {
        let report_info = self.report_info_section;
        let cscore = self.cscore_section;
        KytProviderResult {
            risk_level: KytRiskLevel::from_provider(
                cscore
                    .as_ref()
                    .and_then(|section| section.risk_level.as_deref()),
            ),
            score: cscore.and_then(|section| section.cscore),
            report_id: report_info
                .as_ref()
                .and_then(|section| section.report_id.clone()),
            checked_at: report_info
                .and_then(|section| section.report_time)
                .unwrap_or_else(Utc::now),
        }
    }
}

#[derive(Clone)]
pub struct KytService {
    config: KytConfig,
    provider: Arc<dyn KytProvider>,
    cache: Arc<RwLock<HashMap<KytCacheKey, KytCheckResponse>>>,
}

impl KytService {
    pub fn new(config: KytConfig, provider: Arc<dyn KytProvider>) -> Self {
        Self {
            config,
            provider,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_lukka_provider(config: KytConfig) -> Result<Self> {
        let provider = if config.enabled {
            Arc::new(LukkaKytClient::new(&config)?) as Arc<dyn KytProvider>
        } else {
            Arc::new(DisabledKytProvider) as Arc<dyn KytProvider>
        };
        Ok(Self::new(config, provider))
    }

    pub async fn check_near_account(
        &self,
        network_id: &str,
        near_account_id: &str,
    ) -> KytCheckResponse {
        if !self.config.enabled {
            return self.disabled_response(near_account_id);
        }

        let now = Utc::now();
        let key = KytCacheKey {
            network_id: network_id.to_string(),
            near_account_id: near_account_id.to_string(),
            provider: self.config.provider.clone(),
        };
        if let Some(cached) = self.cache.read().await.get(&key) {
            if cached
                .risk
                .expires_at
                .is_some_and(|expires_at| expires_at > now)
            {
                debug!(
                    provider = %cached.risk.provider,
                    near_account_id,
                    network_id,
                    risk_level = ?cached.risk.level,
                    "KYT cache hit"
                );
                return cached.clone();
            }
        }

        let response = match self.provider.score_near_account(near_account_id).await {
            Ok(result) => {
                let expires_at =
                    result.checked_at + ChronoDuration::seconds(self.config.cache_ttl_seconds);
                let response = Self::completed_response(near_account_id, result, expires_at);
                if response.warning_required {
                    info!(
                        provider = PROVIDER_LUKKA,
                        near_account_id, network_id, "KYT high-risk wallet detected"
                    );
                }
                response
            }
            Err(error) => {
                warn!(
                    provider = PROVIDER_LUKKA,
                    near_account_id,
                    network_id,
                    error = %error,
                    "KYT provider unavailable"
                );
                self.unavailable_response(near_account_id)
            }
        };

        self.cache.write().await.insert(key, response.clone());
        response
    }

    fn completed_response(
        near_account_id: &str,
        result: KytProviderResult,
        expires_at: DateTime<Utc>,
    ) -> KytCheckResponse {
        KytCheckResponse {
            account_id: near_account_id.to_string(),
            address_type: ADDRESS_TYPE_NEAR.to_string(),
            warning_required: result.risk_level == KytRiskLevel::High,
            risk: KytRisk {
                provider: PROVIDER_LUKKA.to_string(),
                level: result.risk_level,
                score: result.score,
                report_id: result.report_id,
                checked_at: Some(result.checked_at),
                expires_at: Some(expires_at),
                status: KytRiskStatus::Completed,
                error_category: None,
            },
        }
    }

    fn disabled_response(&self, near_account_id: &str) -> KytCheckResponse {
        KytCheckResponse {
            account_id: near_account_id.to_string(),
            address_type: ADDRESS_TYPE_NEAR.to_string(),
            warning_required: false,
            risk: KytRisk {
                provider: self.config.provider.clone(),
                level: KytRiskLevel::Unknown,
                score: None,
                report_id: None,
                checked_at: None,
                expires_at: None,
                status: KytRiskStatus::Disabled,
                error_category: None,
            },
        }
    }

    fn unavailable_response(&self, near_account_id: &str) -> KytCheckResponse {
        let checked_at = Utc::now();
        KytCheckResponse {
            account_id: near_account_id.to_string(),
            address_type: ADDRESS_TYPE_NEAR.to_string(),
            warning_required: false,
            risk: KytRisk {
                provider: self.config.provider.clone(),
                level: KytRiskLevel::Unknown,
                score: None,
                report_id: None,
                checked_at: Some(checked_at),
                expires_at: Some(
                    checked_at + ChronoDuration::seconds(self.config.cache_ttl_seconds),
                ),
                status: KytRiskStatus::Unavailable,
                error_category: Some("provider_error".to_string()),
            },
        }
    }
}

struct DisabledKytProvider;

#[async_trait]
impl KytProvider for DisabledKytProvider {
    async fn score_near_account(&self, _near_account_id: &str) -> Result<KytProviderResult> {
        Err(anyhow::anyhow!("KYT provider is disabled"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct KytCacheKey {
    network_id: String,
    near_account_id: String,
    provider: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct MockKytProvider {
        result: Result<KytProviderResult, String>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl KytProvider for MockKytProvider {
        async fn score_near_account(&self, _near_account_id: &str) -> Result<KytProviderResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone().map_err(anyhow::Error::msg)
        }
    }

    fn enabled_config() -> KytConfig {
        KytConfig {
            enabled: true,
            provider: PROVIDER_LUKKA.to_string(),
            lukka_base_url: "https://api.blockchain-analytics.lukka.tech".to_string(),
            lukka_bearer_token: Some("secret".to_string()),
            timeout_seconds: 1,
            retries: 0,
            cache_ttl_seconds: 3600,
        }
    }

    fn provider_result(level: KytRiskLevel) -> KytProviderResult {
        KytProviderResult {
            risk_level: level,
            score: Some(if level == KytRiskLevel::High { 99 } else { 20 }),
            report_id: Some(
                "4512815d6784a68a7101c72c8e0435e49c1652f6a9295639229bc980bc51dd49".to_string(),
            ),
            checked_at: Utc::now(),
        }
    }

    #[test]
    fn lukka_score_url_uses_near_address_type() {
        let url =
            LukkaKytClient::score_url("https://api.blockchain-analytics.lukka.tech", "alice.near")
                .unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.blockchain-analytics.lukka.tech/v3/reports/aml/score/alice.near?address_type=NEAR"
        );
    }

    #[test]
    fn lukka_high_risk_fixture_maps_to_normalized_result() {
        let fixture = serde_json::json!({
            "report_info_section": {
                "report_id": "4512815d6784a68a7101c72c8e0435e49c1652f6a9295639229bc980bc51dd49",
                "report_time": "2026-07-08T19:08:43.545Z"
            },
            "cscore_section": {
                "cscore": 99,
                "risk_level": "HIGH"
            }
        });
        let result = serde_json::from_value::<LukkaAmlScoreResponse>(fixture)
            .unwrap()
            .into_result();

        assert_eq!(result.risk_level, KytRiskLevel::High);
        assert_eq!(result.score, Some(99));
        assert_eq!(
            result.report_id.as_deref(),
            Some("4512815d6784a68a7101c72c8e0435e49c1652f6a9295639229bc980bc51dd49")
        );
    }

    #[test]
    fn provider_risk_values_map_to_cloud_enums() {
        assert_eq!(KytRiskLevel::from_provider(Some("LOW")), KytRiskLevel::Low);
        assert_eq!(
            KytRiskLevel::from_provider(Some("medium")),
            KytRiskLevel::Medium
        );
        assert_eq!(
            KytRiskLevel::from_provider(Some("HIGH")),
            KytRiskLevel::High
        );
        assert_eq!(
            KytRiskLevel::from_provider(Some("unexpected")),
            KytRiskLevel::Unknown
        );
    }

    #[tokio::test]
    async fn high_risk_sets_warning_required() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = KytService::new(
            enabled_config(),
            Arc::new(MockKytProvider {
                result: Ok(provider_result(KytRiskLevel::High)),
                calls,
            }),
        );

        let response = service
            .check_near_account("testnet", "gregoshes.near")
            .await;

        assert_eq!(response.risk.level, KytRiskLevel::High);
        assert!(response.warning_required);
    }

    #[tokio::test]
    async fn non_high_risk_does_not_set_warning_required() {
        for level in [
            KytRiskLevel::Low,
            KytRiskLevel::Medium,
            KytRiskLevel::Unknown,
        ] {
            let service = KytService::new(
                enabled_config(),
                Arc::new(MockKytProvider {
                    result: Ok(provider_result(level)),
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            );

            let response = service.check_near_account("testnet", "alice.near").await;
            assert!(!response.warning_required);
        }
    }

    #[tokio::test]
    async fn provider_failure_maps_to_unknown_unavailable() {
        let service = KytService::new(
            enabled_config(),
            Arc::new(MockKytProvider {
                result: Err("timeout".to_string()),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );

        let response = service.check_near_account("testnet", "alice.near").await;

        assert_eq!(response.risk.level, KytRiskLevel::Unknown);
        assert_eq!(response.risk.status, KytRiskStatus::Unavailable);
        assert_eq!(
            response.risk.error_category.as_deref(),
            Some("provider_error")
        );
        assert!(!response.warning_required);
    }

    #[tokio::test]
    async fn disabled_feature_does_not_call_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut config = enabled_config();
        config.enabled = false;
        let service = KytService::new(
            config,
            Arc::new(MockKytProvider {
                result: Ok(provider_result(KytRiskLevel::High)),
                calls: calls.clone(),
            }),
        );

        let response = service
            .check_near_account("testnet", "gregoshes.near")
            .await;

        assert_eq!(response.risk.status, KytRiskStatus::Disabled);
        assert!(!response.warning_required);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cached_response_reuses_provider_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = KytService::new(
            enabled_config(),
            Arc::new(MockKytProvider {
                result: Ok(provider_result(KytRiskLevel::High)),
                calls: calls.clone(),
            }),
        );

        let first = service
            .check_near_account("testnet", "gregoshes.near")
            .await;
        let second = service
            .check_near_account("testnet", "gregoshes.near")
            .await;

        assert!(first.warning_required);
        assert!(second.warning_required);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
