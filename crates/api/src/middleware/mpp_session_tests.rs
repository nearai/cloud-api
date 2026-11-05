// INTEGRATION: register as a child of middleware::mpp::tests (mpp_tests.rs).
//! HTTP admission tests for the MPP session intent.

use super::*;
use async_trait::async_trait;
use futures::Stream;
use services::completions::usage_observer::{UsageObserver, UsageObserverExt, UsageReport};
use services::mpp::abi::{self, ChannelState};
use services::mpp::channel_memory::InMemoryChannelRepository;
use services::mpp::eip712::{close_authorization_digest, voucher_digest, MAX_UINT96};
use services::mpp::ports::{ChannelRepository, NewChannel, RpcReceipt};
use services::mpp::secp256k1::sign_digest;
use services::mpp::session_wire::{descriptor_json, hex_prefixed};
use services::mpp::signer::LocalSigner;
use services::mpp::tempo_tx::{decode_signed, tx_hash};
use services::mpp::{SessionService, SessionServiceConfig};

const SESSION_PRICE: u128 = 2;
const INITIAL: u128 = 1_250_000;

fn vectors() -> Value {
    serde_json::from_str(include_str!(
        "../../../services/src/mpp/testdata/session_vectors.json"
    ))
    .unwrap()
}

fn vector_text(path: &str) -> String {
    let data = vectors();
    path.split('.')
        .fold(&data, |value, key| &value[key])
        .as_str()
        .unwrap()
        .to_string()
}

