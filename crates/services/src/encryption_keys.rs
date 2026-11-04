//! Resolve ordered database encryption key sources without owning a database ring.
use async_trait::async_trait;
use config::{KeySource, KeySourceSpec};
use dstack_sdk::dstack_client;
use thiserror::Error;

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
    async fn fetch(&self, path: &str) -> Result<[u8; 32], KeyFetchError>;
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
    async fn fetch(&self, path: &str) -> Result<[u8; 32], KeyFetchError> {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.client.get_key(Some(path.to_string()), None),
        )
        .await
        .map_err(|_| KeyFetchError::Timeout)?
        .map_err(|_| KeyFetchError::Unavailable)?;
        key_bytes(
            response
                .decode_key()
                .map_err(|_| KeyFetchError::InvalidEncoding)?,
        )
    }
}

fn key_bytes(bytes: Vec<u8>) -> Result<[u8; 32], KeyFetchError> {
    bytes.try_into().map_err(|_| KeyFetchError::InvalidLength)
}

fn parse_hex(raw: &str) -> Result<[u8; 32], KeyFetchError> {
    key_bytes(hex::decode(raw).map_err(|_| KeyFetchError::InvalidEncoding)?)
}

/// The API constructs the database-owned ring from these write-first pairs.
pub async fn resolve_key_ring(
    specs: &[KeySourceSpec],
    dstack_endpoint: Option<&str>,
) -> Result<Vec<(String, [u8; 32])>, KeyRingResolveError> {
    resolve_with_fetcher(specs, &DstackKeyFetcher::new(dstack_endpoint)).await
}

async fn resolve_with_fetcher(
    specs: &[KeySourceSpec],
    fetcher: &impl KeyFetcher,
) -> Result<Vec<(String, [u8; 32])>, KeyRingResolveError> {
    if specs.is_empty() {
        return Err(KeyRingResolveError {
            key_id: String::new(),
            source_kind: "configuration",
            class: KeyFetchError::Empty,
        });
    }
    let mut keys = Vec::with_capacity(specs.len());
    for spec in specs {
        let (source_kind, result) = match &spec.source {
            KeySource::Hex(raw) => ("hex", parse_hex(raw)),
            KeySource::File(path) => (
                "file",
                match tokio::fs::read_to_string(path).await {
                    Ok(raw) => parse_hex(raw.trim()),
                    Err(_) => Err(KeyFetchError::Unavailable),
                },
            ),
            KeySource::Dstack(path) => ("dstack", fetcher.fetch(path).await),
        };
        let key = result.map_err(|class| KeyRingResolveError {
            key_id: spec.key_id.clone(),
            source_kind,
            class,
        })?;
        keys.push((spec.key_id.clone(), key));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeFetcher(Vec<u8>);
    #[async_trait]
    impl KeyFetcher for FakeFetcher {
        async fn fetch(&self, path: &str) -> Result<[u8; 32], KeyFetchError> {
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
            result.unwrap(),
            vec![
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
