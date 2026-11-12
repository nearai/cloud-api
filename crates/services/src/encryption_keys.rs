//! Resolve ordered database encryption key sources without owning a database ring.
use async_trait::async_trait;
use config::{KeySource, KeySourceSpec};
use dstack_sdk::dstack_client;
use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
#[error("database encryption key {key_id} ({source_kind}): {class}")]
pub struct KeyRingResolveError {
    pub key_id: String,
    pub source_kind: &'static str,
    pub class: KeyFetchError,
}

#[derive(Debug, Error, Clone, Copy)]
pub enum KeyFetchError {
    #[error("source unavailable")]
    Unavailable,
    #[error("invalid key encoding")]
    InvalidEncoding,
    #[error("key must contain 32 bytes")]
    InvalidLength,
    #[error("source timed out")]
    Timeout,
    #[error("key sources must not be empty")]
    Empty,
}

#[async_trait]
pub trait KeyFetcher: Send + Sync {
    async fn fetch(&self, path: &str) -> Result<Zeroizing<[u8; 32]>, KeyFetchError>;
}

pub struct DstackKeyFetcher {
    client: dstack_client::DstackClient,
}

impl DstackKeyFetcher {
    pub fn new(endpoint: Option<&str>) -> Self {
        Self {
            client: dstack_client::DstackClient::new(endpoint),
        }
    }
}

#[async_trait]
impl KeyFetcher for DstackKeyFetcher {
    async fn fetch(&self, path: &str) -> Result<Zeroizing<[u8; 32]>, KeyFetchError> {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.client.get_key(Some(path.to_string()), None),
        )
        .await
        .map_err(|_| KeyFetchError::Timeout)?
        .map_err(|_| KeyFetchError::Unavailable)?;
        let raw = Zeroizing::new(response.key);
        parse_hex(&raw)
    }
}

fn key_bytes(bytes: Vec<u8>) -> Result<Zeroizing<[u8; 32]>, KeyFetchError> {
    let bytes = Zeroizing::new(bytes);
    let key = bytes
        .as_slice()
        .try_into()
        .map_err(|_| KeyFetchError::InvalidLength)?;
    Ok(Zeroizing::new(key))
}

fn parse_hex(raw: &str) -> Result<Zeroizing<[u8; 32]>, KeyFetchError> {
    key_bytes(hex::decode(raw).map_err(|_| KeyFetchError::InvalidEncoding)?)
}

/// The API constructs the database-owned ring from these write-first pairs.
pub async fn resolve_key_ring(
    specs: &[KeySourceSpec],
    dstack_endpoint: Option<&str>,
) -> Result<Zeroizing<Vec<(String, [u8; 32])>>, KeyRingResolveError> {
    resolve_with_fetcher(specs, &DstackKeyFetcher::new(dstack_endpoint)).await
}

