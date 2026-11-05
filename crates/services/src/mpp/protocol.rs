//! MPP wire format (IETF draft-httpauth-payment, "Payment" HTTP authentication scheme).
//!
//! Matches the reference implementation (`mpp` 0.18.1, `protocol/core/{binding,headers,
//! credential,receipt}.rs`) byte for byte:
//!
//! - `request`, `opaque` and `Payment-Receipt` are base64url (no padding) of JCS (RFC 8785)
//!   JSON. Decoding accepts padded and unpadded input.
//! - The challenge id is `base64url(HMAC-SHA256(secret, realm|method|intent|request|expires|
//!   digest|opaque))`, absent fields being empty. It is verified in constant time.
//! - `WWW-Authenticate: Payment id=…, realm=…, method=…, intent=…, request=…, expires=…,
//!   digest=…, opaque=…` in that order, every value a quoted string.
//!
//! Nothing here logs. Error values name a closed reason and never echo input.

use std::collections::BTreeMap;
use std::fmt;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::rlp;

/// The authentication scheme name.
pub const SCHEME: &str = "Payment";
/// The only payment method this server speaks.
pub const METHOD: &str = "tempo";
/// The one-shot payment intent.
pub const INTENT_CHARGE: &str = "charge";
/// The channel-backed, metered payment intent (`session_wire`).
pub const INTENT_SESSION: &str = "session";
/// Largest `Authorization` value read before any decoding.
pub const MAX_AUTHORIZATION_LEN: usize = 16 * 1024;
/// Largest raw (decoded) transaction accepted in a `transaction` payload.
pub const MAX_RAW_TX_BYTES: usize = 16 * 1024;
/// The Tempo transaction type byte (`0x76`, account-abstraction transaction).
pub const TEMPO_TX_TYPE_ID: u8 = 0x76;

/// Why a header or credential could not be read. Closed set; no input is echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed: {0}")]
    Malformed(&'static str),
    /// A well-formed credential of a kind this server does not accept (`proof`).
    #[error("method unsupported")]
    MethodUnsupported,
}

// ---------------------------------------------------------------------------------------
// base64url and JCS
// ---------------------------------------------------------------------------------------

pub fn b64url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Lenient in what it accepts (padded or not, standard or URL alphabet), like the reference.
/// Callers bound the input length first.
pub fn b64url_decode(input: &str) -> Result<Vec<u8>, ProtocolError> {
    let normalized: String = input
        .chars()
        .filter(|c| *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    URL_SAFE_NO_PAD
        .decode(normalized.as_bytes())
        .map_err(|_| ProtocolError::Malformed("invalid base64url"))
}

/// Standard (padded) base64, used only inside the body digest.
fn b64_standard(data: &[u8]) -> String {
    STANDARD.encode(data)
}

/// RFC 8785 canonical JSON for the values this server emits: objects, arrays, strings,
/// booleans, null and integers. Keys are sorted by UTF-16 code units. A float is refused:
/// no emitted value is one, and ES6 number formatting is not implemented here.
pub fn canonical_json(value: &Value) -> Result<String, ProtocolError> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), ProtocolError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                return Err(ProtocolError::Malformed("non-integer number"));
            }
        }
        Value::String(s) => write_canonical_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_string(key, out);
                out.push(':');
                write_canonical(item, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// ES6 `JSON.stringify` string form: only `"`, `\` and control characters are escaped.
fn write_canonical_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------------------
// Challenge
// ---------------------------------------------------------------------------------------

/// The fields a challenge id binds. Absent optional fields hash as the empty string.
#[derive(Debug, Clone, Copy)]
pub struct BoundFields<'a> {
    pub realm: &'a str,
    pub method: &'a str,
    pub intent: &'a str,
    pub request: &'a str,
    pub expires: Option<&'a str>,
    pub digest: Option<&'a str>,
    pub opaque: Option<&'a str>,
}

