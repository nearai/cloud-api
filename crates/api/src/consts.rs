/// Maximum length for generic name fields persisted in the database
pub const MAX_NAME_LENGTH: usize = 255;

/// Maximum length for generic description fields
pub const MAX_DESCRIPTION_LENGTH: usize = 10 * 1024;

/// Maximum serialized size for small metadata blobs (e.g. conversation metadata)
pub const MAX_METADATA_SIZE_BYTES: usize = 16 * 1024;
