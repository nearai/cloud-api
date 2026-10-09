//! Session verification, pinning and attested-domain tests.

use super::*;

#[tokio::test]
async fn session_starts_blocked_and_unverified_calls_are_503() {
    let e = env().await;
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    let msg = expect_http(
        e.provider
            .chat_completion_stream(params(true, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 0);
    assert!(e.server.last_chat.lock().unwrap().is_none());
}

#[tokio::test]
async fn becomes_pinned_only_after_verify_then_serves_nonstream() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    match e.session.fingerprint_state() {
        FingerprintState::Pinned(s) => assert_eq!(s.len(), 1),
        other => panic!("{other:?}"),
    }
    assert!(e.session.model_status(SLUG).is_ok());
    assert!(e.session.model_status("nope").is_err());

    let r = e
        .provider
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    assert_eq!(r.response.model, CANON);
    assert_eq!(r.serving.source, crate::ProviderSource::Tinfoil);
    assert_eq!(r.serving.tier, crate::ProviderTier::Attested3p);
    let v: serde_json::Value = serde_json::from_slice(&r.raw_bytes).unwrap();
    assert!(v["choices"][0]["message"]["reasoning_content"].is_string());

    let (head, body) = e.server.last_chat.lock().unwrap().clone().unwrap();
    assert!(head
        .to_ascii_lowercase()
        .contains("authorization: bearer tk_test_key"));
    let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(b["model"], SLUG);
    assert_eq!(b["stream"], false);
    assert!(
        b.get("x_org_id").is_none(),
        "internal keys never reach Tinfoil"
    );
}

#[tokio::test]
async fn stream_request_always_asks_upstream_for_usage_and_gates_for_client() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    *e.server.chat.lock().unwrap() = ChatReply {
        status: 200,
        content_type: "text/event-stream",
        body: fixture("chat_stream.sse"),
    };
    // Client did not ask for usage.
    let s = e
        .provider
        .chat_completion_stream(params(true, None), "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    assert!(evs
        .iter()
        .all(|ev| !String::from_utf8_lossy(&ev.raw_bytes).contains("usage")));
    assert!(evs
        .iter()
        .any(|ev| matches!(&ev.chunk, Some(crate::StreamChunk::Chat(c)) if c.usage.is_some())));
    let (_, body) = e.server.last_chat.lock().unwrap().clone().unwrap();
    let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(b["stream_options"]["include_usage"], true);
    assert_eq!(b["model"], SLUG);

    // Client asked for usage.
    let s = e
        .provider
        .chat_completion_stream(params(true, Some(true)), "h".into())
        .await
        .unwrap();
    let evs: Vec<_> = s.map(|x| x.unwrap()).collect().await;
    assert!(evs
        .iter()
        .any(|ev| String::from_utf8_lossy(&ev.raw_bytes).contains("\"usage\"")));
}

#[tokio::test]
async fn upstream_statuses_map_per_spec() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let set = |status: u16| {
        *e.server.chat.lock().unwrap() = ChatReply {
            status,
            content_type: "application/json",
            body: br#"{"error":{"message":"nope"}}"#.to_vec(),
        };
    };
    for s in [500u16, 502, 503] {
        set(s);
        expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
    }
    assert_eq!(e.session.upstream_auth_failures(), 0);
    for (n, s) in [401u16, 402, 403].into_iter().enumerate() {
        set(s);
        let msg = expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
        assert!(!msg.contains("nope"), "upstream auth error must not leak");
        assert_eq!(e.session.upstream_auth_failures(), n as u64 + 1);
    }
    set(429);
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        429,
    );
    for s in [404u16, 408, 425] {
        set(s);
        expect_http(
            e.provider
                .chat_completion(params(false, None), "h".into())
                .await,
            503,
        );
    }
    assert_eq!(e.session.upstream_auth_failures(), 3, "not auth failures");
    set(400);
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        400,
    );
    set(422);
    expect_http(
        e.provider
            .chat_completion_stream(params(true, None), "h".into())
            .await,
        422,
    );
    assert_eq!(e.session.upstream_auth_failures(), 3);
}

