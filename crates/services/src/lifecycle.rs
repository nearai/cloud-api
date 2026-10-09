//! Typed account lifecycle, derived from operator-owned columns only.
//! Users: `users.is_active` + `users.auth_provider` (written by the login flow
//! and by erasure). Organizations: `organizations.is_active` + whether every
//! membership row belongs to an erased user (computed by the database layer).
//! Nothing customer-editable is read.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Sentinel stored in `users.auth_provider` when an account is erased.
///
/// All write paths (and SQL that classifies users) must use this constant:
/// [`UserLifecycle::from_columns`] matches it exactly (case-sensitive, no
/// trimming), so a divergent literal would silently break erasure classification.
pub const ERASED_AUTH_PROVIDER: &str = "erased";

/// Returned when a user's `is_active` and `auth_provider` columns contradict each
/// other (an active user carrying [`ERASED_AUTH_PROVIDER`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("active user carries the erased auth_provider sentinel")]
pub struct UserLifecycleError;

/// Account state derived only from operator-owned columns (`users.is_active` +
/// `users.auth_provider`). Serialized as `active`, `deactivated` or `erased`; this
/// lowercase spelling is part of the public API vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum UserLifecycle {
    Active,
    Deactivated,
    Erased,
}

impl UserLifecycle {
    /// Maps `(is_active, auth_provider)`; `auth_provider == ERASED_AUTH_PROVIDER`
    /// marks erasure. An active user carrying the erased sentinel is contradictory
    /// and returns [`UserLifecycleError`] rather than a guess.
    pub fn from_columns(is_active: bool, auth_provider: &str) -> Result<Self, UserLifecycleError> {
        match (is_active, auth_provider == ERASED_AUTH_PROVIDER) {
            (true, false) => Ok(Self::Active),
            (false, false) => Ok(Self::Deactivated),
            (false, true) => Ok(Self::Erased),
            (true, true) => Err(UserLifecycleError),
        }
    }
}

/// Organization state derived from `organizations.is_active` and its membership
/// rows. Serialized as `active`, `deleted` or `erased` (public API vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrganizationLifecycle {
    Active,
    Deleted,
    Erased,
}

impl OrganizationLifecycle {
    /// `all_members_erased` must be computed as "at least one member, and every
    /// member erased": an org with no membership rows is `false`, never vacuously
    /// "all erased", so an empty deleted org reads `Deleted`.
    ///
    /// `is_active` is authoritative: it gates every organization lookup, so an org
    /// that is still active is `Active` even if `all_members_erased` is true. Erasure
    /// deactivates an org in the same transaction that erases its last member, so
    /// `(true, true)` is not expected. Unlike [`UserLifecycle::from_columns`] this
    /// does not error, because the flag is a derived aggregate rather than a stored
    /// sentinel. Any SQL mirror of this mapping must keep the same precedence.
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
        let u: UserLifecycle = serde_json::from_value("deactivated".into()).unwrap();
        assert_eq!(u, UserLifecycle::Deactivated);
        let f: OrganizationLifecycle = serde_json::from_value("erased".into()).unwrap();
        assert_eq!(f, OrganizationLifecycle::Erased);
    }
}
