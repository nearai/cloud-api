use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::conversations::models::ConversationId;

/// Result from a web search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Parameters for web search
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSearchParams {
    /// The user's search query term (required)
    pub query: String,

    /// The search query country (2 character country code, e.g., "US", "GB")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,

    /// The search language preference (2+ character language code, e.g., "en", "es")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_lang: Option<String>,

    /// User interface language (e.g., "en-US", "es-ES")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_lang: Option<String>,

    /// Number of search results (max 20)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,

    /// Zero-based offset for pagination (max 9)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,

    /// Safe search filter: "off", "moderate", "strict"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safesearch: Option<String>,

    /// Freshness filter: "pd" (24h), "pw" (7d), "pm" (31d), "py" (365d), or "YYYY-MM-DDtoYYYY-MM-DD"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<String>,

    /// Whether to include text decoration markers (highlighting)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_decorations: Option<bool>,

    /// Whether to enable spellcheck
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spellcheck: Option<bool>,

    /// Comma-delimited string of result types: "discussions", "faq", "infobox", "news", "videos", "web", "locations"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_filter: Option<String>,

    /// Measurement units: "metric" or "imperial"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,

    /// Get up to 5 additional alternative excerpts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_snippets: Option<bool>,

    /// Enable summary key generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
}

impl WebSearchParams {
    /// Create a new WebSearchParams with just a query
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Default::default()
        }
    }

    /// Builder method to set country
    pub fn with_country(mut self, country: impl Into<String>) -> Self {
        self.country = Some(country.into());
        self
    }

    /// Builder method to set search language
    pub fn with_search_lang(mut self, lang: impl Into<String>) -> Self {
        self.search_lang = Some(lang.into());
        self
    }

    /// Builder method to set count
    pub fn with_count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }

    /// Builder method to set safesearch
    pub fn with_safesearch(mut self, safesearch: impl Into<String>) -> Self {
        self.safesearch = Some(safesearch.into());
        self
    }

    /// Builder method to set freshness
    pub fn with_freshness(mut self, freshness: impl Into<String>) -> Self {
        self.freshness = Some(freshness.into());
        self
    }
}

/// Result from a file search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchResult {
    pub file_id: String,
    pub file_name: String,
    pub content: String,
    pub relevance_score: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum WebSearchError {
    #[error("Web search failed: {0}")]
    WebSearchRequestFailed(String),
    #[error("Web search response parsing failed: {0}")]
    WebSearchResponseParsingFailed(String),
}

/// Web search provider trait
#[async_trait]
pub trait WebSearchProviderTrait: Send + Sync {
    /// Perform a web search with the given parameters
    async fn search(&self, params: WebSearchParams)
        -> Result<Vec<WebSearchResult>, WebSearchError>;
}

#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    #[error("File search failed: {0}")]
    FileSearchFailed(String),
}

/// File search provider trait
#[async_trait]
pub trait FileSearchProviderTrait: Send + Sync {
    /// Search files within a conversation
    async fn search_conversation_files(
        &self,
        conversation_id: ConversationId,
        query: String,
    ) -> Result<Vec<FileSearchResult>, FileSearchError>;
}
