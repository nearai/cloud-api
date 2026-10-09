//! Provider request/response, timeout, refresh and context-window tests.

use super::*;

// ------------------------------------------------------- review-fix coverage

fn provider_with_timeout(e: &Env, secs: i64) -> Provider {
    let cfg = Config::new("tk_test_key".into(), secs);
    Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
}

async fn rotate_to_unattested_cert(e: &Env) {
    *e.server.acceptor.lock().unwrap() = acceptor(&e.pki.leaf_b, &e.pki.key_b);
}

#[tokio::test]
async fn connect_failure_returns_503_promptly_while_the_reverify_stalls() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    rotate_to_unattested_cert(&e).await;
    e.server.atc_stall.store(true, Ordering::SeqCst);

    let started = std::time::Instant::now();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        e.provider.chat_completion(params(false, None), "h".into()),
    )
    .await
    .expect("a connect failure must not wait for the re-verify");
    let msg = expect_http(r, 503);
    assert!(msg.contains("connect_failed"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));

    // The detached verify reached the (stalled) ATC exactly once; a second
    // failure while it is pending neither waits nor starts another.
    wait_for(|| e.server.hits("/attestation") == 2).await;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        e.provider.chat_completion(params(false, None), "h".into()),
    )
    .await
    .expect("second failure must not wait either");
    expect_http(r, 503);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(e.server.hits("/attestation"), 2, "no second verify");
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1,
        "the stalled verify never reached the verifier"
    );
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn second_connect_failure_within_cooldown_does_not_reverify() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let g = e.session.generation();
    e.session.on_connect_failure(g);
    wait_for(|| {
        e.verifier.router_calls.load(Ordering::SeqCst) == 2 && !e.session.reverify_pending()
    })
    .await;
    // A fresh generation, nothing pending: only the cooldown can stop this one.
    e.session.on_connect_failure(e.session.generation());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!e.session.reverify_pending());
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
    // A stale generation is deduped against the verify that already ran.
    e.session.on_connect_failure(g);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_timeout_maps_to_503_timeout() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    e.server.chat_delay_ms.store(3000, Ordering::SeqCst);
    let p = provider_with_timeout(&e, 1);
    let started = std::time::Instant::now();
    let msg = expect_http(
        p.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("timeout"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_millis(2500));
    // A timeout is not a connection failure: no re-verification.
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stalled_error_body_is_bounded_by_the_request_timeout() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 500,
        content_type: "application/json",
        body: vec![b'x'; 100],
    };
    e.server.chat_stall_body.store(true, Ordering::SeqCst);
    let p = provider_with_timeout(&e, 1);
    let started = std::time::Instant::now();
    let msg = expect_http(
        p.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("upstream_error"), "{msg}");
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

#[tokio::test]
async fn oversized_error_body_is_truncated() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let big = super::super::session::MAX_DOC_BYTES + 4096;
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 500,
        content_type: "text/plain",
        body: vec![b'x'; big],
    };
    let snap = e.session.snapshot();
    let t = snap.as_ref().as_ref().unwrap().transport.clone();
    let resp = t
        .client
        .post(format!("{}/v1/chat/completions", t.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    let text = super::super::read_capped_text(resp).await;
    assert_eq!(text.len(), super::super::session::MAX_DOC_BYTES);
    // The same cap through the provider: a 4xx surfaces, bounded.
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 400,
        content_type: "text/plain",
        body: vec![b'y'; big],
    };
    match e
        .provider
        .chat_completion(params(false, None), "h".into())
        .await
    {
        Err(CompletionError::HttpError {
            status_code: 400,
            message,
            ..
        }) => assert!(message.len() <= super::super::session::MAX_DOC_BYTES + 256),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn oversized_models_document_is_rejected_without_failing_the_verify() {
    let e = env().await;
    *e.server.models_body.lock().unwrap() =
        Some(vec![b' '; super::super::session::MAX_DOC_BYTES + 1]);
    e.session.verify_now().await.unwrap();
    assert!(e.session.model_status(SLUG).is_ok());
    assert_eq!(e.session.published_context_window(SLUG), None);
}

#[tokio::test]
async fn client_e2ee_pubkey_is_rejected_before_any_upstream_request() {
    use crate::attested::nearai::encryption_headers as eh;
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let mk = |stream| {
        let mut p = params(stream, None);
        p.extra
            .insert(eh::CLIENT_PUB_KEY.to_string(), serde_json::json!("abcd"));
        p
    };
    for result in [
        e.provider
            .chat_completion(mk(false), "h".into())
            .await
            .map(|_| ()),
        e.provider
            .chat_completion_stream(mk(true), "h".into())
            .await
            .map(|_| ()),
    ] {
        match result {
            Err(CompletionError::CompletionError(m)) => assert!(m.contains("E2EE"), "{m}"),
            other => panic!("expected an E2EE rejection, got {other:?}"),
        }
    }
    assert_eq!(e.server.hits("/v1/chat/completions"), 0);
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn continuous_usage_stats_alone_counts_as_asking_for_usage() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 200,
        content_type: "text/event-stream",
        body: fixture("chat_stream.sse"),
    };
    let mut p = params(true, None);
    p.stream_options =
        Some(serde_json::from_value(serde_json::json!({"continuous_usage_stats": true})).unwrap());
    let s = e
        .provider
        .chat_completion_stream(p, "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    let usage_chunks = evs
        .iter()
        .filter_map(client_json)
        .filter(|v| v.get("usage").is_some())
        .count();
    assert_eq!(usage_chunks, 1, "the final usage chunk reaches the client");
}

#[tokio::test]
async fn proxy_reread_fetch_failure_escalates_to_full_verify_and_fails_closed() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    rotate_to_unattested_cert(&e).await;
    e.session.reread_proxy().await;
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        2,
        "the failed re-read escalated to a full verification"
    );
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(e.session.model_status(SLUG).is_err());
    assert_eq!(
        e.session.last_verify_error(),
        Some(TinfoilVerifyError::Fetch)
    );
}

