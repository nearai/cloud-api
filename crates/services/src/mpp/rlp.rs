//! A strict, bounded RLP reader for the one structure MPP looks into: a Tempo transaction.
//!
//! Only the shapes the verification needs are offered: read one item, walk the items of a
//! list, check a list's framing. Everything is borrowed from the input and bounded by it:
//! lengths are checked arithmetic against the remaining bytes, nothing is allocated, and
//! there is no recursion (callers descend one level at a time). The canonical-form rules
//! of the Yellow Paper are enforced, so the same bytes cannot be read two ways:
//! a single byte below `0x80` is its own item, a long form is used only above 55 bytes,
//! and a length never has a leading zero byte.

/// One RLP item: a byte string or a list, borrowed from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item<'a> {
    /// A byte string (its content).
    String(&'a [u8]),
    /// A list (the concatenated encodings of its items).
    List(&'a [u8]),
}

/// The input is not canonical, bounded RLP. Carries no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid RLP")]
pub struct RlpError;

/// Header of the item at the start of `buf`: `(is_list, header_len, payload_len)`.
/// `header_len` is 0 for a single byte below `0x80`, whose payload is the byte itself.
fn header(buf: &[u8]) -> Result<(bool, usize, usize), RlpError> {
    let first = *buf.first().ok_or(RlpError)?;
    let (is_list, header_len, payload_len) = match first {
        0x00..=0x7f => (false, 0, 1),
        0x80..=0xb7 => (false, 1, usize::from(first - 0x80)),
        0xc0..=0xf7 => (true, 1, usize::from(first - 0xc0)),
        0xb8..=0xbf | 0xf8..=0xff => {
            let is_list = first >= 0xf8;
            let length_of_length = usize::from(first - if is_list { 0xf7 } else { 0xb7 });
            let length_bytes = buf.get(1..1 + length_of_length).ok_or(RlpError)?;
            if length_bytes.first() == Some(&0) {
                return Err(RlpError);
            }
            let mut payload_len = 0usize;
            for &byte in length_bytes {
                payload_len = payload_len
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(usize::from(byte)))
                    .ok_or(RlpError)?;
            }
            // The long form exists only for what the short form cannot hold.
            if payload_len <= 55 {
                return Err(RlpError);
            }
            (is_list, 1 + length_of_length, payload_len)
        }
    };
    Ok((is_list, header_len, payload_len))
}

/// Read the item at the start of `buf`; returns it and the bytes after it.
pub fn read_item(buf: &[u8]) -> Result<(Item<'_>, &[u8]), RlpError> {
    let (is_list, header_len, payload_len) = header(buf)?;
    let end = header_len.checked_add(payload_len).ok_or(RlpError)?;
    let payload = buf.get(header_len..end).ok_or(RlpError)?;
    // `0x81 b` with `b < 0x80` is a non-canonical spelling of the single byte `b`.
    if !is_list && header_len == 1 && payload_len == 1 && payload.first().is_some_and(|b| *b < 0x80)
    {
        return Err(RlpError);
    }
    let rest = buf.get(end..).ok_or(RlpError)?;
    let item = if is_list {
        Item::List(payload)
    } else {
        Item::String(payload)
    };
    Ok((item, rest))
}

/// Whether `bytes` is exactly one RLP list: a list header whose declared payload length
/// accounts for every remaining byte. A Tempo transaction is `0x76 || rlp([fields…, sig])`,
/// so this is the cheapest structural check before the bytes are sent to a node.
pub fn list_spans_exactly(bytes: &[u8]) -> bool {
    match header(bytes) {
        Ok((true, header_len, payload_len)) => {
            header_len.checked_add(payload_len) == Some(bytes.len())
        }
        _ => false,
    }
}

/// The items of a list payload, in order. Stops with an error at the first malformed item.
#[derive(Debug, Clone, Copy)]
pub struct ListReader<'a> {
    rest: &'a [u8],
}

