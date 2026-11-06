//! Minimal, strict ABI codec for the TIP-1034 channel reserve precompile
//! (`0x4D50500000000000000000000000000000000000`).
//!
//! Encoders build the calldata the server sends (`settle`, `close`) and the reads it makes
//! (`getChannelState`, `getChannelStatesBatch`, plus `open` for tests and tooling).
//! Decoders read what an untrusted client or the node returns: `open` calldata from a
//! client transaction, and the state reads. Decoding is strict: the selector, the exact
//! length, zero padding of every narrower-than-32-byte value, `uint96`/`uint32` ranges and
//! the dynamic-array layout are all checked, so one byte string has one meaning.
//!
//! Descriptor: `(address payer, address payee, address operator, address token,
//! bytes32 salt, address authorizedSigner, bytes32 expiringNonceHash)`, seven static words.

use super::eip712::MAX_UINT96;

/// `open(address,address,address,uint96,bytes32,address)`.
pub const OPEN_SELECTOR: [u8; 4] = [0xed, 0xc5, 0x3b, 0x00];
/// `settle((address,address,address,address,bytes32,address,bytes32),uint96,bytes)`.
pub const SETTLE_SELECTOR: [u8; 4] = [0x97, 0xfb, 0x51, 0x04];
/// `close((address,address,address,address,bytes32,address,bytes32),uint96,uint96,bytes)`.
pub const CLOSE_SELECTOR: [u8; 4] = [0x73, 0xb5, 0x7f, 0x74];
/// `requestClose((address,address,address,address,bytes32,address,bytes32))`.
pub const REQUEST_CLOSE_SELECTOR: [u8; 4] = [0x67, 0x54, 0x02, 0xe5];
/// `topUp((address,address,address,address,bytes32,address,bytes32),uint96)`.
pub const TOP_UP_SELECTOR: [u8; 4] = [0xdc, 0x48, 0x47, 0x1e];
/// `getChannelState(bytes32)`.
pub const GET_CHANNEL_STATE_SELECTOR: [u8; 4] = [0xd1, 0x8d, 0xa8, 0xb1];
/// `getChannelStatesBatch(bytes32[])`.
pub const GET_CHANNEL_STATES_BATCH_SELECTOR: [u8; 4] = [0xd1, 0xf4, 0xcd, 0xa2];

/// Most channel ids in one batch read (the supervision task reads in chunks of 100).
pub const MAX_BATCH_CHANNELS: usize = 256;
/// Longest `bytes signature` argument the encoders accept.
pub const MAX_SIGNATURE_BYTES: usize = 1024;

const WORD: usize = 32;
const DESCRIPTOR_WORDS: usize = 7;
/// Bytes of one `ChannelState` (three words).
const STATE_LEN: usize = 3 * WORD;

/// Why calldata or return data was refused. A closed set, carrying no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AbiError {
    /// The first four bytes are not the expected selector.
    #[error("unexpected selector")]
    Selector,
    /// The length is not the one this call or return has.
    #[error("unexpected length")]
    Length,
    /// A value narrower than 32 bytes has non-zero padding.
    #[error("non-zero padding")]
    Padding,
    /// An amount does not fit a `uint96` (or a block time a `uint32`).
    #[error("value out of range")]
    Range,
    /// An offset, array length or dynamic-bytes length is not the canonical layout.
    #[error("invalid layout")]
    Layout,
}

/// The identity of a channel: everything the channel id commits to except the escrow and
/// the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelDescriptor {
    pub payer: [u8; 20],
    pub payee: [u8; 20],
    pub operator: [u8; 20],
    pub token: [u8; 20],
    pub salt: [u8; 32],
    /// Zero means the payer signs vouchers.
    pub authorized_signer: [u8; 20],
    pub expiring_nonce_hash: [u8; 32],
}

/// Decoded `open(...)` arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenArgs {
    pub payee: [u8; 20],
    pub operator: [u8; 20],
    pub token: [u8; 20],
    pub deposit: u128,
    pub salt: [u8; 32],
    pub authorized_signer: [u8; 20],
}

/// `getChannelState` return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelState {
    /// Amount already settled on chain (`uint96`).
    pub settled: u128,
    /// Escrowed deposit (`uint96`); zero for a channel that does not exist or is closed.
    pub deposit: u128,
    /// Unix time the payer requested a close, `0` when none (`uint32`).
    pub close_requested_at: u64,
}

