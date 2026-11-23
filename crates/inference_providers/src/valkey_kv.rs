//! A minimal key-value client over the placement Valkey endpoint, for caches
//! that live outside inference routing: `GET` and `EVAL`.
//!
//! It reuses the placement client construction (`ValkeyEndpoint`, the pinned
//! CA, [`install_crypto_provider`]) and never connects on the caller's path:
//! [`ValkeyKv::start`] returns at once and a background task connects with
//! backoff. Until it has connected, every call fails fast with
//! [`KvErrorKind::Conn`], so a caller that fails open pays nothing for an
//! unreachable server.
//!
//! No redis type leaves this module: replies are [`KvReply`], failures are
//! [`KvErrorKind`].
//!
//! Privacy: never log the password, the URL, a redis error's `Display` (it can
//! echo the URL), keys or values. Only [`KvErrorKind`]s leave this module.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use crate::placement_io::{endpoint_client, install_crypto_provider, ValkeyEndpoint};

/// Why a Valkey call failed. A closed set so callers can count it as a metric
/// tag without ever touching the redis error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvErrorKind {
    /// The server did not answer in time.
    Timeout,
    /// The ACL forbids the command or key (`NOPERM`).
    NoPerm,
    /// Not connected, or the connection dropped or was refused.
    Conn,
    /// Anything else (protocol, parse, unexpected server error).
    Other,
}

impl KvErrorKind {
    /// Metric tag value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::NoPerm => "noperm",
            Self::Conn => "conn",
            Self::Other => "other",
        }
    }

    fn from_redis(e: &redis::RedisError) -> Self {
        // A script touching a key the user may not access fails as
        // `ERR ACL failure in script: ...`, not `NOPERM`; it is the same ACL
        // refusal. The text is only compared, never logged.
        let acl_in_script = e
            .detail()
            .is_some_and(|d| d.starts_with("ACL failure in script"));
        if e.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm) || acl_in_script {
            Self::NoPerm
        } else if e.is_timeout() {
            Self::Timeout
        } else if e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() {
            Self::Conn
        } else {
            Self::Other
        }
    }
}

/// The reply of an `EVAL`, reduced to what the callers use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvReply {
    Nil,
    Int(i64),
    Str(String),
}

