use std::{fmt, time::Duration};

use async_trait::async_trait;
use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    Client, Script,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{AdmissionCache, KeyAdmissionSnapshot, OrganizationAdmissionSnapshot};

const ORG_PREFIX: &str = "admission:v1:org:";
const SCHEMA: u8 = 1;
const PUT_SCRIPT: &str = r#"
local current = redis.call('HGET', KEYS[1], 'revision')
local incoming = ARGV[1]
local function greater(a, b)
  if #a ~= #b then return #a > #b end
  for i = 1, #a do
    local x, y = string.byte(a, i), string.byte(b, i)
    if x ~= y then return x > y end
  end
  return false
end
if current and #current == 20 and string.match(current, '^%d+$') and greater(current, incoming) then return 0 end
local clock = redis.call('TIME')
local now = tonumber(clock[1]) * 1000 + math.floor(tonumber(clock[2]) / 1000)
if now < tonumber(ARGV[4]) then return 0 end
if now - tonumber(ARGV[4]) > tonumber(ARGV[5]) then return 0 end
redis.call('HSET', KEYS[1], 'revision', incoming, 'payload', ARGV[2])
redis.call('PEXPIREAT', KEYS[1], ARGV[3])
return 1
"#;

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    schema: u8,
    organization_id: Uuid,
    api_key_id: Option<Uuid>,
    revision: String,
    expires_at_ms: i64,
    filled_at_ms: i64,
    snapshot: T,
}

pub struct RedisAdmissionCache {
    client: Client,
    config: config::AdmissionCacheConfig,
    connection: OnceCell<ConnectionManager>,
}

impl fmt::Debug for RedisAdmissionCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisAdmissionCache")
            .field("url", &"<redacted>")
            .field("config", &self.config)
            .finish()
    }
}

impl RedisAdmissionCache {
    pub fn new(config: &config::AdmissionCacheConfig) -> anyhow::Result<Self> {
        config.validate().map_err(anyhow::Error::msg)?;
        let url = config
            .redis_url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("admission cache is disabled"))?;
        let client = Client::open(url.as_str())
            .map_err(|_| anyhow::anyhow!("invalid admission cache endpoint"))?;
        Ok(Self {
            client,
            config: config.clone(),
            connection: OnceCell::new(),
        })
    }

    async fn connection(&self) -> anyhow::Result<ConnectionManager> {
        let connection = tokio::time::timeout(
            Duration::from_millis(self.config.command_deadline_ms),
            self.connection.get_or_try_init(|| async {
                let deadline = Duration::from_millis(self.config.command_deadline_ms);
                ConnectionManager::new_with_config(
                    self.client.clone(),
                    ConnectionManagerConfig::new()
                        .set_connection_timeout(deadline)
                        .set_response_timeout(deadline)
                        .set_number_of_retries(1),
                )
                .await
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("admission cache connection timed out"))?
        .map_err(|_| anyhow::anyhow!("admission cache connection failed"))?;
        Ok(connection.clone())
    }

    async fn command<T, F>(&self, operation: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = redis::RedisResult<T>>,
    {
        tokio::time::timeout(
            Duration::from_millis(self.config.command_deadline_ms),
            operation,
        )
        .await
        .map_err(|_| anyhow::anyhow!("admission cache command timed out"))?
        .map_err(|_| anyhow::anyhow!("admission cache command failed"))
    }

    fn key_org(id: Uuid) -> String {
        format!("{ORG_PREFIX}{{{id}}}")
    }
    fn key_api(org: Uuid, key: Uuid) -> String {
        format!("{ORG_PREFIX}{{{org}}}:key:{key}")
    }
    fn expiration(&self, started: i64) -> anyhow::Result<i64> {
        let jitter = if self.config.ttl_jitter_seconds == 0 {
            0
        } else {
            started.unsigned_abs() % (self.config.ttl_jitter_seconds + 1)
        };
        started
            .checked_add((self.config.ttl_seconds - jitter) as i64 * 1000)
            .ok_or_else(|| anyhow::anyhow!("admission cache expiration overflow"))
    }

    async fn get<T: DeserializeOwned>(&self, key: String) -> anyhow::Result<Option<Envelope<T>>> {
        let mut connection = self.connection().await?;
        let payload: Option<String> = self
            .command(
                redis::cmd("HGET")
                    .arg(key)
                    .arg("payload")
                    .query_async(&mut connection),
            )
            .await?;
        payload
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| anyhow::anyhow!("invalid admission cache payload"))
            })
            .transpose()
    }

    async fn put<T: Serialize>(
        &self,
        key: String,
        snapshot: &T,
        org: Uuid,
        key_id: Option<Uuid>,
        revision: i64,
        started: i64,
    ) -> anyhow::Result<bool> {
        if revision < 0 {
            return Ok(false);
        }
        let expires = self.expiration(started)?;
        let envelope = Envelope {
            schema: SCHEMA,
            organization_id: org,
            api_key_id: key_id,
            revision: format!("{revision:020}"),
            expires_at_ms: expires,
            filled_at_ms: started,
            snapshot,
        };
        let payload = serde_json::to_string(&envelope)?;
        let mut connection = self.connection().await?;
        let result: i32 = self
            .command(
                Script::new(PUT_SCRIPT)
                    .key(key)
                    .arg(format!("{revision:020}"))
                    .arg(payload)
                    .arg(expires)
                    .arg(started)
                    .arg(self.config.max_fill_age_ms)
                    .invoke_async(&mut connection),
            )
            .await?;
        Ok(result == 1)
    }

    async fn server_time_ms(&self) -> anyhow::Result<i64> {
        let mut connection = self.connection().await?;
        let parts: Vec<String> = self
            .command(redis::cmd("TIME").query_async(&mut connection))
            .await?;
        let seconds: i64 = parts
            .first()
            .ok_or_else(|| anyhow::anyhow!("invalid admission cache time"))?
            .parse()?;
        let micros: i64 = parts
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("invalid admission cache time"))?
            .parse()?;
        Ok(seconds.saturating_mul(1000).saturating_add(micros / 1000))
    }

    fn valid<T>(envelope: Envelope<T>, org: Uuid, key: Option<Uuid>) -> anyhow::Result<Option<T>> {
        if envelope.schema != SCHEMA
            || envelope.organization_id != org
            || envelope.api_key_id != key
            || envelope
                .revision
                .parse::<i64>()
                .ok()
                .filter(|revision| *revision >= 0)
                .is_none()
        {
            return Ok(None);
        }
        Ok(Some(envelope.snapshot))
    }
}