impl BoundFields<'_> {
    fn hmac(&self, secret: &[u8]) -> Result<Hmac<Sha256>, ProtocolError> {
        let input = [
            self.realm,
            self.method,
            self.intent,
            self.request,
            self.expires.unwrap_or(""),
            self.digest.unwrap_or(""),
            self.opaque.unwrap_or(""),
        ]
        .join("|");
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret)
            .map_err(|_| ProtocolError::Malformed("invalid HMAC key"))?;
        mac.update(input.as_bytes());
        Ok(mac)
    }
}

/// `base64url(HMAC-SHA256(secret, join("|", fields)))`.
pub fn compute_challenge_id(
    secret: &[u8],
    fields: &BoundFields<'_>,
) -> Result<String, ProtocolError> {
    Ok(b64url_encode(&fields.hmac(secret)?.finalize().into_bytes()))
}

/// Constant-time check that `id` is the HMAC of `fields`. The id must also be in canonical
/// (unpadded URL-safe) form, so one challenge has exactly one id string.
pub fn verify_challenge_id(secret: &[u8], id: &str, fields: &BoundFields<'_>) -> bool {
    let Ok(tag) = b64url_decode(id) else {
        return false;
    };
    if b64url_encode(&tag) != id {
        return false;
    }
    match fields.hmac(secret) {
        Ok(mac) => mac.verify_slice(&tag).is_ok(),
        Err(_) => false,
    }
}

/// `sha-256=:<standard base64 of SHA-256(body)>:` (RFC 9530 byte-sequence form).
pub fn body_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", b64_standard(&Sha256::digest(body)))
}

/// Bytes of the random nonce inside every challenge's opaque value.
pub const OPAQUE_NONCE_BYTES: usize = 16;

/// The `opaque` value of a challenge: `base64url(JCS({"nonce": <b64url nonce>, "route":
/// <route>}))`. The nonce makes the challenge id unique per issue: two requests with the
/// same body in the same second would otherwise be given the same id, and with it the same
/// memo, so a push-mode payment made for one could be front-run onto the other.
pub fn opaque_for(route: &str, nonce: &[u8; OPAQUE_NONCE_BYTES]) -> Result<String, ProtocolError> {
    let mut object = serde_json::Map::new();
    object.insert("nonce".to_string(), Value::String(b64url_encode(nonce)));
    object.insert("route".to_string(), Value::String(route.to_string()));
    Ok(b64url_encode(
        canonical_json(&Value::Object(object))?.as_bytes(),
    ))
}

/// The `route` an (already HMAC-checked) opaque value names. `None` when the value is not
/// a flat map of strings with a `route`.
pub fn opaque_route(opaque: &str) -> Option<String> {
    let bytes = b64url_decode(opaque).ok()?;
    let mut fields: BTreeMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    fields.remove("route")
}

