//! [`SessionService`]: the `session` intent over the channel ledger, the Tempo RPC and the
//! payee's signer.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use config::{MppModelPrice, MppSessionSettings};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;

use crate::mpp::abi::{self, ChannelDescriptor, ChannelState, TOP_UP_SELECTOR};
use crate::mpp::channel::{channel_id, voucher_signer};
use crate::mpp::eip712::{voucher_digest, MAX_UINT96};
use crate::mpp::error::{
    MppError, SessionMismatch, TopUpReason, UnavailableReason, VerifyFailure, VoucherReason,
};
use crate::mpp::open_check::{verify_open_transaction, OpenTerms};
use crate::mpp::ports::{
    chain_read_claim_at, chain_read_is_fresh, ChannelLifecycle, ChannelRecord, ChannelRepository,
    ChargeOutcome, NewChannel, ReserveOutcome, TempoRpc, TempoSigner, VoucherOutcome, VoucherRules,
};
use crate::mpp::pricing::{LimitViolation, Price};
use crate::mpp::protocol::{self, Challenge, INTENT_SESSION, METHOD};
use crate::mpp::secp256k1::{recover_address, SIGNATURE_LEN};
use crate::mpp::session_chain::{broadcast, SignedPayeeTx};
use crate::mpp::session_wire::{
    session_challenge, HighestVoucher, SessionPayload, SessionReceipt, SessionSnapshot,
    SessionTerms,
};
use crate::mpp::tempo_tx::{decode_signed, tx_hash};

mod closing;

pub(crate) const CHAIN_READ_FRESHNESS_SECONDS: i64 = 60;
static MANAGEMENT: Semaphore = Semaphore::const_new(8);

/// Immutable offer configuration. Secrets are never included in Debug output.
#[derive(Clone)]
pub struct SessionServiceConfig {
    pub settings: MppSessionSettings,
    pub model_prices: BTreeMap<String, MppModelPrice>,
    pub realm: String,
    pub secret_key: Vec<u8>,
    pub challenge_ttl: Duration,
    pub chain_id: u64,
    pub escrow: [u8; 20],
    pub currency: [u8; 20],
}

impl fmt::Debug for SessionServiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionServiceConfig { .. }")
    }
}

/// A statelessly verified offer and parsed action. Only this service can construct it.
/// The price travels with the action through reservation and metering.
#[derive(Clone)]
pub struct SessionCredential {
    challenge_id: String,
    payload: SessionPayload,
    token_price: u128,
    min_voucher_delta: u128,
}

impl SessionCredential {
    pub fn payload(&self) -> &SessionPayload {
        &self.payload
    }

    pub fn challenge_id(&self) -> &str {
        &self.challenge_id
    }
}

impl fmt::Debug for SessionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCredential { .. }")
    }
}

/// A management action answers HTTP 200 with `{"status":"ok"}` and this receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementReceipt {
    pub receipt: SessionReceipt,
}

/// An atomic reservation made before inference. Keep it for charge or release.
#[derive(Clone)]
pub struct ReservedRequest {
    pub channel_id: [u8; 32],
    pub request_id: Uuid,
    pub reserved: u128,
    pub receipt_preview: SessionReceipt,
    token_price: u128,
}

impl fmt::Debug for ReservedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReservedRequest { .. }")
    }
}

/// Session lifecycle and metering over the domain ports. No HTTP or persistence adapter.
pub struct SessionService {
    config: SessionServiceConfig,
    repo: Arc<dyn ChannelRepository>,
    rpc: Arc<dyn TempoRpc>,
    signer: Arc<dyn TempoSigner>,
}