async fn resolve_with_fetcher(
    specs: &[KeySourceSpec],
    fetcher: &impl KeyFetcher,
) -> Result<Zeroizing<Vec<(String, [u8; 32])>>, KeyRingResolveError> {
    if specs.is_empty() {
        return Err(KeyRingResolveError {
            key_id: String::new(),
            source_kind: "configuration",
            class: KeyFetchError::Empty,
        });
    }
    let mut keys = Zeroizing::new(Vec::with_capacity(specs.len()));
    for spec in specs {
        let (source_kind, result) = match &spec.source {
            KeySource::Hex(raw) => ("hex", parse_hex(raw)),
            KeySource::File(path) => (
                "file",
                match tokio::fs::read_to_string(path).await {
                    Ok(raw) => parse_hex(Zeroizing::new(raw).trim()),
                    Err(error) => {
                        tracing::warn!(
                            key_id = %spec.key_id,
                            path = %path.display(),
                            error_kind = ?error.kind(),
                            "database encryption key file unavailable"
                        );
                        Err(KeyFetchError::Unavailable)
                    }
                },
            ),
            KeySource::Dstack(path) => ("dstack", fetcher.fetch(path).await),
        };
        let key = result.map_err(|class| KeyRingResolveError {
            key_id: spec.key_id.clone(),
            source_kind,
            class,
        })?;
        keys.push((spec.key_id.clone(), *key));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);
    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn missing_file_logs_safe_diagnostics() {
        // Given a missing source and a local tracing capture.
        let path = std::env::temp_dir().join(format!("missing-key-{}", uuid::Uuid::new_v4()));
        let specs = [KeySourceSpec {
            key_id: "db-missing".into(),
            source: KeySource::File(path.clone()),
        }];
        let buffer = LogBuffer(Arc::default());
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        // When resolution fails, then diagnostics preserve id/path/kind, not contents.
        let error = resolve_with_fetcher(&specs, &FakeFetcher(vec![]))
            .with_subscriber(subscriber)
            .await
            .unwrap_err();
        assert!(matches!(error.class, KeyFetchError::Unavailable));
        assert_eq!(
            error.to_string(),
            "database encryption key db-missing (file): source unavailable"
        );
        let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("db-missing"), "{logs}");
        assert!(logs.contains(path.to_str().unwrap()), "{logs}");
        assert!(logs.contains("NotFound"), "{logs}");
    }

    struct FakeFetcher(Vec<u8>);
    #[async_trait]
    impl KeyFetcher for FakeFetcher {
        async fn fetch(&self, path: &str) -> Result<Zeroizing<[u8; 32]>, KeyFetchError> {
            assert_eq!(path, "/keys/v1");
            key_bytes(self.0.clone())
        }
    }

    #[tokio::test]
    async fn resolves_file_hex_and_dstack_in_write_first_order() {
        let path = std::env::temp_dir().join(format!("encryption-keys-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, format!(" {}\n", "07".repeat(32)))
            .await
            .unwrap();
        let specs = vec![
            KeySourceSpec {
                key_id: "db-v2".into(),
                source: KeySource::Hex("09".repeat(32)),
            },
            KeySourceSpec {
                key_id: "db-v1".into(),
                source: KeySource::File(path.clone()),
            },
            KeySourceSpec {
                key_id: "db-v0".into(),
                source: KeySource::Dstack("/keys/v1".into()),
            },
        ];
        let result = resolve_with_fetcher(&specs, &FakeFetcher(vec![8; 32])).await;
        tokio::fs::remove_file(path).await.unwrap();
        assert_eq!(
            result.unwrap().as_slice(),
            &[
                ("db-v2".into(), [9; 32]),
                ("db-v1".into(), [7; 32]),
                ("db-v0".into(), [8; 32])
            ]
        );
    }

    #[tokio::test]
    async fn rejects_short_dstack_keys_with_safe_errors() {
        let specs = [KeySourceSpec {
            key_id: "db-v1".into(),
            source: KeySource::Dstack("/keys/v1".into()),
        }];
        let error = resolve_with_fetcher(&specs, &FakeFetcher(vec![7; 31]))
            .await
            .unwrap_err();
        assert!(matches!(error.class, KeyFetchError::InvalidLength));
        assert_eq!(
            error.to_string(),
            "database encryption key db-v1 (dstack): key must contain 32 bytes"
        );
    }

    #[tokio::test]
    async fn unreachable_dstack_endpoint_returns_an_error() {
        let specs = [KeySourceSpec {
            key_id: "db-v1".into(),
            source: KeySource::Dstack("/keys/v1".into()),
        }];
        let error = resolve_key_ring(&specs, Some("http://127.0.0.1:1"))
            .await
            .unwrap_err();
        assert!(matches!(error.class, KeyFetchError::Unavailable));
        assert_eq!(error.source_kind, "dstack");
    }

    #[tokio::test]
    async fn rejects_empty_sources_and_invalid_hex_without_values() {
        assert!(resolve_with_fetcher(&[], &FakeFetcher(vec![]))
            .await
            .is_err());
        for raw in ["not-hex".to_string(), "07".repeat(31)] {
            let specs = [KeySourceSpec {
                key_id: "db-v1".into(),
                source: KeySource::Hex(raw.clone()),
            }];
            let error = resolve_with_fetcher(&specs, &FakeFetcher(vec![]))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("db-v1 (hex)"));
            assert!(!error.to_string().contains(&raw));
        }
    }
}
