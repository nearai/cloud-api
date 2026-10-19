//! Provider configuration and discovery are independent of the billable model catalog.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    pub name: String,
    /// Existing backend config, without model_name or credentials.
    pub config: Value,
    /// Environment variable containing this provider's API key. Never returned as a value.
    pub credential_env: Option<String>,
    /// Relative to base_url; null disables discovery (manual inventory is supported).
    pub models_path: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

impl ProviderDefinition {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.name.len() > 200 {
            bail!("Provider name must contain 1 to 200 characters");
        }
        inference_providers::non_attested::external::validate_external_provider_config(
            &self.config,
        )
        .map_err(anyhow::Error::msg)?;
        if self.config.get("api_key").is_some() || self.config.get("model_name").is_some() {
            bail!("Use credential_env for credentials and model mappings for upstream names");
        }
        let url = url::Url::parse(self.base_url())?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("base_url cannot contain credentials, query parameters, or a fragment");
        }
        if let Some(key) = &self.credential_env {
            // Avoid using arbitrary application secrets as provider credentials.
            if !key.ends_with("_API_KEY")
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            {
                bail!("credential_env must be an uppercase environment name ending in _API_KEY");
            }
        }
        if let Some(path) = &self.models_path {
            let root = url::Url::parse(&format!("{}/", self.base_url().trim_end_matches('/')))?;
            let target = root.join(path)?;
            if path.is_empty()
                || path.starts_with('/')
                || path.contains("..")
                || target.origin() != root.origin()
                || target.query().is_some()
                || target.fragment().is_some()
            {
                bail!("models_path must be a relative path on the provider origin");
            }
        }
        Ok(())
    }

    pub fn base_url(&self) -> &str {
        self.config
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn backend(&self) -> &str {
        self.config
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn api_key(&self) -> Result<Option<String>> {
        self.credential_env
            .as_ref()
            .map(|key| {
                std::env::var(key)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .context("Provider credential is not configured on this replica")
            })
            .transpose()
    }
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RegisteredProvider {
    pub id: Uuid,
    #[serde(flatten)]
    pub definition: ProviderDefinition,
    /// These adapters currently provide non-attested inference only.
    pub execution_security: String,
    pub revision: i64,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub sync_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct ProviderModel {
    pub upstream_model: String,
    pub metadata: Value,
    pub available: bool,
    pub model_id: Option<Uuid>,
    pub approved: bool,
    pub archived: bool,
    /// None means unknown; an empty list explicitly means unsupported.
    pub reasoning_efforts: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelApproval {
    pub upstream_model: String,
    /// Existing catalog model, including its customer pricing.
    pub model_name: String,
    pub approved: bool,
    #[serde(default)]
    pub archived: bool,
    /// Overrides discovery when supplied. Null restores discovered capabilities.
    pub reasoning_efforts: Option<Vec<String>>,
}

pub fn validate_efforts(efforts: Option<&[String]>) -> Result<()> {
    if let Some(efforts) = efforts {
        if efforts.iter().any(|v| {
            !matches!(
                v.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        }) {
            bail!("Unsupported reasoning effort value");
        }
    }
    Ok(())
}

fn discovered_efforts(model: &Value) -> Option<Vec<String>> {
    for path in [
        "/effort/supported_levels",
        "/reasoning/effort",
        "/reasoning/efforts",
        "/supported_reasoning_efforts",
        "/reasoning_efforts",
    ] {
        if let Some(values) = model.pointer(path).and_then(Value::as_array) {
            return Some(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            );
        }
    }
    model
        .pointer("/capabilities/effort")
        .and_then(Value::as_object)
        .map(|efforts| {
            efforts
                .iter()
                .filter(|(key, value)| {
                    key.as_str() != "supported"
                        && (value.as_bool() == Some(true)
                            || value.get("supported").and_then(Value::as_bool) == Some(true))
                })
                .map(|(key, _)| key.clone())
                .collect()
        })
}

/// Parse a complete page before changing inventory. A malformed page is never an empty snapshot.
pub fn parse_models_page(body: &Value) -> Result<Vec<ProviderModel>> {
    let rows = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array)
        .context("Provider model response must contain a data or models array")?;
    rows.iter()
        .map(|row| {
            let id = row
                .get("id")
                .or_else(|| row.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty() && s.len() <= 500)
                .context("Provider model is missing a valid ID")?;
            Ok(ProviderModel {
                upstream_model: id.to_owned(),
                metadata: row.clone(),
                available: true,
                model_id: None,
                approved: false,
                archived: false,
                reasoning_efforts: discovered_efforts(row),
            })
        })
        .collect()
}

/// Bounded, same-origin discovery with no credential-bearing redirects.
/// Pagination follows cursors, never provider-supplied next URLs.
pub async fn discover_models(definition: &ProviderDefinition) -> Result<Vec<ProviderModel>> {
    definition.validate()?;
    let path = definition
        .models_path
        .as_ref()
        .context("Model discovery is disabled")?;
    let root = url::Url::parse(&format!("{}/", definition.base_url().trim_end_matches('/')))?;
    let base = root.join(path)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()?;
    let key = definition.api_key()?;
    let mut cursor: Option<(String, String)> = None;
    let mut seen_cursors = HashSet::new();
    let mut seen_models = HashSet::new();
    let mut models = Vec::new();
    for _ in 0..100 {
        let mut url = base.clone();
        if let Some((name, value)) = &cursor {
            url.query_pairs_mut().append_pair(name, value);
        }
        let mut request = client.get(url);
        if definition.backend() == "anthropic" {
            request = request.header(
                "anthropic-version",
                definition
                    .config
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("2023-06-01"),
            );
            if let Some(key) = &key {
                request = request.header("x-api-key", key);
            }
        } else if definition.backend() == "gemini" {
            if let Some(key) = &key {
                request = request.header("x-goog-api-key", key);
            }
        } else if let Some(key) = &key {
            request = request.bearer_auth(key);
        }
        if let Some(org) = definition
            .config
            .get("organization_id")
            .and_then(Value::as_str)
        {
            request = request.header("OpenAI-Organization", org);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Provider model discovery transport failed"))?;
        if !response.status().is_success() {
            bail!(
                "Provider model discovery returned HTTP {}",
                response.status().as_u16()
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Provider discovery body read failed"))?
        {
            if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                bail!("Provider model page exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Provider discovery returned invalid JSON"))?;
        for model in parse_models_page(&body)? {
            if seen_models.insert(model.upstream_model.clone()) {
                models.push(model);
            }
        }
        if models.len() > 20_000 {
            bail!("Provider model inventory exceeds size limit");
        }
        cursor = if let Some(token) = body
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(("pageToken".into(), token.into()))
        } else if body.get("has_more").and_then(Value::as_bool) == Some(true) {
            Some((
                "after_id".into(),
                body.get("last_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .context("Missing model pagination cursor")?
                    .into(),
            ))
        } else {
            return Ok(models);
        };
        if !seen_cursors.insert(cursor.clone()) {
            bail!("Repeated model pagination cursor");
        }
    }
    bail!("Provider model discovery exceeded page limit")
}

#[cfg(test)]
mod tests {
    use crate::provider_registry::{parse_models_page, ProviderDefinition};
    use serde_json::json;

    #[test]
    fn parses_anthropic_and_deepseek_effort_metadata() {
        let models=parse_models_page(&json!({"data":[
            {"id":"claude","capabilities":{"effort":{"supported":true,"low":{"supported":true},"high":{"supported":false}}}},
            {"id":"deepseek","effort":{"supported_levels":["low","high","max"]}},
            {"id":"unknown"}
        ]})).unwrap();
        assert_eq!(models[0].reasoning_efforts, Some(vec!["low".into()]));
        assert_eq!(
            models[1].reasoning_efforts,
            Some(vec!["low".into(), "high".into(), "max".into()])
        );
        assert_eq!(models[2].reasoning_efforts, None);
    }

    #[test]
    fn discovery_never_approves_models_and_retains_capabilities() {
        let models = parse_models_page(
            &json!({"data":[{"id":"reasoner","reasoning_efforts":["low","high"]}]}),
        )
        .unwrap();
        assert!(!models[0].approved);
        assert_eq!(
            models[0].reasoning_efforts,
            Some(vec!["low".into(), "high".into()])
        );
        assert!(parse_models_page(&json!({"error":"unauthorized"})).is_err());
        assert!(parse_models_page(&json!({"data":[{}]})).is_err());
    }

    #[test]
    fn discovery_path_cannot_exfiltrate_credentials() {
        let mut provider = ProviderDefinition {
            name: "example".into(),
            config: json!({"backend":"openai_compatible","base_url":"https://example.com/v1"}),
            credential_env: Some("EXAMPLE_API_KEY".into()),
            models_path: Some("models".into()),
            archived: false,
        };
        assert!(provider.validate().is_ok());
        for path in [
            "https://evil.example/models",
            "//evil.example/models",
            "../models",
            "models?key=secret",
        ] {
            provider.models_path = Some(path.into());
            assert!(provider.validate().is_err());
        }
    }
}