#[tokio::test]
async fn reread_proxy_when_unverified_runs_full_verify() {
    let e = env().await;
    assert!(e.session.model_status(SLUG).is_err());
    e.session.reread_proxy().await;
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    assert!(e.session.model_status(SLUG).is_ok());
}

#[tokio::test]
async fn declared_ctx_above_the_published_window_fails_closed() {
    let e = env().await;
    // Registered while unverified: the provider exists, the check applies later.
    let cfg = Config::new("tk_test_key".into(), 5);
    let over = Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(200_000);
    let fits = Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(131_072);
    let msg = expect_http(
        over.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("not_verified"), "{msg}");

    e.session.verify_now().await.unwrap();
    for _ in 0..2 {
        let msg = expect_http(
            over.chat_completion(params(false, None), "h".into()).await,
            503,
        );
        assert!(msg.contains("ctx_exceeds_published"), "{msg}");
    }
    expect_http(
        over.chat_completion_stream(params(true, None), "h".into())
            .await,
        503,
    );
    assert!(
        e.server.last_chat.lock().unwrap().is_none(),
        "no request left the gateway for the oversized declaration"
    );
    fits.chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    assert!(e.server.last_chat.lock().unwrap().is_some());
}

#[tokio::test]
async fn failed_models_fetch_on_reverify_keeps_the_last_published_window() {
    let e = env().await;
    let cfg = Config::new("tk_test_key".into(), 5);
    let over = Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(200_000);
    e.session.verify_now().await.unwrap();
    let published = e.session.published_context_window(SLUG).unwrap();
    assert!(published < 200_000);

    // `/v1/models` becomes unavailable (unparseable) during the next full verify.
    *e.server.models_body.lock().unwrap() = Some(b"not json".to_vec());
    e.session.verify_now().await.unwrap();

    assert_eq!(e.session.published_context_window(SLUG), Some(published));
    let msg = expect_http(
        over.chat_completion(params(false, None), "h".into()).await,
        503,
    );
    assert!(msg.contains("ctx_exceeds_published"), "{msg}");
    assert!(e.server.last_chat.lock().unwrap().is_none());

    // A successful fetch still replaces the windows, dropping unlisted models.
    *e.server.models_body.lock().unwrap() = Some(br#"{"data":[]}"#.to_vec());
    e.session.verify_now().await.unwrap();
    assert_eq!(e.session.published_context_window(SLUG), None);
}

#[tokio::test]
async fn unknown_published_window_leaves_the_declared_ctx_standing() {
    let e = env().await;
    *e.server.models_body.lock().unwrap() = Some(b"{}".to_vec());
    e.session.verify_now().await.unwrap();
    let cfg = Config::new("tk_test_key".into(), 5);
    Provider::new(e.session.clone(), &cfg, SLUG.into(), CANON.into())
        .with_declared_ctx(200_000)
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn spawn_refresh_rereads_proxy_then_reverifies_and_stops_when_dropped() {
    use std::time::Duration;
    let Env {
        session,
        verifier,
        server,
        provider,
        pki: _pki,
    } = env().await;
    session.verify_now().await.unwrap();
    // Pause only after the real-network setup: auto-advance during a live TLS
    // handshake would trip the fetch timeouts.
    tokio::time::pause();
    let proxy_path = "/.well-known/tinfoil-proxy";
    let proxy0 = server.hits(proxy_path);
    assert_eq!(verifier.router_calls.load(Ordering::SeqCst), 1);

    let rt = tokio::runtime::Handle::current();
    let before = rt.metrics().num_alive_tasks();
    session.spawn_refresh();
    let with_task = rt.metrics().num_alive_tasks();
    assert_eq!(with_task, before + 1);
    session.spawn_refresh();
    assert_eq!(
        rt.metrics().num_alive_tasks(),
        with_task,
        "a second spawn_refresh adds no task"
    );

    // Let the task create its intervals (at the paused "now") before advancing.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // PROXY_REREAD: the model document is re-read, the router is not re-verified.
    tokio::time::advance(super::super::PROXY_REREAD + Duration::from_secs(1)).await;
    paused_wait(|| server.hits(proxy_path) > proxy0).await;
    assert_eq!(verifier.router_calls.load(Ordering::SeqCst), 1);

    // ROUTER_REVERIFY: a full verification runs.
    tokio::time::advance(super::super::ROUTER_REVERIFY).await;
    paused_wait(|| verifier.router_calls.load(Ordering::SeqCst) >= 2).await;

    // Dropping the last strong reference ends the task.
    drop(provider);
    drop(session);
    paused_wait(|| rt.metrics().num_alive_tasks() < with_task).await;
}
