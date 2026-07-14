use super::query::{
    validate_time_range, ReportingUsageQuery, ReportingUsageQueryError, ReportingUsageQueryParams,
    ReportingUsageRowSource, ReportingUsageSource,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use services::usage::InferenceType;
use uuid::Uuid;

const REPORTING_USAGE_CURSOR_VERSION: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportingUsageCursor {
    pub created_at: DateTime<Utc>,
    pub source: ReportingUsageRowSource,
    pub id: Uuid,
    context: ReportingUsageCursorContext,
}

impl ReportingUsageCursor {
    pub fn for_query(
        organization_id: Uuid,
        created_at: DateTime<Utc>,
        source: ReportingUsageRowSource,
        id: Uuid,
        query: &ReportingUsageQuery,
    ) -> Result<Self, ReportingUsageQueryError> {
        let (Some(start_time), Some(end_time)) = (query.start_time, query.end_time) else {
            return Err(ReportingUsageQueryError::InvalidCursor);
        };
        Ok(Self {
            created_at,
            source,
            id,
            context: ReportingUsageCursorContext {
                organization_id,
                start_time,
                end_time,
                source: query.source,
                workspace_id: query.workspace_id,
                api_key_id: query.api_key_id,
                model: query.model.clone(),
                inference_type: query.inference_type,
                service_name: query.service_name.clone(),
            },
        })
    }

    pub fn validate_organization(
        &self,
        organization_id: Uuid,
    ) -> Result<(), ReportingUsageQueryError> {
        if self.context.organization_id == organization_id {
            Ok(())
        } else {
            Err(ReportingUsageQueryError::InvalidCursor)
        }
    }

    pub fn with_position(&self, source: ReportingUsageRowSource, id: Uuid) -> Self {
        Self {
            created_at: self.created_at,
            source,
            id,
            context: self.context.clone(),
        }
    }

    pub fn encode(&self) -> Result<String, ReportingUsageQueryError> {
        let payload = ReportingUsageCursorPayload {
            version: REPORTING_USAGE_CURSOR_VERSION,
            created_at: self.created_at,
            source: self.source,
            id: self.id,
            context: self.context.clone(),
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
        validate_time_range(payload.context.start_time, payload.context.end_time)
            .map_err(|_| ReportingUsageQueryError::InvalidCursor)?;
        Ok(Self {
            created_at: payload.created_at,
            source: payload.source,
            id: payload.id,
            context: payload.context,
        })
    }

    pub(super) fn restore_context(
        &self,
        params: &ReportingUsageQueryParams,
        start_time: Option<DateTime<Utc>>,
        end_time: Option<DateTime<Utc>>,
        source: Option<ReportingUsageSource>,
        inference_type: Option<InferenceType>,
    ) -> Result<ReportingUsageCursorFilters, ReportingUsageQueryError> {
        let context = &self.context;
        let conflicts = start_time.is_some_and(|value| value != context.start_time)
            || end_time.is_some_and(|value| value != context.end_time)
            || source.is_some_and(|value| value != context.source)
            || params
                .workspace_id
                .is_some_and(|value| Some(value) != context.workspace_id)
            || params
                .api_key_id
                .is_some_and(|value| Some(value) != context.api_key_id)
            || params
                .model
                .as_deref()
                .is_some_and(|value| context.model.as_deref() != Some(value))
            || inference_type.is_some_and(|value| Some(value) != context.inference_type)
            || params
                .service_name
                .as_deref()
                .is_some_and(|value| context.service_name.as_deref() != Some(value));
        if conflicts {
            return Err(ReportingUsageQueryError::InvalidCursor);
        }
        Ok(context.filters())
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
    context: ReportingUsageCursorContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReportingUsageCursorContext {
    organization_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    source: ReportingUsageSource,
    workspace_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    model: Option<String>,
    inference_type: Option<InferenceType>,
    service_name: Option<String>,
}

impl ReportingUsageCursorContext {
    fn filters(&self) -> ReportingUsageCursorFilters {
        ReportingUsageCursorFilters {
            start_time: self.start_time,
            end_time: self.end_time,
            source: self.source,
            workspace_id: self.workspace_id,
            api_key_id: self.api_key_id,
            model: self.model.clone(),
            inference_type: self.inference_type,
            service_name: self.service_name.clone(),
        }
    }
}

pub(super) struct ReportingUsageCursorFilters {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub source: ReportingUsageSource,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<InferenceType>,
    pub service_name: Option<String>,
}
