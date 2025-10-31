pub use super::ports::*;
use serde::{Deserialize, Serialize};

static BRAVE_API_URL: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveWebSearchProvider {
    pub api_key: String,
    pub client: reqwest::Client,
}

impl BraveWebSearchProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("BRAVE_SEARCH_PRO_API_KEY").unwrap_or_else(|_| {
            panic!("BRAVE_SEARCH_PRO_API_KEY is not set");
        });
        Self {
            api_key: api_key,               // TODO: Remove this once we have a proper config
            client: reqwest::Client::new(), // TODO: Add a timeout
        }
    }

    fn brave_get_builder(&self) -> reqwest::RequestBuilder {
        self.client
            .get(BRAVE_API_URL)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
    }
}

/// Root response from Brave Search API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveSearchResponse {
    #[serde(default)]
    pub web: Option<BraveWebResults>,
}

/// Web search results container
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveWebResults {
    #[serde(default)]
    pub results: Vec<BraveWebSearchResult>,
}

/// Individual web search result from Brave API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveWebSearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[async_trait::async_trait]
impl WebSearchProviderTrait for BraveWebSearchProvider {
    async fn search(&self, query: String) -> Result<Vec<WebSearchResult>, WebSearchError> {
        tracing::debug!("Searching for query: {}", query);
        let response = self
            .brave_get_builder()
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| WebSearchError::WebSearchRequestFailed(e.to_string()))?;

        // Check response status
        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error body".to_string());
            tracing::error!("Brave API error (status {}): {}", status, error_body);
            return Err(WebSearchError::WebSearchRequestFailed(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        // Get response text for debugging
        let response_text = response
            .text()
            .await
            .map_err(|e| WebSearchError::WebSearchResponseParsingFailed(e.to_string()))?;

        tracing::debug!("Brave API response body: {}", response_text);

        // Parse JSON
        let brave_response: BraveSearchResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                tracing::error!("Failed to parse Brave response: {}", e);
                WebSearchError::WebSearchResponseParsingFailed(format!("JSON parsing error: {}", e))
            })?;

        tracing::debug!("Brave response: {:?}", brave_response);
        // Extract web results and convert to our internal format
        let results: Vec<WebSearchResult> = brave_response
            .web
            .map(|web| {
                web.results
                    .into_iter()
                    .map(|result| WebSearchResult {
                        title: result.title,
                        url: result.url,
                        snippet: result.description.unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        tracing::debug!("Found {} results", results.len());
        tracing::debug!("Results: {:?}", results);
        Ok(results)
    }
}