// ---------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------

/// A 32-byte word holding an address, left padded with zeros.
pub(super) fn address_word(address: &[u8; 20]) -> [u8; 32] {
    let mut word = [0u8; WORD];
    for (slot, byte) in word.iter_mut().skip(12).zip(address) {
        *slot = *byte;
    }
    word
}

/// A big-endian `uint256` word holding `value`.
pub(super) fn uint_word(value: u128) -> [u8; 32] {
    let mut word = [0u8; WORD];
    for (slot, byte) in word.iter_mut().skip(16).zip(value.to_be_bytes()) {
        *slot = byte;
    }
    word
}

fn check_amount(value: u128) -> Result<(), AbiError> {
    if value > MAX_UINT96 {
        Err(AbiError::Range)
    } else {
        Ok(())
    }
}

fn push_descriptor(out: &mut Vec<u8>, descriptor: &ChannelDescriptor) {
    out.extend_from_slice(&address_word(&descriptor.payer));
    out.extend_from_slice(&address_word(&descriptor.payee));
    out.extend_from_slice(&address_word(&descriptor.operator));
    out.extend_from_slice(&address_word(&descriptor.token));
    out.extend_from_slice(&descriptor.salt);
    out.extend_from_slice(&address_word(&descriptor.authorized_signer));
    out.extend_from_slice(&descriptor.expiring_nonce_hash);
}

/// `bytes` tail: length word, then the content right padded to a word boundary.
fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), AbiError> {
    if bytes.len() > MAX_SIGNATURE_BYTES {
        return Err(AbiError::Length);
    }
    out.extend_from_slice(&uint_word(bytes.len() as u128));
    out.extend_from_slice(bytes);
    let padding = (WORD - bytes.len() % WORD) % WORD;
    out.extend(std::iter::repeat_n(0u8, padding));
    Ok(())
}

/// The word at word index `index` of `data`.
fn word_at(data: &[u8], index: usize) -> Result<&[u8; 32], AbiError> {
    let start = index.checked_mul(WORD).ok_or(AbiError::Length)?;
    let end = start.checked_add(WORD).ok_or(AbiError::Length)?;
    data.get(start..end)
        .and_then(|word| word.try_into().ok())
        .ok_or(AbiError::Length)
}

fn word_to_address(word: &[u8; 32]) -> Result<[u8; 20], AbiError> {
    let (padding, address) = word.split_at(12);
    if padding.iter().any(|byte| *byte != 0) {
        return Err(AbiError::Padding);
    }
    address.try_into().map_err(|_| AbiError::Length)
}

/// A word holding an unsigned integer of at most `width` bytes (`width <= 16`: the value
/// is read into a `u128`, so the bytes above it must be zero or it would be truncated).
fn word_to_uint(word: &[u8; 32], width: usize) -> Result<u128, AbiError> {
    let width = width.min(16);
    let (padding, value) = word.split_at(WORD - width);
    if padding.iter().any(|byte| *byte != 0) {
        return Err(AbiError::Range);
    }
    Ok(value
        .iter()
        .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte)))
}

/// A word as a `uint96`; non-zero bits above bit 95 are a range error.
fn word_to_uint96(word: &[u8; 32]) -> Result<u128, AbiError> {
    word_to_uint(word, 12)
}

/// The arguments after `selector`, or the selector error.
fn after_selector<'a>(data: &'a [u8], selector: &[u8; 4]) -> Result<&'a [u8], AbiError> {
    match data.split_at_checked(4) {
        Some((head, args)) if head == selector => Ok(args),
        _ => Err(AbiError::Selector),
    }
}

// ---------------------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------------------

/// `open(payee, operator, token, deposit, salt, authorizedSigner)` calldata.
pub fn open_calldata(
    payee: &[u8; 20],
    operator: &[u8; 20],
    token: &[u8; 20],
    deposit: u128,
    salt: &[u8; 32],
    authorized_signer: &[u8; 20],
) -> Result<Vec<u8>, AbiError> {
    check_amount(deposit)?;
    let mut out = Vec::with_capacity(4 + 6 * WORD);
    out.extend_from_slice(&OPEN_SELECTOR);
    out.extend_from_slice(&address_word(payee));
    out.extend_from_slice(&address_word(operator));
    out.extend_from_slice(&address_word(token));
    out.extend_from_slice(&uint_word(deposit));
    out.extend_from_slice(salt);
    out.extend_from_slice(&address_word(authorized_signer));
    Ok(out)
}

