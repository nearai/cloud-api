//! Machine Payments Protocol on `POST /v1/chat/completions`.
//!
//! Two middlewares on the chat-completions router, active only when `MPP_ENABLED=true`:
//!
//! 1. [`mpp_auth_middleware`] replaces `auth_middleware_with_workspace_context`. It hands the
//!    request to that function unless MPP applies: the `Authorization` header is `Payment …`,
//!    or there is none and the body prices.
//!    - Session management is verified before body pricing and needs no inference body.
//!      Inference still passes the handler's pre-flight before a challenge or payment.
//!    - No header, priced model: 402 with a fresh challenge. Limits broken: 400.
//!    - `Payment` credential: stateless verification, then the payment is verified on chain
//!      (`verify_payment`: reads the chain, claims nothing), and only then does the request
//!      run as the configured service API key with a [`PendingMppPayment`] attached. An
//!      unpaid credential therefore never reaches the shared service key's rate limit.
//! 2. [`mpp_settle_middleware`], the innermost layer, runs after the rate limit and the usage
//!    check, just before the handler. Only here is the payment claimed, so a request refused
//!    earlier leaves a verified credential unspent and presentable again.
//!
//! With MPP disabled both middlewares hold `None` and change nothing.
//!
//! Nothing here logs a credential, header value, transaction hash, payer address, request
//! body or the service key. Log fields are closed outcome codes.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{FromRequest, Request, State},
    http::{
        header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE},
        HeaderName, HeaderValue, Method, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::Utc;
use services::mpp::{
    protocol::{is_payment_scheme, with_private_cache_control},
    MppError, MppResponse, MppService, PaidCredential, Pricing, ProblemCode, ProblemStatus,
    ServeOutcome, SessionService, CHAT_COMPLETIONS_ROUTE,
};

use super::auth::{
    auth_middleware_with_workspace_context, authenticate_api_key_with_context, caller_key,
    with_caller_key, AuthState,
};
use super::body_hash::RequestBodyHash;
use crate::models::ErrorResponse;
use crate::routes::completions::{reject_invalid_chat_request, ChatRequestBody};
use crate::routes::extractors::OpenAiJson;

const PAYMENT_RECEIPT: HeaderName = HeaderName::from_static("payment-receipt");
const PROBLEM_JSON: &str = "application/problem+json";
/// `Retry-After` when the service workspace cannot pay for inference (its credits or limits).
const SERVICE_ORG_RETRY_AFTER_SECS: u64 = 30;

/// State of [`mpp_auth_middleware`].
#[derive(Clone)]
pub struct MppAuthState {
    pub auth: AuthState,
    pub mpp: Option<Arc<MppService>>,
    pub session: Option<Arc<SessionService>>,
}

/// State of [`mpp_settle_middleware`].
#[derive(Clone)]
pub struct MppSettleState {
    pub mpp: Option<Arc<MppService>>,
}

/// A payment that was seen on chain and awaits its claim. Inserted by
/// [`mpp_auth_middleware`], consumed by [`mpp_settle_middleware`].
#[derive(Clone)]
struct PendingMppPayment(PaidCredential);

impl std::fmt::Debug for PendingMppPayment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingMppPayment { .. }")
    }
}

// ---------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------

pub(super) fn header_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value).ok()
}

pub(super) fn with_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) fn problem_response(status: StatusCode, content: String) -> Response {
    let mut response = Response::new(Body::from(content));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    with_no_store(response)
}

/// 503 for "could not take payment now". Not a 402, so a payment client does not read it as
/// a request to pay. Every one is logged by its closed `kind`.
pub(super) fn retry_response(kind: &'static str, retry_after_secs: u64) -> Response {
    tracing::warn!(outcome = kind, retry_after_secs, "MPP answered 503");
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(ErrorResponse::new(
            "Payment processing is temporarily unavailable. Retry shortly.".to_string(),
            "service_unavailable".to_string(),
        )),
    )
        .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
    with_no_store(response)
}

pub(super) fn limit_response(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(ErrorResponse::new(message, "validation_error".to_string())),
    )
        .into_response()
}

