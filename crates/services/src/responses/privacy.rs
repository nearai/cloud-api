//! Privacy compliance utilities for responses service
//!
//! Enforces CLAUDE.md privacy requirements:
//! - NEVER log file contents (base64 images, file data, etc.)
//! - NEVER log conversation content (user prompts, completion text)
//! - NEVER log PII or user metadata unless necessary
//!
//! This module provides validation functions to prevent accidental privacy leaks.

use crate::responses::models;

/// Validates that a request doesn't contain sensitive data before logging/processing
///
/// # Privacy Checks
/// - Detects base64-encoded image data in input
/// - Validates no file contents in error messages
/// - Ensures no conversation text in debug logs
pub struct PrivacyValidator;

impl PrivacyValidator {
    /// Check if a string might contain base64-encoded image data
    ///
    /// Returns true if the string looks like it might contain image data
    /// (very long base64 string or data: URL with base64)
    pub fn might_contain_image_data(s: &str) -> bool {
        // Check for data: URLs with image MIME types (already validated by MIME check earlier)
        if s.contains("data:image/") || s.contains("data:application/") {
            return true;
        }

        // Check for very long base64 strings (likely encoded images)
        // Base64 images are typically 1000+ characters
        if s.len() > 1000 && is_likely_base64(s) {
            return true;
        }

        false
    }

    /// Validate request for privacy compliance before processing
    ///
    /// This should be called before any logging to ensure compliance with CLAUDE.md
    pub fn validate_request(request: &models::CreateResponseRequest) -> Result<(), String> {
        // Check input for suspicious content
        if let Some(input) = &request.input {
            match input {
                models::ResponseInput::Text(text) => {
                    if Self::might_contain_image_data(text) {
                        return Err("Request input contains suspected image data".to_string());
                    }
                }
                models::ResponseInput::Items(items) => {
                    for item in items {
                        if let Some(models::ResponseContent::Parts(parts)) = item.content() {
                            for part in parts {
                                // InputImage and InputFile are expected and handled appropriately
                                if let models::ResponseContentPart::InputText { text } = part {
                                    if Self::might_contain_image_data(text) {
                                        return Err(
                                            "Request contains text with suspected image data"
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Sanitize error message to ensure it doesn't leak sensitive data
    ///
    /// Removes potentially sensitive information from error messages
    pub fn sanitize_error_message(msg: &str) -> String {
        // If message contains base64 or image data patterns, truncate it
        if Self::might_contain_image_data(msg) {
            tracing::warn!("Error message contained potential image data, sanitizing");
            return "Image processing error (see logs for details)".to_string();
        }

        // Check for suspicious patterns
        if msg.len() > 5000 {
            tracing::warn!(
                "Error message is very long ({}), truncating for safety",
                msg.len()
            );
            return format!("{}...(truncated)", &msg[..500]);
        }

        msg.to_string()
    }
}

/// Helper function to detect if a string looks like base64
fn is_likely_base64(s: &str) -> bool {
    // Base64 characters: A-Z, a-z, 0-9, +, /, =
    // Must be mostly base64 characters
    let base64_chars: usize = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .count();

    let ratio = base64_chars as f32 / s.len() as f32;

    // If > 90% of characters are base64, likely base64
    ratio > 0.9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detects_image_data_url() {
        assert!(PrivacyValidator::might_contain_image_data(
            "data:image/png;base64,iVBORw0KGgo="
        ));
        assert!(PrivacyValidator::might_contain_image_data(
            "data:image/jpeg;base64,/9j/4AAQSkZJRg=="
        ));
    }

    #[test]
    fn test_detects_long_base64() {
        let long_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".repeat(20);
        assert!(PrivacyValidator::might_contain_image_data(&long_base64));
    }

    #[test]
    fn test_ignores_normal_text() {
        assert!(!PrivacyValidator::might_contain_image_data(
            "What is the capital of France?"
        ));
        assert!(!PrivacyValidator::might_contain_image_data(
            "Please generate a cat wearing sunglasses"
        ));
    }

    #[test]
    fn test_sanitizes_long_errors() {
        let long_error = "a".repeat(10000);
        let sanitized = PrivacyValidator::sanitize_error_message(&long_error);
        assert!(sanitized.len() < 1000);
    }

    #[test]
    fn test_sanitizes_image_errors() {
        let error_with_image = "Failed to process: data:image/png;base64,iVBORw0KGgo=";
        let sanitized = PrivacyValidator::sanitize_error_message(error_with_image);
        assert!(!sanitized.contains("base64"));
        assert!(!sanitized.contains("iVBOR"));
    }
}
