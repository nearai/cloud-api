//! Machine Payments Protocol (MPP) configuration.
//!
//! MPP is off by default. With `MPP_ENABLED` unset or false nothing else is read or
//! validated, so a deployment without MPP can never fail to boot because of it. With it
//! true, every required value must be present and valid or startup fails.
//!
//! Secrets (`MPP_SECRET_KEY`, `MPP_SERVICE_API_KEY`) follow the repo pattern: a value in
//! the environment or a file path in the matching `*_FILE` variable, redacted in `Debug`.

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

use crate::types::{
    non_empty_env, parse_bool_env, parse_u64_env, read_optional_secret_env_absent_empty,
};

pub const DEFAULT_MPP_CHALLENGE_TTL_SECS: u64 = 600;
pub const MIN_MPP_CHALLENGE_TTL_SECS: u64 = 60;
pub const MAX_MPP_CHALLENGE_TTL_SECS: u64 = 3600;
pub const DEFAULT_MPP_RPC_TIMEOUT_SECS: u64 = 15;
pub const MAX_MPP_RPC_TIMEOUT_SECS: u64 = 120;
/// The HMAC key must be at least this long (matches the reference implementation).
pub const MIN_MPP_SECRET_KEY_BYTES: usize = 32;

/// MPP settings: either off, or fully validated.
#[derive(Clone, Default)]
pub enum MppConfig {
    #[default]
    Disabled,
    Enabled(Box<MppSettings>),
}

impl MppConfig {
    /// The settings when MPP is enabled.
    pub fn settings(&self) -> Option<&MppSettings> {
        match self {
            Self::Disabled => None,
            Self::Enabled(settings) => Some(settings),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        if !parse_bool_env("MPP_ENABLED", false)? {
            return Ok(Self::Disabled);
        }
        Ok(Self::Enabled(Box::new(MppSettings::from_env()?)))
    }
}

impl fmt::Debug for MppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("MppConfig::Disabled"),
            Self::Enabled(settings) => f.debug_tuple("MppConfig::Enabled").field(settings).finish(),
        }
    }
}

#[derive(Clone)]
pub struct MppSettings {
    /// Protection space, also the attribution server id. An ASCII host name.
    pub realm: String,
    /// HMAC key that binds challenge ids. Identical on every instance. Never logged.
    pub secret_key: String,
    /// `sk-` API key of the internal workspace that MPP traffic runs and is metered
    /// under. Never logged.
    pub service_api_key: String,
    pub challenge_ttl_secs: u64,
    pub tempo: MppTempoSettings,
    /// Flat per-request price per model name.
    pub model_prices: BTreeMap<String, MppModelPrice>,
}

impl fmt::Debug for MppSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MppSettings")
            .field("realm", &self.realm)
            .field("secret_key", &"<redacted>")
            .field("service_api_key", &"<redacted>")
            .field("challenge_ttl_secs", &self.challenge_ttl_secs)
            .field("tempo", &self.tempo)
            .field("model_prices", &self.model_prices)
            .finish()
    }
}

#[derive(Clone)]
pub struct MppTempoSettings {
    /// JSON-RPC endpoint. May embed a provider key, so `Debug` never prints it.
    pub rpc_url: String,
    pub chain_id: u64,
    /// TIP-20 token contract, `0x` + 40 lowercase hex.
    pub currency: String,
    /// Payee, `0x` + 40 lowercase hex.
    pub recipient: String,
    pub rpc_timeout_secs: u64,
}

impl fmt::Debug for MppTempoSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MppTempoSettings")
            .field("rpc_url", &"<redacted>")
            .field("chain_id", &self.chain_id)
            .field("currency", &self.currency)
            .field("recipient", &self.recipient)
            .field("rpc_timeout_secs", &self.rpc_timeout_secs)
            .finish()
    }
}

/// One model's flat price and the request limits that keep the price honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MppModelPrice {
    /// Price per request in token base units. Always greater than zero.
    pub amount: u128,
    /// Largest `max_tokens` / `max_completion_tokens` a paid request may ask for.
    pub max_tokens: u32,
    /// Largest request body, in bytes, a paid request may send.
    pub max_request_bytes: u32,
}