/// `402 Payment Required` with the given fresh challenge.
pub(super) fn challenge_response(
    challenge: Result<services::mpp::Challenge, MppError>,
    problem: ProblemCode,
    detail: &str,
) -> Response {
    let challenge = match challenge {
        Ok(challenge) => challenge,
        // The body stopped pricing between verify and now (it cannot, the digest binds it);
        // never answer 402 without a challenge.
        Err(MppError::Limit(limit)) => return limit_response(limit.message()),
        Err(error) => return retry_response(error.kind(), retry_secs(error)),
    };
    let Some(header) = challenge.to_header().ok().as_deref().and_then(header_value) else {
        return retry_response("challenge_header", 5);
    };
    let mut response = problem_response(StatusCode::PAYMENT_REQUIRED, problem.body(402, detail));
    response.headers_mut().insert(WWW_AUTHENTICATE, header);
    response
}

/// `Retry-After` seconds of an error that answers with a retry (a default for any other).
fn retry_secs(error: MppError) -> u64 {
    match error.response() {
        MppResponse::Retry(after) => after.as_secs().max(1),
        MppResponse::Challenge(_) | MppResponse::Problem(..) | MppResponse::Limit(_) => 5,
    }
}

/// Answer an [`MppError`]: the one match over [`MppResponse`]. `challenge` builds the fresh
/// 402 challenge for the layer's request.
pub(super) fn error_response(
    error: MppError,
    challenge: impl FnOnce() -> Option<Result<services::mpp::Challenge, MppError>>,
) -> Response {
    tracing::debug!(outcome = error.kind(), "MPP payment refused");
    match error.response() {
        MppResponse::Challenge(problem) => match challenge() {
            Some(challenge) => challenge_response(challenge, problem, error.detail()),
            // Header-only requests have no inference price from which to mint an offer.
            None => problem_response(StatusCode::BAD_REQUEST, problem.body(400, error.detail())),
        },
        MppResponse::Problem(status, problem) => {
            let http = match status {
                ProblemStatus::BadRequest => StatusCode::BAD_REQUEST,
                ProblemStatus::Forbidden => StatusCode::FORBIDDEN,
                ProblemStatus::Gone => StatusCode::GONE,
            };
            problem_response(http, problem.body(status.code(), error.detail()))
        }
        MppResponse::Limit(limit) => limit_response(limit.message()),
        MppResponse::Retry(after) => retry_response(error.kind(), after.as_secs().max(1)),
    }
}

// ---------------------------------------------------------------------------------------
// Auth wrapper
// ---------------------------------------------------------------------------------------

/// Today's API-key authentication, unchanged.
async fn delegate(state: AuthState, request: Request, next: Next) -> Response {
    match auth_middleware_with_workspace_context(State(state), request, next).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// What the `Authorization` header says, for requests MPP may take.
enum Presented {
    Nothing,
    Payment(String),
}

/// `None` for a Bearer key, another scheme, several headers, or bytes that are not text:
/// those are not MPP requests.
fn presented(request: &Request) -> Option<Presented> {
    let mut values = request.headers().get_all(AUTHORIZATION).iter();
    match (values.next(), values.next()) {
        (None, _) => Some(Presented::Nothing),
        (Some(value), None) => match value.to_str() {
            Ok(text) if is_payment_scheme(text) => Some(Presented::Payment(text.to_string())),
            _ => None,
        },
        (Some(_), Some(_)) => None,
    }
}

/// A copy of the request for the handler's pre-flight: the buffered body, the content type,
/// and the extensions (which carry the route's body limit, `DefaultBodyLimit`, to the
/// extractor). The credential header is not copied.
fn preflight_probe(request: &Request, body: &Bytes) -> Request {
    let mut probe = Request::new(Body::from(body.clone()));
    *probe.method_mut() = request.method().clone();
    *probe.uri_mut() = request.uri().clone();
    if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
        probe
            .headers_mut()
            .insert(CONTENT_TYPE, content_type.clone());
    }
    *probe.extensions_mut() = request.extensions().clone();
    probe
}

