//! Local copies of response shapes the e2e tests assert on.

/// Local copy of the organization limits history shape (the admin route that returned it is gone).
#[derive(Debug)]
pub struct LimitsHistoryResponse {
    pub history: Vec<LimitsHistoryEntry>,
}

#[derive(Debug)]
pub struct LimitsHistoryEntry {
    pub credit_type: String,
    pub source: Option<String>,
    pub spend_limit: LimitAmount,
    pub effective_until: Option<String>,
}

#[derive(Debug)]
pub struct LimitAmount {
    pub amount: i64,
}
