use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Organization model - top level entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    /// API rate limits for the organization (requests per minute)
    pub rate_limit: Option<i32>,
    /// Custom settings for the organization
    pub settings: Option<serde_json::Value>,
}

/// User model - can belong to multiple organizations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    /// OAuth provider (github, google, etc.)
    pub auth_provider: String,
    /// OAuth provider user ID
    pub provider_user_id: String,
    /// Timestamp when all tokens were revoked (for invalidating access tokens)
    pub tokens_revoked_at: Option<DateTime<Utc>>,
}

/// Organization membership - many-to-many relationship between users and organizations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationMember {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub user_id: Uuid,
    pub role: OrganizationRole,
    pub joined_at: DateTime<Utc>,
    pub invited_by: Option<Uuid>,
}

/// Organization invitation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationInvitation {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub email: String,
    pub role: OrganizationRole,
    pub invited_by_user_id: Uuid,
    pub status: InvitationStatus,
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
}

/// Invitation status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Declined,
    Expired,
}

impl std::fmt::Display for InvitationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvitationStatus::Pending => write!(f, "pending"),
            InvitationStatus::Accepted => write!(f, "accepted"),
            InvitationStatus::Declined => write!(f, "declined"),
            InvitationStatus::Expired => write!(f, "expired"),
        }
    }
}

/// Role within an organization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrganizationRole {
    Owner,
    Admin,
    Member,
}

impl std::fmt::Display for OrganizationRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrganizationRole::Owner => write!(f, "owner"),
            OrganizationRole::Admin => write!(f, "admin"),
            OrganizationRole::Member => write!(f, "member"),
        }
    }
}

/// Workspace model - belongs to an organization, owns API keys
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub organization_id: Uuid,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    pub settings: Option<serde_json::Value>,
}

/// API Key for authentication - now workspace-owned
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub key_hash: String,   // Store hashed API key
    pub key_prefix: String, // First 8-10 chars for display (e.g., "sk-abc123")
    pub name: String,
    pub workspace_id: Uuid, // Changed from organization_id to workspace_id
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub deleted_at: Option<DateTime<Utc>>, // Soft delete timestamp
    /// Optional spending limit in nano-dollars (scale 9, USD). None means no limit.
    pub spend_limit: Option<i64>,
    /// Total usage/spend in nano-dollars (scale 9, USD). Computed from usage logs.
    pub usage: i64,
}

/// Session for OAuth authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String, // Store hashed session token
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

/// Admin access token for tracking and managing admin access tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAccessToken {
    pub id: Uuid,
    pub token_hash: String,
    pub created_by_user_id: Uuid,
    pub name: String,
    pub creation_reason: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by_user_id: Option<Uuid>,
    pub revocation_reason: Option<String>,
    pub user_agent: Option<String>,
}

/// Request/Response DTOs

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddOrganizationMemberRequest {
    pub user_id: Uuid,
    pub role: OrganizationRole,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateOrganizationMemberRequest {
    pub role: OrganizationRole,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateOrganizationRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub rate_limit: Option<i32>,
    pub settings: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiKeyResponse {
    pub id: Uuid,
    pub key: String, // Only returned on creation
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub settings: Option<serde_json::Value>,
}

impl OrganizationRole {
    pub fn can_manage_organization(&self) -> bool {
        matches!(self, OrganizationRole::Owner | OrganizationRole::Admin)
    }

    pub fn can_manage_members(&self) -> bool {
        matches!(self, OrganizationRole::Owner | OrganizationRole::Admin)
    }

    pub fn can_manage_api_keys(&self) -> bool {
        // All members can create and manage their own API keys
        true
    }

    pub fn can_delete_organization(&self) -> bool {
        matches!(self, OrganizationRole::Owner)
    }

    pub fn can_manage_mcp_connectors(&self) -> bool {
        matches!(self, OrganizationRole::Owner | OrganizationRole::Admin)
    }
}

/// MCP Connector Authentication Type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthType {
    None,
    Bearer,
}

impl std::fmt::Display for McpAuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpAuthType::None => write!(f, "none"),
            McpAuthType::Bearer => write!(f, "bearer"),
        }
    }
}