#[async_trait]
impl AdmissionCache for RedisAdmissionCache {
    async fn server_time_ms(&self) -> anyhow::Result<i64> {
        RedisAdmissionCache::server_time_ms(self).await
    }
    async fn get_organization(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationAdmissionSnapshot>> {
        let Some(envelope) = self
            .get::<OrganizationAdmissionSnapshot>(Self::key_org(organization_id))
            .await?
        else {
            return Ok(None);
        };
        let revision = envelope.revision.parse::<i64>().ok();
        let snapshot = Self::valid(envelope, organization_id, None)?;
        Ok(snapshot.filter(|snapshot| {
            snapshot.organization_id == organization_id
                && snapshot.revision >= 0
                && Some(snapshot.revision) == revision
        }))
    }
    async fn get_key(
        &self,
        organization_id: Uuid,
        api_key_id: Uuid,
    ) -> anyhow::Result<Option<KeyAdmissionSnapshot>> {
        let Some(envelope) = self
            .get::<KeyAdmissionSnapshot>(Self::key_api(organization_id, api_key_id))
            .await?
        else {
            return Ok(None);
        };
        let revision = envelope.revision.parse::<i64>().ok();
        let snapshot = Self::valid(envelope, organization_id, Some(api_key_id))?;
        Ok(snapshot.filter(|snapshot| {
            snapshot.organization_id == organization_id
                && snapshot.api_key_id == api_key_id
                && snapshot.revision >= 0
                && Some(snapshot.revision) == revision
        }))
    }
    async fn put_organization(
        &self,
        snapshot: &OrganizationAdmissionSnapshot,
        started_at_ms: i64,
    ) -> anyhow::Result<bool> {
        self.put(
            Self::key_org(snapshot.organization_id),
            snapshot,
            snapshot.organization_id,
            None,
            snapshot.revision,
            started_at_ms,
        )
        .await
    }
    async fn put_key(
        &self,
        snapshot: &KeyAdmissionSnapshot,
        started_at_ms: i64,
    ) -> anyhow::Result<bool> {
        self.put(
            Self::key_api(snapshot.organization_id, snapshot.api_key_id),
            snapshot,
            snapshot.organization_id,
            Some(snapshot.api_key_id),
            snapshot.revision,
            started_at_ms,
        )
        .await
    }
}

#[cfg(test)]
#[path = "redis_tests.rs"]
mod tests;
