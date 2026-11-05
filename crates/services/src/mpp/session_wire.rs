//! Wire format of the `session` intent (TIP-1034 payment channels on Tempo): the challenge
//! request, the four credential payloads (`open`, `topUp`, `voucher`, `close`), the
//! session snapshot and the receipt. Pure data and parsing: no I/O, no store, no chain.
//!
//! Every field name and shape is the one the official TypeScript client (`mppx`,
//! `tempo/session/{client,server}`) reads and writes, so an unmodified `mppx` client
//! interoperates:
//!
//! - `request` = `base64url(JCS({amount, currency, methodDetails{chainId, escrowContract,
//!   minVoucherDelta, sessionProtocol, sessionSnapshot?}, recipient, suggestedDeposit,
//!   unitType}))`. No `feePayer`, `feeToken`, `operator` or `machineTokenEnabled` is
//!   advertised.
//! - Credential payloads are discriminated on `action`. `open` and `topUp` carry
//!   `type: "transaction"`; `voucher` and `close` carry no `type`. `authorizedSigner` at the
//!   top level of an `open` is ignored (the descriptor's own is authoritative);
//!   `settlementRoute` is refused.
//! - The receipt is `base64url(JCS({acceptedCumulative, challengeId, channelId, intent,
//!   method, reference, spent, status, timestamp, txHash?, units}))`; `reference` is the
//!   channel id, not a transaction hash.
//!
//! Parsing is strict and bounded: hex fields have exact lengths, signatures are exactly 65
//! bytes, amounts are canonical decimal strings that fit a `uint96`, raw transactions are
//! at most [`MAX_RAW_TX_BYTES`]. Nothing here logs; errors name a closed reason and never
//! echo input.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::abi::ChannelDescriptor;
use super::eip712::MAX_UINT96;
use super::protocol::{
    b64url_decode, b64url_encode, canonical_json, compute_challenge_id, format_timestamp,
    opaque_for, BoundFields, Challenge, ProtocolError, INTENT_SESSION, MAX_RAW_TX_BYTES, METHOD,
    OPAQUE_NONCE_BYTES, TEMPO_TX_TYPE_ID,
};
use super::rlp;
use super::secp256k1::SIGNATURE_LEN;

/// `methodDetails.sessionProtocol`: the TIP-1034 precompile-backed protocol.
pub const SESSION_PROTOCOL: &str = "v2";
/// `unitType`: one unit is one LLM token.
pub const UNIT_TYPE: &str = "llm_token";

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Why session wire data was refused. A closed set; no input is echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Missing, mistyped or ill-formed data; the reason is a fixed snake_case code.
    Malformed(&'static str),
    /// The `action` is not one of `open`, `topUp`, `voucher`, `close`.
    UnknownAction,
    /// Well formed, but a feature this server does not offer (`settlementRoute`).
    Unsupported(&'static str),
    /// A signature that is not exactly 65 bytes.
    UnsupportedSignature,
    /// An amount that is not positive where required, or does not fit a `uint96`.
    AmountOutOfRange,
}

impl WireError {
    /// A fixed snake_case code, safe to log and to map to a problem detail.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Malformed(reason) | Self::Unsupported(reason) => reason,
            Self::UnknownAction => "unknown_action",
            Self::UnsupportedSignature => "unsupported_signature",
            Self::AmountOutOfRange => "amount_out_of_range",
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session wire: {}", self.reason())
    }
}

impl std::error::Error for WireError {}

impl From<ProtocolError> for WireError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Malformed(reason) => Self::Malformed(reason),
            ProtocolError::MethodUnsupported => Self::Unsupported("method_unsupported"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Field parsers
// ---------------------------------------------------------------------------------------

/// `0x` + lowercase hex of `bytes`.
pub fn hex_prefixed(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// `0x` + exactly `2 * N` hex digits (either case: `mppx` sends checksummed addresses).
fn hex_exact<const N: usize>(value: &str) -> Result<[u8; N], WireError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(WireError::Malformed("hex_prefix"))?;
    if digits.len() != N * 2 {
        return Err(WireError::Malformed("hex_length"));
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(digits, &mut out).map_err(|_| WireError::Malformed("hex_digits"))?;
    Ok(out)
}

/// A secp256k1 signature: exactly [`SIGNATURE_LEN`] bytes. Nothing else (compact,
/// Tempo envelopes) is accepted.
fn signature(value: &str) -> Result<[u8; SIGNATURE_LEN], WireError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(WireError::Malformed("hex_prefix"))?;
    if digits.len() != SIGNATURE_LEN * 2 {
        return Err(WireError::UnsupportedSignature);
    }
    hex_exact::<SIGNATURE_LEN>(value)
}

/// A canonical decimal amount (ASCII digits, no sign, no leading zero) that fits a
/// `uint96`.
fn amount(value: &str) -> Result<u128, WireError> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(WireError::Malformed("invalid_amount"));
    }
    // `u128::MAX` has 39 digits; anything longer cannot be a uint96 either.
    if value.len() > 39 {
        return Err(WireError::AmountOutOfRange);
    }
    match value.parse::<u128>() {
        Ok(parsed) if parsed <= MAX_UINT96 => Ok(parsed),
        _ => Err(WireError::AmountOutOfRange),
    }
}

