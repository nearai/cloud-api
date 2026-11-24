//! A minimal key-value client over the placement Valkey endpoint, for caches
//! that live outside inference routing: `GET` and `FCALL`.
//!
//! It reuses the placement client construction (`ValkeyEndpoint`, the pinned
//! CA, [`install_crypto_provider`]) but authenticates as the ACL user it is
//! given, not as the placement user. It sends no scripts: server-side logic
//! is a function that an operator loaded beforehand, called by name.
//!
//! It never connects on the caller's path: [`ValkeyKv::start`] returns at
//! once and a background task connects with backoff. Until it has connected, every call fails fast with
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

use crate::placement_io::{endpoint_client_as, install_crypto_provider, ValkeyEndpoint};

/// Why a Valkey call failed. A closed set so callers can count it as a metric
/// tag without ever touching the redis error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvErrorKind {
    /// The server did not answer in time.
    Timeout,
    /// The ACL forbids the command or key (`NOPERM`), or a function touched
    /// a key it may not (`ACL failure`).
    NoPerm,
    /// The function (or its library) is not loaded on the server.
    NoFunction,
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
            Self::NoFunction => "no_function",
            Self::Conn => "conn",
            Self::Other => "other",
        }
    }

    fn from_redis(e: &redis::RedisError) -> Self {
        // A function touching a key the user may not access fails as
        // `ERR ACL failure in ...`, not `NOPERM`; it is the same ACL refusal.
        // A missing function fails as `ERR Function not found`. The text is
        // only compared, never logged.
        let detail = e.detail().unwrap_or_default();
        if e.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm)
            || detail.starts_with("ACL failure in")
        {
            Self::NoPerm
        } else if detail.starts_with("Function not found") {
            Self::NoFunction
        } else if e.is_timeout() {
            Self::Timeout
        } else if e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() {
            Self::Conn
        } else {
            Self::Other
        }
    }
}

/// The reply of an `FCALL`, reduced to what the callers use.
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

/// A lazily connected Valkey client for `GET` and `FCALL`.
pub struct ValkeyKv {
    conn: Arc<ArcSwapOption<ConnectionManager>>,
}

impl ValkeyKv {
    /// Builds the client for `endpoint`, authenticating as ACL user `user`
    /// (1 to 32 characters of `a-z`, `0-9` and `-`) with `password`, and
    /// starts connecting in the background.
    ///
    /// Errors carry only a kind, never the host or password. Needs a running
    /// tokio runtime.
    pub fn start(endpoint: &ValkeyEndpoint, user: &str, password: String) -> Result<Self, String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| "no_runtime".to_string())?;
        install_crypto_provider();
        let client = endpoint_client_as(endpoint, user, password)?;
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

    /// `FCALL function len(keys) keys.. args..`: calls a function that is
    /// already loaded on the server. Never `EVAL`, never `FUNCTION LOAD`.
    pub async fn fcall(
        &self,
        function: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<KvReply, KvErrorKind> {
        let mut conn = self.connection()?;
        let mut cmd = redis::cmd("FCALL");
        cmd.arg(function).arg(keys.len());
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
            kv.fcall("f", &["k".to_string()], &[]).await.unwrap_err(),
            KvErrorKind::Conn
        );
    }

    #[test]
    fn start_without_a_runtime_is_an_error_not_a_panic() {
        assert_eq!(
            ValkeyKv::start(&endpoint(false), "spend-test", "pw".to_string())
                .err()
                .as_deref(),
            Some("no_runtime")
        );
    }

    #[tokio::test]
    async fn start_with_tls_but_no_ca_is_a_config_error() {
        assert_eq!(
            ValkeyKv::start(&endpoint(true), "spend-test", "pw".to_string())
                .err()
                .as_deref(),
            Some("ca_missing")
        );
    }

    #[tokio::test]
    async fn start_with_a_user_that_could_alter_the_url_is_a_config_error() {
        assert_eq!(
            ValkeyKv::start(&endpoint(false), "a@b", "pw".to_string())
                .err()
                .as_deref(),
            Some("user_invalid")
        );
    }

    #[test]
    fn error_kind_tags_are_stable() {
        assert_eq!(KvErrorKind::NoFunction.as_str(), "no_function");
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
