use inference_providers::{ProviderSource, ProviderTier};
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
    Tinfoil,
}

impl ServedProviderType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ServedProviderType::Vllm => "vllm",
            ServedProviderType::External => "external",
            ServedProviderType::Chutes => "chutes",
            ServedProviderType::Tinfoil => "tinfoil",
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
            "tinfoil" => Ok(ServedProviderType::Tinfoil),
            _ => Err(format!("Unknown served provider type: {s}")),
        }
    }
}

// Provider identity <-> recorded attribution, both directions, exhaustive.
impl From<ProviderTier> for ServedProviderTier {
    fn from(tier: ProviderTier) -> Self {
        match tier {
            ProviderTier::Near => Self::Near,
            ProviderTier::Attested3p => Self::Attested3p,
            ProviderTier::NonAttested => Self::NonAttested,
        }
    }
}

impl From<ServedProviderTier> for ProviderTier {
    fn from(tier: ServedProviderTier) -> Self {
        match tier {
            ServedProviderTier::Near => Self::Near,
            ServedProviderTier::Attested3p => Self::Attested3p,
            ServedProviderTier::NonAttested => Self::NonAttested,
        }
    }
}

impl From<ProviderSource> for ServedProviderType {
    fn from(source: ProviderSource) -> Self {
        match source {
            ProviderSource::Vllm => Self::Vllm,
            ProviderSource::External => Self::External,
            ProviderSource::Chutes => Self::Chutes,
            ProviderSource::Tinfoil => Self::Tinfoil,
        }
    }
}

impl From<ServedProviderType> for ProviderSource {
    fn from(ty: ServedProviderType) -> Self {
        match ty {
            ServedProviderType::Vllm => Self::Vllm,
            ServedProviderType::External => Self::External,
            ServedProviderType::Chutes => Self::Chutes,
            ServedProviderType::Tinfoil => Self::Tinfoil,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_provider_type_round_trips_tinfoil() {
        assert_eq!(
            "tinfoil".parse::<ServedProviderType>().unwrap(),
            ServedProviderType::Tinfoil
        );
        assert_eq!(ServedProviderType::Tinfoil.as_str(), "tinfoil");
    }

    #[test]
    fn identity_conversions_round_trip() {
        for s in ProviderSource::ALL {
            assert_eq!(ProviderSource::from(ServedProviderType::from(s)), s);
            assert_eq!(ServedProviderType::from(s).as_str(), s.as_str());
        }
        for t in [
            ProviderTier::Near,
            ProviderTier::Attested3p,
            ProviderTier::NonAttested,
        ] {
            assert_eq!(ProviderTier::from(ServedProviderTier::from(t)), t);
        }
    }
}
