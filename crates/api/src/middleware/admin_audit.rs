//! Admin audit trail: exactly one INFO event per `/v1/admin/*` request.
//!
//! [`admin_audit_middleware`] is installed once on the root router, inside
//! `request_correlation_middleware` (it reuses the server-generated request
//! id) and outside CORS. It therefore sees every admin request: allowed and
//! denied ones, CORS preflights, and admin paths no route matches. Like the
//! metrics middleware it runs after Axum routing, so `MatchedPath` holds the
//! route template.
//!
//! The middleware attaches an [`AdminAudit`] record to the request.
//! `admin_middleware` fills in the authentication facts, so this module never
//! reads credentials, bodies, the query string or the raw path. The event carries only:
//!
//! - `audit_event`: always `admin_request`
//! - `request_id`: the server-generated id, also returned in the
//!   `x-admin-audit-request-id` response header
//! - `admin_route`: the matched route template, or `/unmatched`
//! - `admin_method`, `admin_status`, `admin_duration_ms` (time to response head)
//! - `admin_auth_outcome`: `allowed`, `denied`, or `unknown` when admin
//!   authentication never ran (no matching route, CORS preflight)
//! - `admin_credential_kind`: `session`, `admin_access_token` or `none`
//! - `admin_credential_id`: admin token id, or the session's `sid` claim
//! - `admin_user_id`: set once a credential authenticated, even if the
//!   request is then denied; never for a rejected credential
//! - `admin_organization_id`: the `{org_id}` of an allowed org-scoped route
//!
//! Optional fields are omitted when unknown. A request whose future is
//! dropped before a response (client disconnect) is still logged once,
//! without `admin_status`.