/// The claim-store key of a payment: `tempo:` and the hex of `HMAC-SHA256(secret, tx hash)`.
/// The transaction hash is public on chain, so storing it would link a ledger row to the
/// payer; the keyed digest cannot be linked without the server secret. Challenge ids are
/// MACs of a `|`-joined text of at least six separators, never of a bare 32-byte hash, so the
/// two uses of the secret do not overlap. A secret rotation also retires every challenge, so
/// no credential can be replayed against a re-keyed ledger.
pub fn payment_key(secret: &[u8], tx_hash: &[u8; 32]) -> Result<String, ProtocolError> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret)
        .map_err(|_| ProtocolError::Malformed("invalid HMAC key"))?;
    mac.update(tx_hash);
    Ok(format!(
        "{METHOD}:{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

/// The charge `request` of a challenge:
/// `{"amount":"<u128>","currency":"0x…","methodDetails":{"chainId":<u64>},"recipient":"0x…"}`.
/// No fee payer, splits, memo or machine token is advertised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeRequest {
    pub amount: u128,
    pub currency: String,
    pub chain_id: u64,
    pub recipient: String,
}

impl ChargeRequest {
    pub fn to_json(&self) -> Result<String, ProtocolError> {
        let mut details = serde_json::Map::new();
        details.insert("chainId".to_string(), Value::from(self.chain_id));
        let mut object = serde_json::Map::new();
        object.insert("amount".to_string(), Value::String(self.amount.to_string()));
        object.insert("currency".to_string(), Value::String(self.currency.clone()));
        object.insert("methodDetails".to_string(), Value::Object(details));
        object.insert(
            "recipient".to_string(),
            Value::String(self.recipient.clone()),
        );
        canonical_json(&Value::Object(object))
    }

    pub fn to_b64(&self) -> Result<String, ProtocolError> {
        Ok(b64url_encode(self.to_json()?.as_bytes()))
    }

    /// Strict read of an echoed `request`: an object with exactly the fields above (extra
    /// keys are refused, so a client cannot smuggle a split, memo or fee payer past the
    /// HMAC-bound value; the HMAC already covers the string, this checks its meaning).
    pub fn from_b64(request: &str) -> Result<Self, ProtocolError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            amount: String,
            currency: String,
            #[serde(rename = "methodDetails")]
            method_details: RawDetails,
            recipient: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawDetails {
            #[serde(rename = "chainId")]
            chain_id: u64,
        }
        let bytes = b64url_decode(request)?;
        let raw: Raw = serde_json::from_slice(&bytes)
            .map_err(|_| ProtocolError::Malformed("invalid request"))?;
        if raw.amount.is_empty() || !raw.amount.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProtocolError::Malformed("invalid amount"));
        }
        let amount = raw
            .amount
            .parse::<u128>()
            .map_err(|_| ProtocolError::Malformed("invalid amount"))?;
        Ok(Self {
            amount,
            currency: raw.currency,
            chain_id: raw.method_details.chain_id,
            recipient: raw.recipient,
        })
    }
}

/// A challenge this server issues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub id: String,
    pub realm: String,
    /// base64url of the JCS request (`ChargeRequest`, or `session_wire::SessionTerms`).
    pub request: String,
    pub expires: DateTime<Utc>,
    /// `sha-256=:…:` of the request body. Empty for a `session` challenge, which is reused
    /// across requests and so binds no body: the header omits it and the id hashes it as
    /// the empty string.
    pub digest: String,
    /// base64url of the JCS `{"route": …}`.
    pub opaque: String,
}

impl Challenge {
    /// The `expires` parameter: RFC 3339, UTC, `Z`, seconds precision.
    pub fn expires_string(&self) -> String {
        format_timestamp(self.expires)
    }

    /// The challenge's intent: a challenge that binds a request body is a `charge`, one
    /// that binds none is a `session`.
    pub fn intent(&self) -> &'static str {
        if self.digest.is_empty() {
            INTENT_SESSION
        } else {
            INTENT_CHARGE
        }
    }

    /// The `WWW-Authenticate` header value. Quoting follows the reference
    /// `format_www_authenticate`: `\` and `"` are escaped, anything outside visible ASCII is
    /// refused (this server never emits any).
    pub fn to_header(&self) -> Result<String, ProtocolError> {
        let expires = self.expires_string();
        let mut parts = vec![
            ("id", self.id.as_str()),
            ("realm", self.realm.as_str()),
            ("method", METHOD),
            ("intent", self.intent()),
            ("request", self.request.as_str()),
            ("expires", expires.as_str()),
        ];
        if !self.digest.is_empty() {
            parts.push(("digest", self.digest.as_str()));
        }
        parts.push(("opaque", self.opaque.as_str()));
        let mut rendered = Vec::with_capacity(parts.len());
        for (name, value) in parts {
            rendered.push(format!("{name}=\"{}\"", escape_quoted(value)?));
        }
        Ok(format!("{SCHEME} {}", rendered.join(", ")))
    }
}

fn escape_quoted(value: &str) -> Result<String, ProtocolError> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' | ' '..='~' => out.push(c),
            _ => return Err(ProtocolError::Malformed("unsupported header character")),
        }
    }
    Ok(out)
}

pub fn format_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------------------
// Credential
// ---------------------------------------------------------------------------------------

