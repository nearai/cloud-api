//! SQL inputs for the typed lifecycle in `services::lifecycle`. The rules live in
//! `OrganizationLifecycle::from_columns`; this module only computes their inputs and the
//! list filter, so erasure and the admin lists share one definition.

/// SQL boolean: the org (alias `o`) has at least one member and every member is an
/// erased user. Feed it with `o.is_active` to `OrganizationLifecycle::from_columns`.
pub const ORG_ALL_MEMBERS_ERASED_SQL: &str = "COALESCE((
    SELECT bool_and(lu.auth_provider = 'erased')
    FROM organization_members lm
    JOIN users lu ON lu.id = lm.user_id
    WHERE lm.organization_id = o.id
), false)";

/// SQL predicate over alias `o` for a lifecycle filter. Mirrors
/// `OrganizationLifecycle::from_columns` (is_active wins; then all-members-erased).
pub fn org_lifecycle_predicate(filter: services::admin::OrganizationLifecycleFilter) -> String {
    use services::admin::OrganizationLifecycleFilter as F;
    match filter {
        F::Active => "o.is_active".to_string(),
        F::Deleted => format!("(NOT o.is_active AND NOT {ORG_ALL_MEMBERS_ERASED_SQL})"),
        F::Erased => format!("(NOT o.is_active AND {ORG_ALL_MEMBERS_ERASED_SQL})"),
        F::All => "true".to_string(),
    }
}
