use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id_prefixes::PREFIX_CONV;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl From<Uuid> for ConversationId {
    fn from(uuid: Uuid) -> Self {
        ConversationId(uuid)
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", PREFIX_CONV, self.0.simple())
    }
}
