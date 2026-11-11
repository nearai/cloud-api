//! Signup attribution: the campaign a sign-in request carried, recorded on the user it creates.
//!
//! The values come from an unauthenticated request (the login redirect's query or the NEAR login
//! body), so each is checked against a narrow charset and length. A value that fails is dropped,
//! never refused: attribution must not make a sign-in fail. The values are not personal data,
//! but they are caller-supplied text, so they are never logged; logs record presence only.

/// Longest accepted value.
pub const MAX_ATTRIBUTION_LEN: usize = 100;

/// `utm_source`, `utm_medium` and `utm_campaign`, each present only when it passed validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignupAttribution {
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
}

impl SignupAttribution {
    /// Keeps each value that is 1 to [`MAX_ATTRIBUTION_LEN`] characters of `[A-Za-z0-9._~-]`.
    pub fn from_request(
        utm_source: Option<&str>,
        utm_medium: Option<&str>,
        utm_campaign: Option<&str>,
    ) -> Self {
        Self {
            utm_source: utm_source.and_then(accepted),
            utm_medium: utm_medium.and_then(accepted),
            utm_campaign: utm_campaign.and_then(accepted),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.utm_source.is_none() && self.utm_medium.is_none() && self.utm_campaign.is_none()
    }
}

fn accepted(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ATTRIBUTION_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'));
    valid.then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_kept_or_dropped() {
        let at_limit = "a".repeat(MAX_ATTRIBUTION_LEN);
        let over_limit = "a".repeat(MAX_ATTRIBUTION_LEN + 1);
        let cases: &[(&str, Option<&str>, Option<&str>)] = &[
            ("plain", Some("partner-app"), Some("partner-app")),
            (
                "all allowed punctuation",
                Some("v1.2_b~c-d"),
                Some("v1.2_b~c-d"),
            ),
            ("at the length limit", Some(&at_limit), Some(&at_limit)),
            ("absent", None, None),
            ("empty", Some(""), None),
            ("over the length limit", Some(&over_limit), None),
            ("space", Some("partner app"), None),
            ("surrounding space is not trimmed", Some(" partner"), None),
            ("markup", Some("<script>"), None),
            ("url", Some("https://example.com"), None),
            ("quote", Some("a\"b"), None),
            ("newline", Some("a\nb"), None),
            ("percent", Some("a%20b"), None),
            ("non-ascii", Some("caf\u{e9}"), None),
        ];
        for (name, input, expected) in cases {
            let parsed = SignupAttribution::from_request(*input, *input, *input);
            let expected = expected.map(str::to_string);
            assert_eq!(parsed.utm_source, expected, "{name}: utm_source");
            assert_eq!(parsed.utm_medium, expected, "{name}: utm_medium");
            assert_eq!(parsed.utm_campaign, expected, "{name}: utm_campaign");
            assert_eq!(parsed.is_empty(), expected.is_none(), "{name}: is_empty");
        }
    }

    #[test]
    fn each_field_is_checked_on_its_own() {
        let parsed = SignupAttribution::from_request(Some("partner-app"), Some("a b"), None);
        assert_eq!(
            parsed,
            SignupAttribution {
                utm_source: Some("partner-app".to_string()),
                utm_medium: None,
                utm_campaign: None,
            }
        );
        assert!(!parsed.is_empty());
    }
}