/// MCP Connector Status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionStatus {
    Pending,
    Connected,
    Failed,
}

/// MCP Connector model - external MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnector {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub mcp_server_url: String,
    pub auth_type: McpAuthType,
    pub auth_config: Option<serde_json::Value>,
    pub is_active: bool,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
    pub connection_status: McpConnectionStatus,
    pub error_message: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
}

/// Bearer token configuration for MCP connectors
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpBearerConfig {
    pub token: String,
}

/// Create MCP Connector Request
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateMcpConnectorRequest {
    pub name: String,
    pub description: Option<String>,
    pub mcp_server_url: String,
    pub auth_type: McpAuthType,
    pub bearer_token: Option<String>, // Required if auth_type is Bearer
}

/// Update MCP Connector Request
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateMcpConnectorRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub mcp_server_url: Option<String>,
    pub auth_type: Option<McpAuthType>,
    pub bearer_token: Option<String>,
    pub is_active: Option<bool>,
}

/// MCP Connector Usage Log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnectorUsage {
    pub id: Uuid,
    pub connector_id: Uuid,
    pub user_id: Uuid,
    pub method: String,
    pub request_payload: Option<serde_json::Value>,
    pub response_payload: Option<serde_json::Value>,
    pub status_code: Option<i32>,
    pub error_message: Option<String>,
    pub duration_ms: Option<i32>,
    pub created_at: DateTime<Utc>,
}

// ============================================
// Response and Conversation Models
// ============================================

/// Response model - stores AI response data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub model: String,
    pub status: ResponseStatus,
    pub instructions: Option<String>,
    pub conversation_id: Option<Uuid>,
    pub previous_response_id: Option<Uuid>, // Previous response (parent in thread)
    pub next_response_ids: Option<serde_json::Value>, // JSONB array of next response UUIDs
    pub usage: Option<serde_json::Value>,   // JSONB storing token usage
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Response status enum
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for ResponseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseStatus::InProgress => write!(f, "in_progress"),
            ResponseStatus::Completed => write!(f, "completed"),
            ResponseStatus::Failed => write!(f, "failed"),
            ResponseStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Conversation model - stores conversation metadata
/// Note: This model is scoped to workspace/api_key, not user (for app developers)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub pinned_at: Option<DateTime<Utc>>, // Timestamp when pinned, NULL if not pinned
    pub archived_at: Option<DateTime<Utc>>, // Timestamp when archived, NULL if not archived
    pub deleted_at: Option<DateTime<Utc>>, // Timestamp when soft-deleted, NULL if not deleted
    pub cloned_from_id: Option<Uuid>,     // ID of conversation this was cloned from
    pub metadata: serde_json::Value, // JSONB storing conversation metadata (includes title/name)
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ============================================
// Model Pricing Models
// ============================================

/// Model pricing and metadata - stores information about models and their pricing
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: Uuid,
    /// Canonical model name (e.g., "nearai/gpt-oss-120b") used for vLLM
    pub model_name: String,
    pub model_display_name: String,
    pub model_description: String,
    pub model_icon: Option<String>,

    // Pricing (fixed scale 9 = nano-dollars, USD only)
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,

    // Model metadata
    pub context_length: i32,
    pub verifiable: bool,
    pub aliases: Vec<String>,

    // Tracking fields
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Request to update model pricing
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateModelPricingRequest {
    pub input_cost_per_token: Option<i64>,
    pub output_cost_per_token: Option<i64>,
    pub model_display_name: Option<String>,
    pub model_description: Option<String>,
    pub model_icon: Option<String>,
    pub context_length: Option<i32>,
    pub verifiable: Option<bool>,
    pub is_active: Option<bool>,
    pub aliases: Option<Vec<String>>,
}