/// The handler's own pre-flight on the buffered body, through the handler's own extractor
/// type and validation function (`ChatRequestBody`, `reject_invalid_chat_request`), so the
/// two cannot drift. On failure: the handler's own error response, so a request that could
/// never be served gets neither a challenge nor a payment.
async fn handler_preflight(probe: Request) -> Result<(), Response> {
    let OpenAiJson(parsed) = ChatRequestBody::from_request(probe, &())
        .await
        .map_err(IntoResponse::into_response)?;
    match reject_invalid_chat_request(&parsed) {
        Some(response) => Err(response),
        None => Ok(()),
    }
}

/// Replaces `auth_middleware_with_workspace_context` on `POST /chat/completions`.
pub async fn mpp_auth_middleware(
    State(state): State<MppAuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let MppAuthState { auth, mpp, session } = state;
    let Some(mpp) = mpp else {
        return delegate(auth, request, next).await;
    };
    // The router's layers also run for methods the route does not serve; only the POST
    // inference request may be offered a challenge, or a client could pay for a 405.
    if request.method() != Method::POST {
        return delegate(auth, request, next).await;
    }
    let Some(presented) = presented(&request) else {
        return delegate(auth, request, next).await;
    };
    let parsed = match &presented {
        Presented::Payment(header) => {
            Some(services::mpp::protocol::parse_any_authorization(header))
        }
        Presented::Nothing => None,
    };
    if let Some(Ok(services::mpp::protocol::ParsedAuthorization::Session(credential))) = &parsed {
        use services::mpp::session_wire::SessionPayload;
        match SessionPayload::parse(&credential.payload) {
            Ok(
                SessionPayload::Open { .. }
                | SessionPayload::TopUp { .. }
                | SessionPayload::Close { .. },
            ) => {
                let Some(service) = session.as_deref() else {
                    return error_response(
                        MppError::InvalidChallenge("session_unavailable"),
                        || None,
                    );
                };
                return super::mpp_session::manage(service, credential).await;
            }
            Ok(SessionPayload::Voucher { .. }) => {}
            Err(error) => return error_response(MppError::Malformed(error.reason()), || None),
        }
    }
    let is_session = matches!(
        &parsed,
        Some(Ok(services::mpp::protocol::ParsedAuthorization::Session(_)))
    );
    let Some(body) = request
        .extensions()
        .get::<RequestBodyHash>()
        .map(|hash| hash.body_bytes.clone())
    else {
        return if is_session {
            error_response(MppError::Malformed("body"), || None)
        } else {
            delegate(auth, request, next).await
        };
    };

    // Charge credentials retain the pre-session fallback for bodies without a price.
    let price = match mpp.price_body(&body) {
        Pricing::Unpriced => {
            return if is_session {
                error_response(MppError::Malformed("body"), || None)
            } else {
                delegate(auth, request, next).await
            };
        }
        Pricing::Violation(limit) => {
            tracing::debug!(outcome = limit.code(), "MPP request limit violated");
            return limit_response(limit.message());
        }
        Pricing::Priced(price) => price,
    };
    // A body the handler would refuse is not worth a challenge, and must never be paid for.
    if let Err(response) = handler_preflight(preflight_probe(&request, &body)).await {
        return response;
    }
    let issue = || Some(mpp.challenge(CHAT_COMPLETIONS_ROUTE, &body, &price));

    let header = match presented {
        Presented::Payment(header) => header,
        Presented::Nothing => {
            return super::mpp_session::offers(
                &mpp,
                session.as_deref(),
                &body,
                &price,
                ProblemCode::PaymentRequired,
                "Payment is required to access this resource.",
            )
            .await;
        }
    };

    match parsed {
        Some(Ok(services::mpp::protocol::ParsedAuthorization::Session(credential))) => {
            let Some(session) = session.filter(|_| price.token_price.is_some()) else {
                return error_response(MppError::InvalidChallenge("session_unavailable"), issue);
            };
            return super::mpp_session::serve(
                session, &mpp, &auth, credential, price, &body, request, next,
            )
            .await;
        }
        Some(Err(error)) if !price.is_chargeable() => {
            if let Some(session) = session.as_deref() {
                return super::mpp_session::error_response(session, &price, error.into()).await;
            }
        }
        // Preserve the charge verifier and its error mapping, including malformed input.
        Some(Ok(services::mpp::protocol::ParsedAuthorization::Charge(_))) | Some(Err(_)) | None => {
        }
    }

    let verified = match mpp.verify(&header, CHAT_COMPLETIONS_ROUTE, &body) {
        Ok(verified) => verified,
        Err(error) => return error_response(error, issue),
    };
    // The chain work happens here, before the service key is touched: a credential that was
    // not paid must not consume the shared service key's rate limit. Nothing is claimed.
    let paid = match mpp.verify_payment(&verified).await {
        Ok(paid) => paid,
        Err(error) => return error_response(error, issue),
    };

    // MPP traffic runs and is metered as the configured service key, through the function the
    // Bearer path uses, so workspace/organization activity checks are the same.
    let service_key = mpp.service_api_key();
    let authenticated = match authenticate_api_key_with_context(&auth, service_key).await {
        Ok(authenticated) => authenticated,
        Err((status, _)) => {
            tracing::error!(
                status = status.as_u16(),
                "MPP service API key was not accepted; check MPP_SERVICE_API_KEY"
            );
            return retry_response("service_key_rejected", SERVICE_ORG_RETRY_AFTER_SECS);
        }
    };
    request.extensions_mut().insert(authenticated);
    request.extensions_mut().insert(PendingMppPayment(paid));

    let caller = caller_key(Some(service_key));
    let response = with_caller_key(caller, next.run(request)).await;

    // Our own 402s always carry a challenge. One that does not is a layer below answering
    // for the service workspace (the usage check: no credits) or a failed inference. Neither
    // is a payment request, and a payment client must not read it as one; the payment is
    // either unclaimed (refused before the settle layer) or released (the handler failed).
    if response.status() == StatusCode::PAYMENT_REQUIRED
        && !response.headers().contains_key(WWW_AUTHENTICATE)
    {
        return retry_response("service_org_blocked", SERVICE_ORG_RETRY_AFTER_SECS);
    }
    response
}