fn vector_bytes<const N: usize>(path: &str) -> [u8; N] {
    hex::decode(vector_text(path).trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

fn address_word(address: &[u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(address);
    word
}

fn uint_word(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn descriptor() -> abi::ChannelDescriptor {
    services::mpp::session_wire::parse_descriptor(&vectors()["descriptor"]).unwrap()
}

fn voucher_signature(amount: u128) -> [u8; 65] {
    let digest = voucher_digest(
        &vector_bytes("escrow"),
        42431,
        &vector_bytes("channelId"),
        amount,
    )
    .unwrap();
    let key = k256::SecretKey::from_slice(&vector_bytes::<32>("payerKey")).unwrap();
    sign_digest(&key, &digest).unwrap()
}

fn voucher(amount: u128) -> Value {
    json!({
        "action": "voucher",
        "channelId": vector_text("channelId"),
        "descriptor": descriptor_json(&descriptor()),
        "cumulativeAmount": amount.to_string(),
        "signature": hex_prefixed(&voucher_signature(amount)),
    })
}

fn open_payload() -> Value {
    let mut payload = voucher(INITIAL);
    payload["action"] = json!("open");
    payload["type"] = json!("transaction");
    payload["transaction"] = json!(vector_text("open.serialized"));
    payload
}

fn close_payload() -> Value {
    let mut payload = voucher(INITIAL);
    payload["action"] = json!("close");
    let digest = close_authorization_digest(
        &vector_bytes("escrow"),
        42431,
        &vector_bytes("channelId"),
        INITIAL,
    )
    .unwrap();
    let key = k256::SecretKey::from_slice(&vector_bytes::<32>("payerKey")).unwrap();
    payload["closeSignature"] = json!(hex_prefixed(&sign_digest(&key, &digest).unwrap()));
    payload
}

fn top_up_payload() -> (Value, Vec<u8>) {
    let mut input = abi::TOP_UP_SELECTOR.to_vec();
    let d = descriptor();
    for address in [d.payer, d.payee, d.operator, d.token] {
        input.extend(address_word(&address));
    }
    input.extend(d.salt);
    input.extend(address_word(&d.authorized_signer));
    input.extend(d.expiring_nonce_hash);
    input.extend(uint_word(200));
    let opening = hex::decode(vector_text("open.serialized").trim_start_matches("0x")).unwrap();
    let (mut tx, _) = decode_signed(&opening).unwrap();
    tx.calls[0].input = input;
    let key = k256::SecretKey::from_slice(&vector_bytes::<32>("payerKey")).unwrap();
    let signature = sign_digest(&key, &tx.signing_hash()).unwrap();
    let raw = tx.serialize_signed(&signature);
    (
        json!({
            "action": "topUp", "type": "transaction",
            "channelId": vector_text("channelId"),
            "descriptor": descriptor_json(&d),
            "additionalDeposit": "200",
            "transaction": hex_prefixed(&raw),
        }),
        raw,
    )
}

#[derive(Clone, Copy)]
enum SessionHandlerMode {
    Report,
    Fail,
    Missing,
    Stream,
    Hang,
}

struct SessionBody {
    observer: Arc<dyn UsageObserver>,
    request_id: Uuid,
    emitted: bool,
    finished: bool,
}

impl SessionBody {
    fn new(observer: Arc<dyn UsageObserver>) -> Self {
        Self {
            observer,
            request_id: Uuid::new_v4(),
            emitted: false,
            finished: false,
        }
    }
}

impl Stream for SessionBody {
    type Item = Result<bytes::Bytes, std::convert::Infallible>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let body = self.get_mut();
        if body.finished {
            return std::task::Poll::Ready(None);
        }
        if !body.emitted {
            body.emitted = true;
            return std::task::Poll::Ready(Some(Ok(bytes::Bytes::from_static(b"data: one\n\n"))));
        }
        body.finished = true;
        body.observer.on_finished(UsageReport {
            request_id: body.request_id,
            prompt_tokens: 7,
            completion_tokens: 5,
            completed: true,
        });
        std::task::Poll::Ready(None)
    }
}

impl Drop for SessionBody {
    fn drop(&mut self) {
        if !self.finished {
            self.observer.on_finished(UsageReport {
                request_id: self.request_id,
                prompt_tokens: 0,
                completion_tokens: 0,
                completed: false,
            });
        }
    }
}

#[derive(Clone)]
struct SessionHandlerState {
    calls: Arc<AtomicUsize>,
    mode: Arc<Mutex<SessionHandlerMode>>,
}

async fn session_handler(State(state): State<SessionHandlerState>, request: Request) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let observer = request
        .extensions()
        .get::<UsageObserverExt>()
        .map(|value| value.0.clone());
    let mode = *state.mode.lock().unwrap();
    match mode {
        SessionHandlerMode::Report => {
            let observer = observer.expect("voucher carries usage observer");
            let report = UsageReport {
                request_id: Uuid::new_v4(),
                prompt_tokens: 7,
                completion_tokens: 5,
                completed: true,
            };
            observer.on_finished(report);
            observer.on_finished(report); // A duplicate callback must not double charge.
            axum::Json(json!({"status":"ok"})).into_response()
        }
        SessionHandlerMode::Fail => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        SessionHandlerMode::Missing => axum::Json(json!({"status":"ok"})).into_response(),
        SessionHandlerMode::Stream => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(SessionBody::new(
                observer.expect("voucher carries usage observer"),
            )))
            .unwrap(),
        SessionHandlerMode::Hang => std::future::pending().await,
    }
}

async fn session_inner_block(
    State(blocked): State<Arc<AtomicBool>>,
    request: Request,
    next: Next,
) -> Response {
    if blocked.load(Ordering::SeqCst) {
        StatusCode::TOO_MANY_REQUESTS.into_response()
    } else {
        next.run(request).await
    }
}

/// The payee close transaction contains a wall-clock nonce, so its hash is known
/// only after signing. Return a receipt for those exact bytes.
struct DynamicReceiptRpc(Arc<FakeTempo>);

#[async_trait]
impl services::mpp::TempoRpc for DynamicReceiptRpc {
    async fn send_raw_transaction_sync(&self, raw: &[u8]) -> Result<RpcReceipt, RpcError> {
        let (tx, signature) = decode_signed(raw).map_err(|_| RpcError::InvalidResponse)?;
        let sender = tx
            .recover_sender(&signature)
            .map_err(|_| RpcError::InvalidResponse)?;
        let receipt = RpcReceipt {
            tx_hash: tx_hash(raw),
            block_number: 1,
            success: true,
            from: sender,
            logs: vec![],
        };
        self.0.mine(receipt.clone());
        Ok(receipt)
    }

    async fn get_transaction_receipt(
        &self,
        hash: [u8; 32],
    ) -> Result<Option<RpcReceipt>, RpcError> {
        services::mpp::TempoRpc::get_transaction_receipt(self.0.as_ref(), hash).await
    }

    async fn get_block_timestamp(&self, number: u64) -> Result<Option<u64>, RpcError> {
        services::mpp::TempoRpc::get_block_timestamp(self.0.as_ref(), number).await
    }

    async fn call(&self, to: [u8; 20], data: &[u8]) -> Result<Vec<u8>, RpcError> {
        services::mpp::TempoRpc::call(self.0.as_ref(), to, data).await
    }

    async fn gas_price(&self) -> Result<u128, RpcError> {
        services::mpp::TempoRpc::gas_price(self.0.as_ref()).await
    }
}

struct SessionApp {
    router: Router,
    session: Arc<SessionService>,
    ledger: Arc<InMemoryChannelRepository>,
    rpc: Arc<FakeTempo>,
    state: SessionHandlerState,
    blocked: Arc<AtomicBool>,
    price: services::mpp::Price,
    auth_calls: Arc<AtomicUsize>,
}

fn session_app(service_key: &str, amount: u128, token_price: Option<u128>) -> SessionApp {
    let ledger = Arc::new(InMemoryChannelRepository::new());
    session_app_with_repository(service_key, amount, token_price, ledger.clone(), ledger)
}

fn session_app_with_repository(
    service_key: &str,
    amount: u128,
    token_price: Option<u128>,
    ledger: Arc<InMemoryChannelRepository>,
    repository: Arc<dyn ChannelRepository>,
) -> SessionApp {
    let mut config = settings(service_key);
    config.tempo.currency = vector_text("token");
    config.tempo.recipient = vector_text("payee");
    config.model_prices.get_mut(MODEL).unwrap().amount = amount;
    config.model_prices.get_mut(MODEL).unwrap().token_price = token_price;
    if token_price == Some(MAX_UINT96) {
        config.model_prices.get_mut(MODEL).unwrap().max_tokens = u32::MAX;
    }
    let rpc = Arc::new(FakeTempo::default());
    let mpp = Arc::new(
        MppService::new(
            &config,
            Arc::new(InMemoryClaimRepository::default()),
            rpc.clone(),
        )
        .unwrap(),
    );
    let signer = Arc::new(LocalSigner::from_hex(&vector_text("payeeKey")).unwrap());
    let session = Arc::new(
        SessionService::new(
            SessionServiceConfig {
                settings: config::MppSessionSettings {
                    signer_key: None,
                    min_voucher_delta: Some(100),
                    suggested_deposit: 5_000_000,
                    settle_interval_secs: 3600,
                    settle_amount: 5_000_000,
                    close_supervision_secs: 60,
                },
                model_prices: config.model_prices.clone(),
                realm: config.realm.clone(),
                secret_key: config.secret_key.as_bytes().to_vec(),
                challenge_ttl: Duration::from_secs(config.challenge_ttl_secs),
                chain_id: config.tempo.chain_id,
                escrow: vector_bytes("escrow"),
                currency: vector_bytes("token"),
            },
            repository,
            Arc::new(DynamicReceiptRpc(rpc.clone())),
            signer,
        )
        .unwrap(),
    );
    let state = SessionHandlerState {
        calls: Arc::new(AtomicUsize::new(0)),
        mode: Arc::new(Mutex::new(SessionHandlerMode::Report)),
    };
    let blocked = Arc::new(AtomicBool::new(false));
    let auth_calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/chat/completions", post(session_handler))
        .with_state(state.clone())
        .layer(from_fn_with_state(blocked.clone(), session_inner_block))
        .layer(from_fn_with_state(
            MppAuthState {
                auth: auth_state_with_calls(Some(auth_calls.clone())),
                mpp: Some(mpp),
                session: Some(session.clone()),
            },
            mpp_auth_middleware,
        ))
        .layer(from_fn_with_state(
            BodyHashLimit(1 << 20),
            body_hash_middleware,
        ));
    SessionApp {
        router: Router::new().nest("/v1", router),
        session,
        ledger,
        rpc,
        state,
        blocked,
        auth_calls,
        price: services::mpp::Price {
            amount,
            token_price,
        },
    }
}

fn body() -> String {
    json!({"model": MODEL, "max_tokens": 8, "messages": [{"role":"user","content":"hi"}]})
        .to_string()
}

async fn post_session(app: &SessionApp, credential: Option<&str>) -> Response {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = credential {
        request = request.header(header::AUTHORIZATION, value);
    }
    app.router
        .clone()
        .oneshot(request.body(Body::from(body())).unwrap())
        .await
        .unwrap()
}

async fn session_credential(app: &SessionApp, payload: Value) -> String {
    let challenge = app
        .session
        .challenge(CHAT_COMPLETIONS_ROUTE, &app.price, None)
        .await
        .unwrap();
    let header = credential_header(&challenge, payload, None);
    let mut envelope: Value = serde_json::from_slice(
        &protocol::b64url_decode(header.strip_prefix("Payment ").unwrap()).unwrap(),
    )
    .unwrap();
    envelope["challenge"]["intent"] = json!("session");
    envelope["challenge"]
        .as_object_mut()
        .unwrap()
        .remove("digest");
    format!(
        "Payment {}",
        protocol::b64url_encode(envelope.to_string().as_bytes())
    )
}

async fn seed(app: &SessionApp) {
    app.ledger
        .open_or_refresh(NewChannel {
            channel_id: vector_bytes("channelId"),
            chain_id: 42431,
            escrow: vector_bytes("escrow"),
            descriptor: descriptor(),
            deposit: 5_000_000,
            settled_on_chain: 0,
            highest_cumulative: INITIAL,
            highest_signature: voucher_signature(INITIAL),
        })
        .await
        .unwrap();
    set_chain(app, 5_000_000);
}

fn set_chain(app: &SessionApp, deposit: u128) {
    app.rpc.set_call_handler(Box::new(move |to, input| {
        assert_eq!(to, vector_bytes("escrow"));
        assert_eq!(
            input,
            abi::get_channel_state_calldata(&vector_bytes("channelId")).as_slice()
        );
        let state = ChannelState {
            deposit,
            settled: 0,
            close_requested_at: 0,
        };
        Ok([
            uint_word(state.settled),
            uint_word(state.deposit),
            uint_word(u128::from(state.close_requested_at)),
        ]
        .concat())
    }));
}

async fn row(app: &SessionApp) -> services::mpp::ChannelRecord {
    app.ledger
        .get(&vector_bytes("channelId"))
        .await
        .unwrap()
        .unwrap()
}

fn receipt_value(response: &Response) -> Value {
    let raw = response
        .headers()
        .get("payment-receipt")
        .unwrap()
        .to_str()
        .unwrap();
    serde_json::from_slice(&protocol::b64url_decode(raw).unwrap()).unwrap()
}

#[tokio::test]
async fn management_open_and_top_up_bypass_service_key_and_handler() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    let raw = hex::decode(vector_text("open.serialized").trim_start_matches("0x")).unwrap();
    app.rpc.mine(RpcReceipt {
        tx_hash: tx_hash(&raw),
        block_number: 1,
        success: true,
        from: vector_bytes("payer"),
        logs: vec![],
    });
    set_chain(&app, 5_000_000);
    let response = post_session(&app, Some(&session_credential(&app, open_payload()).await)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(receipt_value(&response)["acceptedCumulative"], "1250000");
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);

    let (payload, raw) = top_up_payload();
    app.rpc.mine(RpcReceipt {
        tx_hash: tx_hash(&raw),
        block_number: 2,
        success: true,
        from: vector_bytes("payer"),
        logs: vec![],
    });
    set_chain(&app, 5_000_200);
    let response = post_session(&app, Some(&session_credential(&app, payload).await)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(row(&app).await.deposit, 5_000_200);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn close_is_handled_before_service_key_or_handler() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    app.rpc.set_gas_price(Err(RpcError::Unavailable));
    let response = post_session(&app, Some(&session_credential(&app, close_payload()).await)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);
}

#[tokio::test]
async fn successful_close_returns_receipt_without_entering_inference() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    app.blocked.store(true, Ordering::SeqCst);
    let response = post_session(&app, Some(&session_credential(&app, close_payload()).await)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let receipt = receipt_value(&response);
    assert_eq!(receipt["intent"], "session");
    assert!(receipt["txHash"]
        .as_str()
        .is_some_and(|value| value.starts_with("0x")));
    assert_eq!(
        row(&app).await.state,
        services::mpp::ChannelLifecycle::Finalized
    );
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);
}