impl SessionService {
    pub fn new(
        config: SessionServiceConfig,
        repo: Arc<dyn ChannelRepository>,
        rpc: Arc<dyn TempoRpc>,
        signer: Arc<dyn TempoSigner>,
    ) -> Result<Self, MppError> {
        if config.secret_key.len() < config::MIN_MPP_SECRET_KEY_BYTES
            || config.realm.is_empty()
            || config.challenge_ttl.is_zero()
            || chrono::Duration::from_std(config.challenge_ttl).is_err()
        {
            return Err(MppError::Config);
        }
        let service = Self {
            config,
            repo,
            rpc,
            signer,
        };
        for price in service.config.model_prices.values() {
            if price.token_price.is_some() {
                service
                    .terms(
                        &Price {
                            amount: price.amount,
                            token_price: price.token_price,
                        },
                        None,
                    )?
                    .to_b64()
                    .map_err(|_| MppError::Config)?;
            }
        }
        Ok(service)
    }

    fn terms(
        &self,
        price: &Price,
        snapshot: Option<SessionSnapshot>,
    ) -> Result<SessionTerms, MppError> {
        let amount = price
            .token_price
            .filter(|price| *price > 0)
            .ok_or(MppError::Limit(LimitViolation::ModelNotPriced))?;
        let min_voucher_delta = match self.config.settings.min_voucher_delta {
            Some(delta) => delta,
            None => amount
                .checked_mul(config::DEFAULT_MPP_MIN_VOUCHER_DELTA_TOKENS)
                .ok_or(MppError::Config)?,
        };
        Ok(SessionTerms {
            amount,
            currency: self.config.currency,
            recipient: self.signer.address(),
            suggested_deposit: self.config.settings.suggested_deposit,
            chain_id: self.config.chain_id,
            escrow: self.config.escrow,
            min_voucher_delta,
            snapshot,
        })
    }

    pub async fn challenge(
        &self,
        route: &str,
        model_price: &Price,
        snapshot: Option<SessionSnapshot>,
    ) -> Result<Challenge, MppError> {
        let terms = self.terms(model_price, snapshot)?;
        let ttl =
            chrono::Duration::from_std(self.config.challenge_ttl).map_err(|_| MppError::Config)?;
        let deadline = Utc::now().checked_add_signed(ttl).ok_or(MppError::Config)?;
        let expires = Utc
            .timestamp_opt(deadline.timestamp(), 0)
            .single()
            .ok_or(MppError::Config)?;
        session_challenge(
            &self.config.secret_key,
            &self.config.realm,
            &terms,
            expires,
            route,
            &rand::random(),
        )
        .map_err(|_| MppError::Config)
    }

    /// `model_price` must be the current request's resolved price, as obtained from the
    /// charge service's pricing path. A route alone cannot distinguish model prices.
    pub async fn verify_credential(
        &self,
        credential: &protocol::SessionCredential,
        route: &str,
        model_price: &Price,
    ) -> Result<SessionCredential, MppError> {
        self.verify_credential_with_price(credential, route, |_| Ok(*model_price))
    }

    /// Management actions have no model body. Resolve their price from the authenticated
    /// offer, then apply the same verification as an inference credential.
    pub async fn verify_management_credential(
        &self,
        credential: &protocol::SessionCredential,
        route: &str,
    ) -> Result<SessionCredential, MppError> {
        self.verify_credential_with_price(credential, route, |echoed| {
            self.config
                .model_prices
                .values()
                .find(|price| price.token_price == Some(echoed.amount))
                .map(|price| Price {
                    amount: price.amount,
                    token_price: price.token_price,
                })
                .ok_or(MppError::InvalidChallenge("model_price"))
        })
    }

