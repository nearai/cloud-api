//! Machine Payments Protocol on `POST /v1/chat/completions`.
//!
//! Two middlewares on the text inference group, active only when `MPP_ENABLED=true`:
//!
//! 1. [`mpp_auth_middleware`] replaces `auth_middleware_with_workspace_context`. It hands the
//!    request to that function unless MPP applies: the path is `/v1/chat/completions` and either
//!    the `Authorization` header is `Payment …` or there is none and the body prices.
//!    - No header, priced model: 402 with a fresh challenge. Limits broken: 400.
//!    - `Payment` credential: stateless verification, then the request runs as the configured
//!      service API key with a [`PendingMppPayment`] attached.
//! 2. [`mpp_settle_middleware`], the innermost layer, runs after the usage check and just before
//!    the handler. Only here is the payment verified on chain and claimed, so a request that was
//!    refused earlier never consumes a payment.
//!
//! With MPP disabled both middlewares hold `None` and change nothing.
//!
//! Nothing here logs a credential, header value, transaction hash, payer address, request body
//! or the service key. Log fields are closed outcome codes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{OriginalUri, Request, State},
    http::{
        header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE},
        HeaderName, HeaderValue, Method, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use services::mpp::{
    protocol::{is_payment_scheme, with_private_cache_control},
    MppError, MppService, Pricing, ServeOutcome, VerifiedCredential, CHAT_COMPLETIONS_ROUTE,
};

use super::auth::{
    auth_middleware_with_workspace_context, authenticate_api_key_with_context, caller_key,
    with_caller_key, AuthState,
};
use super::body_hash::RequestBodyHash;
use crate::models::ErrorResponse;

const PAYMENT_RECEIPT: HeaderName = HeaderName::from_static("payment-receipt");
const PROBLEM_JSON: &str = "application/problem+json";
/// `Retry-After` when the service workspace cannot pay for inference (its credits or limits).
const SERVICE_ORG_RETRY_AFTER_SECS: u64 = 30;
/// `Retry-After` when the Tempo RPC or the claim store did not answer.
const BACKEND_RETRY_AFTER_SECS: u64 = 5;

/// State of [`mpp_auth_middleware`].
#[derive(Clone)]
pub struct MppAuthState {
    pub auth: AuthState,
    pub mpp: Option<Arc<MppService>>,
}

/// State of [`mpp_settle_middleware`].
#[derive(Clone)]
pub struct MppSettleState {
    pub mpp: Option<Arc<MppService>>,
}

/// A credential that passed stateless verification and awaits settlement. Inserted by
/// [`mpp_auth_middleware`], consumed by [`mpp_settle_middleware`].
#[derive(Clone)]
pub struct PendingMppPayment {
    verified: VerifiedCredential,
    settle_attempted: Arc<AtomicBool>,
}