/// `settle(descriptor, cumulativeAmount, signature)` calldata.
pub fn settle_calldata(
    descriptor: &ChannelDescriptor,
    cumulative: u128,
    signature: &[u8],
) -> Result<Vec<u8>, AbiError> {
    check_amount(cumulative)?;
    let mut out = Vec::with_capacity(4 + (DESCRIPTOR_WORDS + 4) * WORD);
    out.extend_from_slice(&SETTLE_SELECTOR);
    push_descriptor(&mut out, descriptor);
    out.extend_from_slice(&uint_word(cumulative));
    // Offset of the `bytes` tail from the start of the arguments: descriptor + 2 head words.
    out.extend_from_slice(&uint_word(((DESCRIPTOR_WORDS + 2) * WORD) as u128));
    push_bytes(&mut out, signature)?;
    Ok(out)
}

/// `close(descriptor, cumulativeAmount, captureAmount, signature)` calldata.
pub fn close_calldata(
    descriptor: &ChannelDescriptor,
    cumulative: u128,
    capture: u128,
    signature: &[u8],
) -> Result<Vec<u8>, AbiError> {
    check_amount(cumulative)?;
    check_amount(capture)?;
    let mut out = Vec::with_capacity(4 + (DESCRIPTOR_WORDS + 5) * WORD);
    out.extend_from_slice(&CLOSE_SELECTOR);
    push_descriptor(&mut out, descriptor);
    out.extend_from_slice(&uint_word(cumulative));
    out.extend_from_slice(&uint_word(capture));
    out.extend_from_slice(&uint_word(((DESCRIPTOR_WORDS + 3) * WORD) as u128));
    push_bytes(&mut out, signature)?;
    Ok(out)
}

/// `getChannelState(channelId)` calldata.
pub fn get_channel_state_calldata(channel_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + WORD);
    out.extend_from_slice(&GET_CHANNEL_STATE_SELECTOR);
    out.extend_from_slice(channel_id);
    out
}

