use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum CreditEventError {
    NotFound(String),
    EventInactive,
    ClaimPeriodNotStarted,
    ClaimPeriodEnded,
    MaxClaimsReached,
    InvalidCode,
    CodeAlreadyClaimed,
    UserAlreadyClaimed,
    Unauthorized(String),
    InternalError(String),
    ValidationError(String),
}

impl std::fmt::Display for CreditEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreditEventError::NotFound(msg) => write!(f, "Not found: {msg}"),
            CreditEventError::EventInactive => write!(f, "Event is inactive"),
            CreditEventError::ClaimPeriodNotStarted => {
                write!(f, "Claim period has not started yet")
            }
            CreditEventError::ClaimPeriodEnded => write!(f, "Claim period has ended"),
            CreditEventError::MaxClaimsReached => {
                write!(f, "Maximum claims reached for this event")
            }
            CreditEventError::InvalidCode => write!(f, "Invalid promo code"),
            CreditEventError::CodeAlreadyClaimed => {
                write!(f, "Promo code has already been claimed")
            }
            CreditEventError::UserAlreadyClaimed => {
                write!(f, "User has already claimed credits for this event")
            }
            CreditEventError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            CreditEventError::InternalError(msg) => write!(f, "Internal error: {msg}"),
            CreditEventError::ValidationError(msg) => write!(f, "Validation error: {msg}"),
        }
    }
}

impl std::error::Error for CreditEventError {}

// ============================================
// Data Transfer Types
// ============================================

#[derive(Debug, Clone)]
pub struct CreditEventData {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub credit_amount: i64,
    pub currency: String,
    pub max_claims: Option<i32>,
    pub claim_count: i32,
    pub starts_at: DateTime<Utc>,
    pub claim_deadline: Option<DateTime<Utc>>,
    pub credit_expires_at: DateTime<Utc>,
    pub is_active: bool,
    pub created_by_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CreditEventCodeData {
    pub id: Uuid,
    pub credit_event_id: Uuid,
    pub code: String,
    pub is_claimed: bool,
    pub claimed_by_user_id: Option<Uuid>,
    pub claimed_by_near_account_id: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CreditClaimData {
    pub id: Uuid,
    pub credit_event_id: Uuid,
    pub code_id: Uuid,
    pub near_account_id: String,
    pub user_id: Uuid,
    pub organization_id: Uuid,
    pub organization_limit_id: Option<Uuid>,
    pub claimed_at: DateTime<Utc>,
}

// ============================================
// Request/Response Types
// ============================================

#[derive(Debug, Clone)]
pub struct CreditAdditionParams {
    pub spend_limit: i64,
    pub credit_type: String,
    pub source: Option<String>,
    pub currency: String,
    pub credit_expires_at: Option<DateTime<Utc>>,
    pub changed_by: Option<String>,
    pub change_reason: Option<String>,
    pub changed_by_user_id: Option<Uuid>,
    pub changed_by_user_email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateEventRequest {
    pub name: String,
    pub description: Option<String>,
    pub credit_amount: i64,
    pub currency: Option<String>,
    pub max_claims: Option<i32>,
    pub starts_at: Option<DateTime<Utc>>,
    pub claim_deadline: Option<DateTime<Utc>>,
    pub credit_expires_at: DateTime<Utc>,
    pub created_by_user_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct ClaimCreditsRequest {
    pub event_id: Uuid,
    pub code: String,
    pub near_account_id: String,
    pub user_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct ClaimResult {
    pub claim_id: Uuid,
    pub event_id: Uuid,
    pub near_account_id: String,
    pub organization_id: Uuid,
    pub credit_amount: i64,
    pub api_key: Option<String>,
    pub credit_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct GenerateCodesRequest {
    pub event_id: Uuid,
    pub count: i32,
}

#[derive(Debug, Clone)]
pub struct CreditEventInfo {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub credit_amount: i64,
    pub currency: String,
    pub max_claims: Option<i32>,
    pub claim_count: i32,
    pub starts_at: DateTime<Utc>,
    pub claim_deadline: Option<DateTime<Utc>>,
    pub credit_expires_at: DateTime<Utc>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CreditEventCodeInfo {
    pub id: Uuid,
    pub code: String,
    pub is_claimed: bool,
    pub claimed_by_user_id: Option<Uuid>,
    pub claimed_by_near_account_id: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
}

// ============================================
// Repository Trait (implemented in database crate)
// ============================================

#[async_trait::async_trait]
pub trait CreditEventRepositoryTrait: Send + Sync {
    async fn create_event(
        &self,
        name: String,
        description: Option<String>,
        credit_amount: i64,
        currency: String,
        max_claims: Option<i32>,
        starts_at: DateTime<Utc>,
        claim_deadline: Option<DateTime<Utc>>,
        credit_expires_at: DateTime<Utc>,
        created_by_user_id: Option<Uuid>,
    ) -> Result<CreditEventData, CreditEventError>;

    async fn get_event(&self, event_id: Uuid) -> Result<Option<CreditEventData>, CreditEventError>;
    async fn list_active_events(&self) -> Result<Vec<CreditEventData>, CreditEventError>;
    async fn deactivate_event(
        &self,
        event_id: Uuid,
    ) -> Result<Option<CreditEventData>, CreditEventError>;
    async fn generate_codes(
        &self,
        event_id: Uuid,
        codes: Vec<String>,
    ) -> Result<Vec<CreditEventCodeData>, CreditEventError>;
    async fn get_codes_for_event(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<CreditEventCodeData>, CreditEventError>;
    async fn find_unclaimed_code(
        &self,
        event_id: Uuid,
        code: &str,
    ) -> Result<Option<CreditEventCodeData>, CreditEventError>;
    async fn claim_code(
        &self,
        code_id: Uuid,
        event_id: Uuid,
        user_id: Uuid,
        near_account_id: &str,
        organization_id: Uuid,
        credits: CreditAdditionParams,
    ) -> Result<CreditClaimData, CreditEventError>;
}

// ============================================
// Service Trait
// ============================================

#[async_trait::async_trait]
pub trait CreditEventServiceTrait: Send + Sync {
    async fn create_event(
        &self,
        request: CreateEventRequest,
    ) -> Result<CreditEventInfo, CreditEventError>;

    async fn get_event(&self, event_id: Uuid) -> Result<CreditEventInfo, CreditEventError>;

    async fn list_events(&self) -> Result<Vec<CreditEventInfo>, CreditEventError>;

    async fn deactivate_event(&self, event_id: Uuid) -> Result<CreditEventInfo, CreditEventError>;

    async fn generate_codes(
        &self,
        request: GenerateCodesRequest,
    ) -> Result<Vec<String>, CreditEventError>;

    async fn get_codes(&self, event_id: Uuid)
        -> Result<Vec<CreditEventCodeInfo>, CreditEventError>;

    async fn claim_credits(
        &self,
        request: ClaimCreditsRequest,
    ) -> Result<ClaimResult, CreditEventError>;
}
