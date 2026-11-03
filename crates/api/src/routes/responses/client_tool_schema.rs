//! Public request schemas for client-managed tools and request-local Cloud search.

use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum StatelessResponseToolSchema {
    #[serde(rename = "function")]
    Function {
        name: String,
        description: Option<String>,
        parameters: Option<serde_json::Value>,
    },
    #[serde(rename = "custom")]
    Custom {
        name: String,
        description: Option<String>,
        format: Option<StatelessCustomToolFormatSchema>,
    },
    #[serde(rename = "web_search")]
    WebSearch {
        /// Maximum results per search (0–20); zero skips the search provider. Overrides the model's count when set.
        #[schema(minimum = 0, maximum = 20)]
        count: Option<u32>,
        filters: Option<serde_json::Value>,
        search_context_size: Option<String>,
        user_location: Option<services::responses::models::UserLocation>,
    },
    #[serde(rename = "web_context_search")]
    WebContextSearch {},
    #[serde(rename = "namespace")]
    Namespace {
        name: String,
        description: Option<String>,
        tools: Vec<StatelessNamespacedToolSchema>,
    },
}

/// Namespace members are client-managed function or custom tools.
#[derive(Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum StatelessNamespacedToolSchema {
    #[serde(rename = "function")]
    Function {
        name: String,
        description: Option<String>,
        parameters: Option<serde_json::Value>,
    },
    #[serde(rename = "custom")]
    Custom {
        name: String,
        description: Option<String>,
        format: Option<StatelessCustomToolFormatSchema>,
    },
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum StatelessCustomToolFormatSchema {
    #[serde(rename = "text")]
    Text,
    /// The typed Chat adapter supports the Codex exec Lark source grammar.
    /// Other grammars require a native Responses backend that supports them.
    #[serde(rename = "grammar")]
    Grammar { syntax: String, definition: String },
}

#[derive(Deserialize, ToSchema)]
#[serde(untagged)]
pub enum StatelessToolChoiceSchema {
    Mode(StatelessToolChoiceModeSchema),
    Function {
        #[serde(rename = "type")]
        type_: StatelessFunctionToolTypeSchema,
        name: String,
        namespace: Option<String>,
    },
    Custom {
        #[serde(rename = "type")]
        type_: StatelessCustomToolTypeSchema,
        name: String,
        namespace: Option<String>,
    },
    /// Legacy Chat-style named function choice is also accepted.
    LegacyFunction {
        #[serde(rename = "type")]
        type_: StatelessFunctionToolTypeSchema,
        function: services::responses::models::ResponseToolChoiceFunction,
        namespace: Option<String>,
    },
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatelessToolChoiceModeSchema {
    Auto,
    None,
    Required,
}

#[derive(Deserialize, ToSchema)]
pub enum StatelessFunctionToolTypeSchema {
    #[serde(rename = "function")]
    Function,
}

#[derive(Deserialize, ToSchema)]
pub enum StatelessCustomToolTypeSchema {
    #[serde(rename = "custom")]
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use utoipa::PartialSchema;

    #[test]
    fn typed_search_schema_exposes_count_bounds_and_client_only_namespaces() {
        let schema = serde_json::to_value(StatelessResponseToolSchema::schema()).unwrap();
        let variants = schema["oneOf"].as_array().unwrap();
        let search = variants
            .iter()
            .find(|variant| variant["properties"]["type"]["enum"] == json!(["web_search"]))
            .unwrap();
        assert_eq!(search["properties"]["count"]["minimum"], 0);
        assert_eq!(search["properties"]["count"]["maximum"], 20);
        assert!(variants.iter().any(|variant| {
            variant["properties"]["type"]["enum"] == json!(["web_context_search"])
        }));
        let namespace = serde_json::to_value(StatelessNamespacedToolSchema::schema()).unwrap();
        let names: Vec<_> = namespace["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| variant["properties"]["type"]["enum"].clone())
            .collect();
        assert_eq!(names, vec![json!(["function"]), json!(["custom"])]);
    }
}