/// A configured amount: at most a `uint96`, and positive when `positive`.
fn check_configured(value: u128, positive: bool) -> Result<(), WireError> {
    if value > MAX_UINT96 || (positive && value == 0) {
        return Err(WireError::AmountOutOfRange);
    }
    Ok(())
}

/// A raw Tempo transaction: `0x76` then one exact RLP list, at most [`MAX_RAW_TX_BYTES`].
/// The length is checked before decoding. The full decode is `tempo_tx::decode_signed`.
fn transaction(value: &str) -> Result<Vec<u8>, WireError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(WireError::Malformed("hex_prefix"))?;
    if digits.len() > MAX_RAW_TX_BYTES * 2 {
        return Err(WireError::Malformed("transaction_too_large"));
    }
    let raw = hex::decode(digits).map_err(|_| WireError::Malformed("hex_digits"))?;
    if raw.first() != Some(&TEMPO_TX_TYPE_ID) {
        return Err(WireError::Malformed("not_tempo_transaction"));
    }
    if !raw.get(1..).is_some_and(rlp::list_spans_exactly) {
        return Err(WireError::Malformed("invalid_transaction_encoding"));
    }
    Ok(raw)
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, WireError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(WireError::Malformed("missing_field"))
}

// ---------------------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDescriptor {
    payer: String,
    payee: String,
    operator: String,
    token: String,
    salt: String,
    authorized_signer: String,
    expiring_nonce_hash: String,
}

impl RawDescriptor {
    fn into_descriptor(self) -> Result<ChannelDescriptor, WireError> {
        Ok(ChannelDescriptor {
            payer: hex_exact(&self.payer)?,
            payee: hex_exact(&self.payee)?,
            operator: hex_exact(&self.operator)?,
            token: hex_exact(&self.token)?,
            salt: hex_exact(&self.salt)?,
            authorized_signer: hex_exact(&self.authorized_signer)?,
            expiring_nonce_hash: hex_exact(&self.expiring_nonce_hash)?,
        })
    }
}

/// Read a descriptor object: exactly `payer, payee, operator, token, salt, authorizedSigner,
/// expiringNonceHash`, addresses 20 bytes, salt and nonce hash 32.
pub fn parse_descriptor(value: &Value) -> Result<ChannelDescriptor, WireError> {
    RawDescriptor::deserialize(value)
        .map_err(|_| WireError::Malformed("invalid_descriptor"))?
        .into_descriptor()
}

/// The descriptor as `mppx` writes it (lowercase hex).
pub fn descriptor_json(descriptor: &ChannelDescriptor) -> Value {
    let mut object = Map::new();
    object.insert(
        "authorizedSigner".to_string(),
        Value::String(hex_prefixed(&descriptor.authorized_signer)),
    );
    object.insert(
        "expiringNonceHash".to_string(),
        Value::String(hex_prefixed(&descriptor.expiring_nonce_hash)),
    );
    object.insert(
        "operator".to_string(),
        Value::String(hex_prefixed(&descriptor.operator)),
    );
    object.insert(
        "payee".to_string(),
        Value::String(hex_prefixed(&descriptor.payee)),
    );
    object.insert(
        "payer".to_string(),
        Value::String(hex_prefixed(&descriptor.payer)),
    );
    object.insert(
        "salt".to_string(),
        Value::String(hex_prefixed(&descriptor.salt)),
    );
    object.insert(
        "token".to_string(),
        Value::String(hex_prefixed(&descriptor.token)),
    );
    Value::Object(object)
}

// ---------------------------------------------------------------------------------------
// Credential payloads
// ---------------------------------------------------------------------------------------

/// The four session actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Open,
    TopUp,
    Voucher,
    Close,
}

