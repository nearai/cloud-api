/// Maximum length for generic name fields persisted in the database
pub const MAX_NAME_LENGTH: usize = 255;

/// Maximum length for generic description fields
pub const MAX_DESCRIPTION_LENGTH: usize = 10 * 1024;

/// Maximum length for email fields
pub const MAX_EMAIL_LENGTH: usize = 255;

/// Maximum length for organization/system prompts
pub const MAX_SYSTEM_PROMPT_LENGTH: usize = 64 * 1024;

/// Maximum serialized size for settings / larger JSON blobs
pub const MAX_SETTINGS_SIZE_BYTES: usize = 32 * 1024;

/// Maximum number of invitations allowed in a single batch request
pub const MAX_INVITATIONS_PER_REQUEST: usize = 100;

/// Maximum length for signatures in VPC login or similar requests
pub const MAX_SIGNATURE_LENGTH: usize = 4096;

/// Maximum length for avatar URL fields
pub const MAX_AVATAR_URL_LENGTH: usize = 1024 * 1024;