#[tokio::test]
async fn dual_offers_are_separate_headers_and_charge_only_rejects_session() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    let response = post_session(&app, None).await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let offers: Vec<_> = response
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .collect();
    assert_eq!(offers.len(), 2);
    assert!(offers
        .iter()
        .any(|h| h.to_str().unwrap().contains("intent=\"charge\"")));
    assert!(offers
        .iter()
        .any(|h| h.to_str().unwrap().contains("intent=\"session\"")));

    let charge_only = session_app(SERVICE_KEY, AMOUNT, None);
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let response = post_session(&charge_only, Some(&credential)).await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        response
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .count(),
        1
    );
    assert!(response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("intent=\"charge\""));
    assert_eq!(charge_only.state.calls.load(Ordering::SeqCst), 0);
    let problem: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        problem["type"],
        "https://paymentauth.org/problems/invalid-challenge"
    );
}

#[tokio::test]
async fn charge_only_wrapper_preserves_bearer_authentication() {
    let charge_only = app(true);
    let disabled = app(false);
    for bearer in ["Bearer sk-wrong", "Bearer sk-live-service-key-0001"] {
        let current = charge_only.post(&super::body(), Some(bearer)).await;
        let previous = disabled.post(&super::body(), Some(bearer)).await;
        assert_eq!(current.status, previous.status, "{bearer}");
        assert_eq!(current.json(), previous.json(), "{bearer}");
    }
}