/// Model pricing history - stores historical pricing data for models
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricingHistory {
    pub id: Uuid,
    pub model_id: Uuid,

    // Pricing snapshot (fixed scale 9 = nano-dollars, USD only)
    pub input_cost_per_token: i64,
    pub output_cost_per_token: i64,

    // Model metadata snapshot
    pub context_length: i32,
    pub model_display_name: String,
    pub model_description: String,

    // Temporal fields
    pub effective_from: DateTime<Utc>,
    pub effective_until: Option<DateTime<Utc>>,

    // Tracking fields
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Model alias - maps client-facing alias names to canonical vLLM model names
/// This allows clients to use friendly names that resolve to official model names
/// Example: "phala/gpt-oss-120b" (alias) -> "openai/gpt-oss-120b" (canonical)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAlias {
    pub id: Uuid,
    pub alias_name: String,
    pub canonical_model_id: Uuid,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ============================================
// Organization Limits Models
// ============================================

/// Organization limits history - stores historical spending limit data for organizations
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationLimitsHistory {
    pub id: Uuid,
    pub organization_id: Uuid,

    // Spend limit (fixed scale 9 = nano-dollars, USD only)
    pub spend_limit: i64,

    // Temporal fields
    pub effective_from: DateTime<Utc>,
    pub effective_until: Option<DateTime<Utc>>,

    // Tracking fields
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub changed_by_user_id: Option<Uuid>, // The authenticated user ID who made the change
    pub changed_by_user_email: Option<String>, // The email of the authenticated user who made the change
    pub created_at: DateTime<Utc>,
}

/// Request to update organization limits
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateOrganizationLimitsDbRequest {
    pub spend_limit: i64,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub changed_by_user_id: Option<Uuid>, // The authenticated user ID who made the change
    pub changed_by_user_email: Option<String>, // The email of the authenticated user who made the change
}

// ============================================
// Organization Usage Tracking Models
// ============================================

/// Organization usage log entry - records individual API calls with costs
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationUsageLog {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub response_id: Option<Uuid>,
    pub model_id: Uuid,
    pub model: String, // Canonical model name from models table
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub request_type: String,
    pub created_at: DateTime<Utc>,
}

/// Organization balance summary - cached aggregate spending
/// All amounts use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationBalance {
    pub organization_id: Uuid,
    pub total_spent: i64,
    pub last_usage_at: Option<DateTime<Utc>>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: DateTime<Utc>,
}

/// Request to record usage
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Clone)]
pub struct RecordUsageRequest {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub response_id: Option<Uuid>,
    pub model_id: Uuid,
    pub model_name: String, // Denormalized for performance
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub request_type: String,
}

// ============================================
// File Storage Models
// ============================================

/// File model - stores uploaded file metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub id: Uuid,
    pub filename: String,
    pub bytes: i64,
    pub content_type: String,
    pub purpose: String,
    pub storage_key: String,
    pub workspace_id: Uuid,
    pub uploaded_by_api_key_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// File purpose enum
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum FilePurpose {
    Assistants,
    Batch,
    FineTune,
    Vision,
    UserData,
    Evals,
}

impl std::fmt::Display for FilePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilePurpose::Assistants => write!(f, "assistants"),
            FilePurpose::Batch => write!(f, "batch"),
            FilePurpose::FineTune => write!(f, "fine-tune"),
            FilePurpose::Vision => write!(f, "vision"),
            FilePurpose::UserData => write!(f, "user_data"),
            FilePurpose::Evals => write!(f, "evals"),
        }
    }
}

impl std::str::FromStr for FilePurpose {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "assistants" => Ok(FilePurpose::Assistants),
            "batch" => Ok(FilePurpose::Batch),
            "fine-tune" => Ok(FilePurpose::FineTune),
            "vision" => Ok(FilePurpose::Vision),
            "user_data" => Ok(FilePurpose::UserData),
            "evals" => Ok(FilePurpose::Evals),
            _ => Err(format!("Invalid file purpose: {s}")),
        }
    }
}