impl PendingMppPayment {
    fn new(verified: VerifiedCredential) -> Self {
        Self {
            verified,
            settle_attempted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl std::fmt::Debug for PendingMppPayment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingMppPayment { .. }")
    }
}

/// Build the service when MPP is enabled in the config. A construction failure disables MPP
/// (the safe state: no payment is accepted) and is logged; the settings were already validated
/// at startup, so this is not expected to happen.
pub fn build_mpp_service(
    pool: database::DbPool,
    config: &config::ApiConfig,
) -> Option<Arc<MppService>> {
    let settings = config.mpp.settings()?;
    let repository = Arc::new(database::repositories::MppPaymentClaimRepository::new(pool));
    match MppService::from_settings(settings, repository) {
        Ok(service) => {
            tracing::info!(
                realm = %settings.realm,
                chain_id = settings.tempo.chain_id,
                priced_models = settings.model_prices.len(),
                "MPP payments enabled"
            );
            Some(Arc::new(service))
        }
        Err(error) => {
            tracing::error!(
                outcome = error.kind(),
                "MPP enabled but the service could not be built; MPP is off"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------

fn header_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value).ok()
}

fn with_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn problem_response(status: StatusCode, content: String) -> Response {
    let mut response = Response::new(Body::from(content));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    with_no_store(response)
}

/// 503 for "could not take payment now". Not a 402, so a payment client does not read it as a
/// request to pay.
fn unavailable_response(retry_after_secs: u64) -> Response {
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

fn limit_response(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(ErrorResponse::new(message, "validation_error".to_string())),
    )
        .into_response()
}

/// `402 Payment Required` with a fresh challenge for `body`.
fn challenge_response(
    mpp: &MppService,
    body: &[u8],
    problem: services::mpp::ProblemCode,
    detail: &str,
) -> Response {
    let challenge = match mpp.challenge_for_body(CHAT_COMPLETIONS_ROUTE, body) {
        Ok(challenge) => challenge,
        // The body stopped pricing between verify and now (it cannot, the digest binds it);
        // never answer 402 without a challenge.
        Err(MppError::Limit(limit)) => return limit_response(limit.message()),
        Err(error) => {
            tracing::warn!(outcome = error.kind(), "MPP challenge could not be built");
            return unavailable_response(BACKEND_RETRY_AFTER_SECS);
        }
    };
    let Some(header) = challenge.to_header().ok().as_deref().and_then(header_value) else {
        tracing::warn!(
            outcome = "challenge_header",
            "MPP challenge could not be rendered"
        );
        return unavailable_response(BACKEND_RETRY_AFTER_SECS);
    };
    let mut response = problem_response(StatusCode::PAYMENT_REQUIRED, problem.body(402, detail));
    response.headers_mut().insert(WWW_AUTHENTICATE, header);
    response
}

/// Answer a verification or settlement error: 402 with a fresh challenge, 403 for a consumed
/// payment, 400 for an unsupported method, 503 when a dependency did not answer.
fn error_response(mpp: &MppService, body: &[u8], error: MppError) -> Response {
    tracing::debug!(
        outcome = error.kind(),
        status = error.status(),
        "MPP payment refused"
    );
    match (error.status(), error.problem()) {
        (402, Some(problem)) => challenge_response(mpp, body, problem, error.detail()),
        (status @ (400 | 403), Some(problem)) => problem_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN),
            problem.body(status, error.detail()),
        ),
        (_, _) => {
            if matches!(error, MppError::Unavailable(_)) {
                tracing::warn!(outcome = error.kind(), "MPP backend unavailable");
            }
            unavailable_response(BACKEND_RETRY_AFTER_SECS)
        }
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

/// `OriginalUri` carries the full path; nested routers strip their prefix from the request URI.
fn is_chat_completions(request: &Request) -> bool {
    if request.method() != Method::POST {
        return false;
    }
    let path = request
        .extensions()
        .get::<OriginalUri>()
        .map(|uri| uri.0.path())
        .unwrap_or_else(|| request.uri().path());
    path == CHAT_COMPLETIONS_ROUTE
}

/// What the `Authorization` header says, for requests MPP may take.
enum Presented {
    Nothing,
    Payment(String),
    /// A Bearer key, another scheme, several headers, or bytes that are not text.
    Other,
}

fn presented(request: &Request) -> Presented {
    let mut values = request.headers().get_all(AUTHORIZATION).iter();
    match (values.next(), values.next()) {
        (None, _) => Presented::Nothing,
        (Some(value), None) => match value.to_str() {
            Ok(text) if is_payment_scheme(text) => Presented::Payment(text.to_string()),
            _ => Presented::Other,
        },
        (Some(_), Some(_)) => Presented::Other,
    }
}

/// Replaces `auth_middleware_with_workspace_context` on the text inference group.
pub async fn mpp_auth_middleware(
    State(state): State<MppAuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let MppAuthState { auth, mpp } = state;
    let Some(mpp) = mpp else {
        return delegate(auth, request, next).await;
    };
    if !is_chat_completions(&request) {
        return delegate(auth, request, next).await;
    }
    let presented = presented(&request);
    if matches!(presented, Presented::Other) {
        return delegate(auth, request, next).await;
    }
    let Some(body) = request
        .extensions()
        .get::<RequestBodyHash>()
        .map(|hash| hash.body_bytes.clone())
    else {
        return delegate(auth, request, next).await;
    };

    // Unparseable or unpriced requests are not MPP requests: today's 401 applies.
    let pricing = mpp.price_body(&body);
    match pricing {
        Pricing::Unpriced => return delegate(auth, request, next).await,
        Pricing::Violation(limit) => {
            tracing::debug!(outcome = limit.code(), "MPP request limit violated");
            return limit_response(limit.message());
        }
        Pricing::Priced(_) => {}
    }
    let header = match presented {
        Presented::Payment(header) => header,
        Presented::Nothing => {
            return challenge_response(
                &mpp,
                &body,
                services::mpp::ProblemCode::PaymentRequired,
                "Payment is required to access this resource.",
            );
        }
        Presented::Other => return delegate(auth, request, next).await,
    };

    let verified = match mpp.verify(&header, CHAT_COMPLETIONS_ROUTE, &body) {
        Ok(verified) => verified,
        Err(error) => return error_response(&mpp, &body, error),
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
            return unavailable_response(SERVICE_ORG_RETRY_AFTER_SECS);
        }
    };
    let pending = PendingMppPayment::new(verified);
    request.extensions_mut().insert(authenticated);
    request.extensions_mut().insert(pending.clone());

    let caller = caller_key(Some(service_key));
    let response = with_caller_key(caller, next.run(request)).await;

    // The usage check of the service workspace answers 402 for "no credits". That is not a
    // payment request: the payment was never taken, and a payment client must not read it as
    // one. Our own 402s always carry a challenge.
    if response.status() == StatusCode::PAYMENT_REQUIRED
        && !response.headers().contains_key(WWW_AUTHENTICATE)
        && !pending.settle_attempted.load(Ordering::SeqCst)
    {
        tracing::warn!(
            outcome = "service_org_blocked",
            "MPP service workspace cannot serve"
        );
        return unavailable_response(SERVICE_ORG_RETRY_AFTER_SECS);
    }
    response
}

// ---------------------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------------------

/// Innermost layer of the text inference group.
pub async fn mpp_settle_middleware(
    State(state): State<MppSettleState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(pending) = request.extensions().get::<PendingMppPayment>().cloned() else {
        return next.run(request).await;
    };
    // A pending payment exists only when the auth wrapper held a service.
    let Some(mpp) = state.mpp else {
        return unavailable_response(BACKEND_RETRY_AFTER_SECS);
    };
    pending.settle_attempted.store(true, Ordering::SeqCst);
    let body = request
        .extensions()
        .get::<RequestBodyHash>()
        .map(|hash| hash.body_bytes.clone())
        .unwrap_or_default();

    let settled = match mpp.settle(&pending.verified).await {
        Ok(settled) => settled,
        Err(error) => return error_response(&mpp, &body, error),
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

    let outcome = if response.status().is_success() {
        ServeOutcome::Served
    } else {
        ServeOutcome::Unserved
    };
    guard.finish(outcome).await;

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
