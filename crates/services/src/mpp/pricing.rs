//! What a request costs and which of its fields pricing reads.
//!
//! Pricing runs on the unauthenticated path, before any payment, so it must stay cheap for
//! a hostile body: [`RequestShape::from_body`] reads four fields and skips every other
//! value without building it, including arrays and objects given for the numeric fields.

use std::fmt;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// Why a request is not priced even though its model is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitViolation {
    BodyTooLarge {
        cap: u32,
    },
    MaxTokensRequired {
        cap: u32,
    },
    MaxTokensInvalid,
    MaxTokensTooLarge {
        cap: u32,
    },
    MultipleChoices,
    /// The model has no price, so there is nothing to charge.
    ModelNotPriced,
}

impl LimitViolation {
    /// Closed code for logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::MaxTokensRequired { .. } => "max_tokens_required",
            Self::MaxTokensInvalid => "max_tokens_invalid",
            Self::MaxTokensTooLarge { .. } => "max_tokens_too_large",
            Self::MultipleChoices => "multiple_choices",
            Self::ModelNotPriced => "model_not_priced",
        }
    }

    /// A message naming the limit. Built from configured numbers only, never from the body.
    pub fn message(&self) -> String {
        match self {
            Self::BodyTooLarge { cap } => format!(
                "Request body exceeds the {cap}-byte limit for pay-per-request (MPP) on this model."
            ),
            Self::MaxTokensRequired { cap } => format!(
                "max_tokens (or max_completion_tokens) is required for pay-per-request (MPP) and must be at most {cap}."
            ),
            Self::MaxTokensInvalid => {
                "max_tokens and max_completion_tokens must be positive integers.".to_string()
            }
            Self::MaxTokensTooLarge { cap } => format!(
                "max_tokens (or max_completion_tokens) exceeds the pay-per-request (MPP) limit of {cap} for this model."
            ),
            Self::MultipleChoices => {
                "n must be 1 (or omitted) for pay-per-request (MPP).".to_string()
            }
            Self::ModelNotPriced => {
                "This model is not available for pay-per-request (MPP).".to_string()
            }
        }
    }
}

/// A numeric request parameter as the pricing sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Param {
    /// Absent or `null`.
    #[default]
    Absent,
    /// A positive JSON integer.
    Value(u64),
    /// Present but not a positive integer (zero, negative, fractional, text, array, ...).
    Invalid,
}

impl<'de> Deserialize<'de> for Param {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ParamVisitor)
    }
}

/// Classifies a value without keeping it: containers are walked and dropped.
struct ParamVisitor;

impl<'de> Visitor<'de> for ParamVisitor {
    type Value = Param;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Param, E> {
        Ok(if value > 0 {
            Param::Value(value)
        } else {
            Param::Invalid
        })
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Param, E> {
        Ok(u64::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .map_or(Param::Invalid, Param::Value))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Param, E> {
        Ok(Param::Invalid)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Param, E> {
        Ok(Param::Invalid)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Param, E> {
        Ok(Param::Invalid)
    }

    fn visit_none<E: de::Error>(self) -> Result<Param, E> {
        Ok(Param::Absent)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Param, E> {
        Ok(Param::Absent)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Param, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Param, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Param::Invalid)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Param, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Param::Invalid)
    }
}

/// The few request fields pricing reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequestShape {
    pub model: String,
    #[serde(default)]
    pub max_tokens: Param,
    #[serde(default)]
    pub max_completion_tokens: Param,
    #[serde(default)]
    pub n: Param,
}

impl RequestShape {
    /// `None` when the body is not a JSON object with a string `model`: such a request is not
    /// priced and falls through to ordinary authentication. A repeated field is refused (the
    /// derive rejects a duplicate known key), so two parsers cannot disagree about which
    /// value the request carries.
    pub fn from_body(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }

    /// The larger of `max_tokens` / `max_completion_tokens` when present.
    pub(super) fn requested_tokens(&self) -> Param {
        match (self.max_tokens, self.max_completion_tokens) {
            (Param::Invalid, _) | (_, Param::Invalid) => Param::Invalid,
            (Param::Absent, Param::Absent) => Param::Absent,
            (Param::Value(a), Param::Value(b)) => Param::Value(a.max(b)),
            (Param::Value(a), Param::Absent) | (Param::Absent, Param::Value(a)) => Param::Value(a),
        }
    }
}

/// The price of a request that passed its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    /// Token base units.
    pub amount: u128,
}

/// Result of pricing one request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pricing {
    /// Not an MPP request: unparseable, too large to be one, or the model has no price.
    /// Ordinary auth applies.
    Unpriced,
    Priced(Price),
    /// The model is priced but the request breaks a limit.
    Violation(LimitViolation),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shape(value: serde_json::Value) -> Option<RequestShape> {
        RequestShape::from_body(value.to_string().as_bytes())
    }

    #[test]
    fn numeric_fields_classify_without_keeping_the_value() {
        let cases = [
            (json!(5), Param::Value(5)),
            (json!(u64::MAX), Param::Value(u64::MAX)),
            (json!(null), Param::Absent),
            (json!(0), Param::Invalid),
            (json!(-3), Param::Invalid),
            (json!(1.5), Param::Invalid),
            (json!(2.0), Param::Invalid),
            (json!(1e30), Param::Invalid),
            (json!("7"), Param::Invalid),
            (json!(true), Param::Invalid),
            (json!([1, [2, {"a": [3]}]]), Param::Invalid),
            (json!({"a": {"b": [1, 2, 3]}}), Param::Invalid),
            (json!([]), Param::Invalid),
            (json!({}), Param::Invalid),
        ];
        for (value, expected) in cases {
            let parsed = shape(json!({"model": "m", "max_tokens": value})).unwrap();
            assert_eq!(parsed.max_tokens, expected, "{value}");
            assert_eq!(parsed.n, Param::Absent);
        }
        let absent = shape(json!({"model": "m"})).unwrap();
        assert_eq!(absent.max_tokens, Param::Absent);
        assert_eq!(absent.max_completion_tokens, Param::Absent);
    }

    #[test]
    fn a_deeply_nested_value_is_skipped_without_recursion_or_allocation() {
        // IgnoredAny skips iteratively: 50k levels neither overflow the stack nor build a tree.
        let depth = 50_000;
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let body = format!(r#"{{"model":"m","max_tokens":{nested},"n":{{"a":{nested}}}}}"#);
        let parsed = RequestShape::from_body(body.as_bytes()).unwrap();
        assert_eq!(parsed.max_tokens, Param::Invalid);
        assert_eq!(parsed.n, Param::Invalid);
    }

    #[test]
    fn a_repeated_field_is_refused_rather_than_resolved() {
        for body in [
            r#"{"model":"m","max_tokens":5,"max_tokens":9999999}"#,
            r#"{"model":"m","n":1,"n":3}"#,
            r#"{"model":"a","model":"b"}"#,
            r#"{"model":"m","max_completion_tokens":5,"max_completion_tokens":6}"#,
        ] {
            assert_eq!(RequestShape::from_body(body.as_bytes()), None, "{body}");
        }
        // Unknown keys may repeat: nothing reads them.
        assert!(RequestShape::from_body(br#"{"model":"m","x":1,"x":2}"#).is_some());
    }

    #[test]
    fn non_objects_and_a_non_string_model_are_not_requests() {
        for body in [
            &b"[]"[..],
            b"null",
            b"3",
            b"\"m\"",
            b"",
            br#"{"model":5}"#,
            br#"{"max_tokens":5}"#,
        ] {
            assert_eq!(RequestShape::from_body(body), None);
        }
    }
}