impl SessionAction {
    /// The wire value of `action`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::TopUp => "topUp",
            Self::Voucher => "voucher",
            Self::Close => "close",
        }
    }

    /// `open`, `topUp` and `close` manage the channel and never run a request; only a
    /// `voucher` does.
    pub fn is_management(self) -> bool {
        !matches!(self, Self::Voucher)
    }
}

/// A session credential payload, parsed. Whether the descriptor hashes to `channel_id`,
/// what the transaction contains and whether the signatures verify is for the caller.
#[derive(Clone, PartialEq, Eq)]
pub enum SessionPayload {
    Open {
        channel_id: [u8; 32],
        /// The signed `0x76` transaction that opens the channel.
        transaction: Vec<u8>,
        descriptor: ChannelDescriptor,
        /// The first voucher's cumulative amount.
        cumulative_amount: u128,
        /// The first voucher's signature.
        signature: [u8; SIGNATURE_LEN],
    },
    TopUp {
        channel_id: [u8; 32],
        transaction: Vec<u8>,
        descriptor: ChannelDescriptor,
        additional_deposit: u128,
    },
    Voucher {
        channel_id: [u8; 32],
        descriptor: ChannelDescriptor,
        cumulative_amount: u128,
        signature: [u8; SIGNATURE_LEN],
    },
    Close {
        channel_id: [u8; 32],
        descriptor: ChannelDescriptor,
        cumulative_amount: u128,
        signature: [u8; SIGNATURE_LEN],
        /// EIP-712 `CloseAuthorization` signature by the payer or the voucher signer.
        close_signature: [u8; SIGNATURE_LEN],
    },
}

impl fmt::Debug for SessionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionPayload({})", self.action().as_str())
    }
}

impl SessionPayload {
    /// Read the `payload` of a session credential (`SessionCredential::payload`).
    pub fn parse(payload: &Value) -> Result<Self, WireError> {
        let object = payload
            .as_object()
            .ok_or(WireError::Malformed("payload_not_object"))?;
        let action = match object.get("action") {
            Some(Value::String(name)) => match name.as_str() {
                "open" => SessionAction::Open,
                "topUp" => SessionAction::TopUp,
                "voucher" => SessionAction::Voucher,
                "close" => SessionAction::Close,
                _ => return Err(WireError::UnknownAction),
            },
            Some(_) => return Err(WireError::UnknownAction),
            None => return Err(WireError::Malformed("missing_action")),
        };
        // Present with any value, `null` included: this server settles to the payee only.
        if object.contains_key("settlementRoute") || object.contains_key("settlement_route") {
            return Err(WireError::Unsupported("settlement_route"));
        }
        let channel_id = hex_exact::<32>(text(object, "channelId")?)?;
        let descriptor = parse_descriptor(
            object
                .get("descriptor")
                .ok_or(WireError::Malformed("missing_descriptor"))?,
        )?;
        match action {
            SessionAction::Open => {
                require_transaction_type(object)?;
                Ok(Self::Open {
                    channel_id,
                    transaction: transaction(text(object, "transaction")?)?,
                    descriptor,
                    cumulative_amount: amount(text(object, "cumulativeAmount")?)?,
                    signature: signature(text(object, "signature")?)?,
                })
            }
            SessionAction::TopUp => {
                require_transaction_type(object)?;
                Ok(Self::TopUp {
                    channel_id,
                    transaction: transaction(text(object, "transaction")?)?,
                    descriptor,
                    additional_deposit: amount(text(object, "additionalDeposit")?)?,
                })
            }
            SessionAction::Voucher => Ok(Self::Voucher {
                channel_id,
                descriptor,
                cumulative_amount: amount(text(object, "cumulativeAmount")?)?,
                signature: signature(text(object, "signature")?)?,
            }),
            SessionAction::Close => Ok(Self::Close {
                channel_id,
                descriptor,
                cumulative_amount: amount(text(object, "cumulativeAmount")?)?,
                signature: signature(text(object, "signature")?)?,
                close_signature: signature(text(object, "closeSignature")?)?,
            }),
        }
    }

    pub fn action(&self) -> SessionAction {
        match self {
            Self::Open { .. } => SessionAction::Open,
            Self::TopUp { .. } => SessionAction::TopUp,
            Self::Voucher { .. } => SessionAction::Voucher,
            Self::Close { .. } => SessionAction::Close,
        }
    }

    pub fn channel_id(&self) -> &[u8; 32] {
        match self {
            Self::Open { channel_id, .. }
            | Self::TopUp { channel_id, .. }
            | Self::Voucher { channel_id, .. }
            | Self::Close { channel_id, .. } => channel_id,
        }
    }

