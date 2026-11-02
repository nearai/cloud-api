//! Public request schemas for tools executed by the Responses caller.

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
