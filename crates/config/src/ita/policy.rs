use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

pub const MAX_ITA_POLICY_IDS: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItaTokenSigningAlg {
    #[serde(rename = "PS384")]
    #[default]
    Ps384,
    #[serde(rename = "RS256")]
    Rs256,
}

impl fmt::Display for ItaTokenSigningAlg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ps384 => f.write_str("PS384"),
            Self::Rs256 => f.write_str("RS256"),
        }
    }
}

impl FromStr for ItaTokenSigningAlg {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim() {
            "PS384" => Ok(Self::Ps384),
            "RS256" => Ok(Self::Rs256),
            _ => Err("ITA_TOKEN_SIGNING_ALG must be PS384 or RS256".to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ItaPolicyIds(Vec<Uuid>);

impl ItaPolicyIds {
    pub fn parse_csv(raw: &str, field: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Ok(Self::default());
        }

        Self::parse_tokens(raw.split(',').map(str::trim), field)
    }

    fn parse_tokens<'a>(
        policy_ids: impl IntoIterator<Item = &'a str>,
        field: &str,
    ) -> Result<Self, String> {
        let mut parsed_policy_ids = Vec::new();

        for policy_id in policy_ids {
            if policy_id.is_empty() {
                return Err(format!("{field} policy id cannot be empty"));
            }

            parsed_policy_ids.push(parse_ita_policy_id(policy_id, field)?);
        }

        if parsed_policy_ids.len() > MAX_ITA_POLICY_IDS {
            return Err(format!(
                "{field} supports at most {MAX_ITA_POLICY_IDS} policy ids"
            ));
        }

        Ok(Self(parsed_policy_ids))
    }

    pub fn as_slice(&self) -> &[Uuid] {
        self.0.as_slice()
    }

    pub fn to_strings(&self) -> Vec<String> {
        self.0.iter().map(Uuid::to_string).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for ItaPolicyIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ItaPolicyIdsInput {
            Csv(String),
            List(Vec<String>),
        }

        match ItaPolicyIdsInput::deserialize(deserializer)? {
            ItaPolicyIdsInput::Csv(raw) => {
                Self::parse_csv(&raw, "policy_ids").map_err(serde::de::Error::custom)
            }
            ItaPolicyIdsInput::List(raw_policy_ids) => Self::parse_tokens(
                raw_policy_ids.iter().map(|policy_id| policy_id.trim()),
                "policy_ids",
            )
            .map_err(serde::de::Error::custom),
        }
    }
}

fn parse_ita_policy_id(policy_id: &str, field: &str) -> Result<Uuid, String> {
    Uuid::parse_str(policy_id)
        .map_err(|_| format!("{field} policy ids must be Intel Trust Authority policy UUIDs"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaPolicyOverride {
    pub policy_ids: Option<ItaPolicyIds>,
    pub policy_must_match: Option<bool>,
    pub token_signing_alg: Option<ItaTokenSigningAlg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItaEffectivePolicy {
    pub policy_ids: ItaPolicyIds,
    pub policy_must_match: bool,
    pub token_signing_alg: ItaTokenSigningAlg,
}
