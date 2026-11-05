# Admin access token permissions

Admin sessions can create tokens using `POST /v1/admin/access-tokens`:

```json
{
  "name": "Monitoring dashboard",
  "reason": "Read admin metrics and configuration",
  "expires_in_hours": 720,
  "permission": "read_only"
}
```

`permission` accepts `read_only` and `read_write`. Omitting it defaults to
`read_write`; explicit null and unknown values are rejected. Creation and listing
responses include the effective permission. Permission is immutable: revoke and
recreate a token to change it. Existing credentials retain `read_write` access.

| Operation | Admin session | `read_write` | `read_only` |
| --- | --- | --- | --- |
| Approved admin reads across organizations | Allow | Allow | Allow |
| Admin mutations or unclassified operations | Allow | Allow | 403 `insufficient_permissions` |
| Create, list, or revoke admin tokens | Allow | 403 `forbidden` | 403 `forbidden` |

The method and matched route template are classified centrally in
`crates/api/src/middleware/admin_policy.rs`, including separately built routers.
GET (and its HEAD fallback) is approved only on the listed routes. POST pricing
and deprecation previews read models and notification recipients without storing
changes or sending notifications. POST database-encryption scans read counts and
schema classification; GET job status reads an existing job. POST verification
creates a job and requires write permission. Newly added routes require write
permission until their handler and service calls are reviewed and explicitly
approved for reads. OpenAPI uses the same registry for documented admin routes.

Read-only access spans organizations; organization and resource scopes are not
supported. Token hashes, expiration, revocation, User-Agent binding and the
creator's email allowlist eligibility are checked as before. Usage timestamps
and audit logging are permitted; business mutations are not.

## Rollout and rollback

1. Keep `AUTH_ADMIN_READ_ONLY_TOKENS_ENABLED=false` (the default). Apply migration
   V0079 and deploy this version's permission enforcement to **all** API instances,
   including any canary or rollback capacity that may serve admin requests.
2. Confirm no older instance can serve admin requests, then enable
   `AUTH_ADMIN_READ_ONLY_TOKENS_ENABLED=true` on the instances that issue tokens.
   Before enabling, requesting `read_only` returns HTTP 503 with
   `read_only_token_issuance_disabled`; omitted or `read_write` issuance still works.
3. The flag controls issuance only. Turning it off never relaxes enforcement on
   existing read-only tokens. Once issued, those credentials must never be routed
   to an older API version: older instances ignore the permission column.
4. To roll back to a version without enforcement, disable read-only issuance on
   every issuer and revoke all active read-only tokens before routing any traffic
   to the old version. Preserve the permission column during rollback. Revocation
   must complete while permission enforcement is still deployed.

## Admin email allowlist migration

Platform admin eligibility is controlled by `AUTH_ADMIN_EMAILS`, a comma-separated
list of complete email addresses matched case-insensitively against the stored
account email (not the editable contact email). There is no domain wildcard or
fallback to the retired `AUTH_ADMIN_DOMAINS`. Empty or unset means no admin access.
Ordinary user login remains available. VPC-provisioned users remain ineligible.

Before deploying, inventory active service tokens and their creators. Configure
`AUTH_ADMIN_EMAILS` with the approved human admins and the creators of service
tokens that must remain usable. Deploy/restart every API instance with the new
configuration; instances running older code still use the domain-wide policy.

Existing `adm_` tokens require no migration or reissuance. Their permissions,
expiration, revocation and User-Agent binding remain unchanged, and token
management still requires an admin session. Every token request rechecks the
creator against the allowlist. Removing a creator therefore also disables their
service tokens once all instances have loaded the updated configuration; replace
needed service credentials under an approved identity before removing that user.
