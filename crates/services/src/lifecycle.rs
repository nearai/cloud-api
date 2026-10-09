//! Typed account lifecycle, derived from operator-owned columns only.
//! Users: `users.is_active` + `users.auth_provider` (written by the login flow
//! and by erasure). Organizations: `organizations.is_active` + whether every
//! membership row belongs to an erased user (computed by the database layer).
//! Nothing customer-editable is read.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const ERASED_AUTH_PROVIDER: &str = "erased";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("active user carries the erased auth_provider sentinel")]
pub struct LifecycleError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum UserLifecycle {
    Active,
    Deactivated,
    Erased,
}

impl UserLifecycle {
    pub fn from_columns(is_active: bool, auth_provider: &str) -> Result<Self, LifecycleError> {
        match (is_active, auth_provider == ERASED_AUTH_PROVIDER) {
            (true, false) => Ok(Self::Active),
            (false, false) => Ok(Self::Deactivated),
            (false, true) => Ok(Self::Erased),
            (true, true) => Err(LifecycleError),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrganizationLifecycle {
    Active,
    Deleted,
    Erased,
}

impl OrganizationLifecycle {
    pub fn from_columns(is_active: bool, all_members_erased: bool) -> Self {
        match (is_active, all_members_erased) {
            (true, _) => Self::Active,
            (false, true) => Self::Erased,
            (false, false) => Self::Deleted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_lifecycle_table() {
        assert_eq!(
            UserLifecycle::from_columns(true, "github").unwrap(),
            UserLifecycle::Active
        );
        assert_eq!(
            UserLifecycle::from_columns(false, "google").unwrap(),
            UserLifecycle::Deactivated
        );
        assert_eq!(
            UserLifecycle::from_columns(false, "erased").unwrap(),
            UserLifecycle::Erased
        );
        assert!(UserLifecycle::from_columns(true, "erased").is_err());
    }

    #[test]
    fn organization_lifecycle_table() {
        assert_eq!(
            OrganizationLifecycle::from_columns(true, false),
            OrganizationLifecycle::Active
        );
        // is_active wins: erasure always deactivates the org in the same transaction.
        assert_eq!(
            OrganizationLifecycle::from_columns(true, true),
            OrganizationLifecycle::Active
        );
        assert_eq!(
            OrganizationLifecycle::from_columns(false, false),
            OrganizationLifecycle::Deleted
        );
        assert_eq!(
            OrganizationLifecycle::from_columns(false, true),
            OrganizationLifecycle::Erased
        );
    }

    #[test]
    fn serialized_names_are_the_api_vocabulary() {
        assert_eq!(
            serde_json::to_value(UserLifecycle::Erased).unwrap(),
            "erased"
        );
        assert_eq!(
            serde_json::to_value(OrganizationLifecycle::Deleted).unwrap(),
            "deleted"
        );
        let f: OrganizationLifecycle = serde_json::from_value("erased".into()).unwrap();
        assert_eq!(f, OrganizationLifecycle::Erased);
    }
}
