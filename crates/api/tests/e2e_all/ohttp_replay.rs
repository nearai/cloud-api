use crate::common::*;
use api::ohttp_gateway::OhttpGateway;
use ohttp::KeyConfig;

#[tokio::test]
async fn ohttp_claims_are_shared_atomic_and_survive_gateway_recreation() {
    let database = setup_test_server_with_database().await.1;
    let seed = *uuid::Uuid::new_v4().as_bytes();
    let mut material = [0u8; 32];
    material[..16].copy_from_slice(&seed);
    let gateway = OhttpGateway::new(&material)
        .unwrap()
        .with_replay_pool(database.pool().clone());
    let peer = OhttpGateway::new(&material)
        .unwrap()
        .with_replay_pool(database.pool().clone());
    let mut config = KeyConfig::decode(gateway.config_bytes()).unwrap();
    let (request, _) = ohttp::ClientRequest::from_config(&mut config)
        .unwrap()
        .encapsulate(b"test request")
        .unwrap();
    gateway.decapsulate(&request).unwrap();
    let (first, second) = tokio::join!(
        gateway.claim_request(&request),
        peer.claim_request(&request)
    );
    assert_ne!(
        first.unwrap(),
        second.unwrap(),
        "only one instance admits the encapsulation"
    );
    let restarted = OhttpGateway::new(&material)
        .unwrap()
        .with_replay_pool(database.pool().clone());
    assert!(!restarted.claim_request(&request).await.unwrap());
    let mut altered_tail = request.clone();
    altered_tail.push(0);
    assert!(
        !restarted.claim_request(&altered_tail).await.unwrap(),
        "the replay key is independent of body framing"
    );
    let (fresh, _) = ohttp::ClientRequest::from_config(&mut config)
        .unwrap()
        .encapsulate(b"test request")
        .unwrap();
    assert!(
        peer.claim_request(&fresh).await.unwrap(),
        "fresh encapsulation is a separate request"
    );
}

#[tokio::test]
async fn ohttp_duplicate_is_rejected_before_a_second_inference() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (server, router, _, _, database) =
        setup_test_server_with_pool_config_and_router(|config| {
            config.server.ohttp_enabled = true;
            config.server.port = port;
        })
        .await;
    let serving = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    let config_response = server.get("/v1/ohttp/config").await;
    assert_eq!(config_response.status_code(), 200);
    let mut config = KeyConfig::decode(config_response.as_bytes()).unwrap();
    let mut inner = bhttp::Message::request(
        b"POST".to_vec(),
        b"https".to_vec(),
        b"example.test".to_vec(),
        b"/v1/chat/completions".to_vec(),
    );
    inner.put_header(
        b"authorization".to_vec(),
        format!("Bearer {key}").into_bytes(),
    );
    inner.put_header(b"content-type".to_vec(), b"application/json".to_vec());
    inner.write_content(serde_json::to_vec(&serde_json::json!({"model":E2E_QWEN_MODEL_NAME,"messages":[{"role":"user","content":"replay test"}]})).unwrap());
    let mut bhttp = Vec::new();
    inner
        .write_bhttp(bhttp::Mode::KnownLength, &mut bhttp)
        .unwrap();
    let (encapsulated, response_context) = ohttp::ClientRequest::from_config(&mut config)
        .unwrap()
        .encapsulate(&bhttp)
        .unwrap();
    let first = server
        .post("/ohttp")
        .add_header("content-type", "message/ohttp-req")
        .bytes(encapsulated.clone().into())
        .await;
    assert_eq!(first.status_code(), 200, "{}", first.text());
    let plaintext = response_context.decapsulate(first.as_bytes()).unwrap();
    let decoded = bhttp::Message::read_bhttp(&mut std::io::Cursor::new(plaintext)).unwrap();
    assert_eq!(decoded.control().status().map(u16::from), Some(200));
    let duplicate = server
        .post("/ohttp")
        .add_header("content-type", "message/ohttp-req")
        .bytes(encapsulated.into())
        .await;
    assert_eq!(duplicate.status_code(), 409);
    use futures_util::AsyncWriteExt;
    let mut chunked_bytes = Vec::new();
    {
        let client = ohttp::ClientRequest::from_config(&mut config).unwrap();
        let mut writer = client.encapsulate_stream(&mut chunked_bytes).unwrap();
        writer.write_all(&bhttp).await.unwrap();
        writer.close().await.unwrap();
    }
    let chunked = server
        .post("/ohttp")
        .add_header("content-type", "message/ohttp-chunked-req")
        .bytes(chunked_bytes.clone().into())
        .await;
    assert_eq!(chunked.status_code(), 200);
    let duplicate = server
        .post("/ohttp")
        .add_header("content-type", "message/ohttp-chunked-req")
        .bytes(chunked_bytes.into())
        .await;
    assert_eq!(duplicate.status_code(), 409);
    let mut sdk_request = bhttp::Message::request(
        b"GET".to_vec(),
        b"https".to_vec(),
        b"example.test".to_vec(),
        b"/v1/models".to_vec(),
    );
    sdk_request.put_header(b"x-api-key".to_vec(), key.as_bytes().to_vec());
    let mut sdk_bhttp = Vec::new();
    sdk_request
        .write_bhttp(bhttp::Mode::KnownLength, &mut sdk_bhttp)
        .unwrap();
    let (sdk_enc, _) = ohttp::ClientRequest::from_config(&mut config)
        .unwrap()
        .encapsulate(&sdk_bhttp)
        .unwrap();
    for expected in [200, 409] {
        let result = server
            .post("/ohttp")
            .add_header("content-type", "message/ohttp-req")
            .bytes(sdk_enc.clone().into())
            .await;
        assert_eq!(
            result.status_code(),
            expected,
            "SDK credential form participates in replay protection"
        );
    }
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let count: i64 = database
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT COUNT(*) FROM organization_usage_log WHERE organization_id = $1",
            &[&org_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 2);
    serving.abort();
}
