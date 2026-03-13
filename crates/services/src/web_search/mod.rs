use crate::responses::tools::{WebSearchParams, WebSearchProviderTrait};
use crate::service_usage::ports::{
    RecordServiceUsageWithPricingParams, ServiceUsageServiceTrait, SERVICE_NAME_WEB_SEARCH,
};
use crate::service_usage::ServiceUsageError;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct WebSearchRequest {
    pub query: String,
    pub count: Option<u32>,
    pub country: Option<String>,
    pub search_lang: Option<String>,
    pub ui_lang: Option<String>,
    pub freshness: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebSearchUsageContext {
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct WebSearchResultItem {
    pub title: String,
    pub url: String,
    pub description: String,
    pub published: Option<String>,
    pub site_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebSearchResponse {
    pub query: String,
    pub result_count: u32,
    pub results: Vec<WebSearchResultItem>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebSearchServiceError {
    #[error("Query parameter 'query' is required and cannot be empty")]
    EmptyQuery,
    #[error("Web search is not configured")]
    NotConfigured,
    #[error("Web search request failed")]
    ProviderFailure,
    #[error("Failed to record service usage")]
    UsageRecordingFailed,
    #[error("Internal server error")]
    Internal,
}

#[derive(Clone)]
pub struct WebSearchService {
    provider: Arc<dyn WebSearchProviderTrait>,
    service_usage_service: Arc<dyn ServiceUsageServiceTrait + Send + Sync>,
}

impl WebSearchService {
    pub fn new(
        provider: Arc<dyn WebSearchProviderTrait>,
        service_usage_service: Arc<dyn ServiceUsageServiceTrait + Send + Sync>,
    ) -> Self {
        Self {
            provider,
            service_usage_service,
        }
    }

    pub async fn execute(
        &self,
        request: WebSearchRequest,
        usage: WebSearchUsageContext,
    ) -> Result<WebSearchResponse, WebSearchServiceError> {
        if request.query.trim().is_empty() {
            return Err(WebSearchServiceError::EmptyQuery);
        }

        let pricing = self
            .service_usage_service
            .get_active_service_pricing(SERVICE_NAME_WEB_SEARCH)
            .await
            .map_err(|_| WebSearchServiceError::Internal)?;
        let Some((service_id, cost_per_unit)) = pricing else {
            return Err(WebSearchServiceError::NotConfigured);
        };

        let results = self
            .provider
            .search(WebSearchParams {
                query: request.query.clone(),
                country: request.country,
                search_lang: request.search_lang,
                ui_lang: request.ui_lang,
                count: request.count,
                offset: None,
                safesearch: None,
                freshness: request.freshness,
                text_decorations: None,
                spellcheck: None,
                units: None,
                extra_snippets: None,
                summary: None,
                result_filter: None,
                goggles: None,
                enable_rich_callback: None,
                include_fetch_metadata: None,
                operators: None,
            })
            .await
            .map_err(|_| WebSearchServiceError::ProviderFailure)?;

        self.service_usage_service
            .record_service_usage_with_pricing(&RecordServiceUsageWithPricingParams {
                organization_id: usage.organization_id,
                workspace_id: usage.workspace_id,
                api_key_id: usage.api_key_id,
                service_id,
                cost_per_unit,
                quantity: 1,
                inference_id: None,
            })
            .await
            .map_err(|err| match err {
                ServiceUsageError::ServiceNotFound(_)
                | ServiceUsageError::InternalError(_)
                | ServiceUsageError::CostOverflow => WebSearchServiceError::UsageRecordingFailed,
            })?;

        Ok(WebSearchResponse {
            query: request.query,
            result_count: results.len() as u32,
            results: results
                .into_iter()
                .map(|result| WebSearchResultItem {
                    title: result.title,
                    url: result.url,
                    description: result.snippet,
                    published: None,
                    site_name: None,
                })
                .collect(),
        })
    }
}
