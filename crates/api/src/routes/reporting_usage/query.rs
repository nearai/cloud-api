use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use services::usage::InferenceType;
use std::str::FromStr;
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_REPORTING_USAGE_LIMIT: u16 = 100;
const MAX_REPORTING_USAGE_LIMIT: u16 = 1000;
const MAX_REPORTING_USAGE_RANGE_DAYS: i64 = 366;
const REPORTING_USAGE_CURSOR_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportingUsageSource {
    #[default]
    All,
    Inference,
    Service,
}

impl FromStr for ReportingUsageSource {
    type Err = ReportingUsageQueryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::All),
            "inference" => Ok(Self::Inference),
            "service" => Ok(Self::Service),
            other => Err(ReportingUsageQueryError::InvalidSource(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportingUsageRowSource {
    Inference,
    Service,
}

impl ReportingUsageRowSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::Service => "service",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
pub struct ReportingUsageLimit(u16);

impl ReportingUsageLimit {
    pub const fn value(self) -> u16 {
        self.0
    }

    fn new(value: u16) -> Result<Self, ReportingUsageQueryError> {
        if value == 0 {
            return Err(ReportingUsageQueryError::LimitNotPositive);
        }
        if value > MAX_REPORTING_USAGE_LIMIT {
            return Err(ReportingUsageQueryError::LimitTooLarge {
                max: MAX_REPORTING_USAGE_LIMIT,
            });
        }
        Ok(Self(value))
    }
}

impl Default for ReportingUsageLimit {
    fn default() -> Self {
        Self(DEFAULT_REPORTING_USAGE_LIMIT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportingUsageQuery {
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub source: ReportingUsageSource,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<InferenceType>,
    pub service_name: Option<String>,
    pub limit: ReportingUsageLimit,
    pub cursor: Option<ReportingUsageCursor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReportingUsageQueryParams {
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub source: Option<String>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<String>,
    pub service_name: Option<String>,
    pub limit: Option<u16>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReportingUsageQueryError {
    #[error("start_time and end_time must be RFC3339 timestamps")]
    InvalidTimestamp,
    #[error("end_time must be greater than or equal to start_time")]
    InvalidTimeRange,
    #[error("time range must not exceed {max_days} days")]
    TimeRangeTooLarge { max_days: i64 },
    #[error("invalid source: {0}")]
    InvalidSource(String),
    #[error("invalid inference_type: {0}")]
    InvalidInferenceType(String),
    #[error("limit must be positive")]
    LimitNotPositive,
    #[error("limit must not exceed {max}")]
    LimitTooLarge { max: u16 },
    #[error("invalid cursor")]
    InvalidCursor,
}

impl TryFrom<ReportingUsageQueryParams> for ReportingUsageQuery {
    type Error = ReportingUsageQueryError;

    fn try_from(params: ReportingUsageQueryParams) -> Result<Self, Self::Error> {
        let (start_time, end_time) = normalize_time_range(
            parse_optional_time(params.start_time.as_deref())?,
            parse_optional_time(params.end_time.as_deref())?,
        )?;

        Ok(Self {
            start_time: Some(start_time),
            end_time: Some(end_time),
            source: params
                .source
                .as_deref()
                .map(ReportingUsageSource::from_str)
                .transpose()?
                .unwrap_or_default(),
            workspace_id: params.workspace_id,
            api_key_id: params.api_key_id,
            model: params.model,
            inference_type: params
                .inference_type
                .as_deref()
                .map(InferenceType::from_str)
                .transpose()
                .map_err(ReportingUsageQueryError::InvalidInferenceType)?,
            service_name: params.service_name,
            limit: params
                .limit
                .map(ReportingUsageLimit::new)
                .transpose()?
                .unwrap_or_default(),
            cursor: params
                .cursor
                .as_deref()
                .map(ReportingUsageCursor::decode)
                .transpose()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportingUsageCursor {
    pub created_at: DateTime<Utc>,
    pub source: ReportingUsageRowSource,
    pub id: Uuid,
}

impl ReportingUsageCursor {
    pub const fn new(created_at: DateTime<Utc>, source: ReportingUsageRowSource, id: Uuid) -> Self {
        Self {
            created_at,
            source,
            id,
        }
    }

    pub fn encode(&self) -> Result<String, ReportingUsageQueryError> {
        let payload = ReportingUsageCursorPayload {
            version: REPORTING_USAGE_CURSOR_VERSION,
            created_at: self.created_at,
            source: self.source,
            id: self.id,
        };
        let bytes =
            serde_json::to_vec(&payload).map_err(|_| ReportingUsageQueryError::InvalidCursor)?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<Self, ReportingUsageQueryError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ReportingUsageQueryError::InvalidCursor)?;
        let payload: ReportingUsageCursorPayload =
            serde_json::from_slice(&bytes).map_err(|_| ReportingUsageQueryError::InvalidCursor)?;
        if payload.version != REPORTING_USAGE_CURSOR_VERSION {
            return Err(ReportingUsageQueryError::InvalidCursor);
        }
        Ok(Self {
            created_at: payload.created_at,
            source: payload.source,
            id: payload.id,
        })
    }

    #[cfg(test)]
    pub fn encode_raw_for_tests(
        value: &serde_json::Value,
    ) -> Result<String, ReportingUsageQueryError> {
        let bytes =
            serde_json::to_vec(value).map_err(|_| ReportingUsageQueryError::InvalidCursor)?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ReportingUsageCursorPayload {
    #[serde(rename = "v")]
    version: u8,
    created_at: DateTime<Utc>,
    source: ReportingUsageRowSource,
    id: Uuid,
}

fn parse_optional_time(
    value: Option<&str>,
) -> Result<Option<DateTime<Utc>>, ReportingUsageQueryError> {
    value
        .map(|timestamp| {
            DateTime::parse_from_rfc3339(timestamp)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| ReportingUsageQueryError::InvalidTimestamp)
        })
        .transpose()
}

fn validate_time_range(
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> Result<(), ReportingUsageQueryError> {
    if end_time < start_time {
        return Err(ReportingUsageQueryError::InvalidTimeRange);
    }
    if end_time - start_time > Duration::days(MAX_REPORTING_USAGE_RANGE_DAYS) {
        return Err(ReportingUsageQueryError::TimeRangeTooLarge {
            max_days: MAX_REPORTING_USAGE_RANGE_DAYS,
        });
    }
    Ok(())
}

fn normalize_time_range(
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ReportingUsageQueryError> {
    let end_time = end_time.unwrap_or_else(Utc::now);
    let start_time =
        start_time.unwrap_or_else(|| end_time - Duration::days(MAX_REPORTING_USAGE_RANGE_DAYS));
    validate_time_range(start_time, end_time)?;
    Ok((start_time, end_time))
}