impl<'a> ListReader<'a> {
    /// A reader over the payload of a list (what [`Item::List`] carries).
    pub fn new(payload: &'a [u8]) -> Self {
        Self { rest: payload }
    }

    /// The next item, `Ok(None)` at the end of the list.
    pub fn next_item(&mut self) -> Result<Option<Item<'a>>, RlpError> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let (item, rest) = read_item(self.rest)?;
        self.rest = rest;
        Ok(Some(item))
    }

    /// The next item; the list ending is an error.
    pub fn expect_item(&mut self) -> Result<Item<'a>, RlpError> {
        self.next_item()?.ok_or(RlpError)
    }

    /// The next item as a byte string.
    pub fn expect_string(&mut self) -> Result<&'a [u8], RlpError> {
        match self.expect_item()? {
            Item::String(bytes) => Ok(bytes),
            Item::List(_) => Err(RlpError),
        }
    }

    /// The next item as a list payload.
    pub fn expect_list(&mut self) -> Result<&'a [u8], RlpError> {
        match self.expect_item()? {
            Item::List(payload) => Ok(payload),
            Item::String(_) => Err(RlpError),
        }
    }

    /// Whether every item has been read.
    pub fn is_finished(&self) -> bool {
        self.rest.is_empty()
    }
}

/// A canonical RLP unsigned integer of at most 8 bytes: no leading zero, empty for zero.
pub fn uint_u64(bytes: &[u8]) -> Result<u64, RlpError> {
    if bytes.len() > 8 || bytes.first() == Some(&0) {
        return Err(RlpError);
    }
    Ok(bytes
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

/// A canonical RLP unsigned integer of at most 16 bytes: no leading zero, empty for zero.
pub fn uint_u128(bytes: &[u8]) -> Result<u128, RlpError> {
    if bytes.len() > 16 || bytes.first() == Some(&0) {
        return Err(RlpError);
    }
    Ok(bytes
        .iter()
        .fold(0u128, |value, byte| (value << 8) | u128::from(*byte)))
}

// ---------------------------------------------------------------------------------------
// Encoding. The writer side of the same canonical form the reader enforces.
// ---------------------------------------------------------------------------------------

/// Append the header of a string (`short_base` 0x80) or list (`short_base` 0xc0) whose
/// payload is `payload_len` bytes.
fn put_header(out: &mut Vec<u8>, short_base: u8, payload_len: usize) {
    if payload_len <= 55 {
        // `payload_len <= 55` fits a u8; the fallback is unreachable.
        out.push(short_base.saturating_add(u8::try_from(payload_len).unwrap_or(55)));
        return;
    }
    let length: Vec<u8> = payload_len
        .to_be_bytes()
        .into_iter()
        .skip_while(|byte| *byte == 0)
        .collect();
    // A usize has at most 8 length bytes.
    out.push(
        short_base
            .saturating_add(55)
            .saturating_add(u8::try_from(length.len()).unwrap_or(8)),
    );
    out.extend(length);
}

/// Append `content` as an RLP byte string.
pub fn put_string(out: &mut Vec<u8>, content: &[u8]) {
    match content {
        [byte] if *byte < 0x80 => out.push(*byte),
        content => {
            put_header(out, 0x80, content.len());
            out.extend_from_slice(content);
        }
    }
}

/// Append `payload` (already-encoded items) wrapped as an RLP list.
pub fn put_list(out: &mut Vec<u8>, payload: &[u8]) {
    put_header(out, 0xc0, payload.len());
    out.extend_from_slice(payload);
}

/// Append an unsigned integer given as big-endian bytes (leading zeros are dropped; zero is
/// the empty string).
pub fn put_uint_be(out: &mut Vec<u8>, big_endian: &[u8]) {
    let first_significant = big_endian
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(big_endian.len());
    put_string(out, big_endian.get(first_significant..).unwrap_or(&[]));
}

/// Append an unsigned integer.
pub fn put_uint(out: &mut Vec<u8>, value: u128) {
    put_uint_be(out, &value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(content: &[u8]) -> Vec<u8> {
        match content {
            [byte] if *byte < 0x80 => vec![*byte],
            content if content.len() <= 55 => {
                let mut out = vec![0x80 + content.len() as u8];
                out.extend(content);
                out
            }
            content => {
                let length: Vec<u8> = content
                    .len()
                    .to_be_bytes()
                    .into_iter()
                    .skip_while(|b| *b == 0)
                    .collect();
                let mut out = vec![0xb7 + length.len() as u8];
                out.extend(length);
                out.extend(content);
                out
            }
        }
    }

    fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.concat();
        let mut out = if payload.len() <= 55 {
            vec![0xc0 + payload.len() as u8]
        } else {
            let length: Vec<u8> = payload
                .len()
                .to_be_bytes()
                .into_iter()
                .skip_while(|b| *b == 0)
                .collect();
            let mut head = vec![0xf7 + length.len() as u8];
            head.extend(length);
            head
        };
        out.extend(payload);
        out
    }

    #[test]
    fn reads_strings_lists_and_the_rest() {
        let encoded = [string(b"dog"), string(&[]), string(&[0x05]), list(&[])].concat();
        let (item, rest) = read_item(&encoded).unwrap();
        assert_eq!(item, Item::String(b"dog"));
        let (item, rest) = read_item(rest).unwrap();
        assert_eq!(item, Item::String(&[]));
        let (item, rest) = read_item(rest).unwrap();
        assert_eq!(item, Item::String(&[0x05]));
        let (item, rest) = read_item(rest).unwrap();
        assert_eq!(item, Item::List(&[]));
        assert!(rest.is_empty());
        // A long string and a long list.
        let long = vec![7u8; 300];
        let encoded = list(&[string(&long), string(&long)]);
        let (Item::List(payload), rest) = read_item(&encoded).unwrap() else {
            panic!("not a list");
        };
        assert!(rest.is_empty());
        let mut reader = ListReader::new(payload);
        assert_eq!(reader.expect_string().unwrap(), &long[..]);
        assert_eq!(reader.expect_string().unwrap(), &long[..]);
        assert!(reader.is_finished());
        assert_eq!(reader.next_item(), Ok(None));
        assert_eq!(reader.expect_item(), Err(RlpError));
    }

    #[test]
    fn non_canonical_and_truncated_input_is_refused() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("0x81 below 0x80", vec![0x81, 0x05]),
            (
                "long form for 55 bytes",
                [vec![0xb8, 55], vec![0; 55]].concat(),
            ),
            ("long list form for 3 bytes", vec![0xf8, 3, 1, 2, 3]),
            (
                "length with leading zero",
                [vec![0xb9, 0, 60], vec![0; 60]].concat(),
            ),
            (
                "list length with leading zero",
                [vec![0xf9, 0, 60], vec![0; 60]].concat(),
            ),
            ("short string truncated", vec![0x83, b'd', b'o']),
            ("short list truncated", vec![0xc3, 0x01]),
            ("length bytes truncated", vec![0xb9, 0x01]),
            ("length past the input", vec![0xb8, 200, 1, 2, 3]),
            (
                "length that overflows usize",
                [vec![0xbf], vec![0xff; 8]].concat(),
            ),
            (
                "length near usize::MAX",
                [vec![0xbf, 0x7f], vec![0xff; 7]].concat(),
            ),
        ];
        for (label, bytes) in cases {
            assert_eq!(read_item(&bytes), Err(RlpError), "{label}");
        }
    }

    #[test]
    fn a_declared_length_cannot_reach_past_the_input_even_near_the_limits() {
        // An 8-byte length of 2^63 on a tiny buffer: refused without allocating or looping.
        let mut huge = vec![0xbf, 0x80];
        huge.extend([0u8; 7]);
        assert_eq!(read_item(&huge), Err(RlpError));
        // A list header that claims more than the buffer holds.
        let mut list_huge = vec![0xff, 0x80];
        list_huge.extend([0u8; 7]);
        assert_eq!(read_item(&list_huge), Err(RlpError));
        assert!(!list_spans_exactly(&list_huge));
    }

    #[test]
    fn list_framing_must_account_for_every_byte() {
        for len in [0usize, 1, 55, 56, 255, 256, 70_000] {
            let encoded = list(&[vec![0x01; len]]);
            // `vec![0x01; len]` is `len` single-byte items, a valid list payload.
            assert!(list_spans_exactly(&encoded), "len {len}");
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert!(!list_spans_exactly(&trailing), "trailing {len}");
            if let Some((_, short)) = encoded.split_last() {
                assert!(!list_spans_exactly(short), "short {len}");
            }
        }
        assert!(!list_spans_exactly(&[]));
        assert!(!list_spans_exactly(&[0x80]), "a string is not a list");
        assert!(!list_spans_exactly(&[0x05]));
        assert!(!list_spans_exactly(&[0xf8]));
        assert!(
            !list_spans_exactly(&[0xf8, 0x01, 0x00]),
            "long form below 56"
        );
        assert!(
            !list_spans_exactly(&[0xf9, 0x00, 0x38]),
            "leading zero length"
        );
    }

    #[test]
    fn integers_are_canonical_and_bounded() {
        assert_eq!(uint_u64(&[]), Ok(0));
        assert_eq!(uint_u64(&[0x01]), Ok(1));
        assert_eq!(uint_u64(&[0xa5, 0xbf]), Ok(42431));
        assert_eq!(uint_u64(&[0xff; 8]), Ok(u64::MAX));
        assert_eq!(uint_u64(&[0x00]), Err(RlpError), "zero is the empty string");
        assert_eq!(uint_u64(&[0x00, 0x01]), Err(RlpError), "leading zero");
        assert_eq!(uint_u64(&[1; 9]), Err(RlpError), "wider than u64");
    }
    #[test]
    fn the_writer_produces_the_canonical_form_the_reader_accepts() {
        for content in [
            vec![],
            vec![0x05],
            vec![0x80],
            vec![7u8; 55],
            vec![7u8; 56],
            vec![7u8; 300],
        ] {
            let mut out = Vec::new();
            put_string(&mut out, &content);
            assert_eq!(out, string(&content));
            assert_eq!(read_item(&out), Ok((Item::String(&content), &[][..])));
        }
        for len in [0usize, 1, 55, 56, 300] {
            let payload = vec![0x01u8; len];
            let mut out = Vec::new();
            put_list(&mut out, &payload);
            assert_eq!(out, list(std::slice::from_ref(&payload)));
            assert!(list_spans_exactly(&out));
        }
        let mut out = Vec::new();
        put_uint(&mut out, 0);
        put_uint(&mut out, 1);
        put_uint(&mut out, 42431);
        put_uint_be(&mut out, &[0, 0, 0xa5, 0xbf]);
        assert_eq!(out, [0x80, 0x01, 0x82, 0xa5, 0xbf, 0x82, 0xa5, 0xbf]);
        let mut out = Vec::new();
        put_uint_be(&mut out, &[0xff; 32]);
        assert_eq!(out.len(), 33);
        assert_eq!(out.first(), Some(&0xa0));
        let mut out = Vec::new();
        put_uint_be(&mut out, &[0; 32]);
        assert_eq!(out, [0x80]);
    }

    #[test]
    fn wide_integers_are_canonical_and_bounded() {
        assert_eq!(uint_u128(&[]), Ok(0));
        assert_eq!(uint_u128(&[0xff; 16]), Ok(u128::MAX));
        assert_eq!(uint_u128(&[0x00, 0x01]), Err(RlpError));
        assert_eq!(uint_u128(&[1; 17]), Err(RlpError));
    }
}
