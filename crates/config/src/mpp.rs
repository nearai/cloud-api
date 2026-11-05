//! Machine Payments Protocol (MPP) configuration.
//!
//! MPP is off by default. With `MPP_ENABLED` unset or false nothing else is read or
//! validated, so a deployment without MPP can never fail to boot because of it. With it
//! true, every required value must be present and valid or startup fails.
//!
//! Secrets (`MPP_SECRET_KEY`, `MPP_SERVICE_API_KEY`) follow the repo pattern: a value in
//! the environment or a file path in the matching `*_FILE` variable, redacted in `Debug`.
//!
//! The `session` intent (payment channels, billed per token) is on for a deployment when at
//! least one model in `MPP_MODEL_PRICES` has a `token_price`; it then also needs the payee's
//! signer key (`MPP_TEMPO_SIGNER_KEY`) and reads the `MPP_SESSION_*` / `MPP_SETTLE_*` /
//! `MPP_CLOSE_*` tuning below. Those are validated whenever MPP is enabled, used or not.

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
/// Deposit suggested in a session challenge, in token base units (1 pathUSD at 6 decimals).
pub const DEFAULT_MPP_SESSION_SUGGESTED_DEPOSIT: u128 = 1_000_000;
/// The channel precompile stores a deposit as a `uint96`.
pub const MAX_MPP_SESSION_DEPOSIT: u128 = (1u128 << 96) - 1;
pub const DEFAULT_MPP_SETTLE_INTERVAL_SECS: u64 = 3600;
pub const MIN_MPP_SETTLE_INTERVAL_SECS: u64 = 60;
/// Spent-since-last-settlement that triggers a settlement, in token base units.
pub const DEFAULT_MPP_SETTLE_AMOUNT: u128 = 5_000_000;
pub const DEFAULT_MPP_CLOSE_SUPERVISION_SECS: u64 = 60;
pub const MIN_MPP_CLOSE_SUPERVISION_SECS: u64 = 10;
pub const MAX_MPP_CLOSE_SUPERVISION_SECS: u64 = 300;
/// Without `MPP_SESSION_MIN_VOUCHER_DELTA`, a voucher must rise by this many tokens' worth.
pub const DEFAULT_MPP_MIN_VOUCHER_DELTA_TOKENS: u128 = 64;

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
    /// Price per model name: a flat per-request price, a per-token price, or both.
    pub model_prices: BTreeMap<String, MppModelPrice>,
    /// The `session` intent. `Some` exactly when a model has a `token_price`.
    pub session: Option<MppSessionSettings>,
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
            .field("session", &self.session)
            .finish()
    }
}

/// Settings of the `session` intent (TIP-1034 payment channels).
#[derive(Clone, PartialEq, Eq)]
pub struct MppSessionSettings {
    /// The payee's secp256k1 key: 32 bytes of hex, `0x` optional. Present whenever sessions
    /// are on. The scalar is checked where the signer is built. Never logged.
    pub signer_key: Option<String>,
    /// Smallest rise, in token base units, between two vouchers of a channel. `None` means
    /// the model's `token_price` times [`DEFAULT_MPP_MIN_VOUCHER_DELTA_TOKENS`], computed
    /// where it is used.
    pub min_voucher_delta: Option<u128>,
    /// Deposit a session challenge suggests to the payer, in token base units.
    pub suggested_deposit: u128,
    /// Longest time between two settlements of a channel with unsettled spend.
    pub settle_interval_secs: u64,
    /// Settle a channel once this much was spent since its last settlement.
    pub settle_amount: u128,
    /// How often the close supervision looks at channels with a close in flight.
    pub close_supervision_secs: u64,
}

impl fmt::Debug for MppSessionSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MppSessionSettings")
            .field(
                "signer_key",
                &self.signer_key.as_ref().map(|_| "<redacted>"),
            )
            .field("min_voucher_delta", &self.min_voucher_delta)
            .field("suggested_deposit", &self.suggested_deposit)
            .field("settle_interval_secs", &self.settle_interval_secs)
            .field("settle_amount", &self.settle_amount)
            .field("close_supervision_secs", &self.close_supervision_secs)
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

/// One model's price and the request limits that keep the price honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MppModelPrice {
    /// Flat price per request (`charge` intent) in token base units. Greater than zero, or
    /// zero for a model that only has a `token_price`: it cannot be paid per request.
    pub amount: u128,
    /// Price per token (`session` intent) in token base units. Greater than zero when set.
    pub token_price: Option<u128>,
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
        let session = MppSessionSettings::from_env(&model_prices)?;
        Ok(Self {
            realm,
            secret_key,
            service_api_key,
            challenge_ttl_secs,
            tempo,
            model_prices,
            session,
        })
    }
}