/// The challenge a credential echoes. Every field is the client's claim until the HMAC and
/// the config checks in the service have passed.
#[derive(Clone, PartialEq, Eq)]
pub struct ChallengeEcho {
    pub id: String,
    pub realm: String,
    pub method: String,
    pub intent: String,
    pub request: String,
    pub expires: Option<String>,
    pub digest: Option<String>,
    pub opaque: Option<String>,
}

impl ChallengeEcho {
    pub fn bound_fields(&self) -> BoundFields<'_> {
        BoundFields {
            realm: &self.realm,
            method: &self.method,
            intent: &self.intent,
            request: &self.request,
            expires: self.expires.as_deref(),
            digest: self.digest.as_deref(),
            opaque: self.opaque.as_deref(),
        }
    }
}

impl fmt::Debug for ChallengeEcho {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChallengeEcho { .. }")
    }
}

/// What the client paid with.
#[derive(Clone, PartialEq, Eq)]
pub enum PaymentPayload {
    /// A signed Tempo transaction (`0x76…`) the server broadcasts.
    Transaction(Vec<u8>),
    /// The hash of a transaction the client already broadcast.
    Hash([u8; 32]),
}

impl fmt::Debug for PaymentPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transaction(_) => f.write_str("PaymentPayload::Transaction(<redacted>)"),
            Self::Hash(_) => f.write_str("PaymentPayload::Hash(<redacted>)"),
        }
    }
}

/// A parsed `Authorization: Payment …` credential.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub challenge: ChallengeEcho,
    pub payload: PaymentPayload,
    /// `did:pkh:eip155:<chainId>:0x<address>`, when the client sent one.
    pub source: Option<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential { .. }")
    }
}

/// A parsed credential for a `session` challenge. The payload is handed through as the
/// client wrote it; `session_wire::SessionPayload::parse` reads it.
#[derive(Clone, PartialEq)]
pub struct SessionCredential {
    pub challenge: ChallengeEcho,
    pub payload: Value,
    /// `did:pkh:eip155:<chainId>:0x<address>`, when the client sent one.
    pub source: Option<String>,
}

impl fmt::Debug for SessionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCredential { .. }")
    }
}

/// A parsed `Authorization: Payment …` credential of either intent, told apart by the
/// intent the credential echoes.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAuthorization {
    Charge(Credential),
    Session(SessionCredential),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireOpaque {
    Raw(String),
    /// Older mppx clients echo `opaque` as an expanded string map.
    Legacy(BTreeMap<String, String>),
}

#[derive(Deserialize)]
struct WireEcho {
    id: String,
    realm: String,
    method: String,
    intent: String,
    request: String,
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    opaque: Option<WireOpaque>,
}

#[derive(Deserialize)]
struct WireCredential {
    challenge: WireEcho,
    payload: Value,
    #[serde(default)]
    source: Option<String>,
}

/// Whether an `Authorization` value uses the `Payment` scheme (case-insensitive, followed by
/// whitespace or nothing).
pub fn is_payment_scheme(header: &str) -> bool {
    let header = header.trim_start();
    let end = header
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(header.len());
    header
        .get(..end)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
}

/// Parse `Authorization: Payment <base64url(JSON)>`, reading the payload as a `charge`
/// payload whatever intent the credential echoes (the service checks the intent). The value
/// is capped before decoding.
pub fn parse_authorization(header: &str) -> Result<Credential, ProtocolError> {
    let (challenge, payload, source) = parse_envelope(header)?;
    Ok(Credential {
        challenge,
        payload: parse_payload(&payload)?,
        source,
    })
}

/// Parse `Authorization: Payment <base64url(JSON)>` of either intent. A credential that
/// echoes a `session` challenge keeps its payload as the client wrote it (for
/// `session_wire::SessionPayload::parse`); any other is read as by [`parse_authorization`].
pub fn parse_any_authorization(header: &str) -> Result<ParsedAuthorization, ProtocolError> {
    let (challenge, payload, source) = parse_envelope(header)?;
    if challenge.intent == INTENT_SESSION {
        return Ok(ParsedAuthorization::Session(SessionCredential {
            challenge,
            payload,
            source,
        }));
    }
    Ok(ParsedAuthorization::Charge(Credential {
        challenge,
        payload: parse_payload(&payload)?,
        source,
    }))
}

