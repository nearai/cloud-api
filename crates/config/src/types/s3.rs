use super::{non_empty_env, parse_key_source_specs_for, KeySource, KeySourceSpec};
use std::env;

/// S3 configuration for file storage
#[derive(Clone)]
pub struct S3Config {
    pub mock: bool,
    pub bucket: String,
    pub region: String,
    pub encryption_key: String,
    pub encryption_keys: Option<Vec<KeySourceSpec>>,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("mock", &self.mock)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("encryption_key", &"<redacted>")
            .field("encryption_keys", &self.encryption_keys)
            .finish()
    }
}

impl S3Config {
    /// Load from environment variables
    pub fn from_env() -> Result<Self, String> {
        // Check if mock mode is enabled
        let mock = env::var("S3_MOCK")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

        let encryption_keys = non_empty_env("S3_ENCRYPTION_KEYS")
            .map(|raw| {
                let specs = parse_key_source_specs_for(&raw, "S3_ENCRYPTION_KEYS")?;
                if env::var("S3_ENCRYPTION_ALLOW_INLINE_KEYS").as_deref() != Ok("true") {
                    if let Some(index) = specs
                        .iter()
                        .position(|spec| matches!(spec.source, KeySource::Hex(_)))
                    {
                        return Err(format!("S3_ENCRYPTION_KEYS entry {index}: hex sources require S3_ENCRYPTION_ALLOW_INLINE_KEYS=true (local development and tests only)"));
                    }
                }
                Ok(specs)
            })
            .transpose()?;
        let encryption_key = if encryption_keys.is_some() {
            if ["S3_ENCRYPTION_KEY_FILE", "S3_ENCRYPTION_KEY"]
                .iter()
                .any(|name| env::var_os(name).is_some())
            {
                eprintln!("WARN: S3_ENCRYPTION_KEY_FILE and S3_ENCRYPTION_KEY are ignored when S3_ENCRYPTION_KEYS is set; the legacy key must be listed as a read entry if it is still needed");
            }
            String::new()
        } else if let Ok(path) = env::var("S3_ENCRYPTION_KEY_FILE") {
            std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read S3_ENCRYPTION_KEY_FILE: {e}"))?
                .trim()
                .to_string()
        } else {
            env::var("S3_ENCRYPTION_KEY").map_err(|_| {
                "Either S3_ENCRYPTION_KEY_FILE or S3_ENCRYPTION_KEY environment variable must be set"
                    .to_string()
            })?
        };

        if encryption_keys.is_none()
            && (encryption_key.len() != 64
                || !encryption_key.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(
                "S3 encryption key must contain 64 hexadecimal characters (32 bytes)".into(),
            );
        }

        Ok(Self {
            mock,
            bucket: env::var("AWS_S3_BUCKET").map_err(|_| "AWS_S3_BUCKET not set".to_string())?,
            region: env::var("AWS_S3_REGION").map_err(|_| "AWS_S3_REGION not set".to_string())?,
            encryption_key,
            encryption_keys,
        })
    }
}