#[tokio::test]
async fn malformed_session_only_payment_still_gets_a_session_challenge() {
    let app = session_app(SERVICE_KEY, 0, Some(SESSION_PRICE));
    let response = post_session(&app, Some("Payment invalid")).await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert!(response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("intent=\"session\""));
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);
}

#[tokio::test]
async fn voucher_reports_usage_once_and_returns_final_receipt() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(receipt_value(&response)["spent"], "24");
    assert_eq!(receipt_value(&response)["units"], 12);
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!((row(&app).await.spent, row(&app).await.units), (24, 12));
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_handler_and_inner_rejection_release_reservations() {
    for blocked in [false, true] {
        let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
        seed(&app).await;
        *app.state.mode.lock().unwrap() = SessionHandlerMode::Fail;
        app.blocked.store(blocked, Ordering::SeqCst);
        let response = post_session(
            &app,
            Some(&session_credential(&app, voucher(INITIAL)).await),
        )
        .await;
        assert_eq!(
            response.status(),
            if blocked {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        );
        assert_eq!(app.ledger.reservation_count(), 0);
        assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
        assert_eq!(
            app.state.calls.load(Ordering::SeqCst),
            usize::from(!blocked)
        );
    }
}

#[tokio::test]
async fn short_voucher_returns_snapshot_without_reservation() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL + 1)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        response
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .count(),
        1
    );
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    let header = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    let request = header
        .split("request=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let terms: Value = serde_json::from_slice(&protocol::b64url_decode(request).unwrap()).unwrap();
    assert_eq!(
        terms["methodDetails"]["sessionSnapshot"]["requiredCumulative"],
        (INITIAL + 100).to_string()
    );
}