impl KvReply {
    fn from_value(value: redis::Value) -> Result<Self, KvErrorKind> {
        match value {
            redis::Value::Nil => Ok(Self::Nil),
            redis::Value::Int(n) => Ok(Self::Int(n)),
            redis::Value::BulkString(bytes) => String::from_utf8(bytes)
                .map(Self::Str)
                .map_err(|_| KvErrorKind::Other),
            redis::Value::SimpleString(s) => Ok(Self::Str(s)),
            redis::Value::Okay => Ok(Self::Str("OK".to_string())),
            _ => Err(KvErrorKind::Other),
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_RETRY_MIN: Duration = Duration::from_secs(1);
const CONNECT_RETRY_MAX: Duration = Duration::from_secs(30);
const WARN_EVERY: Duration = Duration::from_secs(60);

/// A lazily connected Valkey client for `GET` and `EVAL`.
pub struct ValkeyKv {
    conn: Arc<ArcSwapOption<ConnectionManager>>,
}

impl ValkeyKv {
    /// Builds the client for `endpoint` (authenticating as the endpoint's ACL
    /// user with `password`) and starts connecting in the background.
    ///
    /// Errors carry only a kind, never the host or password. Needs a running
    /// tokio runtime.
    pub fn start(endpoint: &ValkeyEndpoint, password: String) -> Result<Self, String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| "no_runtime".to_string())?;
        install_crypto_provider();
        let client = endpoint_client(endpoint, password)?;
        let conn: Arc<ArcSwapOption<ConnectionManager>> = Arc::new(ArcSwapOption::empty());
        handle.spawn(connect_task(client, Arc::downgrade(&conn)));
        Ok(Self { conn })
    }

    fn connection(&self) -> Result<ConnectionManager, KvErrorKind> {
        self.conn
            .load_full()
            .map(|c| (*c).clone())
            .ok_or(KvErrorKind::Conn)
    }

    /// `GET key`. The value must be UTF-8 or the call fails as `Other`.
    pub async fn get(&self, key: &str) -> Result<Option<String>, KvErrorKind> {
        let mut conn = self.connection()?;
        redis::cmd("GET")
            .arg(key)
            .query_async::<Option<String>>(&mut conn)
            .await
            .map_err(|e| KvErrorKind::from_redis(&e))
    }

    /// `EVAL script len(keys) keys.. args..`. Always plain `EVAL`, never
    /// `EVALSHA`: the ACL user cannot load scripts.
    pub async fn eval(
        &self,
        script: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<KvReply, KvErrorKind> {
        let mut conn = self.connection()?;
        let mut cmd = redis::cmd("EVAL");
        cmd.arg(script).arg(keys.len());
        for key in keys {
            cmd.arg(key.as_str());
        }
        for arg in args {
            cmd.arg(arg.as_str());
        }
        let value = cmd
            .query_async::<redis::Value>(&mut conn)
            .await
            .map_err(|e| KvErrorKind::from_redis(&e))?;
        KvReply::from_value(value)
    }
}

/// Connects with backoff and publishes the connection; stops once the owning
/// [`ValkeyKv`] is dropped.
async fn connect_task(client: redis::Client, slot: Weak<ArcSwapOption<ConnectionManager>>) {
    let config = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(CONNECT_TIMEOUT))
        .set_response_timeout(Some(RESPONSE_TIMEOUT))
        .set_number_of_retries(0);
    let mut delay = CONNECT_RETRY_MIN;
    let mut last_warn: Option<Instant> = None;
    loop {
        if slot.strong_count() == 0 {
            return;
        }
        match ConnectionManager::new_with_config(client.clone(), config.clone()).await {
            Ok(conn) => {
                if let Some(slot) = slot.upgrade() {
                    slot.store(Some(Arc::new(conn)));
                    tracing::info!("Valkey key-value client connected");
                }
                return;
            }
            Err(e) => {
                if last_warn.is_none_or(|t| t.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        error_kind = KvErrorKind::from_redis(&e).as_str(),
                        "Valkey key-value client connect failed; retrying"
                    );
                    last_warn = Some(Instant::now());
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(CONNECT_RETRY_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(tls: bool) -> ValkeyEndpoint {
        ValkeyEndpoint {
            host: "127.0.0.1".to_string(),
            port: 6379,
            tls,
            ca_pem: None,
        }
    }

    #[tokio::test]
    async fn unconnected_client_fails_fast_with_conn() {
        let kv = ValkeyKv {
            conn: Arc::new(ArcSwapOption::empty()),
        };
        assert_eq!(kv.get("k").await.unwrap_err(), KvErrorKind::Conn);
        assert_eq!(
            kv.eval("return 1", &["k".to_string()], &[])
                .await
                .unwrap_err(),
            KvErrorKind::Conn
        );
    }

    #[test]
    fn start_without_a_runtime_is_an_error_not_a_panic() {
        assert_eq!(
            ValkeyKv::start(&endpoint(false), "pw".to_string())
                .err()
                .as_deref(),
            Some("no_runtime")
        );
    }

    #[tokio::test]
    async fn start_with_tls_but_no_ca_is_a_config_error() {
        assert_eq!(
            ValkeyKv::start(&endpoint(true), "pw".to_string())
                .err()
                .as_deref(),
            Some("ca_missing")
        );
    }

    #[test]
    fn error_kind_tags_are_stable() {
        assert_eq!(KvErrorKind::NoPerm.as_str(), "noperm");
        assert_eq!(KvErrorKind::Timeout.as_str(), "timeout");
        assert_eq!(KvErrorKind::Conn.as_str(), "conn");
        assert_eq!(KvErrorKind::Other.as_str(), "other");
    }

    #[test]
    fn replies_map_to_the_closed_enum() {
        use redis::Value;
        assert_eq!(KvReply::from_value(Value::Nil), Ok(KvReply::Nil));
        assert_eq!(KvReply::from_value(Value::Int(3)), Ok(KvReply::Int(3)));
        assert_eq!(
            KvReply::from_value(Value::BulkString(b"107".to_vec())),
            Ok(KvReply::Str("107".to_string()))
        );
        assert_eq!(
            KvReply::from_value(Value::BulkString(vec![0xff, 0xfe])),
            Err(KvErrorKind::Other)
        );
        assert_eq!(
            KvReply::from_value(Value::Array(vec![])),
            Err(KvErrorKind::Other)
        );
    }
}
