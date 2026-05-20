pub use super::ports::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

static BRAVE_WEB_SEARCH_API_URL: &str = "https://api.search.brave.com/res/v1/web/search";
static BRAVE_LLM_CONTEXT_API_URL: &str = "https://api.search.brave.com/res/v1/llm/context";

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

    fn brave_get_builder(&self, url: &'static str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
    }
}

fn request_error_category(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "unknown"
    }
}

fn result_stats(results: &[WebSearchResult]) -> (usize, usize) {
    results
        .iter()
        .fold((0, 0), |(snippet_count, total_chars), result| {
            let has_snippet = !result.snippet.trim().is_empty();
            (
                snippet_count + usize::from(has_snippet),
                total_chars + result.snippet.chars().count(),
            )
        })
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

/// Root response from Brave LLM Context API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveContextResponse {
    #[serde(default)]
    pub grounding: BraveContextGrounding,
    #[serde(default)]
    pub sources: HashMap<String, BraveContextSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BraveContextGrounding {
    #[serde(default)]
    pub generic: Vec<BraveContextResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveContextResult {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub snippets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveContextSource {
    #[serde(default)]
    pub title: Option<String>,
}

pub fn context_response_to_web_results(response: BraveContextResponse) -> Vec<WebSearchResult> {
    response
        .grounding
        .generic
        .into_iter()
        .filter_map(|result| {
            let url = result.url.trim().to_string();
            if url.is_empty() {
                return None;
            }

            let snippets = result
                .snippets
                .into_iter()
                .map(|snippet| snippet.trim().to_string())
                .filter(|snippet| !snippet.is_empty())
                .collect::<Vec<_>>();
            if snippets.is_empty() {
                return None;
            }

            let title = result
                .title
                .filter(|title| !title.trim().is_empty())
                .or_else(|| {
                    response
                        .sources
                        .get(&url)
                        .and_then(|source| source.title.clone())
                        .filter(|title| !title.trim().is_empty())
                })
                .unwrap_or_else(|| url.clone());

            Some(WebSearchResult {
                title,
                url,
                snippet: snippets.join("\n\n"),
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl WebSearchProviderTrait for BraveWebSearchProvider {
    async fn search(
        &self,
        params: WebSearchParams,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let started_at = Instant::now();
        let requested_count = params.count;

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

        let response = self
            .brave_get_builder(BRAVE_WEB_SEARCH_API_URL)
            .query(&query_params)
            .send()
            .await
            .map_err(|e| {
                let error_category = request_error_category(&e);
                tracing::warn!(
                    endpoint = "web_search",
                    error_category,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "Brave API request failed"
                );
                WebSearchError::WebSearchRequestFailed(format!(
                    "Brave API request failed: {error_category}"
                ))
            })?;

        // Check response status
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(
                endpoint = "web_search",
                status = status.as_u16(),
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "Brave API error response"
            );
            return Err(WebSearchError::WebSearchRequestFailed(format!(
                "HTTP {}",
                status.as_u16()
            )));
        }

        let response_text = response.text().await.map_err(|e| {
            let error_category = request_error_category(&e);
            WebSearchError::WebSearchResponseParsingFailed(format!(
                "Brave API response read failed: {error_category}"
            ))
        })?;

        // Parse JSON
        let brave_response: BraveSearchResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                tracing::error!(
                    endpoint = "web_search",
                    error = %e,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "Failed to parse Brave response"
                );
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

        let (snippet_count, total_snippet_chars) = result_stats(&results);
        tracing::debug!(
            endpoint = "web_search",
            status = 200_u16,
            requested_count = ?requested_count,
            result_count = results.len(),
            snippet_count,
            total_snippet_chars,
            empty_result = results.is_empty(),
            elapsed_ms = started_at.elapsed().as_millis() as u64,
            "Brave search completed"
        );
        Ok(results)
    }
}

#[async_trait::async_trait]
impl WebContextSearchProviderTrait for BraveWebSearchProvider {
    async fn search_context(
        &self,
        params: WebContextSearchParams,
    ) -> Result<Vec<WebSearchResult>, WebSearchError> {
        let started_at = Instant::now();
        let requested_spellcheck = params.spellcheck;
        let requested_count = params.count;
        let requested_max_urls = params.maximum_number_of_urls;
        let requested_max_tokens = params.maximum_number_of_tokens;
        let requested_max_snippets = params.maximum_number_of_snippets;
        let requested_max_tokens_per_url = params.maximum_number_of_tokens_per_url;
        let requested_max_snippets_per_url = params.maximum_number_of_snippets_per_url;
        let threshold_mode = match params.context_threshold_mode.as_deref() {
            Some("disabled") => Some("disabled".to_string()),
            Some("strict") => Some("strict".to_string()),
            Some("balanced") => Some("balanced".to_string()),
            Some("lenient") => Some("lenient".to_string()),
            _ => None,
        };

        let mut query_params = vec![("q", params.query.clone())];

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

        let freshness;
        if let Some(ref f) = params.freshness {
            freshness = f.clone();
            query_params.push(("freshness", freshness));
        }

        let spellcheck;
        if let Some(value) = params.spellcheck {
            spellcheck = value.to_string();
            query_params.push(("spellcheck", spellcheck));
        }

        let count;
        if let Some(c) = params.count {
            count = c.to_string();
            query_params.push(("count", count));
        }

        let maximum_number_of_urls;
        if let Some(value) = params.maximum_number_of_urls {
            maximum_number_of_urls = value.to_string();
            query_params.push(("maximum_number_of_urls", maximum_number_of_urls));
        }

        let maximum_number_of_tokens;
        if let Some(value) = params.maximum_number_of_tokens {
            maximum_number_of_tokens = value.to_string();
            query_params.push(("maximum_number_of_tokens", maximum_number_of_tokens));
        }

        let maximum_number_of_snippets;
        if let Some(value) = params.maximum_number_of_snippets {
            maximum_number_of_snippets = value.to_string();
            query_params.push(("maximum_number_of_snippets", maximum_number_of_snippets));
        }

        let maximum_number_of_tokens_per_url;
        if let Some(value) = params.maximum_number_of_tokens_per_url {
            maximum_number_of_tokens_per_url = value.to_string();
            query_params.push((
                "maximum_number_of_tokens_per_url",
                maximum_number_of_tokens_per_url,
            ));
        }

        let maximum_number_of_snippets_per_url;
        if let Some(value) = params.maximum_number_of_snippets_per_url {
            maximum_number_of_snippets_per_url = value.to_string();
            query_params.push((
                "maximum_number_of_snippets_per_url",
                maximum_number_of_snippets_per_url,
            ));
        }

        if let Some(ref mode) = threshold_mode {
            query_params.push(("context_threshold_mode", mode.clone()));
        }

        let response = self
            .brave_get_builder(BRAVE_LLM_CONTEXT_API_URL)
            .query(&query_params)
            .send()
            .await
            .map_err(|e| {
                let error_category = request_error_category(&e);
                tracing::warn!(
                    endpoint = "llm_context",
                    error_category,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "Brave API request failed"
                );
                WebSearchError::WebSearchRequestFailed(format!(
                    "Brave API request failed: {error_category}"
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            tracing::warn!(
                endpoint = "llm_context",
                status = status.as_u16(),
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "Brave API error response"
            );
            return Err(WebSearchError::WebSearchRequestFailed(format!(
                "HTTP {}",
                status.as_u16()
            )));
        }

        let response_text = response.text().await.map_err(|e| {
            let error_category = request_error_category(&e);
            WebSearchError::WebSearchResponseParsingFailed(format!(
                "Brave API response read failed: {error_category}"
            ))
        })?;

        let context_response: BraveContextResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                tracing::error!(
                    endpoint = "llm_context",
                    error = %e,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "Failed to parse Brave response"
                );
                WebSearchError::WebSearchResponseParsingFailed(format!("JSON parsing error: {e}"))
            })?;

        let results = context_response_to_web_results(context_response);
        let (snippet_count, total_snippet_chars) = result_stats(&results);
        tracing::debug!(
            endpoint = "llm_context",
            status = 200_u16,
            requested_spellcheck = ?requested_spellcheck,
            requested_count = ?requested_count,
            requested_max_urls = ?requested_max_urls,
            requested_max_tokens = ?requested_max_tokens,
            requested_max_snippets = ?requested_max_snippets,
            requested_max_tokens_per_url = ?requested_max_tokens_per_url,
            requested_max_snippets_per_url = ?requested_max_snippets_per_url,
            threshold_mode = threshold_mode.as_deref().unwrap_or("balanced"),
            result_count = results.len(),
            snippet_count,
            total_snippet_chars,
            empty_result = results.is_empty(),
            elapsed_ms = started_at.elapsed().as_millis() as u64,
            "Brave context search completed"
        );

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_response_to_web_results_joins_snippets_and_uses_source_title() {
        let response = BraveContextResponse {
            grounding: BraveContextGrounding {
                generic: vec![BraveContextResult {
                    url: "https://example.com/page".to_string(),
                    title: None,
                    snippets: vec![
                        " First relevant chunk ".to_string(),
                        "".to_string(),
                        "Second relevant chunk".to_string(),
                    ],
                }],
            },
            sources: HashMap::from([(
                "https://example.com/page".to_string(),
                BraveContextSource {
                    title: Some("Example title".to_string()),
                },
            )]),
        };

        let results = context_response_to_web_results(response);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example title");
        assert_eq!(results[0].url, "https://example.com/page");
        assert_eq!(
            results[0].snippet,
            "First relevant chunk\n\nSecond relevant chunk"
        );
    }

    #[test]
    fn context_response_to_web_results_skips_empty_urls_and_snippets() {
        let response = BraveContextResponse {
            grounding: BraveContextGrounding {
                generic: vec![
                    BraveContextResult {
                        url: "".to_string(),
                        title: Some("Missing URL".to_string()),
                        snippets: vec!["content".to_string()],
                    },
                    BraveContextResult {
                        url: "https://example.com/empty".to_string(),
                        title: Some("No snippets".to_string()),
                        snippets: vec!["  ".to_string()],
                    },
                ],
            },
            sources: HashMap::new(),
        };

        let results = context_response_to_web_results(response);

        assert!(results.is_empty());
    }
}
