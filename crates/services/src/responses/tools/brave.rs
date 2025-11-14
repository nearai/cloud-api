pub use super::ports::*;
use serde::{Deserialize, Serialize};

static BRAVE_API_URL: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveWebSearchProvider {
    pub api_key: String,
    pub client: reqwest::Client,
}

impl Default for BraveWebSearchProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BraveWebSearchProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("BRAVE_SEARCH_PRO_API_KEY").unwrap_or_else(|_| {
            panic!("BRAVE_SEARCH_PRO_API_KEY is not set");
        });
        Self {
            api_key,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
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
    async fn search(
        &self,
        params: WebSearchParams,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        tracing::debug!("Searching with params: {:?}", params);

        // Build query parameters dynamically
        let mut query_params = vec![("q", params.query.clone())];

        // Add optional parameters
        let country;
        if let Some(ref c) = params.country {
            country = c.clone();
            query_params.push(("country", country));
        }

        let search_lang;
        if let Some(ref sl) = params.search_lang {
            search_lang = sl.clone();
            query_params.push(("search_lang", search_lang));
        }

        let ui_lang;
        if let Some(ref ul) = params.ui_lang {
            ui_lang = ul.clone();
            query_params.push(("ui_lang", ui_lang));
        }

        let count;
        if let Some(c) = params.count {
            count = c.to_string();
            query_params.push(("count", count));
        }

        let offset;
        if let Some(o) = params.offset {
            offset = o.to_string();
            query_params.push(("offset", offset));
        }

        let safesearch;
        if let Some(ref ss) = params.safesearch {
            safesearch = ss.clone();
            query_params.push(("safesearch", safesearch));
        }

        let freshness;
        if let Some(ref f) = params.freshness {
            freshness = f.clone();
            query_params.push(("freshness", freshness));
        }

        let text_decorations;
        if let Some(td) = params.text_decorations {
            text_decorations = td.to_string();
            query_params.push(("text_decorations", text_decorations));
        }

        let spellcheck;
        if let Some(sc) = params.spellcheck {
            spellcheck = sc.to_string();
            query_params.push(("spellcheck", spellcheck));
        }

        let units;
        if let Some(ref u) = params.units {
            units = u.clone();
            query_params.push(("units", units));
        }

        let extra_snippets;
        if let Some(es) = params.extra_snippets {
            extra_snippets = es.to_string();
            query_params.push(("extra_snippets", extra_snippets));
        }

        let summary;
        if let Some(s) = params.summary {
            summary = s.to_string();
            query_params.push(("summary", summary));
        }

        tracing::debug!("Query parameters: {:?}", query_params);

        let response = self
            .brave_get_builder()
            .query(&query_params)
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
                "HTTP {status}: {error_body}"
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
                WebSearchError::WebSearchResponseParsingFailed(format!("JSON parsing error: {e}"))
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