/// The intent-independent part of a credential: the echoed challenge, the raw payload and
/// the `source`. Bounded and checked before anything intent-specific reads the payload.
fn parse_envelope(header: &str) -> Result<(ChallengeEcho, Value, Option<String>), ProtocolError> {
    if header.len() > MAX_AUTHORIZATION_LEN {
        return Err(ProtocolError::Malformed("authorization too long"));
    }
    if !is_payment_scheme(header) {
        return Err(ProtocolError::Malformed("not a Payment credential"));
    }
    let header = header.trim_start();
    let token = header
        .get(SCHEME.len()..)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(ProtocolError::Malformed("missing credential"))?;
    // One token only: this also refuses `Payment x, Bearer y` style mixes.
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'+' | b'/' | b'='))
    {
        return Err(ProtocolError::Malformed("invalid base64url"));
    }
    let json = b64url_decode(token)?;
    let wire: WireCredential = serde_json::from_slice(&json)
        .map_err(|_| ProtocolError::Malformed("invalid credential JSON"))?;

    let opaque = match wire.challenge.opaque {
        None => None,
        Some(WireOpaque::Raw(raw)) => Some(raw),
        Some(WireOpaque::Legacy(map)) => {
            let object = map
                .into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect();
            Some(b64url_encode(
                canonical_json(&Value::Object(object))?.as_bytes(),
            ))
        }
    };
    let echo = ChallengeEcho {
        id: wire.challenge.id,
        realm: wire.challenge.realm,
        method: wire.challenge.method,
        intent: wire.challenge.intent,
        request: wire.challenge.request,
        expires: wire.challenge.expires,
        digest: wire.challenge.digest,
        opaque,
    };
    // The HMAC input is `|`-joined, so no bound field may contain one.
    let bound = [
        Some(echo.realm.as_str()),
        Some(echo.method.as_str()),
        Some(echo.intent.as_str()),
        Some(echo.request.as_str()),
        echo.expires.as_deref(),
        echo.digest.as_deref(),
        echo.opaque.as_deref(),
    ];
    if echo.id.is_empty() || bound.iter().flatten().any(|field| field.contains('|')) {
        return Err(ProtocolError::Malformed("invalid challenge fields"));
    }
    Ok((echo, wire.payload, wire.source))
}

fn parse_payload(payload: &Value) -> Result<PaymentPayload, ProtocolError> {
    let object = payload
        .as_object()
        .ok_or(ProtocolError::Malformed("payload is not an object"))?;
    let field = |name: &'static str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .ok_or(ProtocolError::Malformed("missing payload field"))
    };
    match object.get("type").and_then(Value::as_str) {
        Some("transaction") => {
            let raw = decode_prefixed_hex(field("signature")?, MAX_RAW_TX_BYTES)?;
            if raw.first() != Some(&TEMPO_TX_TYPE_ID) {
                return Err(ProtocolError::Malformed("not a Tempo transaction"));
            }
            if !raw.get(1..).is_some_and(rlp::list_spans_exactly) {
                return Err(ProtocolError::Malformed("invalid transaction encoding"));
            }
            Ok(PaymentPayload::Transaction(raw))
        }
        Some("hash") => {
            let bytes = decode_prefixed_hex(field("hash")?, 32)?;
            let hash: [u8; 32] = bytes
                .try_into()
                .map_err(|_| ProtocolError::Malformed("invalid transaction hash"))?;
            Ok(PaymentPayload::Hash(hash))
        }
        Some("proof") => Err(ProtocolError::MethodUnsupported),
        _ => Err(ProtocolError::Malformed("unknown payload type")),
    }
}