    pub fn descriptor(&self) -> &ChannelDescriptor {
        match self {
            Self::Open { descriptor, .. }
            | Self::TopUp { descriptor, .. }
            | Self::Voucher { descriptor, .. }
            | Self::Close { descriptor, .. } => descriptor,
        }
    }
}

/// `open` and `topUp` carry `type: "transaction"`, the only type `mppx` writes for them.
fn require_transaction_type(object: &Map<String, Value>) -> Result<(), WireError> {
    match object.get("type") {
        Some(Value::String(kind)) if kind == "transaction" => Ok(()),
        Some(_) => Err(WireError::Malformed("invalid_type")),
        None => Err(WireError::Malformed("missing_type")),
    }
}

// ---------------------------------------------------------------------------------------
// Session snapshot
// ---------------------------------------------------------------------------------------

/// The highest voucher the server accepted on a channel.
#[derive(Clone, PartialEq, Eq)]
pub struct HighestVoucher {
    pub channel_id: [u8; 32],
    pub cumulative_amount: u128,
    pub signature: [u8; SIGNATURE_LEN],
}

impl fmt::Debug for HighestVoucher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HighestVoucher { .. }")
    }
}

/// `methodDetails.sessionSnapshot`: the server's view of a channel, sent with a 402 that
/// asks for a higher voucher. `mppx` signs a voucher for exactly `required_cumulative` (or
/// tops up when it exceeds `deposit`), after checking `highest_voucher` against
/// `accepted_cumulative`, `spent <= accepted_cumulative` and `settled <= deposit`.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub accepted_cumulative: u128,
    pub chain_id: u64,
    pub channel_id: [u8; 32],
    pub deposit: u128,
    pub descriptor: ChannelDescriptor,
    pub escrow: [u8; 20],
    pub highest_voucher: HighestVoucher,
    pub required_cumulative: u128,
    pub settled: u128,
    pub spent: u128,
    /// Tokens served on the channel (a JSON number, as `mppx` types it).
    pub units: u64,
}

impl fmt::Debug for SessionSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionSnapshot { .. }")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawVoucher {
    channel_id: String,
    cumulative_amount: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSnapshot {
    accepted_cumulative: String,
    chain_id: u64,
    channel_id: String,
    deposit: String,
    descriptor: RawDescriptor,
    escrow: String,
    highest_voucher: RawVoucher,
    required_cumulative: String,
    settled: String,
    spent: String,
    units: u64,
}

fn insert(object: &mut Map<String, Value>, key: &str, value: Value) {
    object.insert(key.to_string(), value);
}

fn decimal(value: u128) -> Value {
    Value::String(value.to_string())
}

impl SessionSnapshot {
    /// The snapshot as `mppx`'s server writes it. No `closeRequestedAt`: a channel with a
    /// close request is never snapshotted.
    pub fn to_json(&self) -> Value {
        let mut voucher = Map::new();
        insert(
            &mut voucher,
            "channelId",
            Value::String(hex_prefixed(&self.highest_voucher.channel_id)),
        );
        insert(
            &mut voucher,
            "cumulativeAmount",
            decimal(self.highest_voucher.cumulative_amount),
        );
        insert(
            &mut voucher,
            "signature",
            Value::String(hex_prefixed(&self.highest_voucher.signature)),
        );
        let mut object = Map::new();
        insert(
            &mut object,
            "acceptedCumulative",
            decimal(self.accepted_cumulative),
        );
        insert(&mut object, "chainId", Value::from(self.chain_id));
        insert(
            &mut object,
            "channelId",
            Value::String(hex_prefixed(&self.channel_id)),
        );
        insert(&mut object, "deposit", decimal(self.deposit));
        insert(&mut object, "descriptor", descriptor_json(&self.descriptor));
        insert(
            &mut object,
            "escrow",
            Value::String(hex_prefixed(&self.escrow)),
        );
        insert(&mut object, "highestVoucher", Value::Object(voucher));
        insert(
            &mut object,
            "requiredCumulative",
            decimal(self.required_cumulative),
        );
        insert(&mut object, "settled", decimal(self.settled));
        insert(&mut object, "spent", decimal(self.spent));
        insert(&mut object, "units", Value::from(self.units));
        Value::Object(object)
    }