#[tokio::test]
async fn stored_voucher_with_insufficient_balance_returns_required_snapshot() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    let request_id = Uuid::new_v4();
    assert!(matches!(
        app.ledger
            .reserve(&vector_bytes("channelId"), request_id, INITIAL)
            .await
            .unwrap(),
        services::mpp::ReserveOutcome::Reserved(_)
    ));
    assert!(matches!(
        app.ledger.charge(request_id, INITIAL - 1, 0).await.unwrap(),
        services::mpp::ChargeOutcome::Charged { charged, .. } if charged == INITIAL - 1
    ));

    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let header = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    let request = header
        .split("request=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let terms: Value = serde_json::from_slice(&protocol::b64url_decode(request).unwrap()).unwrap();
    let expected = INITIAL - 1 + SESSION_PRICE * (body().len().div_ceil(4) as u128 + 8);
    assert_eq!(
        terms["methodDetails"]["sessionSnapshot"]["requiredCumulative"],
        expected.to_string()
    );
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!(
        (row(&app).await.spent, row(&app).await.units),
        (INITIAL - 1, 0)
    );
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn closed_channel_returns_gone_without_reservation() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    app.ledger
        .finalize(&vector_bytes("channelId"), 0)
        .await
        .unwrap();
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn absent_usage_releases_and_stream_uses_provisional_receipt() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Missing;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("payment-receipt").is_none());
    assert_eq!(app.ledger.reservation_count(), 0);

    *app.state.mode.lock().unwrap() = SessionHandlerMode::Stream;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let provisional_spent: u128 = receipt_value(&response)["spent"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        provisional_spent > 24,
        "the stream receipt shows the held cost cap"
    );
    assert_eq!(app.ledger.reservation_count(), 1);
    assert_eq!(
        (row(&app).await.spent, row(&app).await.units),
        (provisional_spent, 0)
    );
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"data: one\n\n");
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.ledger.reservation_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!((row(&app).await.spent, row(&app).await.units), (24, 12));
}