    fn verify_credential_with_price(
        &self,
        credential: &protocol::SessionCredential,
        route: &str,
        resolve_price: impl FnOnce(&SessionTerms) -> Result<Price, MppError>,
    ) -> Result<SessionCredential, MppError> {
        let echo = &credential.challenge;
        if echo.intent != INTENT_SESSION {
            return Err(MppError::InvalidChallenge("intent"));
        }
        if echo.method != METHOD {
            return Err(MppError::MethodUnsupported);
        }
        if echo.realm != self.config.realm {
            return Err(MppError::InvalidChallenge("realm"));
        }
        if echo
            .digest
            .as_deref()
            .is_some_and(|digest| !digest.is_empty())
        {
            return Err(MppError::InvalidChallenge("digest"));
        }
        if !protocol::verify_challenge_id(&self.config.secret_key, &echo.id, &echo.bound_fields()) {
            return Err(MppError::InvalidChallenge("id"));
        }
        let expires = echo
            .expires
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .ok_or(MppError::InvalidChallenge("expires"))?;
        if Utc::now() >= expires {
            return Err(MppError::Expired);
        }
        if echo
            .opaque
            .as_deref()
            .and_then(protocol::opaque_route)
            .as_deref()
            != Some(route)
        {
            return Err(MppError::InvalidChallenge("opaque"));
        }
        let echoed = SessionTerms::from_b64(&echo.request)
            .map_err(|_| MppError::InvalidChallenge("request"))?;
        let model_price = resolve_price(&echoed)?;
        let configured = self.config.model_prices.values().any(|price| {
            price.token_price == model_price.token_price && price.amount == model_price.amount
        });
        if !configured || model_price.token_price.is_none() {
            return Err(MppError::InvalidChallenge("model_price"));
        }
        let expected = self
            .terms(&model_price, None)
            .map_err(|_| MppError::InvalidChallenge("request"))?;
        if !expected.same_offer(&echoed) {
            return Err(MppError::InvalidChallenge("request"));
        }
        if let Some(source) = credential.source.as_deref() {
            let (chain, _) = protocol::parse_source_did(source)?;
            if chain != self.config.chain_id {
                return Err(MppError::VerificationFailed(VerifyFailure::SourceChain));
            }
        }
        let payload = SessionPayload::parse(&credential.payload)
            .map_err(|error| MppError::Malformed(error.reason()))?;
        Ok(SessionCredential {
            challenge_id: echo.id.clone(),
            payload,
            token_price: expected.amount,
            min_voucher_delta: expected.min_voucher_delta,
        })
    }