/// `0x` + hex, at most `max_bytes` decoded. The length is checked before decoding.
pub fn decode_prefixed_hex(value: &str, max_bytes: usize) -> Result<Vec<u8>, ProtocolError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(ProtocolError::Malformed("hex must start with 0x"))?;
    if digits.is_empty() || digits.len() > max_bytes.saturating_mul(2) {
        return Err(ProtocolError::Malformed("hex length out of range"));
    }
    hex::decode(digits).map_err(|_| ProtocolError::Malformed("invalid hex"))
}

/// Parse a `source` DID `did:pkh:eip155:<chainId>:0x<address>` (canonical form, as the
/// reference enforces: no leading zeros in the chain id, no extra segments). Returns the
/// chain id and the 20-byte address.
pub fn parse_source_did(source: &str) -> Result<(u64, [u8; 20]), ProtocolError> {
    let invalid = || ProtocolError::Malformed("invalid source");
    let rest = source.strip_prefix("did:pkh:eip155:").ok_or_else(invalid)?;
    let (chain, address) = rest.split_once(':').ok_or_else(invalid)?;
    if chain.is_empty()
        || (chain.len() > 1 && chain.starts_with('0'))
        || !chain.bytes().all(|b| b.is_ascii_digit())
        || address.contains(':')
    {
        return Err(invalid());
    }
    let chain_id: u64 = chain.parse().map_err(|_| invalid())?;
    let bytes = decode_prefixed_hex(address, 20).map_err(|_| invalid())?;
    let address: [u8; 20] = bytes.try_into().map_err(|_| invalid())?;
    Ok((chain_id, address))
}

// ---------------------------------------------------------------------------------------
// Receipt and problem details
// ---------------------------------------------------------------------------------------

/// `Payment-Receipt` value: `base64url(JCS({"method","reference","status","timestamp"}))`.
/// The timestamp has millisecond precision, like the reference clients' `toISOString()`.
pub fn format_receipt(reference: &str, at: DateTime<Utc>) -> Result<String, ProtocolError> {
    let mut object = serde_json::Map::new();
    object.insert("method".to_string(), Value::String(METHOD.to_string()));
    object.insert(
        "reference".to_string(),
        Value::String(reference.to_string()),
    );
    object.insert("status".to_string(), Value::String("success".to_string()));
    object.insert(
        "timestamp".to_string(),
        Value::String(at.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    Ok(b64url_encode(
        canonical_json(&Value::Object(object))?.as_bytes(),
    ))
}

/// Merge `private` into a `Cache-Control` value (receipts MUST be private).
pub fn with_private_cache_control(existing: Option<&str>) -> String {
    match existing {
        Some(value) if !value.trim().is_empty() => {
            if value
                .split(',')
                .any(|directive| directive.trim().eq_ignore_ascii_case("private"))
            {
                value.to_string()
            } else {
                format!("{value}, private")
            }
        }
        _ => "private".to_string(),
    }
}

/// RFC 9457 problem types used by this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemCode {
    PaymentRequired,
    MalformedCredential,
    InvalidChallenge,
    VerificationFailed,
    PaymentExpired,
    MethodUnsupported,
}

impl ProblemCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PaymentRequired => "payment-required",
            Self::MalformedCredential => "malformed-credential",
            Self::InvalidChallenge => "invalid-challenge",
            Self::VerificationFailed => "verification-failed",
            Self::PaymentExpired => "payment-expired",
            Self::MethodUnsupported => "method-unsupported",
        }
    }

    pub fn type_uri(self) -> String {
        format!("https://paymentauth.org/problems/{}", self.as_str())
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::PaymentRequired => "Payment Required",
            Self::MalformedCredential => "Malformed Credential",
            Self::InvalidChallenge => "Invalid Challenge",
            Self::VerificationFailed => "Payment Verification Failed",
            Self::PaymentExpired => "Payment Expired",
            Self::MethodUnsupported => "Method Unsupported",
        }
    }

    /// The `application/problem+json` body. `detail` is a fixed, input-free sentence.
    pub fn body(self, status: u16, detail: &str) -> String {
        serde_json::json!({
            "type": self.type_uri(),
            "title": self.title(),
            "status": status,
            "detail": detail,
        })
        .to_string()
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
