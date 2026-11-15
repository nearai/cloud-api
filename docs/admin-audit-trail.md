# Admin audit trail

Every request to `/v1/admin` or a path below it logs exactly one INFO event with
`audit_event="admin_request"`: reads, writes, 401/403 denials, CORS preflights,
unsupported methods and admin paths that match no route. A request whose client
disconnects before the response is still logged once, without `admin_status`.

Every admin response carries `x-admin-audit-request-id`, the server-generated
request id that the event logs as `request_id`. It never echoes a client value:
a client-sent copy is removed from the request and the response header is
overwritten. `x-request-id` keeps echoing a valid client id.

| Field | Value |
| --- | --- |
| `request_id` | server request id, same as the header |
| `admin_route` | matched route template, or `/unmatched` |
| `admin_method` | HTTP method (`OTHER` for extension methods) |
| `admin_status` | response status |
| `admin_auth_outcome` | `allowed`, `denied`, or `unknown` when admin auth never ran |
| `admin_credential_kind` | `session`, `admin_access_token` or `none` |
| `admin_credential_id` | admin token id, or the session token's `sid` claim |
| `admin_user_id` | user the credential authenticated, also when policy then denies |
| `admin_organization_id` | `{org_id}` of an allowed org-scoped route |
| `admin_duration_ms` | time to the response head |

Optional fields are omitted when unknown: a rejected credential has no user or
credential id, a legacy session token without `sid` has no credential id, and a
global read such as the users list names no organization. The event never holds
the raw path or query, names, emails, User-Agent, credentials or their hashes,
request or response bodies, or forwarded client IPs.

## Log output

Production renders JSON at INFO without span data, so each event is one line
whose `fields` object holds only these fields and `message`. The request's other
log lines get the request id only from the enclosing request span, which that
output omits: in production, the audit event is the only log line of an admin
request that carries the id. When cloud-platform is configured, calls to it made
while serving the request send the id as `x-request-id`.

The `pretty` development format prints the request span on every line of the
request, the audit event included, with the span's path (UUID and long
token-like segments redacted, other segments as sent) and the client's
`x-request-id`.

## OHTTP

`POST /ohttp` decapsulates its inner request and replays it to the API over
loopback. An inner admin request therefore passes the audit layer and logs its
own event under its own request id. The ingress only sees the outer `/ohttp`
request; the inner response's `x-admin-audit-request-id` returns inside the
encrypted OHTTP response, so these events have no ingress log entry to join.
The OHTTP handler's own INFO line logs the inner request target, query included.

## cloud-platform

These events cover the admin requests that cloud-api serves. cloud-platform
serves its own copy of the admin routes that moved there under the database
freeze (`.claude/rules/database.md`) and writes its own audit records; a request
that cloud-platform answers produces none of the events on this page. Matching
coverage for those routes is a paired cloud-platform change, not described here.

## How it works

`admin_audit_middleware` (`crates/api/src/middleware/admin_audit.rs`) runs once
on the root router, after routing, inside request correlation and outside CORS.
`admin_middleware` records the authentication facts through the `AdminAudit`
extractor. Nothing reads bodies.

New routes under `/v1/admin` are audited automatically. The e2e suite in
`crates/api/tests/e2e_all/admin_audit.rs` checks that:

- every route mounted in `crates/api/src/lib.rs` and
  `crates/api/src/object_encryption.rs` is listed in
  `MOUNTED_ADMIN_ROUTES` and logs one event;

The object-encryption router's six operations use the same audit layer and
authorization policy as the database-encryption routes. Reading keys or job status
and scanning objects are reads; creating, cancelling and verifying jobs require
write permission. The per-route audit test mounts this router with mock object
storage and checks each operation's denied request and audit event.