#[tokio::test]
async fn dropped_stream_releases_the_provisional_charge() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Stream;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app.ledger.reservation_count(), 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.ledger.reservation_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
}

#[tokio::test]
async fn rejected_service_key_releases_before_running_handler() {
    let app = session_app("sk-live-rejected", AMOUNT, Some(SESSION_PRICE));
    seed(&app).await;
    let response = post_session(
        &app,
        Some(&session_credential(&app, voucher(INITIAL)).await),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
}

#[tokio::test]
async fn overflowing_cost_cap_is_a_400_without_reservation() {
    let app = session_app(SERVICE_KEY, AMOUNT, Some(MAX_UINT96));
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let request_body = json!({
        "model": MODEL,
        "max_tokens": u32::MAX,
        "messages": [{"role": "user", "content": "hi"}],
    })
    .to_string();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, credential)
        .body(Body::from(request_body))
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.ledger.reservation_count(), 0);
    assert_eq!(app.state.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_handler_releases_reserved_funds() {
    let app = Arc::new(session_app(SERVICE_KEY, AMOUNT, Some(SESSION_PRICE)));
    seed(&app).await;
    *app.state.mode.lock().unwrap() = SessionHandlerMode::Hang;
    let credential = session_credential(&app, voucher(INITIAL)).await;
    let running = {
        let app = app.clone();
        tokio::spawn(async move { post_session(&app, Some(&credential)).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.ledger.reservation_count() == 0 || app.state.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    running.abort();
    let _ = running.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.ledger.reservation_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!((row(&app).await.spent, row(&app).await.units), (0, 0));
}

#[path = "mpp_tests/session_finalization.rs"]
mod finalization;

#[path = "mpp_tests/session_management.rs"]
mod management;