// ---------------------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------------------

/// Innermost layer of the chat-completions router: claims the payment and serves.
pub async fn mpp_settle_middleware(
    State(state): State<MppSettleState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(PendingMppPayment(paid)) = request.extensions().get::<PendingMppPayment>().cloned()
    else {
        return next.run(request).await;
    };
    // A pending payment exists only when the auth wrapper held a service.
    let Some(mpp) = state.mpp else {
        return retry_response("mpp_disabled", 5);
    };
    let body = request
        .extensions()
        .get::<RequestBodyHash>()
        .map(|hash| hash.body_bytes.clone())
        .unwrap_or_default();

    let settled = match mpp.claim(&paid).await {
        Ok(settled) => settled,
        Err(error) => {
            return error_response(error, || {
                Some(mpp.challenge_for_body(CHAT_COMPLETIONS_ROUTE, &body))
            })
        }
    };
    let receipt = settled
        .receipt_header(Utc::now())
        .ok()
        .as_deref()
        .and_then(header_value);
    // Dropped (client gone before the handler finished) counts as served, so a payment cannot
    // be replayed by disconnecting. Only an error response below releases the claim.
    let guard = mpp.serving(settled);

    let mut response = next.run(request).await;

    // The payment bought a response only if it was served: a receipt on an error would
    // certify a payment whose claim was just released for a retry.
    if !response.status().is_success() {
        guard.finish(ServeOutcome::Unserved).await;
        return response;
    }
    guard.finish(ServeOutcome::Served).await;

    let headers = response.headers_mut();
    if let Some(receipt) = receipt {
        headers.insert(PAYMENT_RECEIPT, receipt);
    }
    let existing = headers
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(value) = header_value(&with_private_cache_control(existing.as_deref())) {
        headers.insert(CACHE_CONTROL, value);
    }
    response
}

#[cfg(test)]
#[path = "mpp_tests.rs"]
mod tests;
