//! Shared parsing of OpenAI-style multimodal message content.
//!
//! OpenAI chat messages carry `content` as either a plain string or an array of
//! typed content parts, e.g.:
//!
//! ```json
//! [
//!   {"type": "text", "text": "What is in this image?"},
//!   {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR..."}}
//! ]
//! ```
//!
//! Anthropic and Gemini have their own native image block shapes. Previously the
//! external converters flattened the entire content array to a string via
//! `Value::to_string()`, which turned the image into a JSON text blob the model
//! never decoded as an image (issue #640: fjord photo described as "a red
//! flower"). This module extracts text and image parts faithfully so the
//! converters can rebuild the provider-native image blocks, preserving the exact
//! base64 payload and media type.

/// A single piece of parsed OpenAI message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    /// Plain text.
    Text(String),
    /// An image supplied as a `data:` URI (base64 inline).
    ImageBase64 {
        /// MIME type, e.g. `image/png`. Defaults to `image/jpeg` when the data
        /// URI omits it (matching how providers treat an unspecified type).
        media_type: String,
        /// The exact base64 payload, with the `data:[mime];base64,` prefix
        /// stripped and surrounding ASCII whitespace removed. The bytes
        /// themselves are not re-encoded.
        data: String,
    },
    /// An image supplied as a remote `http(s)` URL.
    ImageUrl { url: String },
}

/// Parse an OpenAI message `content` value into ordered parts.
///
/// - A JSON string yields a single [`ContentPart::Text`].
/// - A JSON array yields one part per recognised element (`text`, `image_url`),
///   in order. Unrecognised parts are skipped.
/// - Any other value falls back to its JSON string form as a single text part,
///   matching the previous lenient behaviour.
pub fn parse_content(value: &serde_json::Value) -> Vec<ContentPart> {
    match value {
        serde_json::Value::String(s) => vec![ContentPart::Text(s.clone())],
        serde_json::Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                if let Some(part) = parse_content_part(item) {
                    parts.push(part);
                }
            }
            parts
        }
        other => vec![ContentPart::Text(other.to_string())],
    }
}

pub(crate) fn parse_content_part(item: &serde_json::Value) -> Option<ContentPart> {
    let obj = item.as_object()?;

    match obj.get("type").and_then(|t| t.as_str()) {
        Some("text") => obj
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| ContentPart::Text(s.to_string())),
        Some("image_url") => {
            // OpenAI shape: {"type":"image_url","image_url":{"url":"..."}}.
            // Also tolerate {"type":"image_url","image_url":"..."}.
            let url = match obj.get("image_url") {
                Some(serde_json::Value::Object(m)) => m.get("url").and_then(|u| u.as_str()),
                Some(serde_json::Value::String(s)) => Some(s.as_str()),
                _ => None,
            }?;
            Some(parse_image_url(url))
        }
        _ => None,
    }
}

/// Classify an image URL string as either an inline base64 data URI or a remote
/// URL, splitting the data URI into media type + payload without re-encoding.
pub fn parse_image_url(url: &str) -> ContentPart {
    if let Some(rest) = url.strip_prefix("data:") {
        // rest = "[media_type][;base64],<payload>"  (media_type / params optional)
        if let Some(comma) = rest.find(',') {
            let meta = &rest[..comma];
            let payload = &rest[comma + 1..];

            let is_base64 = meta
                .split(';')
                .any(|p| p.trim().eq_ignore_ascii_case("base64"));

            // media type is the segment before the first ';' (if any)
            let media_type = meta.split(';').next().unwrap_or("").trim();
            let media_type = if media_type.is_empty() {
                "image/jpeg".to_string()
            } else {
                media_type.to_string()
            };

            if is_base64 {
                // Strip only surrounding ASCII whitespace (e.g. accidental
                // newlines around the payload). Do NOT touch interior bytes —
                // the payload is forwarded verbatim so providers decode the
                // exact original image.
                let data = payload.trim().to_string();
                return ContentPart::ImageBase64 { media_type, data };
            }
        }
        // Malformed / non-base64 data URI: forward the whole thing as a URL so
        // we never silently corrupt it.
        return ContentPart::ImageUrl {
            url: url.to_string(),
        };
    }

    ContentPart::ImageUrl {
        url: url.to_string(),
    }
}

/// Flatten parsed parts back into a single text string.
///
/// Used by code paths that only support text (e.g. system messages). Image
/// parts are dropped rather than serialised as a JSON blob.
pub fn parts_to_text(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text(t) = part {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Convenience: extract text from a raw content value (string or array),
/// dropping any image parts.
pub fn text_from_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(_) => parts_to_text(&parse_content(value)),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_string() {
        let v = serde_json::json!("hello");
        assert_eq!(parse_content(&v), vec![ContentPart::Text("hello".into())]);
    }

    #[test]
    fn parses_text_and_image_data_uri() {
        let v = serde_json::json!([
            {"type": "text", "text": "describe"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        assert_eq!(
            parse_content(&v),
            vec![
                ContentPart::Text("describe".into()),
                ContentPart::ImageBase64 {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }
            ]
        );
    }

    #[test]
    fn preserves_exact_base64_payload() {
        // base64 of arbitrary bytes including '+', '/', '=' padding
        let payload = "iVBORw0KGgo+/AB==";
        let url = format!("data:image/png;base64,{payload}");
        match parse_image_url(&url) {
            ContentPart::ImageBase64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, payload, "base64 payload must be byte-identical");
            }
            other => panic!("expected ImageBase64, got {other:?}"),
        }
    }

    #[test]
    fn strips_surrounding_whitespace_only() {
        let url = "data:image/jpeg;base64,  AAAA\n ";
        match parse_image_url(url) {
            ContentPart::ImageBase64 { data, media_type } => {
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(data, "AAAA");
            }
            other => panic!("expected ImageBase64, got {other:?}"),
        }
    }

    #[test]
    fn defaults_media_type_when_missing() {
        let url = "data:;base64,AAAA";
        match parse_image_url(url) {
            ContentPart::ImageBase64 { media_type, .. } => assert_eq!(media_type, "image/jpeg"),
            other => panic!("expected ImageBase64, got {other:?}"),
        }
    }

    #[test]
    fn treats_http_url_as_remote() {
        let url = "https://example.com/cat.jpg";
        assert_eq!(
            parse_image_url(url),
            ContentPart::ImageUrl {
                url: url.to_string()
            }
        );
    }

    #[test]
    fn handles_image_url_as_bare_string() {
        let v = serde_json::json!([
            {"type": "image_url", "image_url": "data:image/png;base64,ZZZ"}
        ]);
        assert_eq!(
            parse_content(&v),
            vec![ContentPart::ImageBase64 {
                media_type: "image/png".into(),
                data: "ZZZ".into(),
            }]
        );
    }

    #[test]
    fn text_from_content_drops_images() {
        let v = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            {"type": "text", "text": "b"}
        ]);
        assert_eq!(text_from_content(&v), "a\nb");
    }
}