impl MppSettings {
    fn from_env() -> Result<Self, String> {
        let realm = parse_realm(&required_env("MPP_REALM")?)?;
        let secret_key = required_secret("MPP_SECRET_KEY_FILE", "MPP_SECRET_KEY")?;
        if secret_key.len() < MIN_MPP_SECRET_KEY_BYTES {
            return Err(format!(
                "MPP_SECRET_KEY must be at least {MIN_MPP_SECRET_KEY_BYTES} bytes"
            ));
        }
        let service_api_key = required_secret("MPP_SERVICE_API_KEY_FILE", "MPP_SERVICE_API_KEY")?;
        if !service_api_key.starts_with("sk-") || service_api_key.contains(char::is_whitespace) {
            return Err("MPP_SERVICE_API_KEY must be an sk- API key".to_string());
        }
        let challenge_ttl_secs =
            parse_u64_env("MPP_CHALLENGE_TTL_SECS", DEFAULT_MPP_CHALLENGE_TTL_SECS)?;
        if !(MIN_MPP_CHALLENGE_TTL_SECS..=MAX_MPP_CHALLENGE_TTL_SECS).contains(&challenge_ttl_secs)
        {
            return Err(format!(
                "MPP_CHALLENGE_TTL_SECS must be between {MIN_MPP_CHALLENGE_TTL_SECS} and {MAX_MPP_CHALLENGE_TTL_SECS}"
            ));
        }
        let rpc_timeout_secs = parse_u64_env("MPP_RPC_TIMEOUT_SECS", DEFAULT_MPP_RPC_TIMEOUT_SECS)?;
        if !(1..=MAX_MPP_RPC_TIMEOUT_SECS).contains(&rpc_timeout_secs) {
            return Err(format!(
                "MPP_RPC_TIMEOUT_SECS must be between 1 and {MAX_MPP_RPC_TIMEOUT_SECS}"
            ));
        }
        let chain_id: u64 = required_env("MPP_TEMPO_CHAIN_ID")?
            .parse()
            .map_err(|_| "MPP_TEMPO_CHAIN_ID must be an unsigned integer".to_string())?;
        if chain_id == 0 {
            return Err("MPP_TEMPO_CHAIN_ID must be greater than zero".to_string());
        }
        let tempo = MppTempoSettings {
            rpc_url: parse_rpc_url(&required_env("MPP_TEMPO_RPC_URL")?)?,
            chain_id,
            currency: parse_address("MPP_TEMPO_CURRENCY", &required_env("MPP_TEMPO_CURRENCY")?)?,
            recipient: parse_address("MPP_TEMPO_RECIPIENT", &required_env("MPP_TEMPO_RECIPIENT")?)?,
            rpc_timeout_secs,
        };
        let model_prices = parse_model_prices(&required_env("MPP_MODEL_PRICES")?)?;
        Ok(Self {
            realm,
            secret_key,
            service_api_key,
            challenge_ttl_secs,
            tempo,
            model_prices,
        })
    }
}

fn required_env(key: &str) -> Result<String, String> {
    non_empty_env(key).ok_or_else(|| format!("{key} must be set when MPP_ENABLED=true"))
}

fn required_secret(file_key: &str, value_key: &str) -> Result<String, String> {
    read_optional_secret_env_absent_empty(file_key, value_key)?
        .ok_or_else(|| format!("{value_key} or {file_key} must be set when MPP_ENABLED=true"))
}

/// A realm is an ASCII host name: it is echoed in a quoted header parameter and keyed into
/// the attribution memo, so anything outside `[A-Za-z0-9.-]` is refused.
pub fn parse_realm(raw: &str) -> Result<String, String> {
    let realm = raw.trim();
    let valid = !realm.is_empty()
        && realm.len() <= 253
        && !realm.starts_with(['.', '-'])
        && !realm.ends_with(['.', '-'])
        && realm
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    if valid {
        Ok(realm.to_string())
    } else {
        Err("MPP_REALM must be an ASCII host name such as cloud-api.near.ai".to_string())
    }
}

/// `https`, or `http` only for a loopback host (local tests).
pub fn parse_rpc_url(raw: &str) -> Result<String, String> {
    let invalid =
        || "MPP_TEMPO_RPC_URL must be an https URL, or http on a loopback host".to_string();
    let url = url::Url::parse(raw.trim()).map_err(|_| invalid())?;
    let host_ok = match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    let allowed = match url.scheme() {
        "https" => url.host().is_some(),
        "http" => host_ok,
        _ => false,
    };
    if allowed {
        Ok(url.to_string())
    } else {
        Err(invalid())
    }
}

