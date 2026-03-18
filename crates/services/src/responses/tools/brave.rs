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
        tracing::debug!(
            has_country = params.country.is_some(),
            has_search_lang = params.search_lang.is_some(),
            has_ui_lang = params.ui_lang.is_some(),
            has_count = params.count.is_some(),
            has_offset = params.offset.is_some(),
            has_safesearch = params.safesearch.is_some(),
            has_freshness = params.freshness.is_some(),
            has_text_decorations = params.text_decorations.is_some(),
            has_spellcheck = params.spellcheck.is_some(),
            has_units = params.units.is_some(),
            has_extra_snippets = params.extra_snippets.is_some(),
            has_summary = params.summary.is_some(),
            has_result_filter = params.result_filter.is_some(),
            has_goggles = params.goggles.is_some(),
            has_enable_rich_callback = params.enable_rich_callback.is_some(),
            has_include_fetch_metadata = params.include_fetch_metadata.is_some(),
            has_operators = params.operators.is_some(),
            "Starting Brave web search"
        );

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

        if let Some(ref rf) = params.result_filter {
            query_params.push(("result_filter", rf.clone()));
        }
        if let Some(ref g) = params.goggles {
            query_params.push(("goggles", g.clone()));
        }
        if let Some(erc) = params.enable_rich_callback {
            query_params.push(("enable_rich_callback", erc.to_string()));
        }
        if let Some(ifm) = params.include_fetch_metadata {
            query_params.push(("include_fetch_metadata", ifm.to_string()));
        }
        if let Some(op) = params.operators {
            query_params.push(("operators", op.to_string()));
        }

        tracing::debug!(
            query_param_count = query_params.len(),
            "Built Brave query parameters"
        );

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
            tracing::warn!(status = %status, "Brave API error response");
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

        tracing::debug!(
            response_size_bytes = response_text.len(),
            "Received Brave API response body"
        );

        // Parse JSON
        let brave_response: BraveSearchResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                tracing::error!("Failed to parse Brave response: {}", e);
                WebSearchError::WebSearchResponseParsingFailed(format!("JSON parsing error: {e}"))
            })?;

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
        tracing::debug!(result_count = results.len(), "Parsed Brave search results");
        Ok(results)
    }
}
