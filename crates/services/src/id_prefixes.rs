//! ID prefix constants for resource identifiers.
//!
//! These prefixes are used to create human-readable IDs that follow
//! our naming conventions.

/// Prefix for chat completion IDs
pub const PREFIX_CHATCMPL: &str = "chatcmpl-";

/// Prefix for response IDs
pub const PREFIX_RESP: &str = "resp_";

/// Prefix for file IDs
pub const PREFIX_FILE: &str = "file-";

/// Prefix for message IDs
pub const PREFIX_MSG: &str = "msg_";

/// Prefix for conversation IDs
pub const PREFIX_CONV: &str = "conv_";

/// Prefix for secret/API key IDs
pub const PREFIX_SK: &str = "sk-";

/// Prefix for MCP approval request IDs
pub const PREFIX_MCPR: &str = "mcpr_";

/// Prefix for function call IDs
pub const PREFIX_FC: &str = "fc_";

/// Prefix for function call output IDs
pub const PREFIX_FCO: &str = "fco_";