    fn management_permit(&self) -> Result<SemaphorePermit<'static>, MppError> {
        MANAGEMENT
            .try_acquire()
            .map_err(|_| MppError::Unavailable(UnavailableReason::ManagementBusy))
    }

    async fn get(&self, id: &[u8; 32]) -> Result<ChannelRecord, MppError> {
        self.repo
            .get(id)
            .await
            .map_err(store_error)?
            .ok_or(MppError::ChannelNotFound)
    }

    fn accepting(record: &ChannelRecord) -> Result<(), MppError> {
        if record.accepts_vouchers() {
            return Ok(());
        }
        match record.state {
            ChannelLifecycle::Open | ChannelLifecycle::Closing => Err(MppError::ChannelClosing),
            ChannelLifecycle::Finalized => Err(MppError::ChannelClosed),
        }
    }

    fn bind(&self, record: &ChannelRecord, descriptor: &ChannelDescriptor) -> Result<(), MppError> {
        if record.descriptor != *descriptor
            || record.chain_id != self.config.chain_id
            || record.escrow != self.config.escrow
            || descriptor.payee != self.signer.address()
            || descriptor.token != self.config.currency
            || descriptor.operator != [0; 20]
        {
            return Err(MppError::SessionMismatch(SessionMismatch::Descriptor));
        }
        if channel_id(descriptor, &self.config.escrow, self.config.chain_id) != record.channel_id {
            return Err(MppError::SessionMismatch(SessionMismatch::ChannelId));
        }
        Ok(())
    }

    async fn read_chain(&self, id: &[u8; 32]) -> Result<ChannelState, MppError> {
        let bytes = self
            .rpc
            .call(self.config.escrow, &abi::get_channel_state_calldata(id))
            .await
            .map_err(|_| MppError::Unavailable(UnavailableReason::Rpc))?;
        let state = abi::decode_channel_state(&bytes)
            .map_err(|_| MppError::Unavailable(UnavailableReason::Rpc))?;
        if state.deposit != 0 && state.settled > state.deposit {
            return Err(MppError::Unavailable(UnavailableReason::Rpc));
        }
        Ok(state)
    }

    async fn record_chain(
        &self,
        id: &[u8; 32],
        state: ChannelState,
    ) -> Result<ChannelRecord, MppError> {
        if state.deposit == 0 {
            self.repo
                .finalize(id, state.settled)
                .await
                .map_err(store_error)?;
            return Err(MppError::ChannelClosed);
        }
        let record = self
            .repo
            .record_chain_state(id, state, Utc::now())
            .await
            .map_err(store_error)?
            .ok_or(MppError::ChannelNotFound)?;
        Self::accepting(&record)?;
        Ok(record)
    }

    async fn refresh(&self, record: ChannelRecord) -> Result<ChannelRecord, MppError> {
        Self::accepting(&record)?;
        let now = Utc::now();
        let interval = chrono::Duration::seconds(CHAIN_READ_FRESHNESS_SECONDS);
        if chain_read_is_fresh(record.last_chain_read_at, now, interval) {
            return Ok(record);
        }
        let claimed_at = chain_read_claim_at(now, interval).ok_or(MppError::Config)?;
        if !self
            .repo
            .claim_chain_read(&record.channel_id, now, interval)
            .await
            .map_err(store_error)?
        {
            let current = self.get(&record.channel_id).await?;
            Self::accepting(&current)?;
            return if chain_read_is_fresh(current.last_chain_read_at, Utc::now(), interval) {
                Ok(current)
            } else {
                Err(MppError::Unavailable(UnavailableReason::Rpc))
            };
        }
        let result = match self.read_chain(&record.channel_id).await {
            Ok(chain) => self.record_chain(&record.channel_id, chain).await,
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.repo
                .clear_chain_read_claim(&record.channel_id, claimed_at)
                .await
                .map_err(store_error)?;
        }
        result
    }

    fn verify_signature(
        &self,
        id: &[u8; 32],
        descriptor: &ChannelDescriptor,
        cumulative: u128,
        signature: &[u8; SIGNATURE_LEN],
    ) -> Result<(), MppError> {
        let digest = voucher_digest(&self.config.escrow, self.config.chain_id, id, cumulative)
            .map_err(|_| MppError::VoucherRejected(VoucherReason::AboveDeposit))?;
        let signer = recover_address(&digest, signature)
            .map_err(|_| MppError::VoucherRejected(VoucherReason::InvalidSignature))?;
        if signer != voucher_signer(descriptor) {
            return Err(MppError::VoucherRejected(VoucherReason::SignerMismatch));
        }
        Ok(())
    }

    fn check_voucher(
        &self,
        record: &ChannelRecord,
        cumulative: u128,
        signature: &[u8; SIGNATURE_LEN],
        delta: u128,
        closing: bool,
    ) -> Result<(), MppError> {
        if cumulative > record.deposit {
            return Err(MppError::VoucherRejected(VoucherReason::AboveDeposit));
        }
        if cumulative < record.settled_on_chain {
            return Err(MppError::VoucherRejected(VoucherReason::BelowSettled));
        }
        if cumulative == record.highest_cumulative && signature == &record.highest_signature {
            return Ok(());
        }
        if cumulative < record.highest_cumulative
            || (!closing && cumulative == record.highest_cumulative)
        {
            return Err(MppError::VoucherRejected(VoucherReason::NotHigher));
        }
        if cumulative > record.highest_cumulative && cumulative - record.highest_cumulative < delta
        {
            return Err(MppError::VoucherRejected(VoucherReason::DeltaTooSmall));
        }
        self.verify_signature(
            &record.channel_id,
            &record.descriptor,
            cumulative,
            signature,
        )
    }

    async fn accept(
        &self,
        record: ChannelRecord,
        cumulative: u128,
        signature: [u8; SIGNATURE_LEN],
        min_delta: u128,
    ) -> Result<ChannelRecord, MppError> {
        if cumulative == record.highest_cumulative && signature == record.highest_signature {
            return Ok(record);
        }
        match self
            .repo
            .accept_voucher(
                &record.channel_id,
                cumulative,
                signature,
                VoucherRules {
                    min_delta,
                    settled_floor_from_chain: true,
                },
            )
            .await
            .map_err(store_error)?
        {
            VoucherOutcome::Accepted(record) => Ok(*record),
            VoucherOutcome::NotHigher => {
                let current = self.get(&record.channel_id).await?;
                Self::accepting(&current)?;
                if current.highest_cumulative == cumulative
                    && current.highest_signature == signature
                {
                    Ok(current)
                } else {
                    Err(MppError::VoucherRejected(VoucherReason::NotHigher))
                }
            }
            VoucherOutcome::DeltaTooSmall => {
                Err(MppError::VoucherRejected(VoucherReason::DeltaTooSmall))
            }
            VoucherOutcome::BelowSettled => {
                Err(MppError::VoucherRejected(VoucherReason::BelowSettled))
            }
            VoucherOutcome::AboveDeposit => {
                Err(MppError::VoucherRejected(VoucherReason::AboveDeposit))
            }
            VoucherOutcome::NotAccepting => Err(MppError::ChannelClosing),
            VoucherOutcome::Missing => Err(MppError::ChannelNotFound),
        }
    }

    pub async fn open(
        &self,
        credential: &SessionCredential,
    ) -> Result<ManagementReceipt, MppError> {
        let SessionPayload::Open {
            channel_id: id,
            transaction,
            descriptor,
            cumulative_amount,
            signature,
        } = &credential.payload
        else {
            return Err(MppError::SessionMismatch(SessionMismatch::Action));
        };
        let verified = verify_open_transaction(
            transaction,
            &OpenTerms {
                escrow: self.config.escrow,
                chain_id: self.config.chain_id,
                payee: self.signer.address(),
                token: self.config.currency,
            },
        )
        .map_err(MppError::OpenRejected)?;
        if *descriptor != verified.descriptor {
            return Err(MppError::SessionMismatch(SessionMismatch::Descriptor));
        }
        if *id != verified.channel_id {
            return Err(MppError::SessionMismatch(SessionMismatch::ChannelId));
        }
        if *cumulative_amount > verified.deposit {
            return Err(MppError::VoucherRejected(VoucherReason::AboveDeposit));
        }
        self.verify_signature(id, descriptor, *cumulative_amount, signature)?;
        let _permit = self.management_permit()?;
        if let Some(existing) = self.repo.get(id).await.map_err(store_error)? {
            Self::accepting(&existing)?;
            self.bind(&existing, descriptor)?;
            // Replaying an old open can refresh chain facts, but a new voucher on an
            // existing channel must obey the same minimum increase as a voucher action.
            if *cumulative_amount > existing.highest_cumulative {
                self.check_voucher(
                    &existing,
                    *cumulative_amount,
                    signature,
                    credential.min_voucher_delta,
                    false,
                )?;
            }
        }
        let tx = client_tx(transaction);
        broadcast(self.rpc.as_ref(), &tx)
            .await
            .map_err(|_| MppError::ChainTxFailed)?;
        let chain = self.read_chain(id).await?;
        if chain.deposit == 0 {
            self.repo
                .finalize(id, chain.settled)
                .await
                .map_err(store_error)?;
            return Err(MppError::ChannelClosed);
        }
        if chain.close_requested_at != 0 {
            // Also fence an existing channel if this is a replayed open.
            self.repo
                .record_chain_state(id, chain, Utc::now())
                .await
                .map_err(store_error)?;
            return Err(MppError::ChannelClosed);
        }
        if *cumulative_amount > chain.deposit {
            return Err(MppError::VoucherRejected(VoucherReason::AboveDeposit));
        }
        if *cumulative_amount < chain.settled {
            return Err(MppError::VoucherRejected(VoucherReason::BelowSettled));
        }
        let record = self
            .repo
            .open_or_refresh(NewChannel {
                channel_id: *id,
                chain_id: self.config.chain_id,
                escrow: self.config.escrow,
                descriptor: *descriptor,
                deposit: chain.deposit,
                settled_on_chain: chain.settled,
                highest_cumulative: *cumulative_amount,
                highest_signature: *signature,
            })
            .await
            .map_err(store_error)?;
        Self::accepting(&record)?;
        let record = self.record_chain(id, chain).await?;
        let record = if *cumulative_amount > record.highest_cumulative {
            self.accept(
                record,
                *cumulative_amount,
                *signature,
                credential.min_voucher_delta,
            )
            .await?
        } else {
            record
        };
        Ok(ManagementReceipt {
            receipt: receipt(credential, &record, Some(tx.tx_hash)),
        })
    }

    pub async fn top_up(
        &self,
        credential: &SessionCredential,
    ) -> Result<ManagementReceipt, MppError> {
        let SessionPayload::TopUp {
            channel_id: id,
            transaction,
            descriptor,
            additional_deposit,
        } = &credential.payload
        else {
            return Err(MppError::SessionMismatch(SessionMismatch::Action));
        };
        self.verify_top_up(transaction, descriptor, *additional_deposit)?;
        let _permit = self.management_permit()?;
        let record = self.get(id).await?;
        Self::accepting(&record)?;
        self.bind(&record, descriptor)?;
        let tx = client_tx(transaction);
        broadcast(self.rpc.as_ref(), &tx)
            .await
            .map_err(|_| MppError::ChainTxFailed)?;
        let chain = self.read_chain(id).await?;
        let record = self.record_chain(id, chain).await?;
        Ok(ManagementReceipt {
            receipt: receipt(credential, &record, Some(tx.tx_hash)),
        })
    }

    fn verify_top_up(
        &self,
        raw: &[u8],
        descriptor: &ChannelDescriptor,
        additional: u128,
    ) -> Result<(), MppError> {
        let rejected = MppError::TopUpRejected;
        let (tx, signature) = decode_signed(raw).map_err(|_| rejected(TopUpReason::Malformed))?;
        if tx.chain_id != self.config.chain_id {
            return Err(rejected(TopUpReason::ChainId));
        }
        let [call] = tx.calls.as_slice() else {
            return Err(rejected(TopUpReason::CallCount));
        };
        if call.to != self.config.escrow {
            return Err(rejected(TopUpReason::Target));
        }
        if call.value != 0 {
            return Err(rejected(TopUpReason::Value));
        }
        if call.input.get(..4) != Some(TOP_UP_SELECTOR.as_slice()) {
            return Err(rejected(TopUpReason::Selector));
        }
        if !tx.has_expiring_nonce() {
            return Err(rejected(TopUpReason::ExpiringNonce));
        }
        if additional == 0 || additional > MAX_UINT96 {
            return Err(rejected(TopUpReason::Amount));
        }
        // Static ABI: comparing the complete canonical encoding also checks address padding,
        // uint96 width, descriptor identity, length and the absence of trailing calldata.
        let mut expected = TOP_UP_SELECTOR.to_vec();
        let d = descriptor;
        for address in [&d.payer, &d.payee, &d.operator, &d.token] {
            expected.extend_from_slice(&abi::address_word(address));
        }
        expected.extend_from_slice(&d.salt);
        expected.extend_from_slice(&abi::address_word(&d.authorized_signer));
        expected.extend_from_slice(&d.expiring_nonce_hash);
        expected.extend_from_slice(&abi::uint_word(additional));
        if call.input.len() != expected.len() {
            return Err(rejected(TopUpReason::Calldata));
        }
        if call.input.get(4..228) != expected.get(4..228) {
            return Err(rejected(TopUpReason::Descriptor));
        }
        if call.input != expected {
            return Err(rejected(TopUpReason::Amount));
        }
        let sender = tx
            .recover_sender(&signature)
            .map_err(|_| rejected(TopUpReason::Signature))?;
        if sender != d.payer {
            return Err(rejected(TopUpReason::Sender));
        }
        Ok(())
    }

    pub async fn voucher(
        &self,
        credential: &SessionCredential,
        cost_cap: u128,
        request_id: Uuid,
    ) -> Result<ReservedRequest, MppError> {
        let SessionPayload::Voucher {
            channel_id: id,
            descriptor,
            cumulative_amount,
            signature,
        } = &credential.payload
        else {
            return Err(MppError::SessionMismatch(SessionMismatch::Action));
        };
        if cost_cap == 0 {
            return Err(MppError::Malformed("zero_cost_cap"));
        }
        self.verify_signature(id, descriptor, *cumulative_amount, signature)?;
        let record = self.get(id).await?;
        Self::accepting(&record)?;
        self.bind(&record, descriptor)?;
        let record = self.refresh(record).await?;
        if let Err(error) = self.check_voucher(
            &record,
            *cumulative_amount,
            signature,
            credential.min_voucher_delta,
            false,
        ) {
            if error == MppError::VoucherRejected(VoucherReason::DeltaTooSmall) {
                // A snapshot contains a usable stored voucher. Authenticate ownership
                // before disclosing it, even when the incoming increase is too small.
                self.verify_signature(id, descriptor, *cumulative_amount, signature)?;
                let required = record
                    .spent
                    .checked_add(cost_cap)
                    .zip(
                        record
                            .highest_cumulative
                            .checked_add(credential.min_voucher_delta),
                    )
                    .map(|(cost, delta)| cost.max(delta))
                    .ok_or(MppError::Malformed("cost_overflow"))?;
                return Err(MppError::VoucherDeltaTooSmall(Box::new(snapshot(
                    &record, required,
                ))));
            }
            return Err(error);
        }
        let required = record
            .spent
            .checked_add(cost_cap)
            .ok_or(MppError::Malformed("cost_overflow"))?;
        if *cumulative_amount < required {
            return Err(MppError::VoucherTooLow(Box::new(snapshot(
                &record, required,
            ))));
        }
        if let Err(error) = self
            .accept(
                record,
                *cumulative_amount,
                *signature,
                credential.min_voucher_delta,
            )
            .await
        {
            if error == MppError::VoucherRejected(VoucherReason::DeltaTooSmall) {
                let current = self.get(id).await?;
                let required = current
                    .spent
                    .checked_add(cost_cap)
                    .zip(
                        current
                            .highest_cumulative
                            .checked_add(credential.min_voucher_delta),
                    )
                    .map(|(cost, delta)| cost.max(delta))
                    .ok_or(MppError::Malformed("cost_overflow"))?;
                return Err(MppError::VoucherDeltaTooSmall(Box::new(snapshot(
                    &current, required,
                ))));
            }
            return Err(error);
        }
        match self
            .repo
            .reserve(id, request_id, cost_cap)
            .await
            .map_err(store_error)?
        {
            ReserveOutcome::Reserved(record) => Ok(ReservedRequest {
                channel_id: *id,
                request_id,
                reserved: cost_cap,
                receipt_preview: receipt(credential, &record, None),
                token_price: credential.token_price,
            }),
            ReserveOutcome::Insufficient { .. } => {
                let record = self.get(id).await?;
                let required = record
                    .spent
                    .checked_add(cost_cap)
                    .ok_or(MppError::Malformed("cost_overflow"))?;
                Err(MppError::VoucherTooLow(Box::new(snapshot(
                    &record, required,
                ))))
            }
            ReserveOutcome::NotAccepting => Err(MppError::ChannelClosing),
            ReserveOutcome::Missing => Err(MppError::ChannelNotFound),
        }
    }

    pub async fn charge(
        &self,
        reserved: &ReservedRequest,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> Result<SessionReceipt, MppError> {
        let units = prompt_tokens
            .checked_add(completion_tokens)
            .ok_or(MppError::Malformed("units_overflow"))?;
        let actual = reserved
            .token_price
            .checked_mul(u128::from(units))
            .ok_or(MppError::Malformed("cost_overflow"))?;
        self.charge_amount(reserved, actual, units).await
    }

    /// Charge the full reserved cap when output was delivered without any known counts.
    /// Unknown usage contributes no invented units.
    pub async fn charge_reserved(
        &self,
        reserved: &ReservedRequest,
    ) -> Result<SessionReceipt, MppError> {
        self.charge_amount(reserved, reserved.reserved, 0).await
    }

    async fn charge_amount(
        &self,
        reserved: &ReservedRequest,
        actual: u128,
        units: u64,
    ) -> Result<SessionReceipt, MppError> {
        match self
            .repo
            .charge(reserved.request_id, actual.min(reserved.reserved), units)
            .await
            .map_err(store_error)?
        {
            ChargeOutcome::Charged { record, charged } => {
                if charged < actual {
                    tracing::warn!(
                        error_code = "session_charge_clamped",
                        count = 1u64,
                        "MPP charge clamped"
                    );
                }
                Ok(SessionReceipt {
                    challenge_id: reserved.receipt_preview.challenge_id.clone(),
                    channel_id: record.channel_id,
                    accepted_cumulative: record.highest_cumulative,
                    spent: record.spent,
                    units: record.units,
                    tx_hash: None,
                })
            }
            ChargeOutcome::NoReservation => Err(MppError::ReservationMissing),
        }
    }

    pub async fn release(&self, reserved: &ReservedRequest) -> Result<(), MppError> {
        self.repo
            .release(reserved.request_id)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}

fn store_error(_: crate::common::RepositoryError) -> MppError {
    MppError::Unavailable(UnavailableReason::Store)
}

fn client_tx(raw: &[u8]) -> SignedPayeeTx {
    SignedPayeeTx {
        raw: raw.to_vec(),
        tx_hash: tx_hash(raw),
        valid_before: 0,
    }
}

fn receipt(
    credential: &SessionCredential,
    record: &ChannelRecord,
    tx_hash: Option<[u8; 32]>,
) -> SessionReceipt {
    SessionReceipt {
        challenge_id: credential.challenge_id.clone(),
        channel_id: record.channel_id,
        accepted_cumulative: record.highest_cumulative,
        spent: record.spent,
        units: record.units,
        tx_hash,
    }
}

fn snapshot(record: &ChannelRecord, required_cumulative: u128) -> SessionSnapshot {
    SessionSnapshot {
        accepted_cumulative: record.highest_cumulative,
        chain_id: record.chain_id,
        channel_id: record.channel_id,
        deposit: record.deposit,
        descriptor: record.descriptor,
        escrow: record.escrow,
        highest_voucher: HighestVoucher {
            channel_id: record.channel_id,
            cumulative_amount: record.highest_cumulative,
            signature: record.highest_signature,
        },
        required_cumulative,
        settled: record.settled_on_chain,
        spent: record.spent,
        units: record.units,
    }
}

#[cfg(test)]
#[path = "session_service_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_service/validation_tests.rs"]
mod validation_tests;

#[cfg(test)]
#[path = "session_service/regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
#[path = "session_service/port_tests.rs"]
mod port_tests;

#[cfg(test)]
#[path = "session_service/ledger_regression_tests.rs"]
mod ledger_regression_tests;
// Focused ledger regression coverage remains separate from the service implementation.