/// `getChannelStatesBatch(channelIds)` calldata, at most [`MAX_BATCH_CHANNELS`] ids.
pub fn get_channel_states_batch_calldata(channel_ids: &[[u8; 32]]) -> Result<Vec<u8>, AbiError> {
    if channel_ids.len() > MAX_BATCH_CHANNELS {
        return Err(AbiError::Length);
    }
    let mut out = Vec::with_capacity(4 + (2 + channel_ids.len()) * WORD);
    out.extend_from_slice(&GET_CHANNEL_STATES_BATCH_SELECTOR);
    out.extend_from_slice(&uint_word(WORD as u128));
    out.extend_from_slice(&uint_word(channel_ids.len() as u128));
    for id in channel_ids {
        out.extend_from_slice(id);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------------------

/// Decode `open(...)` calldata: selector, exactly six words, zero padding, `uint96` deposit.
pub fn decode_open_calldata(data: &[u8]) -> Result<OpenArgs, AbiError> {
    let args = after_selector(data, &OPEN_SELECTOR)?;
    if args.len() != 6 * WORD {
        return Err(AbiError::Length);
    }
    Ok(OpenArgs {
        payee: word_to_address(word_at(args, 0)?)?,
        operator: word_to_address(word_at(args, 1)?)?,
        token: word_to_address(word_at(args, 2)?)?,
        deposit: word_to_uint96(word_at(args, 3)?)?,
        salt: *word_at(args, 4)?,
        authorized_signer: word_to_address(word_at(args, 5)?)?,
    })
}

fn state_at(data: &[u8], first_word: usize) -> Result<ChannelState, AbiError> {
    let close_requested_at = word_to_uint(word_at(data, first_word + 2)?, 4)?;
    Ok(ChannelState {
        settled: word_to_uint96(word_at(data, first_word)?)?,
        deposit: word_to_uint96(word_at(data, first_word + 1)?)?,
        // At most a uint32, so it fits.
        close_requested_at: u64::try_from(close_requested_at).map_err(|_| AbiError::Range)?,
    })
}

/// Decode the return of `getChannelState`: exactly three words
/// `(uint96 settled, uint96 deposit, uint32 closeRequestedAt)`.
pub fn decode_channel_state(data: &[u8]) -> Result<ChannelState, AbiError> {
    if data.len() != STATE_LEN {
        return Err(AbiError::Length);
    }
    state_at(data, 0)
}

/// Decode the return of `getChannelStatesBatch`: `ChannelState[]` as the offset word
/// (`0x20`), the element count, then three words per element. At most
/// [`MAX_BATCH_CHANNELS`] elements and no trailing bytes.
pub fn decode_channel_states_batch(data: &[u8]) -> Result<Vec<ChannelState>, AbiError> {
    // Offset and count are whole words; one that does not fit a u128 is a layout error,
    // never a truncated value.
    let offset = word_to_uint(word_at(data, 0)?, 16).map_err(|_| AbiError::Layout)?;
    if offset != WORD as u128 {
        return Err(AbiError::Layout);
    }
    let count = word_to_uint(word_at(data, 1)?, 16)
        .ok()
        .and_then(|count| usize::try_from(count).ok())
        .ok_or(AbiError::Layout)?;
    if count > MAX_BATCH_CHANNELS {
        return Err(AbiError::Layout);
    }
    let expected = count
        .checked_mul(STATE_LEN)
        .and_then(|body| body.checked_add(2 * WORD))
        .ok_or(AbiError::Length)?;
    if data.len() != expected {
        return Err(AbiError::Length);
    }
    (0..count)
        .map(|index| state_at(data, 2 + index * 3))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpp::session_vectors::Vectors;
    use crate::mpp::tempo::keccak256;

    const DESCRIPTOR_TYPE: &str = "(address,address,address,address,bytes32,address,bytes32)";

    fn selector_of(signature: &str) -> [u8; 4] {
        let hash = keccak256(signature.as_bytes());
        [hash[0], hash[1], hash[2], hash[3]]
    }

    #[test]
    fn selectors_are_the_hashes_of_their_signatures() {
        let cases = [
            (
                OPEN_SELECTOR,
                "open(address,address,address,uint96,bytes32,address)".to_string(),
            ),
            (
                SETTLE_SELECTOR,
                format!("settle({DESCRIPTOR_TYPE},uint96,bytes)"),
            ),
            (
                CLOSE_SELECTOR,
                format!("close({DESCRIPTOR_TYPE},uint96,uint96,bytes)"),
            ),
            (
                REQUEST_CLOSE_SELECTOR,
                format!("requestClose({DESCRIPTOR_TYPE})"),
            ),
            (TOP_UP_SELECTOR, format!("topUp({DESCRIPTOR_TYPE},uint96)")),
            (
                GET_CHANNEL_STATE_SELECTOR,
                "getChannelState(bytes32)".to_string(),
            ),
            (
                GET_CHANNEL_STATES_BATCH_SELECTOR,
                "getChannelStatesBatch(bytes32[])".to_string(),
            ),
        ];
        for (selector, signature) in cases {
            assert_eq!(selector, selector_of(&signature), "{signature}");
        }
    }

    #[test]
    fn encoders_match_the_vectors() {
        let v = Vectors::load();
        let d = v.descriptor();
        assert_eq!(
            open_calldata(
                &d.payee,
                &d.operator,
                &d.token,
                v.int("deposit"),
                &d.salt,
                &d.authorized_signer
            ),
            Ok(v.bytes("open.calldata"))
        );
        let signature = v.bytes("voucher.signature");
        let cumulative = v.int("voucher.cumulativeAmount");
        assert_eq!(
            settle_calldata(&d, cumulative, &signature),
            Ok(v.bytes("settle.calldata"))
        );
        assert_eq!(
            close_calldata(&d, cumulative, v.int("close.captureAmount"), &signature),
            Ok(v.bytes("close.calldata"))
        );
        let channel_id = v.fixed::<32>("channelId");
        assert_eq!(
            get_channel_state_calldata(&channel_id),
            v.bytes("getChannelState.calldata")
        );
        assert_eq!(
            get_channel_states_batch_calldata(&[channel_id, channel_id]),
            Ok(v.bytes("getChannelStatesBatch.calldata"))
        );
    }

    #[test]
    fn open_calldata_round_trips_and_matches_the_vector() {
        let v = Vectors::load();
        let d = v.descriptor();
        let args = decode_open_calldata(&v.bytes("open.calldata")).unwrap();
        assert_eq!(
            args,
            OpenArgs {
                payee: d.payee,
                operator: d.operator,
                token: d.token,
                deposit: v.int("deposit"),
                salt: d.salt,
                authorized_signer: d.authorized_signer,
            }
        );
        let encoded = open_calldata(
            &[0x11; 20],
            &[0x22; 20],
            &[0x33; 20],
            MAX_UINT96,
            &[0x44; 32],
            &[0x55; 20],
        )
        .unwrap();
        let args = decode_open_calldata(&encoded).unwrap();
        assert_eq!(args.deposit, MAX_UINT96);
        assert_eq!(args.operator, [0x22; 20]);
        assert_eq!(args.authorized_signer, [0x55; 20]);
    }

    #[test]
    fn open_decoding_is_strict() {
        let v = Vectors::load();
        let good = v.bytes("open.calldata");
        assert!(decode_open_calldata(&good).is_ok());

        let mut wrong_selector = good.clone();
        wrong_selector[0] ^= 1;
        assert_eq!(
            decode_open_calldata(&wrong_selector),
            Err(AbiError::Selector)
        );
        assert_eq!(decode_open_calldata(&[]), Err(AbiError::Selector));
        assert_eq!(decode_open_calldata(&good[..3]), Err(AbiError::Selector));

        assert_eq!(
            decode_open_calldata(&good[..good.len() - 1]),
            Err(AbiError::Length)
        );
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(decode_open_calldata(&trailing), Err(AbiError::Length));

        // Dirty padding on an address word (payee) and on the deposit word.
        let mut dirty_address = good.clone();
        dirty_address[4] = 1;
        assert_eq!(decode_open_calldata(&dirty_address), Err(AbiError::Padding));
        let mut deposit_too_wide = good.clone();
        deposit_too_wide[4 + 3 * 32 + 19] = 1; // bit above uint96
        assert_eq!(
            decode_open_calldata(&deposit_too_wide),
            Err(AbiError::Range)
        );
    }

    #[test]
    fn amounts_above_uint96_are_refused_by_every_encoder() {
        let d = Vectors::load().descriptor();
        let too_big = MAX_UINT96 + 1;
        assert_eq!(
            open_calldata(&[1; 20], &[0; 20], &[2; 20], too_big, &[3; 32], &[0; 20]),
            Err(AbiError::Range)
        );
        assert_eq!(settle_calldata(&d, too_big, &[0; 65]), Err(AbiError::Range));
        assert_eq!(
            close_calldata(&d, too_big, 0, &[0; 65]),
            Err(AbiError::Range)
        );
        assert_eq!(
            close_calldata(&d, 0, too_big, &[0; 65]),
            Err(AbiError::Range)
        );
        assert!(settle_calldata(&d, MAX_UINT96, &[0; 65]).is_ok());
    }

    #[test]
    fn signature_and_batch_sizes_are_bounded() {
        let d = Vectors::load().descriptor();
        let long = vec![0u8; MAX_SIGNATURE_BYTES + 1];
        assert_eq!(settle_calldata(&d, 1, &long), Err(AbiError::Length));
        assert_eq!(close_calldata(&d, 1, 1, &long), Err(AbiError::Length));
        assert!(settle_calldata(&d, 1, &long[..MAX_SIGNATURE_BYTES]).is_ok());
        // Dynamic bytes are padded to a word.
        for len in [0usize, 1, 31, 32, 33, 65] {
            let encoded = settle_calldata(&d, 1, &vec![9u8; len]).unwrap();
            assert_eq!((encoded.len() - 4) % 32, 0, "len {len}");
        }
        let ids = vec![[1u8; 32]; MAX_BATCH_CHANNELS];
        assert!(get_channel_states_batch_calldata(&ids).is_ok());
        let ids = vec![[1u8; 32]; MAX_BATCH_CHANNELS + 1];
        assert_eq!(
            get_channel_states_batch_calldata(&ids),
            Err(AbiError::Length)
        );
    }

    #[test]
    fn channel_state_decodes_the_vector() {
        let v = Vectors::load();
        assert_eq!(
            decode_channel_state(&v.bytes("getChannelState.exampleReturn")),
            Ok(ChannelState {
                settled: 123,
                deposit: 5_000_000,
                close_requested_at: 0,
            })
        );
    }

    #[test]
    fn channel_state_decoding_is_strict() {
        let v = Vectors::load();
        let good = v.bytes("getChannelState.exampleReturn");
        assert_eq!(decode_channel_state(&good[..95]), Err(AbiError::Length));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_channel_state(&long), Err(AbiError::Length));
        assert_eq!(decode_channel_state(&[]), Err(AbiError::Length));
        // settled and deposit above uint96.
        for word in [0usize, 1] {
            let mut wide = good.clone();
            wide[word * 32 + 19] = 1;
            assert_eq!(decode_channel_state(&wide), Err(AbiError::Range), "{word}");
        }
        // closeRequestedAt above uint32.
        let mut late = good.clone();
        late[64 + 27] = 1;
        assert_eq!(decode_channel_state(&late), Err(AbiError::Range));
        // The largest legal values.
        let mut max = vec![0u8; 96];
        max[20..32].fill(0xff);
        max[52..64].fill(0xff);
        max[92..96].fill(0xff);
        assert_eq!(
            decode_channel_state(&max),
            Ok(ChannelState {
                settled: MAX_UINT96,
                deposit: MAX_UINT96,
                close_requested_at: u64::from(u32::MAX),
            })
        );
    }

    #[test]
    fn batch_return_decodes_the_vector() {
        let v = Vectors::load();
        let states =
            decode_channel_states_batch(&v.bytes("getChannelStatesBatch.exampleReturn")).unwrap();
        assert_eq!(
            states,
            vec![
                ChannelState {
                    settled: 123,
                    deposit: 5_000_000,
                    close_requested_at: 0,
                },
                ChannelState {
                    settled: 0,
                    deposit: 0,
                    close_requested_at: 0x6553_f164,
                },
            ]
        );
    }

    #[test]
    fn batch_decoding_is_strict() {
        let v = Vectors::load();
        let good = v.bytes("getChannelStatesBatch.exampleReturn");
        assert_eq!(
            decode_channel_states_batch(&good[..good.len() - 1]),
            Err(AbiError::Length)
        );
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_channel_states_batch(&trailing),
            Err(AbiError::Length)
        );
        assert_eq!(decode_channel_states_batch(&[]), Err(AbiError::Length));

        let mut bad_offset = good.clone();
        bad_offset[31] = 0x40;
        assert_eq!(
            decode_channel_states_batch(&bad_offset),
            Err(AbiError::Layout)
        );
        // A count that claims more elements than the bytes hold, and an absurd one.
        let mut count = good.clone();
        count[63] = 3;
        assert_eq!(decode_channel_states_batch(&count), Err(AbiError::Length));
        let mut huge = good.clone();
        huge[32] = 0xff;
        assert_eq!(decode_channel_states_batch(&huge), Err(AbiError::Layout));
        // Words wider than 128 bits must not be truncated into a plausible value
        // (2^128 + 0x20 as the offset, 2^128 + 2 as the count).
        let mut wide_offset = good.clone();
        wide_offset[15] = 1;
        assert_eq!(
            decode_channel_states_batch(&wide_offset),
            Err(AbiError::Layout)
        );
        let mut wide_count = good.clone();
        wide_count[32 + 15] = 1;
        assert_eq!(
            decode_channel_states_batch(&wide_count),
            Err(AbiError::Layout)
        );
        // An element with a uint96 overflow.
        let mut wide = good.clone();
        wide[64 + 19] = 1;
        assert_eq!(decode_channel_states_batch(&wide), Err(AbiError::Range));

        // Empty array.
        let mut empty = vec![0u8; 64];
        empty[31] = 0x20;
        assert_eq!(decode_channel_states_batch(&empty), Ok(vec![]));
        // Over the bound.
        let mut over = vec![0u8; 64 + (MAX_BATCH_CHANNELS + 1) * 96];
        over[31] = 0x20;
        over[62] = 0x01; // count = 256 + 1
        over[63] = 0x01;
        assert_eq!(decode_channel_states_batch(&over), Err(AbiError::Layout));
    }
}