impl MppSessionSettings {
    /// Parse and validate every session variable; `Some` only when a model has a
    /// `token_price`, which is also when the signer key becomes required.
    fn from_env(model_prices: &BTreeMap<String, MppModelPrice>) -> Result<Option<Self>, String> {
        let signer_key = parse_signer_key()?;
        let min_voucher_delta = optional_amount_env("MPP_SESSION_MIN_VOUCHER_DELTA")?;
        let suggested_deposit = amount_env(
            "MPP_SESSION_SUGGESTED_DEPOSIT",
            DEFAULT_MPP_SESSION_SUGGESTED_DEPOSIT,
        )?;
        if suggested_deposit > MAX_MPP_SESSION_DEPOSIT {
            return Err("MPP_SESSION_SUGGESTED_DEPOSIT must fit in 96 bits".to_string());
        }
        let settle_interval_secs =
            parse_u64_env("MPP_SETTLE_INTERVAL_SECS", DEFAULT_MPP_SETTLE_INTERVAL_SECS)?;
        if settle_interval_secs < MIN_MPP_SETTLE_INTERVAL_SECS {
            return Err(format!(
                "MPP_SETTLE_INTERVAL_SECS must be at least {MIN_MPP_SETTLE_INTERVAL_SECS}"
            ));
        }
        let settle_amount = amount_env("MPP_SETTLE_AMOUNT", DEFAULT_MPP_SETTLE_AMOUNT)?;
        let close_supervision_secs = parse_u64_env(
            "MPP_CLOSE_SUPERVISION_SECS",
            DEFAULT_MPP_CLOSE_SUPERVISION_SECS,
        )?;
        if !(MIN_MPP_CLOSE_SUPERVISION_SECS..=MAX_MPP_CLOSE_SUPERVISION_SECS)
            .contains(&close_supervision_secs)
        {
            return Err(format!(
                "MPP_CLOSE_SUPERVISION_SECS must be between {MIN_MPP_CLOSE_SUPERVISION_SECS} and {MAX_MPP_CLOSE_SUPERVISION_SECS}"
            ));
        }
        if !model_prices
            .values()
            .any(|price| price.token_price.is_some())
        {
            return Ok(None);
        }
        if signer_key.is_none() {
            return Err(
                "MPP_TEMPO_SIGNER_KEY or MPP_TEMPO_SIGNER_KEY_FILE must be set when a model in MPP_MODEL_PRICES has a token_price"
                    .to_string(),
            );
        }
        Ok(Some(Self {
            signer_key,
            min_voucher_delta,
            suggested_deposit,
            settle_interval_secs,
            settle_amount,
            close_supervision_secs,
        }))
    }
}

/// The payee key from `MPP_TEMPO_SIGNER_KEY` or the trimmed content of
/// `MPP_TEMPO_SIGNER_KEY_FILE`; both set is an error. Only the shape is checked here (32 bytes
/// of hex, `0x` optional); an error never carries the value.
fn parse_signer_key() -> Result<Option<String>, String> {
    const VALUE: &str = "MPP_TEMPO_SIGNER_KEY";
    const FILE: &str = "MPP_TEMPO_SIGNER_KEY_FILE";
    let raw = match (non_empty_env(FILE), non_empty_env(VALUE)) {
        (Some(_), Some(_)) => return Err(format!("set only one of {VALUE} and {FILE}")),
        (Some(path), None) => std::fs::read_to_string(&path)
            .map(|content| content.trim().to_string())
            .map_err(|_| format!("failed to read {FILE}"))?,
        (None, Some(value)) => value,
        (None, None) => return Ok(None),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let digits = raw.strip_prefix("0x").unwrap_or(&raw);
    if digits.len() != 64 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{VALUE} must be 32 bytes of hex (64 hex digits, 0x optional)"
        ));
    }
    Ok(Some(raw))
}

/// A decimal string above zero that fits in 128 bits: digits only, no sign or fraction.
fn parse_amount(raw: &str) -> Option<u128> {
    let digits_only = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
    digits_only
        .then(|| raw.parse::<u128>().ok())
        .flatten()
        .filter(|amount| *amount > 0)
}

fn optional_amount_env(key: &str) -> Result<Option<u128>, String> {
    non_empty_env(key)
        .map(|raw| {
            parse_amount(&raw)
                .ok_or_else(|| format!("{key} must be a decimal integer above zero (128 bits)"))
        })
        .transpose()
}

fn amount_env(key: &str, default: u128) -> Result<u128, String> {
    Ok(optional_amount_env(key)?.unwrap_or(default))
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
    #[serde(default, deserialize_with = "present_string")]
    amount: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    token_price: Option<String>,
    max_tokens: u32,
    max_request_bytes: u32,
}

/// An optional field that must be a string when present (a `null` is refused).
fn present_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
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
        if price.amount.is_none() && price.token_price.is_none() {
            return Err(format!(
                "MPP_MODEL_PRICES entry for {model} needs an amount, a token_price, or both"
            ));
        }
        let amount = match &price.amount {
            Some(raw) => parse_amount(raw).ok_or_else(|| {
                format!("MPP_MODEL_PRICES amount for {model} must be a decimal string above zero that fits in 128 bits")
            })?,
            None => 0,
        };
        let token_price = match &price.token_price {
            Some(raw) => Some(parse_amount(raw).ok_or_else(|| {
                format!("MPP_MODEL_PRICES token_price for {model} must be a decimal string above zero that fits in 128 bits")
            })?),
            None => None,
        };
        if price.max_tokens == 0 || price.max_request_bytes == 0 {
            return Err(format!(
                "MPP_MODEL_PRICES limits for {model} must be greater than zero"
            ));
        }
        prices.insert(
            model,
            MppModelPrice {
                amount,
                token_price,
                max_tokens: price.max_tokens,
                max_request_bytes: price.max_request_bytes,
            },
        );
    }
    Ok(prices)
}

#[cfg(test)]
mod tests;