#[tokio::test]
async fn spki_mismatch_reverifies_once_then_503() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 1);
    // The server rotates to a certificate (B) the attested SPKI (A) does not cover.
    *e.server.acceptor.lock().unwrap() = acceptor(&e.pki.leaf_b, &e.pki.key_b);

    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    // The re-verify runs detached from the request.
    wait_for(|| e.verifier.router_calls.load(Ordering::SeqCst) == 2).await;
    wait_for(|| matches!(e.session.fingerprint_state(), FingerprintState::Blocked)).await;
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        2,
        "exactly one re-verify"
    );
    assert!(
        e.server.last_chat.lock().unwrap().is_none(),
        "no request ever reached the unpinned peer"
    );
    // Fail closed: verification could not complete, so later calls stay unavailable
    // without hammering the verifier.
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(e.session.model_status(SLUG).is_err());
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("not_verified"));
    assert_eq!(e.verifier.router_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn verify_failure_fails_closed() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    // Verifier now reports an SPKI that is not what the server presents; proxy fetch fails.
    *e.verifier.spki.lock().unwrap() = [7u8; 32];
    assert_eq!(e.session.verify_now().await, Err(TinfoilVerifyError::Fetch));
    expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
}

#[tokio::test]
async fn proxy_reread_picks_up_pins_miss() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert!(e.session.model_status(SLUG).is_ok());
    e.verifier
        .deny_models
        .lock()
        .unwrap()
        .push(SLUG.to_string());
    e.session.reread_proxy().await;
    assert_eq!(
        e.session.model_status(SLUG).unwrap_err(),
        TinfoilVerifyError::UnknownModelMeasurement
    );
    let msg = expect_http(
        e.provider
            .chat_completion(params(false, None), "h".into())
            .await,
        503,
    );
    assert!(msg.contains("unknown_model_measurement"));
    assert_eq!(
        e.verifier.router_calls.load(Ordering::SeqCst),
        1,
        "reread does not re-verify the router"
    );
}

#[tokio::test]
async fn published_context_window_reads_models() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    assert_eq!(
        e.session.published_context_window("gpt-oss-120b"),
        Some(131072)
    );
    assert_eq!(e.session.published_context_window("missing"), None);
}

#[tokio::test]
async fn attestation_report_payload_shape() {
    let e = env().await;
    // No network: install a verified state directly.
    let bundle: AtcBundle = serde_json::from_slice(&atc_json("inference.tinfoil.sh")).unwrap();
    let doc: ProxyDoc = serde_json::from_value(proxy_doc()).unwrap();
    let entry = doc.models.get(SLUG).unwrap();
    let mut models = BTreeMap::new();
    models.insert(
        SLUG.to_string(),
        Ok(PinnedModel {
            slug: SLUG.into(),
            entry: entry.clone(),
        }),
    );
    e.session.install_state(VerifiedState {
        router: VerifiedRouter {
            spki_sha256: [1; 32],
            measurement_hex: "ab".repeat(48),
            tag: "r@v1".into(),
            tcb: RouterTcb {
                bootloader: 10,
                tee: 0,
                snp: 23,
                microcode: 84,
            },
        },
        models,
        verified_at: Instant::now(),
        context_windows: BTreeMap::new(),
        bundle,
        transport: Arc::new(
            e.session
                .build_transport("inference.tinfoil.sh", &"01".repeat(32))
                .unwrap(),
        ),
    });
    let m = e
        .provider
        .get_attestation_report(CANON.into(), None, Some("ff".into()), None, false)
        .await
        .unwrap();
    assert_eq!(m["provider"], "tinfoil");
    assert_eq!(m["trust"], "router_attested");
    assert_eq!(m["verified"], true);
    assert_eq!(m["model"], CANON);
    assert_eq!(m["router"]["format"], "sev-snp-guest/v2");
    assert_eq!(m["router"]["measurement"], "ab".repeat(48));
    assert_eq!(m["router"]["tag"], "r@v1");
    assert_eq!(
        m["router"]["tcb"],
        serde_json::json!({"bootloader": 10, "tee": 0, "snp": 23, "microcode": 84})
    );
    assert_eq!(m["router"]["report_b64"], "cmVwb3J0");
    assert_eq!(m["model_entry"]["slug"], SLUG);
    assert_eq!(
        m["model_entry"]["registers"],
        serde_json::json!(["aa", "bb"])
    );
    assert_eq!(m["model_entry"]["replicas"][0]["host"], "h1.example");
    assert!(
        m.get("nonce").is_none(),
        "client nonce is not bound into the Tinfoil report"
    );
}

#[tokio::test]
async fn attestation_report_unverified_is_error() {
    let e = env().await;
    assert!(e
        .provider
        .get_attestation_report(CANON.into(), None, None, None, false)
        .await
        .is_err());
}

