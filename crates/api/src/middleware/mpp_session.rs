//! Session admission and completion metering. Reservations precede service-key admission.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::Request,
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE},
        HeaderName, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use services::{
    completions::usage_observer::{UsageObserver, UsageObserverExt, UsageReport},
    mpp::{
        protocol::{self, with_private_cache_control},
        session_wire::{SessionPayload, SessionReceipt},
        MppError, MppResponse, MppService, Param, Price, ProblemCode, ProblemStatus, RequestShape,
        ReservedRequest, SessionService, CHAT_COMPLETIONS_ROUTE,
    },
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{
    auth::{authenticate_api_key_with_context, caller_key, with_caller_key, AuthState},
    mpp::{
        challenge_response, header_value, limit_response, problem_response, retry_response,
        with_no_store,
    },
};

const PAYMENT_RECEIPT: HeaderName = HeaderName::from_static("payment-receipt");
/// Non-streaming usage normally arrives before the handler returns. Bound a broken seam.
const USAGE_WAIT: Duration = Duration::from_secs(1);
/// A lost stream observer must not retain an in-process reservation indefinitely.
const STREAM_WAIT: Duration = Duration::from_secs(15 * 60);

/// Offer each configured intent in a separate header field (never comma-join challenges).
pub(super) async fn offers(
    mpp: &MppService,
    session: Option<&SessionService>,
    body: &[u8],
    price: &Price,
    problem: ProblemCode,
    detail: &str,
) -> Response {
    let mut challenges = Vec::with_capacity(2);
    if price.is_chargeable() {
        challenges.push(mpp.challenge(CHAT_COMPLETIONS_ROUTE, body, price));
    }
    if let Some(service) = session.filter(|_| price.token_price.is_some()) {
        challenges.push(service.challenge(CHAT_COMPLETIONS_ROUTE, price, None).await);
    }
    let mut response = problem_response(StatusCode::PAYMENT_REQUIRED, problem.body(402, detail));
    for challenge in challenges {
        let header = match challenge
            .and_then(|challenge| challenge.to_header().map_err(|_| MppError::Config))
        {
            Ok(header) => header,
            Err(error) => return retry_response(error.kind(), 5),
        };
        let Some(header) = header_value(&header) else {
            return retry_response("challenge_header", 5);
        };
        response.headers_mut().append(WWW_AUTHENTICATE, header);
    }
    if !response.headers().contains_key(WWW_AUTHENTICATE) {
        return retry_response("session_unavailable", 5);
    }
    response
}

pub(super) async fn error_response(
    service: &SessionService,
    price: &Price,
    error: MppError,
) -> Response {
    tracing::debug!(outcome = error.kind(), "MPP session refused");
    match error.response() {
        MppResponse::Challenge(problem) => challenge_response(
            service
                .challenge(
                    CHAT_COMPLETIONS_ROUTE,
                    price,
                    error.session_snapshot().cloned(),
                )
                .await,
            problem,
            error.detail(),
        ),
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

fn receipt_response(mut response: Response, receipt: &SessionReceipt) -> Response {
    let Some(value) = receipt
        .header(Utc::now())
        .ok()
        .as_deref()
        .and_then(header_value)
    else {
        return retry_response("session_receipt_header", 5);
    };
    response.headers_mut().insert(PAYMENT_RECEIPT, value);
    let existing = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|v| v.to_str().ok());
    if let Some(value) = header_value(&with_private_cache_control(existing)) {
        response.headers_mut().insert(CACHE_CONTROL, value);
    }
    response
}

fn cost_cap(price: &Price, body: &[u8]) -> Option<u128> {
    let shape = RequestShape::from_body(body)?;
    let max_tokens = match (shape.max_tokens, shape.max_completion_tokens) {
        (Param::Value(a), Param::Value(b)) => a.max(b),
        (Param::Value(a), Param::Absent) | (Param::Absent, Param::Value(a)) => a,
        _ => return None,
    };
    let prompt = u128::try_from(body.len())
        .ok()?
        .checked_add(3)?
        .checked_div(4)?;
    price
        .token_price?
        .checked_mul(prompt.checked_add(u128::from(max_tokens))?)
}

/// A one-shot sender also rejects duplicate callbacks from an incorrect handler.
struct Observer(Mutex<Option<oneshot::Sender<UsageReport>>>);

impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionUsageObserver { .. }")
    }
}

impl UsageObserver for Observer {
    fn on_finished(&self, report: UsageReport) {
        if let Ok(mut sender) = self.0.lock() {
            if let Some(sender) = sender.take() {
                let _ = sender.send(report);
            }
        }
    }
}

/// Owns the only right to finalize a reservation, including cancellation of the handler.
struct Reservation {
    service: Arc<SessionService>,
    reserved: Option<ReservedRequest>,
}

