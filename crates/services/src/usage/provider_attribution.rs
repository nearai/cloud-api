use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServedProviderTier {
    Near,
    #[serde(rename = "attested_3p")]
    Attested3p,
    NonAttested,
}

impl ServedProviderTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            ServedProviderTier::Near => "near",
            ServedProviderTier::Attested3p => "attested_3p",
            ServedProviderTier::NonAttested => "non_attested",
        }
    }
}

impl std::fmt::Display for ServedProviderTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for ServedProviderTier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "near" => Ok(ServedProviderTier::Near),
            "attested_3p" => Ok(ServedProviderTier::Attested3p),
            "non_attested" => Ok(ServedProviderTier::NonAttested),
            _ => Err(format!("Unknown served provider tier: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServedProviderType {
    Vllm,
    External,
    Chutes,
}

impl ServedProviderType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ServedProviderType::Vllm => "vllm",
            ServedProviderType::External => "external",
            ServedProviderType::Chutes => "chutes",
        }
    }
}

impl std::fmt::Display for ServedProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for ServedProviderType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vllm" => Ok(ServedProviderType::Vllm),
            "external" => Ok(ServedProviderType::External),
            "chutes" => Ok(ServedProviderType::Chutes),
            _ => Err(format!("Unknown served provider type: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAttribution {
    #[serde(default)]
    pub served_provider_tier: Option<ServedProviderTier>,
    #[serde(default)]
    pub served_provider_type: Option<ServedProviderType>,
    #[serde(default)]
    pub served_via_fallback: bool,
}
