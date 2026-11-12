use std::sync::Arc;

use config::S3Config;
use services::files::encryption::ObjectKeyRing;

/// Resolve and validate the complete object ring before constructing storage.
pub(crate) async fn resolve_object_ring(config: &S3Config) -> anyhow::Result<Arc<ObjectKeyRing>> {
    let ring = match &config.encryption_keys {
        Some(specs) => {
            let keys = services::encryption_keys::resolve_key_ring(specs, None).await?;
            let mut keys = keys.iter().map(|(id, key)| (id.clone(), *key));
            let (id, key) = keys
                .next()
                .ok_or_else(|| anyhow::anyhow!("S3 encryption keys must not be empty"))?;
            ObjectKeyRing::new(id, key, keys.collect())?
        }
        None => ObjectKeyRing::from_hex(&config.encryption_key)?,
    };
    Ok(Arc::new(ring))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{KeySource, KeySourceSpec};

    fn config() -> S3Config {
        S3Config {
            object_rewrap_max_bytes: 268_435_456,
            object_encryption_write_enabled: true,
            mock: true,
            bucket: "test".into(),
            region: "us-east-1".into(),
            encryption_key: "07".repeat(32),
            encryption_keys: None,
        }
    }

    #[tokio::test]
    async fn resolves_legacy_and_ordered_object_keys_before_storage() {
        let mut config = config();
        let legacy = resolve_object_ring(&config).await.unwrap();
        assert_eq!(legacy.write_id(), "s3-legacy");
        assert_eq!(legacy.write_key(), &[7; 32]);
        config.encryption_key.clear();
        config.encryption_keys = Some(vec![
            KeySourceSpec {
                key_id: "s3-v2".into(),
                source: KeySource::Hex("09".repeat(32)),
            },
            KeySourceSpec {
                key_id: "s3-v1".into(),
                source: KeySource::Hex("07".repeat(32)),
            },
        ]);
        let ring = resolve_object_ring(&config).await.unwrap();
        assert_eq!(ring.ids(), vec!["s3-v2", "s3-v1"]);
        assert_eq!(ring.write_key(), &[9; 32]);
        assert_eq!(ring.lookup("s3-v1"), Some(&[7; 32]));
    }

    #[tokio::test]
    async fn invalid_object_keys_stop_initialization_without_key_material() {
        let mut config = config();
        config.encryption_key = "invalid-private-value".into();
        let error = resolve_object_ring(&config).await.unwrap_err().to_string();
        assert!(!error.contains(&config.encryption_key));
        config.encryption_keys = Some(vec![KeySourceSpec {
            key_id: "s3-v2".into(),
            source: KeySource::Hex("08".repeat(31)),
        }]);
        let error = resolve_object_ring(&config).await.unwrap_err().to_string();
        assert!(!error.contains(&"08".repeat(31)));
        config.encryption_keys = Some(Vec::new());
        assert!(resolve_object_ring(&config).await.is_err());
    }
}