impl Reservation {
    async fn finish(
        mut self,
        report: Option<UsageReport>,
    ) -> Result<Option<SessionReceipt>, MppError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| MppError::Unavailable(services::mpp::UnavailableReason::Store))?;
        let Some(reserved) = self.reserved.take() else {
            return Ok(None);
        };
        let usage = report.filter(|report| report.completed).and_then(|report| {
            u64::try_from(report.prompt_tokens)
                .ok()
                .zip(u64::try_from(report.completion_tokens).ok())
        });
        let service = self.service.clone();
        // Dropping this JoinHandle detaches the task. Once finalization starts, a client
        // disconnect cannot cancel a ledger write or start a competing release.
        runtime
            .spawn(async move {
                let result = if let Some((prompt, completion)) = usage {
                    service
                        .charge(&reserved, prompt, completion)
                        .await
                        .map(Some)
                } else {
                    service.release(&reserved).await.map(|()| None)
                };
                if let Err(error) = &result {
                    tracing::warn!(outcome = error.kind(), "MPP session finalization failed");
                }
                result
            })
            .await
            .map_err(|_| MppError::Unavailable(services::mpp::UnavailableReason::Store))?
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(reserved) = self.reserved.take() else {
            return;
        };
        let service = self.service.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = service.release(&reserved).await {
                    tracing::warn!(
                        outcome = error.kind(),
                        "MPP session cancellation release failed"
                    );
                }
            });
        } else {
            tracing::warn!(
                outcome = "no_runtime",
                "MPP session release deferred to supervision"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve(
    service: Arc<SessionService>,
    mpp: &MppService,
    auth: &AuthState,
    credential: protocol::SessionCredential,
    price: Price,
    body: &[u8],
    mut request: Request,
    next: Next,
) -> Response {
    let credential = match service
        .verify_credential(&credential, CHAT_COMPLETIONS_ROUTE, &price)
        .await
    {
        Ok(credential) => credential,
        Err(error) => return error_response(&service, &price, error).await,
    };
    let management = match credential.payload() {
        SessionPayload::Open { .. } => Some(service.open(&credential).await),
        SessionPayload::TopUp { .. } => Some(service.top_up(&credential).await),
        SessionPayload::Close { .. } => Some(service.close(&credential).await),
        SessionPayload::Voucher { .. } => None,
    };
    if let Some(result) = management {
        return match result {
            Ok(management) => receipt_response(
                with_no_store(axum::Json(serde_json::json!({"status":"ok"})).into_response()),
                &management.receipt,
            ),
            Err(error) => error_response(&service, &price, error).await,
        };
    }
    let Some(cap) = cost_cap(&price, body) else {
        return limit_response("Session request cost exceeds the supported limit.".into());
    };
    let request_id = request
        .extensions()
        .get::<super::request_correlation::RequestCorrelation>()
        .map_or_else(Uuid::new_v4, |correlation| correlation.request_id);
    let reserved = match service.voucher(&credential, cap, request_id).await {
        Ok(reserved) => reserved,
        Err(error) => return error_response(&service, &price, error).await,
    };
    let preview = reserved.receipt_preview.clone();
    let guard = Reservation {
        service: service.clone(),
        reserved: Some(reserved.clone()),
    };
    let authenticated = match authenticate_api_key_with_context(auth, mpp.service_api_key()).await {
        Ok(authenticated) => authenticated,
        Err(_) => {
            if let Err(error) = guard.finish(None).await {
                tracing::warn!(outcome = error.kind(), "MPP session release failed");
            }
            return retry_response("service_key_rejected", 30);
        }
    };
    let (sender, receiver) = oneshot::channel();
    // Keep a sender alive until response classification, even if the handler ignores it.
    let observer = Arc::new(Observer(Mutex::new(Some(sender))));
    request.extensions_mut().insert(authenticated);
    request.extensions_mut().insert(reserved);
    request
        .extensions_mut()
        .insert(UsageObserverExt(observer.clone()));
    let response =
        with_caller_key(caller_key(Some(mpp.service_api_key())), next.run(request)).await;
    if !response.status().is_success() {
        if let Err(error) = guard.finish(None).await {
            tracing::warn!(outcome = error.kind(), "MPP session release failed");
        }
        if response.status() == StatusCode::PAYMENT_REQUIRED
            && !response.headers().contains_key(WWW_AUTHENTICATE)
        {
            return retry_response("service_org_blocked", 30);
        }
        return response;
    }
    let streaming = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    if streaming {
        tokio::spawn(async move {
            let report = tokio::time::timeout(STREAM_WAIT, receiver)
                .await
                .ok()
                .and_then(Result::ok);
            if report.is_none() {
                tracing::warn!(
                    outcome = "stream_usage_missing",
                    "MPP session usage missing"
                );
            }
            if let Err(error) = guard.finish(report).await {
                tracing::warn!(
                    outcome = error.kind(),
                    "MPP session stream finalization failed"
                );
            }
        });
        receipt_response(response, &preview)
    } else {
        let report = tokio::time::timeout(USAGE_WAIT, receiver)
            .await
            .ok()
            .and_then(Result::ok);
        if report.is_none() {
            tracing::warn!(outcome = "usage_missing", "MPP session usage missing");
        }
        match guard.finish(report).await {
            Ok(Some(receipt)) => receipt_response(response, &receipt),
            Ok(None) => response,
            Err(error) => error_response(&service, &price, error).await,
        }
    }
}