#[tokio::test]
async fn trait_surface() {
    let e = env().await;
    let p = &e.provider;
    assert_eq!(p.tier(), crate::ProviderTier::Attested3p);
    assert_eq!(p.provider_source(), crate::ProviderSource::Tinfoil);
    assert!(!p.supports_chat_signatures());
    assert!(!p.supports_client_e2ee());
    assert!(!p.supports_per_request_pubkey_routing("x"));
    assert!(p.supports_streaming());
    let m = p.models().await.unwrap();
    assert_eq!(m.data[0].id, CANON);
}

// ------------------------------------------------------------ attested domain

#[test]
fn router_domain_syntax() {
    use super::validate_router_domain as ok;
    assert!(ok("inference.tinfoil.sh").is_ok());
    assert!(ok("router-0.tinfoil.sh").is_ok());
    assert!(ok("a.b.tinfoil.sh").is_ok());
    for bad in [
        "https://inference.tinfoil.sh",
        "inference.tinfoil.sh:443",
        "inference.tinfoil.sh/x",
        "user@inference.tinfoil.sh",
        "inference.example.com",
        "tinfoil.sh",
        ".tinfoil.sh",
        "..tinfoil.sh",
        "Router-0.tinfoil.sh",
        "xn--rter-pta.tinfoil.sh",
        "a.xn--b.tinfoil.sh",
        "a..tinfoil.sh",
        "a.tinfoil.sh.",
        "inference.tinfoil.sh.",
        "r\u{00f6}uter.tinfoil.sh",
        "-x.tinfoil.sh",
        "x.tinfoil.sh.evil.com",
        "",
    ] {
        assert_eq!(
            ok(bad),
            Err(TinfoilVerifyError::Malformed {
                stage: "router_domain"
            }),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn bundle_domain_selects_the_request_host() {
    let pki = test_pki();
    let server = start_server(&pki).await;
    *server.atc_domain.lock().unwrap() = "router-0.tinfoil.sh".to_string();
    let cfg = Config::new("tk_test_key".into(), 5)
        .with_route(server.addr, &format!("https://{}/attestation", server.addr));
    let verifier =
        StubVerifier::new(&compute_spki_fingerprint_from_der(pki.leaf_a.as_ref()).unwrap());
    let session =
        TinfoilRouterSession::new_with_roots(cfg.clone(), verifier, pki.roots.clone()).unwrap();
    session.verify_now().await.unwrap();
    let provider = Provider::new(session.clone(), &cfg, SLUG.into(), CANON.into());
    provider
        .chat_completion(params(false, None), "h".into())
        .await
        .unwrap();
    let head = server.last_chat.lock().unwrap().clone().unwrap().0;
    assert!(
        head.to_ascii_lowercase()
            .contains("host: router-0.tinfoil.sh"),
        "{head}"
    );
    assert!(session.published_context_window(SLUG).is_some());
}

#[tokio::test]
async fn invalid_bundle_domain_is_rejected_and_closed() {
    let e = env().await;
    for bad in [
        "https://inference.tinfoil.sh",
        "inference.tinfoil.sh:443",
        "inference.example.com",
        "tinfoil.sh",
        "Inference.tinfoil.sh",
    ] {
        *e.server.atc_domain.lock().unwrap() = bad.to_string();
        assert_eq!(
            e.session.verify_now().await,
            Err(TinfoilVerifyError::Malformed {
                stage: "router_domain"
            }),
            "{bad}"
        );
        assert!(e.session.model_status(SLUG).is_err());
        assert!(matches!(
            e.session.fingerprint_state(),
            FingerprintState::Blocked
        ));
    }
}

#[tokio::test]
async fn failed_proxy_fetch_does_not_publish_the_new_pin() {
    let e = env().await;
    e.session.verify_now().await.unwrap();
    let before = e.session.fingerprint_state();
    // The new key is not what the server presents: the proxy fetch over the
    // candidate client fails, and the (closed) session never adopts the new pin.
    *e.verifier.spki.lock().unwrap() = [7u8; 32];
    assert_eq!(e.session.verify_now().await, Err(TinfoilVerifyError::Fetch));
    assert!(matches!(
        e.session.fingerprint_state(),
        FingerprintState::Blocked
    ));
    assert!(matches!(before, FingerprintState::Pinned(_)));
}