    fn from_value(value: &Value) -> Result<Self, WireError> {
        let raw = RawSnapshot::deserialize(value)
            .map_err(|_| WireError::Malformed("invalid_snapshot"))?;
        Ok(Self {
            accepted_cumulative: amount(&raw.accepted_cumulative)?,
            chain_id: raw.chain_id,
            channel_id: hex_exact(&raw.channel_id)?,
            deposit: amount(&raw.deposit)?,
            descriptor: raw.descriptor.into_descriptor()?,
            escrow: hex_exact(&raw.escrow)?,
            highest_voucher: HighestVoucher {
                channel_id: hex_exact(&raw.highest_voucher.channel_id)?,
                cumulative_amount: amount(&raw.highest_voucher.cumulative_amount)?,
                signature: signature(&raw.highest_voucher.signature)?,
            },
            required_cumulative: amount(&raw.required_cumulative)?,
            settled: amount(&raw.settled)?,
            spent: amount(&raw.spent)?,
            units: raw.units,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Challenge request
// ---------------------------------------------------------------------------------------

/// The terms a `session` challenge offers: the `request` of the challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTerms {
    /// The price of one unit (one LLM token), in token base units. Positive.
    pub amount: u128,
    /// The TIP-20 token.
    pub currency: [u8; 20],
    /// The payee: the address the server settles with.
    pub recipient: [u8; 20],
    /// The deposit a client is advised to open with. Positive.
    pub suggested_deposit: u128,
    pub chain_id: u64,
    /// The channel reserve precompile.
    pub escrow: [u8; 20],
    /// The smallest increase of the cumulative amount a voucher may carry.
    pub min_voucher_delta: u128,
    /// The channel's state, when the challenge answers a request that needs a higher voucher.
    pub snapshot: Option<SessionSnapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawRequest {
    amount: String,
    currency: String,
    method_details: RawDetails,
    recipient: String,
    suggested_deposit: String,
    unit_type: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDetails {
    chain_id: u64,
    escrow_contract: String,
    min_voucher_delta: String,
    session_protocol: String,
    #[serde(default)]
    session_snapshot: Option<Value>,
}

impl SessionTerms {
    fn validate(&self) -> Result<(), WireError> {
        check_configured(self.amount, true)?;
        check_configured(self.suggested_deposit, true)?;
        check_configured(self.min_voucher_delta, false)
    }

    /// The request object, keys as `mppx`'s server emits them.
    pub fn request_value(&self) -> Result<Value, WireError> {
        self.validate()?;
        let mut details = Map::new();
        insert(&mut details, "chainId", Value::from(self.chain_id));
        insert(
            &mut details,
            "escrowContract",
            Value::String(hex_prefixed(&self.escrow)),
        );
        insert(
            &mut details,
            "minVoucherDelta",
            decimal(self.min_voucher_delta),
        );
        insert(
            &mut details,
            "sessionProtocol",
            Value::String(SESSION_PROTOCOL.to_string()),
        );
        if let Some(snapshot) = &self.snapshot {
            insert(&mut details, "sessionSnapshot", snapshot.to_json());
        }
        let mut object = Map::new();
        insert(&mut object, "amount", decimal(self.amount));
        insert(
            &mut object,
            "currency",
            Value::String(hex_prefixed(&self.currency)),
        );
        insert(&mut object, "methodDetails", Value::Object(details));
        insert(
            &mut object,
            "recipient",
            Value::String(hex_prefixed(&self.recipient)),
        );
        insert(
            &mut object,
            "suggestedDeposit",
            decimal(self.suggested_deposit),
        );
        insert(
            &mut object,
            "unitType",
            Value::String(UNIT_TYPE.to_string()),
        );
        Ok(Value::Object(object))
    }

    /// The request as JCS (RFC 8785) JSON.
    pub fn request_json(&self) -> Result<String, WireError> {
        Ok(canonical_json(&self.request_value()?)?)
    }

    /// The challenge's `request` parameter: base64url of [`Self::request_json`].
    pub fn to_b64(&self) -> Result<String, WireError> {
        Ok(b64url_encode(self.request_json()?.as_bytes()))
    }

    /// Strict read of a challenge's `request`, as a credential echoes it: exactly the fields
    /// [`Self::request_value`] writes, `sessionProtocol` `v2`, `unitType` `llm_token`, extra
    /// keys refused (so a client cannot smuggle a fee payer or operator past the HMAC-bound
    /// value; the HMAC covers the string, this checks its meaning).
    pub fn from_b64(request: &str) -> Result<Self, WireError> {
        let bytes = b64url_decode(request)?;
        let raw: RawRequest =
            serde_json::from_slice(&bytes).map_err(|_| WireError::Malformed("invalid_request"))?;
        if raw.unit_type != UNIT_TYPE {
            return Err(WireError::Malformed("invalid_unit_type"));
        }
        if raw.method_details.session_protocol != SESSION_PROTOCOL {
            return Err(WireError::Malformed("invalid_session_protocol"));
        }
        let snapshot = raw
            .method_details
            .session_snapshot
            .as_ref()
            .map(SessionSnapshot::from_value)
            .transpose()?;
        let terms = Self {
            amount: amount(&raw.amount)?,
            currency: hex_exact(&raw.currency)?,
            recipient: hex_exact(&raw.recipient)?,
            suggested_deposit: amount(&raw.suggested_deposit)?,
            chain_id: raw.method_details.chain_id,
            escrow: hex_exact(&raw.method_details.escrow_contract)?,
            min_voucher_delta: amount(&raw.method_details.min_voucher_delta)?,
            snapshot,
        };
        terms.validate()?;
        Ok(terms)
    }

    /// Whether `other` offers the same terms, ignoring the snapshot (which changes per
    /// channel and per request, so an echoed request is compared with configuration this way).
    pub fn same_offer(&self, other: &Self) -> bool {
        Self {
            snapshot: None,
            ..self.clone()
        } == Self {
            snapshot: None,
            ..other.clone()
        }
    }
}

/// Build a `session` challenge: `intent="session"`, the request of `terms`, and no `digest`
/// (a session challenge is reused across requests, so it is bound to the route and a nonce
/// only). The id is `HMAC-SHA256(secret, realm|tempo|session|request|expires||opaque)`.
pub fn session_challenge(
    secret: &[u8],
    realm: &str,
    terms: &SessionTerms,
    expires: DateTime<Utc>,
    route: &str,
    nonce: &[u8; OPAQUE_NONCE_BYTES],
) -> Result<Challenge, WireError> {
    let request = terms.to_b64()?;
    let opaque = opaque_for(route, nonce)?;
    let expires_string = format_timestamp(expires);
    let id = compute_challenge_id(
        secret,
        &BoundFields {
            realm,
            method: METHOD,
            intent: INTENT_SESSION,
            request: &request,
            expires: Some(&expires_string),
            digest: None,
            opaque: Some(&opaque),
        },
    )?;
    Ok(Challenge {
        id,
        realm: realm.to_string(),
        request,
        expires,
        digest: String::new(),
        opaque,
    })
}

// ---------------------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------------------

/// The `Payment-Receipt` of a session action.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionReceipt {
    /// The id of the challenge the credential answered.
    pub challenge_id: String,
    pub channel_id: [u8; 32],
    /// The highest accepted cumulative voucher amount.
    pub accepted_cumulative: u128,
    pub spent: u128,
    pub units: u64,
    /// The transaction an `open`, `topUp` or `close` sent.
    pub tx_hash: Option<[u8; 32]>,
}

impl fmt::Debug for SessionReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionReceipt { .. }")
    }
}

impl SessionReceipt {
    /// `base64url(JCS(receipt))`. The timestamp has millisecond precision, like `mppx`'s
    /// `toISOString()`; `reference` is the channel id.
    pub fn header(&self, at: DateTime<Utc>) -> Result<String, WireError> {
        let channel_id = hex_prefixed(&self.channel_id);
        let mut object = Map::new();
        insert(
            &mut object,
            "acceptedCumulative",
            decimal(self.accepted_cumulative),
        );
        insert(
            &mut object,
            "challengeId",
            Value::String(self.challenge_id.clone()),
        );
        insert(&mut object, "channelId", Value::String(channel_id.clone()));
        insert(
            &mut object,
            "intent",
            Value::String(INTENT_SESSION.to_string()),
        );
        insert(&mut object, "method", Value::String(METHOD.to_string()));
        insert(&mut object, "reference", Value::String(channel_id));
        insert(&mut object, "spent", decimal(self.spent));
        insert(&mut object, "status", Value::String("success".to_string()));
        insert(
            &mut object,
            "timestamp",
            Value::String(at.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        if let Some(tx_hash) = &self.tx_hash {
            insert(&mut object, "txHash", Value::String(hex_prefixed(tx_hash)));
        }
        insert(&mut object, "units", Value::from(self.units));
        Ok(b64url_encode(
            canonical_json(&Value::Object(object))?.as_bytes(),
        ))
    }
}

#[cfg(test)]
#[path = "session_wire_tests.rs"]
mod tests;
