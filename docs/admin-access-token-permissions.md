# Admin access token permissions

Admin access tokens (`adm_` prefix) carry an immutable permission, `read_only` or
`read_write`. This service authenticates them but no longer serves routes to create,
list or revoke them. Existing credentials retain `read_write` access.

| Operation | Admin session | `read_write` | `read_only` |
| --- | --- | --- | --- |
| Approved admin reads across organizations | Allow | Allow | Allow |
| Admin mutations or unclassified operations | Allow | Allow | 403 `insufficient_permissions` |

The method and matched route template are classified centrally in
`crates/api/src/middleware/admin_policy.rs`, including separately built routers.
GET (and its HEAD fallback) is approved only on the listed routes. POST database-encryption scans read counts and
schema classification; GET job status reads an existing job. GET (and HEAD)
`/v1/admin/database-encryption/keys` reads key IDs, fingerprints and write status.
POST verification and jobs with `actions: ["rewrap"]` create work and require
write permission. Newly added routes require write
permission until their handler and service calls are reviewed and explicitly
approved for reads. OpenAPI uses the same registry for documented admin routes.

Read-only access spans organizations; organization and resource scopes are not
supported. Token hashes, expiration, revocation, User-Agent binding and the
creator's email allowlist eligibility are checked as before. Usage timestamps
and audit logging are permitted; business mutations are not.

## Enforcement

Permission enforcement applies to every token request. Once issued, read-only
credentials must never be routed to an API version without enforcement: older
instances ignore the permission column.

Object encryption uses the same separation: `GET /v1/admin/object-encryption/keys`,
`GET /v1/admin/object-encryption/jobs/{id}`, and `POST /v1/admin/object-encryption/scan`
require Read permission. Creating, cancelling, or verifying object encryption jobs
requires Write permission. See [object encryption key rotation](object-encryption.md).

## Admin email allowlist migration

Platform admin eligibility is controlled by `AUTH_ADMIN_EMAILS`, a comma-separated
list of complete email addresses matched case-insensitively against the stored
account email (not the editable contact email). There is no domain wildcard or
fallback to the retired `AUTH_ADMIN_DOMAINS`. Remove the retired variable before
startup, even if it is empty or `AUTH_ADMIN_EMAILS` is also configured; its presence
is a configuration error. Invalid email entries and wildcards also fail startup
with an entry index, without logging the configured address. Empty or unset
`AUTH_ADMIN_EMAILS` means no admin access.
Ordinary user login remains available. VPC-provisioned users remain ineligible.

Before deploying, inventory active service tokens and their creators. Configure
`AUTH_ADMIN_EMAILS` with the approved human admins and the creators of service
tokens that must remain usable. Deploy/restart every API instance with the new
configuration; instances running older code still use the domain-wide policy.

Existing `adm_` tokens require no migration or reissuance. Their permissions,
expiration, revocation and User-Agent binding remain unchanged. Every token request rechecks the
creator against the allowlist. Removing a creator therefore also disables their
service tokens once all instances have loaded the updated configuration; replace
needed service credentials under an approved identity before removing that user.