/// `0x` + 40 hex digits, any case in, lowercase out.
pub fn parse_address(key: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let valid = raw.len() == 42
        && raw.starts_with("0x")
        && raw.bytes().skip(2).all(|b| b.is_ascii_hexdigit());
    if valid {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(format!("{key} must be 0x followed by 40 hex digits"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelPrice {
    amount: String,
    max_tokens: u32,
    max_request_bytes: u32,
}

/// A JSON object that rejects a repeated key, so a duplicated model name cannot silently
/// keep the last price.
struct StrictPrices(BTreeMap<String, RawModelPrice>);

impl<'de> Deserialize<'de> for StrictPrices {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PricesVisitor;

        impl<'de> Visitor<'de> for PricesVisitor {
            type Value = StrictPrices;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object mapping model names to prices")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut prices = BTreeMap::new();
                while let Some((model, price)) = map.next_entry::<String, RawModelPrice>()? {
                    if prices.insert(model, price).is_some() {
                        return Err(serde::de::Error::custom("duplicate model name"));
                    }
                }
                Ok(StrictPrices(prices))
            }
        }

        deserializer.deserialize_map(PricesVisitor)
    }
}

/// Strict parse of `MPP_MODEL_PRICES`.
pub fn parse_model_prices(raw: &str) -> Result<BTreeMap<String, MppModelPrice>, String> {
    let StrictPrices(parsed) = serde_json::from_str(raw)
        .map_err(|e| format!("MPP_MODEL_PRICES is not a valid price table: {e}"))?;
    if parsed.is_empty() {
        return Err("MPP_MODEL_PRICES must price at least one model".to_string());
    }
    let mut prices = BTreeMap::new();
    for (model, price) in parsed {
        if model.is_empty() || model.trim() != model {
            return Err("MPP_MODEL_PRICES has an empty or padded model name".to_string());
        }
        let digits_only =
            !price.amount.is_empty() && price.amount.bytes().all(|b| b.is_ascii_digit());
        let amount = digits_only
            .then(|| price.amount.parse::<u128>().ok())
            .flatten()
            .filter(|amount| *amount > 0)
            .ok_or_else(|| {
                format!("MPP_MODEL_PRICES amount for {model} must be a decimal string above zero that fits in 128 bits")
            })?;
        if price.max_tokens == 0 || price.max_request_bytes == 0 {
            return Err(format!(
                "MPP_MODEL_PRICES limits for {model} must be greater than zero"
            ));
        }
        prices.insert(
            model,
            MppModelPrice {
                amount,
                max_tokens: price.max_tokens,
                max_request_bytes: price.max_request_bytes,
            },
        );
    }
    Ok(prices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const KEYS: [&str; 13] = [
        "MPP_ENABLED",
        "MPP_REALM",
        "MPP_SECRET_KEY",
        "MPP_SECRET_KEY_FILE",
        "MPP_SERVICE_API_KEY",
        "MPP_SERVICE_API_KEY_FILE",
        "MPP_CHALLENGE_TTL_SECS",
        "MPP_TEMPO_RPC_URL",
        "MPP_TEMPO_CHAIN_ID",
        "MPP_TEMPO_CURRENCY",
        "MPP_TEMPO_RECIPIENT",
        "MPP_RPC_TIMEOUT_SECS",
        "MPP_MODEL_PRICES",
    ];

    const SECRET: &str = "0123456789abcdef0123456789abcdef-hmac";
    const PRICES: &str =
        r#"{"zai-org/GLM-5":{"amount":"20000","max_tokens":2048,"max_request_bytes":65536}}"#;

    fn clear() {
        for key in KEYS {
            std::env::remove_var(key);
        }
    }

    fn set_valid() {
        clear();
        std::env::set_var("MPP_ENABLED", "true");
        std::env::set_var("MPP_REALM", "cloud-api.near.ai");
        std::env::set_var("MPP_SECRET_KEY", SECRET);
        std::env::set_var("MPP_SERVICE_API_KEY", "sk-live-service-key-0001");
        std::env::set_var("MPP_TEMPO_RPC_URL", "https://rpc.moderato.tempo.xyz");
        std::env::set_var("MPP_TEMPO_CHAIN_ID", "42431");
        std::env::set_var(
            "MPP_TEMPO_CURRENCY",
            "0x20C0000000000000000000000000000000000000",
        );
        std::env::set_var(
            "MPP_TEMPO_RECIPIENT",
            "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01",
        );
        std::env::set_var("MPP_MODEL_PRICES", PRICES);
    }

    #[test]
    #[serial]
    fn disabled_by_default_and_reads_nothing_else() {
        clear();
        // Garbage in every other variable must not matter while disabled.
        std::env::set_var("MPP_MODEL_PRICES", "not json");
        std::env::set_var("MPP_SECRET_KEY", "short");
        assert!(MppConfig::from_env().unwrap().settings().is_none());
        std::env::set_var("MPP_ENABLED", "false");
        assert!(MppConfig::from_env().unwrap().settings().is_none());
        clear();
    }

    #[test]
    #[serial]
    fn enabled_with_valid_values_normalizes_addresses() {
        set_valid();
        let config = MppConfig::from_env().unwrap();
        let settings = config.settings().unwrap();
        assert_eq!(settings.realm, "cloud-api.near.ai");
        assert_eq!(settings.challenge_ttl_secs, 600);
        assert_eq!(settings.tempo.rpc_timeout_secs, 15);
        assert_eq!(settings.tempo.chain_id, 42431);
        assert_eq!(
            settings.tempo.currency,
            "0x20c0000000000000000000000000000000000000"
        );
        assert_eq!(
            settings.tempo.recipient,
            "0xabcdef0123456789abcdef0123456789abcdef01"
        );
        let price = settings.model_prices["zai-org/GLM-5"];
        assert_eq!(price.amount, 20_000);
        assert_eq!(price.max_tokens, 2048);
        clear();
    }

    #[test]
    #[serial]
    fn enabled_requires_every_value() {
        for missing in [
            "MPP_REALM",
            "MPP_SECRET_KEY",
            "MPP_SERVICE_API_KEY",
            "MPP_TEMPO_RPC_URL",
            "MPP_TEMPO_CHAIN_ID",
            "MPP_TEMPO_CURRENCY",
            "MPP_TEMPO_RECIPIENT",
            "MPP_MODEL_PRICES",
        ] {
            set_valid();
            std::env::remove_var(missing);
            let error = MppConfig::from_env().unwrap_err();
            assert!(error.contains(missing), "{missing}: {error}");
        }
        clear();
    }

    #[test]
    #[serial]
    fn rejects_weak_or_malformed_values() {
        let cases: [(&str, &str, &str); 9] = [
            ("MPP_SECRET_KEY", "too-short", "MPP_SECRET_KEY"),
            (
                "MPP_SERVICE_API_KEY",
                "not-an-sk-key",
                "MPP_SERVICE_API_KEY",
            ),
            ("MPP_REALM", "bad realm!", "MPP_REALM"),
            ("MPP_CHALLENGE_TTL_SECS", "59", "MPP_CHALLENGE_TTL_SECS"),
            ("MPP_CHALLENGE_TTL_SECS", "3601", "MPP_CHALLENGE_TTL_SECS"),
            ("MPP_RPC_TIMEOUT_SECS", "0", "MPP_RPC_TIMEOUT_SECS"),
            ("MPP_TEMPO_CHAIN_ID", "0", "MPP_TEMPO_CHAIN_ID"),
            ("MPP_TEMPO_CURRENCY", "0x1234", "MPP_TEMPO_CURRENCY"),
            (
                "MPP_TEMPO_RPC_URL",
                "http://rpc.example.com",
                "MPP_TEMPO_RPC_URL",
            ),
        ];
        for (key, value, expected) in cases {
            set_valid();
            std::env::set_var(key, value);
            let error = MppConfig::from_env().unwrap_err();
            assert!(error.contains(expected), "{key}: {error}");
        }
        clear();
    }

    #[test]
    #[serial]
    fn ttl_bounds_are_inclusive() {
        for ttl in ["60", "3600"] {
            set_valid();
            std::env::set_var("MPP_CHALLENGE_TTL_SECS", ttl);
            assert!(MppConfig::from_env().is_ok(), "{ttl}");
        }
        clear();
    }

    #[test]
    #[serial]
    fn secrets_load_from_files() {
        set_valid();
        std::env::remove_var("MPP_SECRET_KEY");
        std::env::remove_var("MPP_SERVICE_API_KEY");
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("secret");
        let key_path = dir.path().join("key");
        std::fs::write(&secret_path, format!("{SECRET}\n")).unwrap();
        std::fs::write(&key_path, "sk-live-from-file-0002\n").unwrap();
        std::env::set_var("MPP_SECRET_KEY_FILE", &secret_path);
        std::env::set_var("MPP_SERVICE_API_KEY_FILE", &key_path);
        let config = MppConfig::from_env().unwrap();
        let settings = config.settings().unwrap();
        assert_eq!(settings.secret_key, SECRET);
        assert_eq!(settings.service_api_key, "sk-live-from-file-0002");
        clear();
    }

    #[test]
    #[serial]
    fn debug_redacts_secrets_and_rpc_url() {
        set_valid();
        std::env::set_var(
            "MPP_TEMPO_RPC_URL",
            "https://rpc.example.com/v1/provider-key-xyz",
        );
        let config = MppConfig::from_env().unwrap();
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(SECRET));
        assert!(!debug.contains("sk-live-service-key-0001"));
        assert!(!debug.contains("provider-key-xyz"));
        clear();
    }

    #[test]
    fn rpc_url_allows_https_and_loopback_http_only() {
        assert!(parse_rpc_url("https://rpc.moderato.tempo.xyz").is_ok());
        assert!(parse_rpc_url("http://127.0.0.1:8545").is_ok());
        assert!(parse_rpc_url("http://localhost:8545/").is_ok());
        assert!(parse_rpc_url("http://[::1]:8545").is_ok());
        assert!(parse_rpc_url("http://rpc.example.com").is_err());
        assert!(parse_rpc_url("http://127.0.0.1.evil.example").is_err());
        assert!(parse_rpc_url("ftp://localhost").is_err());
        assert!(parse_rpc_url("not a url").is_err());
    }

    #[test]
    fn model_prices_parse_strictly() {
        let ok = parse_model_prices(PRICES).unwrap();
        assert_eq!(ok.len(), 1);

        for (label, raw) in [
            ("empty table", "{}"),
            ("not json", "nope"),
            ("array", "[]"),
            (
                "unknown field",
                r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":1,"fee":1}}"#,
            ),
            ("missing field", r#"{"m":{"amount":"1","max_tokens":1}}"#),
            (
                "zero amount",
                r#"{"m":{"amount":"0","max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "numeric amount",
                r#"{"m":{"amount":5,"max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "signed amount",
                r#"{"m":{"amount":"+5","max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "fractional amount",
                r#"{"m":{"amount":"1.5","max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "amount over u128",
                r#"{"m":{"amount":"340282366920938463463374607431768211456","max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "zero token cap",
                r#"{"m":{"amount":"1","max_tokens":0,"max_request_bytes":1}}"#,
            ),
            (
                "zero size cap",
                r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":0}}"#,
            ),
            (
                "padded model name",
                r#"{" m":{"amount":"1","max_tokens":1,"max_request_bytes":1}}"#,
            ),
            (
                "duplicate model",
                r#"{"m":{"amount":"1","max_tokens":1,"max_request_bytes":1},"m":{"amount":"2","max_tokens":1,"max_request_bytes":1}}"#,
            ),
        ] {
            assert!(parse_model_prices(raw).is_err(), "{label} must be rejected");
        }
    }

    #[test]
    fn u128_max_amount_is_accepted() {
        let raw = r#"{"m":{"amount":"340282366920938463463374607431768211455","max_tokens":1,"max_request_bytes":1}}"#;
        assert_eq!(parse_model_prices(raw).unwrap()["m"].amount, u128::MAX);
    }

    #[test]
    fn realm_must_be_an_ascii_host() {
        assert!(parse_realm("cloud-api.near.ai").is_ok());
        assert!(parse_realm("localhost").is_ok());
        for bad in ["", ".x", "x.", "-x", "a b", "a\"b", "é.com", "a/b", "a:80"] {
            assert!(parse_realm(bad).is_err(), "{bad}");
        }
    }
}
