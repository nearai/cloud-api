mod redis;

use crate::{
    metrics::MetricsServiceTrait,
    usage::{OrganizationLimit, UsageCheckResult, UsageError},
};
use async_trait::async_trait;
pub use redis::RedisAdmissionCache;
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationAdmissionSnapshot {
    pub organization_id: Uuid,
    pub revision: i64,
    pub total_spent: Option<i64>,
    pub limit: Option<OrganizationLimit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyAdmissionSnapshot {
    pub organization_id: Uuid,
    pub api_key_id: Uuid,
    pub revision: i64,
    pub spend_limit: Option<i64>,
    pub inference_spent: i64,
}

#[async_trait]
pub trait AdmissionSnapshotRepository: Send + Sync {
    async fn load_organization(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<OrganizationAdmissionSnapshot>;
    async fn load_key(
        &self,
        organization_id: Uuid,
        api_key_id: Uuid,
    ) -> anyhow::Result<KeyAdmissionSnapshot>;
}

#[async_trait]
pub trait AdmissionCache: Send + Sync {
    async fn server_time_ms(&self) -> anyhow::Result<i64>;
    async fn get_organization(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationAdmissionSnapshot>>;
    async fn get_key(
        &self,
        organization_id: Uuid,
        api_key_id: Uuid,
    ) -> anyhow::Result<Option<KeyAdmissionSnapshot>>;
    async fn put_organization(
        &self,
        snapshot: &OrganizationAdmissionSnapshot,
        started_at_ms: i64,
    ) -> anyhow::Result<bool>;
    async fn put_key(
        &self,
        snapshot: &KeyAdmissionSnapshot,
        started_at_ms: i64,
    ) -> anyhow::Result<bool>;
}

/// One shared owner for cache policy and the per-process primary-read budget.
pub struct AdmissionCoordinator {
    repository: Arc<dyn AdmissionSnapshotRepository>,
    cache: Option<Arc<dyn AdmissionCache>>,
    config: config::AdmissionCacheConfig,
    metrics: Arc<dyn MetricsServiceTrait>,
    fallback_slots: Semaphore,
}

impl AdmissionCoordinator {
    pub fn new(
        repository: Arc<dyn AdmissionSnapshotRepository>,
        cache: Option<Arc<dyn AdmissionCache>>,
        config: config::AdmissionCacheConfig,
        metrics: Arc<dyn MetricsServiceTrait>,
    ) -> Self {
        Self {
            fallback_slots: Semaphore::new(config.fallback_concurrency),
            repository,
            cache,
            config,
            metrics,
        }
    }

    pub async fn check_organization(&self, id: Uuid) -> Result<UsageCheckResult, UsageError> {
        let start = Instant::now();
        let result = self
            .load_organization(id)
            .await
            .map(|value| evaluate_organization(&value));
        self.metrics.record_latency(
            "admission_check_latency",
            start.elapsed(),
            &["subject:organization"],
        );
        result
    }

    pub async fn key_snapshot(
        &self,
        org: Uuid,
        key: Uuid,
    ) -> Result<KeyAdmissionSnapshot, UsageError> {
        let start = Instant::now();
        let result = self.load_key(org, key).await;
        self.metrics
            .record_latency("admission_check_latency", start.elapsed(), &["subject:key"]);
        result
    }

    async fn load_organization(
        &self,
        id: Uuid,
    ) -> Result<OrganizationAdmissionSnapshot, UsageError> {
        if let Some(cache) = &self.cache {
            match self.cache_call(cache.get_organization(id)).await {
                Ok(Some(snapshot)) if snapshot.organization_id == id && snapshot.revision >= 0 => {
                    self.count("hit");
                    return Ok(snapshot);
                }
                Err(_) | Ok(Some(_)) => {
                    self.count("error");
                    return self
                        .read_primary(self.repository.load_organization(id))
                        .await;
                }
                Ok(None) => self.count("miss"),
            }
            let started = self.cache_call(cache.server_time_ms()).await;
            let snapshot = self
                .read_primary(self.repository.load_organization(id))
                .await?;
            match started {
                Ok(started) => self.record_put(
                    self.cache_call(cache.put_organization(&snapshot, started))
                        .await,
                    false,
                ),
                Err(_) => self.count("fill_error"),
            }
            return Ok(snapshot);
        }
        self.read_primary(self.repository.load_organization(id))
            .await
    }

    async fn load_key(&self, org: Uuid, key: Uuid) -> Result<KeyAdmissionSnapshot, UsageError> {
        if let Some(cache) = &self.cache {
            match self.cache_call(cache.get_key(org, key)).await {
                Ok(Some(snapshot))
                    if snapshot.organization_id == org
                        && snapshot.api_key_id == key
                        && snapshot.revision >= 0 =>
                {
                    self.count("hit");
                    return Ok(snapshot);
                }
                Err(_) | Ok(Some(_)) => {
                    self.count("error");
                    return self.read_primary(self.repository.load_key(org, key)).await;
                }
                Ok(None) => self.count("miss"),
            }
            let started = self.cache_call(cache.server_time_ms()).await;
            let snapshot = self
                .read_primary(self.repository.load_key(org, key))
                .await?;
            match started {
                Ok(started) => self.record_put(
                    self.cache_call(cache.put_key(&snapshot, started)).await,
                    false,
                ),
                Err(_) => self.count("fill_error"),
            }
            return Ok(snapshot);
        }
        self.read_primary(self.repository.load_key(org, key)).await
    }

    pub async fn refresh_organization(&self, id: Uuid) {
        let Some(cache) = &self.cache else { return };
        let result = tokio::time::timeout(self.read_budget(), async {
            let started = self.cache_call(cache.server_time_ms()).await?;
            let snapshot = self
                .read_primary(self.repository.load_organization(id))
                .await?;
            self.cache_call(cache.put_organization(&snapshot, started))
                .await
        })
        .await;
        self.record_put(
            result.unwrap_or_else(|_| Err(anyhow::anyhow!("admission refresh timed out"))),
            true,
        );
    }

    pub async fn refresh_key(&self, org: Uuid, key: Uuid) {
        let Some(cache) = &self.cache else { return };
        let result = tokio::time::timeout(self.read_budget(), async {
            let started = self.cache_call(cache.server_time_ms()).await?;
            let snapshot = self
                .read_primary(self.repository.load_key(org, key))
                .await?;
            self.cache_call(cache.put_key(&snapshot, started)).await
        })
        .await;
        self.record_put(
            result.unwrap_or_else(|_| Err(anyhow::anyhow!("admission refresh timed out"))),
            true,
        );
    }

    pub async fn refresh_after_usage(&self, org: Uuid, key: Uuid) {
        // Each refresh has the same total deadline and shares the primary-read gate.
        tokio::join!(self.refresh_organization(org), self.refresh_key(org, key));
    }

    async fn read_primary<T>(
        &self,
        read: impl Future<Output = anyhow::Result<T>>,
    ) -> Result<T, UsageError> {
        let start = Instant::now();
        let result = tokio::time::timeout(self.read_budget(), async {
            let _permit = self
                .fallback_slots
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("admission read gate closed"))?;
            read.await
        })
        .await;
        self.metrics
            .record_latency("admission_primary_latency", start.elapsed(), &[]);
        match result {
            Ok(Ok(value)) => {
                self.count("primary_read");
                Ok(value)
            }
            Ok(Err(error)) => {
                self.count("primary_error");
                Err(UsageError::InternalError(format!(
                    "failed to load admission snapshot: {error:#}"
                )))
            }
            Err(_) => {
                self.count("primary_timeout");
                Err(UsageError::InternalError(
                    "admission primary read timed out".into(),
                ))
            }
        }
    }

    async fn cache_call<T>(
        &self,
        operation: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        tokio::time::timeout(
            Duration::from_millis(self.config.command_deadline_ms),
            operation,
        )
        .await
        .map_err(|_| anyhow::anyhow!("admission cache deadline exceeded"))?
    }

    fn read_budget(&self) -> Duration {
        Duration::from_millis(self.config.fallback_deadline_ms)
    }
    fn record_put(&self, result: anyhow::Result<bool>, refresh: bool) {
        self.count(match (refresh, result) {
            (true, Ok(true)) => "refresh",
            (true, Ok(false)) => "refresh_rejected",
            (true, Err(_)) => "refresh_error",
            (false, Ok(true)) => "fill",
            (false, Ok(false)) => "fill_rejected",
            (false, Err(_)) => "fill_error",
        });
    }
    fn count(&self, outcome: &'static str) {
        self.metrics.record_count(
            "admission_cache_operations",
            1,
            &[&format!("outcome:{outcome}")],
        );
    }
}

fn evaluate_organization(snapshot: &OrganizationAdmissionSnapshot) -> UsageCheckResult {
    match (snapshot.total_spent, snapshot.limit.as_ref()) {
        (Some(spent), Some(limit)) if limit.unfunded > 0 || limit.available == 0 => {
            UsageCheckResult::LimitExceeded {
                spent,
                limit: limit.spend_limit,
            }
        }
        (Some(_), Some(limit)) => UsageCheckResult::Allowed {
            remaining: limit.available,
        },
        (Some(_), None) => UsageCheckResult::NoLimitSet,
        (None, Some(limit)) if limit.unfunded == 0 && limit.available > 0 => {
            UsageCheckResult::Allowed {
                remaining: limit.available,
            }
        }
        (None, Some(_)) | (None, None) => UsageCheckResult::NoCredits,
    }
}

#[cfg(test)]
impl AdmissionCoordinator {
    pub fn new_for_tests() -> Self {
        struct UnusedRepository;
        #[async_trait]
        impl AdmissionSnapshotRepository for UnusedRepository {
            async fn load_organization(
                &self,
                _: Uuid,
            ) -> anyhow::Result<OrganizationAdmissionSnapshot> {
                anyhow::bail!("unexpected admission read in fixture")
            }
            async fn load_key(&self, _: Uuid, _: Uuid) -> anyhow::Result<KeyAdmissionSnapshot> {
                anyhow::bail!("unexpected admission read in fixture")
            }
        }
        Self::new(
            Arc::new(UnusedRepository),
            None,
            config::AdmissionCacheConfig::default(),
            Arc::new(crate::metrics::MockMetricsService),
        )
    }
}

#[cfg(test)]
mod coordinator_tests;