use crate::middleware::{metrics::bounded_method, RequestCorrelation};
use axum::{
    body::Body,
    extract::{FromRequestParts, MatchedPath, RawPathParams},
    http::{request::Parts, Extensions, HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use uuid::Uuid;

/// Response header with the server-generated id of an admin request.
pub const ADMIN_AUDIT_REQUEST_ID_HEADER: &str = "x-admin-audit-request-id";

/// Value of the `audit_event` field that selects admin audit events.
pub const ADMIN_AUDIT_EVENT: &str = "admin_request";

const UNMATCHED_ROUTE: &str = "/unmatched";

/// Route parameter naming the organization an admin route operates on.
const ORGANIZATION_ROUTE_PARAM: &str = "org_id";

/// Whether a request path belongs to the admin API.
pub(crate) fn is_admin_path(path: &str) -> bool {
    path == "/v1/admin" || path.starts_with("/v1/admin/")
}

/// The credential `admin_middleware` validated, or started to validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminCredentialKind {
    Session,
    AdminAccessToken,
}

impl AdminCredentialKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::AdminAccessToken => "admin_access_token",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthOutcome {
    Allowed,
    Denied,
}

#[derive(Debug, Clone, Copy, Default)]
struct Record {
    outcome: Option<AuthOutcome>,
    credential_kind: Option<AdminCredentialKind>,
    credential_id: Option<Uuid>,
    user_id: Option<Uuid>,
}

/// Request-scoped audit record of an admin request.
///
/// Extracting it never fails: outside the audit layer the handler gets a
/// detached record and recording into it is a no-op.
#[derive(Clone, Default)]
pub struct AdminAudit(Arc<Mutex<Record>>);

impl AdminAudit {
    pub(crate) fn from_extensions(extensions: &Extensions) -> Self {
        extensions.get::<Self>().cloned().unwrap_or_default()
    }

    /// `admin_middleware` took the validation path for this credential kind.
    /// The id is set only once the credential has validated.
    pub(crate) fn record_credential(&self, kind: AdminCredentialKind, credential_id: Option<Uuid>) {
        self.update(|record| {
            record.credential_kind = Some(kind);
            record.credential_id = credential_id;
        });
    }

    /// The validated credential authenticated `user_id`.
    pub(crate) fn record_authenticated(&self, user_id: Uuid) {
        self.update(|record| record.user_id = Some(user_id));
    }

    pub(crate) fn record_allowed(&self) {
        self.update(|record| record.outcome = Some(AuthOutcome::Allowed));
    }

    pub(crate) fn record_denied(&self) {
        self.update(|record| record.outcome = Some(AuthOutcome::Denied));
    }

    fn update(&self, apply: impl FnOnce(&mut Record)) {
        // The guard never crosses an await, and a poisoned record still holds
        // the facts recorded before the panic.
        apply(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn snapshot(&self) -> Record {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AdminAudit {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_extensions(&parts.extensions))
    }
}

/// Emits the audit event of one admin request exactly once, on drop.
struct AdminAuditEvent {
    request_id: Uuid,
    route: String,
    method: &'static str,
    /// The route's `{org_id}`; reported only when the request was allowed.
    route_organization_id: Option<Uuid>,
    started: Instant,
    record: AdminAudit,
    status: Option<u16>,
}

impl AdminAuditEvent {
    /// Record the response status; dropping `self` here emits the event.
    fn finish(mut self, status: u16) {
        self.status = Some(status);
    }
}

impl Drop for AdminAuditEvent {
    fn drop(&mut self) {
        let record = self.record.snapshot();
        let allowed = record.outcome == Some(AuthOutcome::Allowed);
        let organization_id = self.route_organization_id.filter(|_| allowed);
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // A root event, so the full and compact formats leave out the request
        // span's fields (raw path, client x-request-id). Production's JSON
        // format prints no span data at all; the dev `pretty` format still does.
        tracing::info!(
            parent: None,
            audit_event = ADMIN_AUDIT_EVENT,
            request_id = %self.request_id,
            admin_route = self.route.as_str(),
            admin_method = self.method,
            admin_status = self.status,
            admin_auth_outcome = match record.outcome {
                Some(AuthOutcome::Allowed) => "allowed",
                Some(AuthOutcome::Denied) => "denied",
                None => "unknown",
            },
            admin_user_id = record.user_id.map(tracing::field::display),
            admin_credential_kind = record
                .credential_kind
                .map_or("none", AdminCredentialKind::as_str),
            admin_credential_id = record.credential_id.map(tracing::field::display),
            admin_organization_id = organization_id.map(tracing::field::display),
            admin_duration_ms = duration_ms,
            "admin request"
        );
    }
}

pub async fn admin_audit_middleware(request: Request<Body>, next: Next) -> Response {
    if !is_admin_path(request.uri().path()) {
        return next.run(request).await;
    }

    let started = Instant::now();
    let (mut parts, body) = request.into_parts();
    // Only the server sets this header; nothing downstream may read a client copy.
    parts.headers.remove(ADMIN_AUDIT_REQUEST_ID_HEADER);
    let record = AdminAudit::default();
    parts.extensions.insert(record.clone());
    let event = AdminAuditEvent {
        // The correlation middleware wraps this one, so the id is always set.
        request_id: parts
            .extensions
            .get::<RequestCorrelation>()
            .map_or_else(Uuid::new_v4, |correlation| correlation.request_id),
        route: parts
            .extensions
            .get::<MatchedPath>()
            .map_or(UNMATCHED_ROUTE, MatchedPath::as_str)
            .to_owned(),
        method: bounded_method(&parts.method),
        route_organization_id: route_organization_id(&mut parts).await,
        started,
        record,
        status: None,
    };

    let mut response = next.run(Request::from_parts(parts, body)).await;
    if let Ok(value) = HeaderValue::from_str(&event.request_id.to_string()) {
        response
            .headers_mut()
            .insert(ADMIN_AUDIT_REQUEST_ID_HEADER, value);
    }
    event.finish(response.status().as_u16());
    response
}

/// The matched route's `{org_id}`, when it is a UUID.
async fn route_organization_id(parts: &mut Parts) -> Option<Uuid> {
    let params = RawPathParams::from_request_parts(parts, &()).await.ok()?;
    params
        .iter()
        .find(|(name, _)| *name == ORGANIZATION_ROUTE_PARAM)
        .and_then(|(_, value)| Uuid::parse_str(value).ok())
}

#[cfg(test)]
#[path = "admin_audit_tests.rs"]
mod tests;
