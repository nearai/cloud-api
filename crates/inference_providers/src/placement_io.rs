//! Valkey IO for smart placement: a background reader that turns signed
//! replica frames, host-level routed counters and follow pins into a
//! [`placement::snapshot::Snapshot`], and a bounded, fire-and-forget write
//! queue for routed counters and pins.
//!
//! Nothing here runs on the request path: the reader swaps a fresh snapshot
//! into an [`ArcSwap`] every [`READ_INTERVAL`], and [`PlacementIo::record`]
//! only `try_send`s onto a bounded channel (a full channel drops and counts).
//!
//! Fail open: an invalid endpoint or CA, a failed connect, or a failed read
//! never panics. The last snapshot is kept, ages out by `built_ms`, and the
//! placer falls back to the legacy path within `FRESH_MAX_MS`.
//!
//! Privacy: never log the password, the connection URL, a redis error's
//! `Display` (it can echo the URL), frame contents, pin ids or keys. Only
//! error kinds and counts are logged or measured.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use placement::affinity::PinTable;
use placement::consts::{HOST_REPLICA, PIN_TTL_MS};
use placement::frame::Envelope;
use placement::snapshot::{Ingest, Reject, ReplicaView, RoutedCounts, Snapshot};
use placement::KeyRegistry;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::sync::mpsc;

use crate::BackendHosts;

/// Placement Valkey endpoint (TLS, ACL user `router`). The password comes
/// from `PLACEMENT_REDIS_PASSWORD` at runtime and is never embedded here.
///
/// PLACEHOLDER until infra-aws#15 is applied. To fill it in:
/// 1. Replace `VALKEY_EIP` with the Elastic IP literal that the apply outputs
///    for the placement Valkey, e.g. `"rediss://router@203.0.113.7:6379"`.
///    Use the IP, not a DNS name: the server certificate's SAN is
///    `IP:<EIP>`, so TLS hostname verification only passes against the IP.
/// 2. Replace the contents of `placement_valkey_ca.pem` (next to this file)
///    with the private CA certificate PEM from the same apply's output (the
///    CA that signed that server certificate, not the server certificate).
///
/// While either is a placeholder, [`PlacementIo::start`] logs the error
/// kind at warn and stays inert, so every request takes the legacy path.
pub const VALKEY_ENDPOINT: &str = "rediss://router@VALKEY_EIP:6379";
/// Private CA that signs the placement Valkey's certificate. Placeholder
/// until infra-aws#15 is applied; see [`VALKEY_ENDPOINT`] to fill it in.
pub const VALKEY_CA_PEM: &str = include_str!("placement_valkey_ca.pem");

/// How often the reader builds a new snapshot.
pub const READ_INTERVAL: Duration = Duration::from_millis(500);
/// Capacity of the write queue; a full queue drops the write and counts it.
pub const WRITE_QUEUE_CAPACITY: usize = 1024;
/// TTL of each per-second routed counter hash.
pub const ROUTED_TTL_SECS: u64 = 5;
/// Stream holding follow-pin writes from every cloud-api node.
pub const PINS_STREAM: &str = "pins";
/// Approximate cap on the pins stream.
pub const PINS_MAXLEN: usize = 200_000;
/// Newest pins read on start.
pub const PINS_WARMUP_COUNT: usize = 50_000;
/// Max pins read per reader cycle.
pub const PINS_READ_COUNT: usize = 1_000;
/// Prune expired pins every this many reader cycles (~10 s).
const PRUNE_EVERY_CYCLES: u64 = 20;
/// Pins written further than this into the future (clock skew between
/// cloud-api nodes) are ignored, so a bad clock cannot pin for longer than TTL.
const PIN_MAX_FUTURE_MS: u64 = 5_000;
/// Max writes sent in one pipeline.
const WRITE_BATCH: usize = 128;
/// Connect backoff bounds.
const CONNECT_RETRY_MIN: Duration = Duration::from_secs(1);
const CONNECT_RETRY_MAX: Duration = Duration::from_secs(30);
/// Minimum gap between repeated "still failing" warnings.
const WARN_EVERY: Duration = Duration::from_secs(60);

pub const METRIC_WRITES_DROPPED: &str = "cloud_api.placement.valkey_writes_dropped";
pub const METRIC_WRITE_ERRORS: &str = "cloud_api.placement.valkey_write_errors";
pub const METRIC_READ_ERRORS: &str = "cloud_api.placement.valkey_read_errors";
pub const METRIC_FRAMES_REJECTED: &str = "cloud_api.placement.frames_rejected";
pub const METRIC_PINS_MALFORMED: &str = "cloud_api.placement.pins_malformed";
pub const METRIC_SNAPSHOT_AGE_MS: &str = "cloud_api.placement.snapshot_age_ms";
/// One per placement decision on a covered model, tagged
/// `outcome:{place|legacy}` plus `selection:{..}` (place) or `reason:{..}`
/// (legacy). Never host or request ids.
pub const METRIC_DECISIONS: &str = "cloud_api.placement.decisions";
/// Replicas excluded per decision, by eligibility rule (`rule:{..}`).
pub const METRIC_EXCLUDED: &str = "cloud_api.placement.excluded";
/// One per decision, tagged `affinity:{client|prefix|none}` and `outcome:{..}`.
pub const METRIC_AFFINITY: &str = "cloud_api.placement.affinity";
/// Prefill backlog (tokens) on the chosen host, per placed request.
pub const METRIC_CHOSEN_BACKLOG: &str = "cloud_api.placement.chosen_backlog_tokens";

/// Everything a provider's `Fleet` needs to place covered-model requests.
/// `hosts` is the same `ArcSwap` the [`PlacementIo`] reader resolves frame
/// signing keys from, so discovery's host map is shared, not copied.
#[derive(Clone)]
pub struct PlacementHandles {
    pub placer: Arc<placement::decision::Placer>,
    pub io: Arc<PlacementIo>,
    pub hosts: Arc<ArcSwap<BackendHosts>>,
}

impl PlacementHandles {
    /// Starts a [`PlacementIo`] with a fresh host map for exactly one
    /// `Fleet` (see `InferenceProvider::set_placement`): each installed Fleet
    /// gets its own hosts `ArcSwap`, written only by that Fleet. The reader
    /// stops once the Fleet (and so these handles) is dropped.
    pub fn start<M: PlacementMetrics + ?Sized + 'static>(
        password: String,
        placer: Arc<placement::decision::Placer>,
        metrics: Arc<M>,
    ) -> Self {
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let io = PlacementIo::start(password, hosts.clone(), metrics);
        Self { placer, io, hosts }
    }
}

/// The metrics this module emits. Same method shapes as
/// `services::metrics::MetricsServiceTrait` (which this crate cannot depend
/// on); `services` implements it for `dyn MetricsServiceTrait`. Tags are
/// `key:value` strings and must stay low-cardinality (no host ids).
pub trait PlacementMetrics: Send + Sync {
    fn record_count(&self, name: &str, value: i64, tags: &[&str]);
    fn record_histogram(&self, name: &str, value: f64, tags: &[&str]);
}

/// Adapts any (possibly unsized) `PlacementMetrics` behind an `Arc` into a
/// `dyn PlacementMetrics`.
struct ErasedMetrics<M: ?Sized>(Arc<M>);

impl<M: PlacementMetrics + ?Sized> PlacementMetrics for ErasedMetrics<M> {
    fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
        self.0.record_count(name, value, tags)
    }
    fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
        self.0.record_histogram(name, value, tags)
    }
}

/// `replica:{host}:{replica}`: a replica's latest signed envelope (JSON
/// string, `EX 5`), matching inference-proxy's `redis_sink::state_key`.
pub fn replica_key(host: &str, replica: &str) -> String {
    format!("replica:{host}:{replica}")
}

/// `routed:{host}:{replica}:{sec}`: per-second routed counters (`req`,
/// `tok`). Host-level placement writes `replica = "_host"`.
pub fn routed_key(host: &str, replica: &str, sec: u64) -> String {
    format!("routed:{host}:{replica}:{sec}")
}

/// A fire-and-forget write. Holds a pin id, so it intentionally has no
/// `Debug`.
pub enum Write {
    /// One routed request of `tok` tokens to `host` in unix second `sec`.
    /// `replica: None` (host-level placement) writes under `"_host"`.
    Routed {
        host: String,
        replica: Option<String>,
        tok: u64,
        sec: u64,
    },
    /// A follow pin: `id_hex` (32 hex chars) now points at `host`.
    Pin {
        id_hex: String,
        host: String,
        at_ms: u64,
    },
}

impl Write {
    /// Low-cardinality `kind:` tag for the drop counter.
    fn kind_tag(&self) -> &'static str {
        match self {
            Write::Routed { .. } => "kind:routed",
            Write::Pin { .. } => "kind:pin",
        }
    }
}

/// Appends `w`'s commands to `pipe`.
fn push_write(pipe: &mut redis::Pipeline, w: &Write) {
    match w {
        Write::Routed {
            host,
            replica,
            tok,
            sec,
        } => {
            let key = routed_key(host, replica.as_deref().unwrap_or(HOST_REPLICA), *sec);
            pipe.cmd("HINCRBY").arg(&key).arg("req").arg(1).ignore();
            pipe.cmd("HINCRBY").arg(&key).arg("tok").arg(*tok).ignore();
            pipe.cmd("EXPIRE").arg(&key).arg(ROUTED_TTL_SECS).ignore();
        }
        Write::Pin {
            id_hex,
            host,
            at_ms,
        } => {
            pipe.cmd("XADD")
                .arg(PINS_STREAM)
                .arg("MAXLEN")
                .arg("~")
                .arg(PINS_MAXLEN)
                .arg("*")
                .arg("k")
                .arg(id_hex)
                .arg("h")
                .arg(host)
                .arg("t")
                .arg(*at_ms)
                .ignore();
        }
    }
}

/// Handle to the placement Valkey reader and write queue.
pub struct PlacementIo {
    /// The latest snapshot; `Snapshot::default()` (so every placement is
    /// Legacy) until the first successful read.
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    writes: mpsc::Sender<Write>,
    metrics: Arc<dyn PlacementMetrics>,
    /// Set by the first `Fleet::set_placement`: one handle set per Fleet, so
    /// each hosts `ArcSwap` has exactly one writer.
    installed: AtomicBool,
}

impl PlacementIo {
    /// Spawns the reader and writer against [`VALKEY_ENDPOINT`] with
    /// `password`. Never panics and never blocks: if the endpoint or CA is
    /// invalid it logs the error kind and returns an inert handle whose
    /// snapshot stays empty. Must be called inside a Tokio runtime.
    ///
    /// The reader and writer tasks are detached: the writer ends once every
    /// handle (and so the channel sender) is dropped, and the reader once
    /// only it still holds `hosts`. Start one per placement-enabled provider
    /// (see [`PlacementHandles::start`]), never per request.
    pub fn start<M: PlacementMetrics + ?Sized + 'static>(
        password: String,
        hosts: Arc<ArcSwap<BackendHosts>>,
        metrics: Arc<M>,
    ) -> Arc<Self> {
        let metrics: Arc<dyn PlacementMetrics> = Arc::new(ErasedMetrics(metrics));
        let (io, rx) = Self::new(metrics.clone());
        install_crypto_provider();
        match client(VALKEY_ENDPOINT, Some(VALKEY_CA_PEM), Some(password)) {
            Ok(client) => {
                tokio::spawn(run(client, hosts, io.snapshot.clone(), rx, metrics));
            }
            Err(kind) => tracing::warn!(
                error_kind = kind,
                "Placement Valkey client not configured; placement stays on the legacy path"
            ),
        }
        Arc::new(io)
    }

    fn new(metrics: Arc<dyn PlacementMetrics>) -> (Self, mpsc::Receiver<Write>) {
        let (tx, rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
        let io = Self {
            snapshot: Arc::new(ArcSwap::from_pointee(Snapshot::default())),
            writes: tx,
            metrics,
            installed: AtomicBool::new(false),
        };
        (io, rx)
    }

    /// A network-free handle for tests: the snapshot is set directly and
    /// queued writes are read from the returned receiver.
    #[cfg(test)]
    pub(crate) fn for_test(
        metrics: Arc<dyn PlacementMetrics>,
    ) -> (Arc<Self>, mpsc::Receiver<Write>) {
        let (io, rx) = Self::new(metrics);
        (Arc::new(io), rx)
    }

    /// The metrics sink this handle was started with, so the request-path
    /// placement hook reports through the same adapter.
    pub fn metrics(&self) -> &dyn PlacementMetrics {
        self.metrics.as_ref()
    }

    /// Claims this handle for one Fleet. `false` when it was already
    /// installed elsewhere.
    pub(crate) fn claim_install(&self) -> bool {
        !self.installed.swap(true, Ordering::AcqRel)
    }

    /// Queues `w` without waiting. A full queue drops it and increments
    /// `valkey_writes_dropped{kind}`; a closed queue (inert handle) drops it
    /// silently.
    pub fn record(&self, w: Write) {
        let tag = w.kind_tag();
        if let Err(mpsc::error::TrySendError::Full(_)) = self.writes.try_send(w) {
            self.metrics.record_count(METRIC_WRITES_DROPPED, 1, &[tag]);
        }
    }
}

/// Installs ring as the process-wide rustls provider, as inference-proxy's
/// `redis_sink::install_crypto_provider`: the lock enables both ring and
/// aws-lc-rs, so rustls has no implicit default and a `rediss://` connect
/// would panic. An `Err` just means a provider is already installed.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Builds a client for `url`, trusting only `ca_pem` (which must hold at
/// least one certificate) when given. `password`, when non-empty, overrides
/// the URL's. Errors carry only a kind, never the URL or password.
fn client(
    url: &str,
    ca_pem: Option<&str>,
    password: Option<String>,
) -> Result<redis::Client, String> {
    use redis::IntoConnectionInfo;
    let mut info = url
        .into_connection_info()
        .map_err(|e| format!("url:{:?}", e.kind()))?;
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        info.redis.password = Some(pw);
    }
    match ca_pem {
        Some(pem) => {
            use rustls::pki_types::{pem::PemObject, CertificateDer};
            let certs = CertificateDer::pem_slice_iter(pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "ca_pem_invalid".to_string())?;
            if certs.is_empty() {
                return Err("ca_pem_empty".to_string());
            }
            redis::Client::build_with_tls(
                info,
                redis::TlsCertificates {
                    client_tls: None,
                    root_cert: Some(pem.as_bytes().to_vec()),
                },
            )
            .map_err(|e| format!("tls:{:?}", e.kind()))
        }
        None => redis::Client::open(info).map_err(|e| format!("client:{:?}", e.kind())),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn should_warn(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= WARN_EVERY)
}

/// `true` once only the background task still holds `hosts`: the Fleet that
/// owned these handles was dropped (discovery replaced its provider), so the
/// task stops instead of polling Valkey for the life of the process.
fn orphaned(hosts: &Arc<ArcSwap<BackendHosts>>) -> bool {
    Arc::strong_count(hosts) == 1
}

/// Connects with backoff; `None` once the handles are orphaned.
async fn connect(
    client: &redis::Client,
    hosts: &Arc<ArcSwap<BackendHosts>>,
) -> Option<ConnectionManager> {
    let config = ConnectionManagerConfig::new()
        .set_connection_timeout(Duration::from_secs(2))
        .set_response_timeout(Duration::from_secs(1))
        .set_number_of_retries(1);
    let mut delay = CONNECT_RETRY_MIN;
    let mut last_warn: Option<Instant> = None;
    loop {
        if orphaned(hosts) {
            return None;
        }
        match ConnectionManager::new_with_config(client.clone(), config.clone()).await {
            Ok(conn) => {
                tracing::info!("Placement Valkey connected");
                return Some(conn);
            }
            Err(e) => {
                if should_warn(last_warn, Instant::now()) {
                    tracing::warn!(
                        error_kind = ?e.kind(),
                        "Placement Valkey connect failed; retrying"
                    );
                    last_warn = Some(Instant::now());
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(CONNECT_RETRY_MAX);
            }
        }
    }
}

async fn run(
    client: redis::Client,
    hosts: Arc<ArcSwap<BackendHosts>>,
    slot: Arc<ArcSwap<Snapshot>>,
    rx: mpsc::Receiver<Write>,
    metrics: Arc<dyn PlacementMetrics>,
) {
    let Some(conn) = connect(&client, &hosts).await else {
        return;
    };
    tokio::spawn(writer(conn.clone(), rx, metrics.clone()));
    reader(conn, hosts, slot, metrics).await;
}

async fn writer(
    mut conn: ConnectionManager,
    mut rx: mpsc::Receiver<Write>,
    metrics: Arc<dyn PlacementMetrics>,
) {
    let mut last_warn: Option<Instant> = None;
    while let Some(first) = rx.recv().await {
        let mut pipe = redis::pipe();
        push_write(&mut pipe, &first);
        let mut n = 1;
        while n < WRITE_BATCH {
            match rx.try_recv() {
                Ok(w) => {
                    push_write(&mut pipe, &w);
                    n += 1;
                }
                Err(_) => break,
            }
        }
        if let Err(e) = pipe.query_async::<()>(&mut conn).await {
            metrics.record_count(METRIC_WRITE_ERRORS, n as i64, &[]);
            if should_warn(last_warn, Instant::now()) {
                tracing::warn!(
                    error_kind = ?e.kind(),
                    writes = n,
                    "Placement Valkey write failed; writes dropped"
                );
                last_warn = Some(Instant::now());
            }
        }
    }
}

async fn reader(
    mut conn: ConnectionManager,
    hosts: Arc<ArcSwap<BackendHosts>>,
    slot: Arc<ArcSwap<Snapshot>>,
    metrics: Arc<dyn PlacementMetrics>,
) {
    let mut state = ReaderState::default();
    let mut last_warn: Option<Instant> = None;
    let mut ticker = tokio::time::interval(READ_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if orphaned(&hosts) {
            tracing::info!("Placement handles dropped; Valkey reader stopping");
            return;
        }
        let hosts_now = hosts.load();
        let reg = &hosts_now.keys;
        let result = read_cycle(&mut conn, &mut state, reg, metrics.as_ref()).await;
        let now = now_ms();
        if let Err(e) = publish_cycle(&mut state, &slot, result, reg, now, metrics.as_ref()) {
            if should_warn(last_warn, Instant::now()) {
                tracing::warn!(
                    error_kind = ?e.kind(),
                    "Placement Valkey read failed; keeping the last snapshot"
                );
                last_warn = Some(Instant::now());
            }
        }
        let age = now.saturating_sub(slot.load().built_ms);
        metrics.record_histogram(METRIC_SNAPSHOT_AGE_MS, age as f64, &[]);
    }
}

/// Warms the pin table up on the first successful cycle, then fetches.
async fn read_cycle(
    conn: &mut ConnectionManager,
    state: &mut ReaderState,
    reg: &KeyRegistry,
    metrics: &dyn PlacementMetrics,
) -> redis::RedisResult<(ReadTargets, RawRead)> {
    if !state.warmed() {
        let entries = warm_up(conn).await?;
        count_malformed_pins(metrics, state.warm_up(entries, now_ms()));
    }
    fetch(conn, ReadTargets::from_registry(reg), state, now_ms()).await
}

/// What one reader cycle reads, derived from the attested key registry: the
/// replica keys of every attested (host, replica), and every attested host's
/// routed counters. Sorted for a stable pipeline layout.
pub(crate) struct ReadTargets {
    replicas: Vec<(String, String)>,
    hosts: Vec<String>,
}

impl ReadTargets {
    fn from_registry(reg: &KeyRegistry) -> Self {
        let mut hosts: Vec<String> = reg.by_host.keys().cloned().collect();
        hosts.sort();
        let mut replicas: Vec<(String, String)> = reg
            .by_host
            .iter()
            .flat_map(|(h, keys)| {
                keys.iter()
                    .flat_map(move |k| k.replica_ids.iter().map(move |r| (h.clone(), r.clone())))
            })
            .collect();
        replicas.sort();
        replicas.dedup();
        Self { replicas, hosts }
    }
}

/// A follow-pin stream entry's fields, as read from Valkey.
pub(crate) struct PinEntry {
    id: String,
    fields: HashMap<String, String>,
}

/// Everything one reader cycle fetched, aligned with its [`ReadTargets`].
pub(crate) struct RawRead {
    /// Raw `MGET` element per `targets.replicas` entry (`Nil` when the key is
    /// gone). Decoded one by one, so a single bad value (a compromised proxy
    /// can write anything to its own key) never fails the whole cycle.
    frames: Vec<redis::Value>,
    /// `(req, tok)` summed over the last two seconds per `targets.hosts` entry.
    routed: Vec<(u64, u64)>,
    /// New pins stream entries, oldest first.
    pins: Vec<PinEntry>,
    /// Unix second the routed counters were read for.
    now_s: u64,
}

/// Reader-owned state that persists across cycles: monotonic ingest, the
/// last accepted view per replica key, and the pin table.
#[derive(Default)]
pub(crate) struct ReaderState {
    ingest: Ingest,
    /// Last accepted frame per `(host, replica)` key.
    views: HashMap<(String, String), Accepted>,
    /// Last rejected raw value per key and why (`None`: not a valid
    /// envelope), so identical repeats are neither re-verified nor re-counted.
    rejected: HashMap<(String, String), (Vec<u8>, Option<Reject>)>,
    pins: Arc<PinTable>,
    /// Last pins stream id read; `None` until warm-up has run.
    last_pin_id: Option<String>,
    cycles: u64,
}

/// A replica's last accepted frame: its raw JSON, the envelope's key id, and
/// the verified view.
#[derive(Clone)]
struct Accepted {
    json: String,
    key_id: String,
    view: ReplicaView,
}

/// Decodes one `MGET` element: `Ok(None)` when the key is gone, `Err` when
/// the value is not a UTF-8 string.
fn decode_frame(v: redis::Value) -> Result<Option<String>, ()> {
    match v {
        redis::Value::Nil => Ok(None),
        v => redis::from_owned_redis_value::<String>(v)
            .map(Some)
            .map_err(|_| ()),
    }
}

/// Sums `req` and `tok` of routed-counter `HGETALL` replies. An element that
/// is not a hash of strings contributes nothing, as does an unparsable field.
fn routed_sum(hashes: impl IntoIterator<Item = redis::Value>) -> (u64, u64) {
    let mut req = 0u64;
    let mut tok = 0u64;
    for v in hashes {
        let Ok(h) = redis::from_owned_redis_value::<HashMap<String, String>>(v) else {
            continue;
        };
        req = req.saturating_add(h.get("req").and_then(|v| v.parse().ok()).unwrap_or(0));
        tok = tok.saturating_add(h.get("tok").and_then(|v| v.parse().ok()).unwrap_or(0));
    }
    (req, tok)
}

/// Outcome of applying one cycle's reads.
pub(crate) struct Applied {
    snapshot: Snapshot,
    rejects: Vec<Reject>,
    bad_envelopes: u32,
    bad_pins: u32,
}

impl ReaderState {
    fn warmed(&self) -> bool {
        self.last_pin_id.is_some()
    }

    /// Loads the warm-up pins (`XREVRANGE`, newest first), ignoring entries
    /// already older than `PIN_TTL_MS`, and resumes `XREAD` after the newest
    /// one (or from the start of an empty stream). Returns the malformed count.
    fn warm_up(&mut self, mut newest_first: Vec<PinEntry>, now_ms: u64) -> u32 {
        let resume = newest_first
            .first()
            .map_or_else(|| "0-0".to_string(), |e| e.id.clone());
        newest_first.reverse();
        let bad = self.apply_pins(newest_first, now_ms);
        self.last_pin_id = Some(resume);
        bad
    }

    /// Applies pins stream entries (oldest first, so a newer pin for the same
    /// id wins), skipping expired, future-dated (beyond `PIN_MAX_FUTURE_MS`)
    /// and malformed ones, and advances the
    /// `XREAD` cursor. Copies the shared table only when something changed.
    /// Returns the malformed count.
    fn apply_pins(&mut self, entries: Vec<PinEntry>, now_ms: u64) -> u32 {
        let mut bad = 0;
        for entry in entries {
            match parse_pin(&entry.fields) {
                Some((id, host, at_ms)) => {
                    let live = now_ms < at_ms.saturating_add(PIN_TTL_MS);
                    let skewed = at_ms > now_ms.saturating_add(PIN_MAX_FUTURE_MS);
                    if live && !skewed {
                        Arc::make_mut(&mut self.pins).insert(id, host, at_ms);
                    }
                }
                None => bad += 1,
            }
            self.last_pin_id = Some(entry.id);
        }
        bad
    }

    /// Builds this cycle's snapshot from `raw`: replicas whose key is present
    /// (a newly accepted frame, or the last accepted view when the frame is
    /// a duplicate or regressed), host-level routed counts, and the pins.
    fn apply(
        &mut self,
        targets: &ReadTargets,
        raw: RawRead,
        reg: &KeyRegistry,
        now_ms: u64,
    ) -> Applied {
        let mut rejects = Vec::new();
        let mut bad_envelopes = 0;
        let mut views = HashMap::with_capacity(targets.replicas.len());
        let mut rejected = HashMap::new();
        for ((host, replica), value) in targets.replicas.iter().zip(raw.frames) {
            let key = (host.clone(), replica.clone());
            let prev = self.views.remove(&key);
            let last_reject = self.rejected.remove(&key);
            let json = match decode_frame(value) {
                Ok(Some(json)) => json,
                Ok(None) => continue, // key expired: the replica drops out
                Err(()) => {
                    // Not a string: an unusable envelope, dropped (not
                    // remembered, it is rare and cheap to recount).
                    bad_envelopes += 1;
                    continue;
                }
            };
            // The same frame read again (the reader outpaces the publisher):
            // reuse the view, provided its signing key is still attested.
            if let Some(acc) = prev.as_ref() {
                let still_attested = reg
                    .by_host
                    .get(host)
                    .is_some_and(|keys| keys.iter().any(|k| k.key_id == acc.key_id));
                if acc.json == json && still_attested {
                    views.insert(key, acc.clone());
                    continue;
                }
            }
            // The same rejected value again: keep its outcome without
            // re-verifying or re-counting.
            if let Some((bytes, reason)) = last_reject {
                if bytes == json.as_bytes() {
                    if reason == Some(Reject::Regressed) {
                        if let Some(acc) = prev {
                            views.insert(key.clone(), acc);
                        }
                    }
                    rejected.insert(key, (bytes, reason));
                    continue;
                }
            }
            let Ok(env) = serde_json::from_str::<Envelope>(&json) else {
                bad_envelopes += 1;
                rejected.insert(key, (json.into_bytes(), None));
                continue;
            };
            match self.ingest.accept(host, replica, &env, reg, now_ms) {
                Ok(view) => {
                    let key_id = env.key_id;
                    views.insert(key, Accepted { json, key_id, view });
                }
                Err(r) => {
                    rejects.push(r);
                    if r == Reject::Regressed {
                        if let Some(acc) = prev {
                            views.insert(key.clone(), acc);
                        }
                    }
                    rejected.insert(key, (json.into_bytes(), Some(r)));
                }
            }
        }
        self.views = views;
        self.rejected = rejected;

        let since_ms = raw.now_s.saturating_sub(1).saturating_mul(1000);
        let routed = targets
            .hosts
            .iter()
            .zip(raw.routed)
            .filter(|(_, (req, tok))| *req > 0 || *tok > 0)
            .map(|(host, (req, tok))| {
                (
                    (host.clone(), HOST_REPLICA.to_string()),
                    RoutedCounts {
                        req: u32::try_from(req).unwrap_or(u32::MAX),
                        tok,
                        since_ms,
                    },
                )
            })
            .collect();

        let bad_pins = self.apply_pins(raw.pins, now_ms);
        self.cycles = self.cycles.wrapping_add(1);
        if self.cycles.is_multiple_of(PRUNE_EVERY_CYCLES) {
            Arc::make_mut(&mut self.pins).prune(now_ms);
        }

        let mut replicas: Vec<ReplicaView> = self.views.values().map(|a| a.view.clone()).collect();
        replicas.sort_by(|a, b| (&a.host_id, &a.replica_id).cmp(&(&b.host_id, &b.replica_id)));
        Applied {
            snapshot: Snapshot {
                built_ms: now_ms,
                replicas,
                routed,
                pins: self.pins.clone(),
            },
            rejects,
            bad_envelopes,
            bad_pins,
        }
    }
}

/// Parses a pins stream entry: `k` (32 hex chars), `h` (host), `t` (ms).
fn parse_pin(fields: &HashMap<String, String>) -> Option<([u8; 16], String, u64)> {
    let id: [u8; 16] = hex::decode(fields.get("k")?).ok()?.try_into().ok()?;
    let host = fields.get("h").filter(|h| !h.is_empty())?.clone();
    let at_ms = fields.get("t")?.parse().ok()?;
    Some((id, host, at_ms))
}

fn count_malformed_pins(metrics: &dyn PlacementMetrics, bad: u32) {
    if bad > 0 {
        metrics.record_count(METRIC_PINS_MALFORMED, bad as i64, &[]);
    }
}

/// Applies one cycle's read result: on success swaps in the new snapshot and
/// counts rejects; on error counts it and keeps the previous snapshot, which
/// then ages out by `built_ms`.
fn publish_cycle(
    state: &mut ReaderState,
    slot: &ArcSwap<Snapshot>,
    result: redis::RedisResult<(ReadTargets, RawRead)>,
    reg: &KeyRegistry,
    now_ms: u64,
    metrics: &dyn PlacementMetrics,
) -> redis::RedisResult<()> {
    let (targets, raw) = match result {
        Ok(r) => r,
        Err(e) => {
            metrics.record_count(METRIC_READ_ERRORS, 1, &[]);
            return Err(e);
        }
    };
    let applied = state.apply(&targets, raw, reg, now_ms);
    let mut by_reason: HashMap<&'static str, i64> = HashMap::new();
    for r in &applied.rejects {
        *by_reason.entry(r.as_str()).or_default() += 1;
    }
    if applied.bad_envelopes > 0 {
        *by_reason.entry("envelope").or_default() += applied.bad_envelopes as i64;
    }
    for (reason, n) in by_reason {
        let tag = format!("reason:{reason}");
        metrics.record_count(METRIC_FRAMES_REJECTED, n, &[&tag]);
    }
    count_malformed_pins(metrics, applied.bad_pins);
    slot.store(Arc::new(applied.snapshot));
    Ok(())
}

fn pin_entries(ids: Vec<redis::streams::StreamId>) -> Vec<PinEntry> {
    ids.into_iter()
        .map(|sid| PinEntry {
            id: sid.id,
            fields: sid
                .map
                .into_iter()
                .filter_map(|(k, v)| {
                    redis::from_owned_redis_value::<String>(v)
                        .ok()
                        .map(|v| (k, v))
                })
                .collect(),
        })
        .collect()
}

/// `XREVRANGE pins + - COUNT 50000` (newest first).
async fn warm_up(conn: &mut ConnectionManager) -> redis::RedisResult<Vec<PinEntry>> {
    let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
        .arg(PINS_STREAM)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(PINS_WARMUP_COUNT)
        .query_async(conn)
        .await?;
    Ok(pin_entries(reply.ids))
}

/// One pipeline: `MGET` every replica key, `HGETALL` each host's routed
/// hashes for `now_s-1` and `now_s`, and `XREAD` new pins.
async fn fetch(
    conn: &mut ConnectionManager,
    targets: ReadTargets,
    state: &ReaderState,
    now_ms: u64,
) -> redis::RedisResult<(ReadTargets, RawRead)> {
    let now_s = now_ms / 1000;
    let last_id = state.last_pin_id.as_deref().unwrap_or("0-0");
    let mut pipe = redis::pipe();
    if !targets.replicas.is_empty() {
        let keys: Vec<String> = targets
            .replicas
            .iter()
            .map(|(h, r)| replica_key(h, r))
            .collect();
        pipe.cmd("MGET").arg(keys);
    }
    for h in &targets.hosts {
        pipe.cmd("HGETALL")
            .arg(routed_key(h, HOST_REPLICA, now_s.saturating_sub(1)));
        pipe.cmd("HGETALL").arg(routed_key(h, HOST_REPLICA, now_s));
    }
    pipe.cmd("XREAD")
        .arg("COUNT")
        .arg(PINS_READ_COUNT)
        .arg("STREAMS")
        .arg(PINS_STREAM)
        .arg(last_id);
    let values: Vec<redis::Value> = pipe.query_async(conn).await?;
    let mut it = values.into_iter();
    let mut next = || {
        it.next().ok_or_else(|| {
            redis::RedisError::from((redis::ErrorKind::TypeError, "short pipeline reply"))
        })
    };

    let frames: Vec<redis::Value> = if targets.replicas.is_empty() {
        Vec::new()
    } else {
        redis::from_owned_redis_value(next()?)?
    };
    if frames.len() != targets.replicas.len() {
        return Err(redis::RedisError::from((
            redis::ErrorKind::TypeError,
            "MGET reply length mismatch",
        )));
    }
    let mut routed = Vec::with_capacity(targets.hosts.len());
    for _ in &targets.hosts {
        routed.push(routed_sum([next()?, next()?]));
    }
    let xread: Option<redis::streams::StreamReadReply> = redis::from_owned_redis_value(next()?)?;
    let pins = xread
        .map(|r| {
            r.keys
                .into_iter()
                .flat_map(|k| pin_entries(k.ids))
                .collect()
        })
        .unwrap_or_default();

    Ok((
        targets,
        RawRead {
            frames,
            routed,
            pins,
            now_s,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use placement::affinity::{pin_id, AffinityKey};
    use placement::consts::{COVERED_MODELS, FRESH_MAX_MS};
    use placement::decision::{AffinitySource, Decision, LegacyReason, PlaceInput, Placer};
    use placement::frame::{self, Lifecycle, Limits, Load, ReplicaReport, SIGNING_DOMAIN};
    use placement::snapshot::HostKey;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::sync::Mutex;

    const HOST: &str = "glm53-gpu03";
    const REPLICA: &str = "r1";

    #[derive(Default)]
    struct FakeMetrics {
        counts: Mutex<Vec<(String, i64, Vec<String>)>>,
    }

    impl FakeMetrics {
        fn total(&self, name: &str, tag: Option<&str>) -> i64 {
            self.counts
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _, tags)| n == name && tag.is_none_or(|t| tags.iter().any(|x| x == t)))
                .map(|(_, v, _)| *v)
                .sum()
        }
    }

    impl PlacementMetrics for FakeMetrics {
        fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
            self.counts.lock().unwrap().push((
                name.to_string(),
                value,
                tags.iter().map(|t| t.to_string()).collect(),
            ));
        }
        fn record_histogram(&self, _name: &str, _value: f64, _tags: &[&str]) {}
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn registry() -> KeyRegistry {
        let pk = signing_key().verifying_key();
        let mut by_host = HashMap::new();
        by_host.insert(
            HOST.to_string(),
            vec![HostKey {
                key_id: frame::key_id(&pk),
                key: pk,
                replica_ids: vec![REPLICA.to_string()],
                model: COVERED_MODELS[0].to_string(),
            }],
        );
        KeyRegistry { by_host }
    }

    fn report(seq: u64, sampled_ms: u64) -> ReplicaReport {
        ReplicaReport {
            schema: 1,
            host_id: HOST.into(),
            replica_id: REPLICA.into(),
            boot_id: "boot-a".into(),
            seq,
            engine_sampled_at_ms: Some(sampled_ms),
            reported_at_ms: sampled_ms,
            lifecycle_state: Lifecycle::Ready,
            model: COVERED_MODELS[0].into(),
            engine: "sglang".into(),
            engine_version: None,
            limits: Limits { max_running: None },
            load: Load {
                running: Some(0),
                queued: Some(0),
                ..Load::default()
            },
            proxy_inflight: 0,
            report_key_id: frame::key_id(&signing_key().verifying_key()),
        }
    }

    /// Envelope JSON as inference-proxy writes it to `replica:{host}:{replica}`.
    fn sealed_json(r: &ReplicaReport) -> String {
        let frame = serde_json::to_string(r).unwrap();
        let mut msg = SIGNING_DOMAIN.to_vec();
        msg.extend_from_slice(frame.as_bytes());
        let sig = signing_key().sign(&msg);
        serde_json::json!({
            "frame": frame,
            "sig": base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
            "key_id": r.report_key_id,
        })
        .to_string()
    }

    fn raw(frames: Vec<Option<String>>, now_ms: u64) -> RawRead {
        RawRead {
            frames: frames
                .into_iter()
                .map(|f| {
                    f.map_or(redis::Value::Nil, |s| {
                        redis::Value::BulkString(s.into_bytes())
                    })
                })
                .collect(),
            routed: vec![(0, 0)],
            pins: Vec::new(),
            now_s: now_ms / 1000,
        }
    }

    fn pin_entry(id: &str, k: &str, h: &str, t: u64) -> PinEntry {
        PinEntry {
            id: id.to_string(),
            fields: [
                ("k".to_string(), k.to_string()),
                ("h".to_string(), h.to_string()),
                ("t".to_string(), t.to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn place(snap: &Snapshot, now_ms: u64) -> Decision {
        let input = PlaceInput {
            request_id: "req".into(),
            model: COVERED_MODELS[0].into(),
            prompt_tokens_est: 100,
            affinity: None,
            affinity_source: AffinitySource::None,
            long_context_hosts: Vec::new(),
            now_ms,
        };
        let mut rng = StdRng::seed_from_u64(1);
        Placer::new([0u8; 32]).place(&input, snap, &HashMap::new(), &mut rng)
    }

    #[test]
    fn reader_is_orphaned_once_the_handles_are_dropped() {
        let metrics: Arc<dyn PlacementMetrics> = Arc::new(FakeMetrics::default());
        let (io, _rx) = PlacementIo::for_test(metrics);
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let handles = PlacementHandles {
            placer: Arc::new(Placer::new([0u8; 32])),
            io,
            hosts: hosts.clone(),
        };
        assert!(!orphaned(&hosts));
        drop(handles);
        assert!(orphaned(&hosts));
    }

    #[test]
    fn stale_after_error_ages_out() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let targets = ReadTargets::from_registry(&reg);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();

        let ok = raw(vec![Some(sealed_json(&report(1, t0)))], t0);
        publish_cycle(&mut state, &slot, Ok((targets, ok)), &reg, t0, &metrics).unwrap();
        assert_eq!(slot.load().built_ms, t0);
        assert_eq!(slot.load().replicas.len(), 1);
        assert!(matches!(
            place(&slot.load(), t0 + 100),
            Decision::Place { .. }
        ));

        // Valkey goes away: the previous snapshot is kept, not replaced.
        let err = redis::RedisError::from((redis::ErrorKind::IoError, "down"));
        assert!(publish_cycle(&mut state, &slot, Err(err), &reg, t0 + 500, &metrics).is_err());
        assert_eq!(slot.load().built_ms, t0);
        assert_eq!(metrics.total(METRIC_READ_ERRORS, None), 1);

        // ...and ages out to Legacy once older than FRESH_MAX_MS.
        match place(&slot.load(), t0 + FRESH_MAX_MS + 1) {
            Decision::Legacy { reason, .. } => assert_eq!(reason, LegacyReason::Stale),
            Decision::Place { .. } => panic!("stale snapshot must not place"),
        }
    }

    #[test]
    fn duplicate_or_regressed_frame_keeps_last_view_and_gone_key_drops_it() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let f2 = sealed_json(&report(2, t0));

        let cycle = |state: &mut ReaderState, frames, now| {
            let targets = ReadTargets::from_registry(&reg);
            publish_cycle(
                state,
                &slot,
                Ok((targets, raw(frames, now))),
                &reg,
                now,
                &metrics,
            )
            .unwrap();
        };
        cycle(&mut state, vec![Some(f2.clone())], t0);
        // The same frame read again (reader runs faster than the publisher)
        // is a duplicate, not a reject.
        cycle(&mut state, vec![Some(f2)], t0 + 500);
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().built_ms, t0 + 500);
        assert_eq!(metrics.total(METRIC_FRAMES_REJECTED, None), 0);

        // An older seq is rejected as regressed, but the last view stays.
        cycle(
            &mut state,
            vec![Some(sealed_json(&report(1, t0)))],
            t0 + 1_000,
        );
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().replicas[0].report.seq, 2);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:regressed")),
            1
        );

        // The key expired: the replica drops out.
        cycle(&mut state, vec![None], t0 + 1_500);
        assert!(slot.load().replicas.is_empty());

        // A garbage value is counted, never trusted.
        cycle(&mut state, vec![Some("not json".into())], t0 + 2_000);
        assert!(slot.load().replicas.is_empty());
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            1
        );
    }

    #[test]
    fn routed_counts_are_host_level() {
        let t0 = 10_000_500u64;
        let reg = registry();
        let targets = ReadTargets::from_registry(&reg);
        let mut state = ReaderState::default();
        let mut r = raw(vec![None], t0);
        r.routed = vec![(3, 900)];
        let applied = state.apply(&targets, r, &reg, t0);
        let rc = applied.snapshot.routed[&(HOST.to_string(), HOST_REPLICA.to_string())];
        assert_eq!((rc.req, rc.tok, rc.since_ms), (3, 900, 9_999_000));
    }

    #[test]
    fn pins_older_than_ttl_ignored_on_warmup() {
        let now = 10_000_000u64;
        let fresh = AffinityKey::from_bytes([1u8; 16]);
        let old = AffinityKey::from_bytes([2u8; 16]);
        let secret = [3u8; 32];
        let fresh_id = pin_id(&fresh, &secret);
        let old_id = pin_id(&old, &secret);
        let mut state = ReaderState::default();
        let bad = state.warm_up(
            vec![
                pin_entry("3-0", &fresh_id.to_hex(), "gpu02", now - 1_000),
                pin_entry("2-0", "zz-not-hex", "gpu05", now - 1_000),
                pin_entry("1-0", &old_id.to_hex(), "gpu01", now - PIN_TTL_MS),
            ],
            now,
        );
        assert_eq!(bad, 1);
        assert_eq!(state.pins.len(), 1);
        assert_eq!(state.pins.get(&fresh_id, now), Some(("gpu02", now - 1_000)));
        assert_eq!(state.pins.get(&old_id, now), None);
        // XREAD resumes after the newest warm-up entry.
        assert_eq!(state.last_pin_id.as_deref(), Some("3-0"));
    }

    #[test]
    fn empty_warmup_reads_from_stream_start_and_pins_apply_incrementally() {
        let now = 10_000_000u64;
        let id = pin_id(&AffinityKey::from_bytes([4u8; 16]), &[5u8; 32]);
        let mut state = ReaderState::default();
        assert_eq!(state.warm_up(Vec::new(), now), 0);
        assert_eq!(state.last_pin_id.as_deref(), Some("0-0"));

        let before = state.pins.clone();
        assert_eq!(
            state.apply_pins(
                vec![
                    pin_entry("5-0", &id.to_hex(), "gpu01", now),
                    pin_entry("6-0", &id.to_hex(), "gpu07", now + 10),
                ],
                now + 10,
            ),
            0
        );
        assert_eq!(state.pins.get(&id, now + 10), Some(("gpu07", now + 10)));
        assert_eq!(state.last_pin_id.as_deref(), Some("6-0"));
        // Copy-on-write: a snapshot holding the old table is unaffected.
        assert!(before.is_empty());

        // No new pins: the table Arc is reused, not copied.
        let shared = state.pins.clone();
        state.apply_pins(Vec::new(), now + 20);
        assert!(Arc::ptr_eq(&shared, &state.pins));
    }

    #[test]
    fn full_channel_drops_and_counts() {
        let metrics = Arc::new(FakeMetrics::default());
        let (io, _rx) = PlacementIo::new(metrics.clone());
        let routed = || Write::Routed {
            host: HOST.into(),
            replica: None,
            tok: 10,
            sec: 1,
        };
        for _ in 0..WRITE_QUEUE_CAPACITY {
            io.record(routed());
        }
        assert_eq!(metrics.total(METRIC_WRITES_DROPPED, None), 0);
        io.record(routed());
        io.record(Write::Pin {
            id_hex: "00".repeat(16),
            host: HOST.into(),
            at_ms: 1,
        });
        assert_eq!(metrics.total(METRIC_WRITES_DROPPED, Some("kind:routed")), 1);
        assert_eq!(metrics.total(METRIC_WRITES_DROPPED, Some("kind:pin")), 1);
        // Tags never carry the host.
        assert!(metrics
            .counts
            .lock()
            .unwrap()
            .iter()
            .all(|(_, _, tags)| tags.iter().all(|t| !t.contains(HOST))));
    }

    #[test]
    fn non_utf8_value_is_isolated_to_its_replica() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let mut reg = registry();
        reg.by_host.get_mut(HOST).unwrap()[0]
            .replica_ids
            .push("r2".to_string());
        let targets = ReadTargets::from_registry(&reg);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let mut r = raw(vec![Some(sealed_json(&report(1, t0)))], t0);
        // r2's key holds bytes that are not a UTF-8 string.
        r.frames
            .push(redis::Value::BulkString(vec![0xff, 0xfe, 0x00]));
        publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, t0, &metrics).unwrap();
        let snap = slot.load();
        assert_eq!(snap.built_ms, t0);
        assert_eq!(snap.replicas.len(), 1);
        assert_eq!(snap.replicas[0].replica_id, REPLICA);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            1
        );
    }

    #[test]
    fn bad_routed_hash_contributes_nothing() {
        let good = redis::Value::Map(vec![
            (
                redis::Value::BulkString(b"req".to_vec()),
                redis::Value::BulkString(b"2".to_vec()),
            ),
            (
                redis::Value::BulkString(b"tok".to_vec()),
                redis::Value::BulkString(b"50".to_vec()),
            ),
        ]);
        let bad = redis::Value::BulkString(vec![0xff]);
        assert_eq!(routed_sum([good, bad]), (2, 50));
        let unparsable = redis::Value::Map(vec![(
            redis::Value::BulkString(b"req".to_vec()),
            redis::Value::BulkString(b"lots".to_vec()),
        )]);
        assert_eq!(routed_sum([unparsable, redis::Value::Nil]), (0, 0));
    }

    #[test]
    fn future_dated_pins_ignored() {
        let now = 10_000_000u64;
        let secret = [3u8; 32];
        let ok = pin_id(&AffinityKey::from_bytes([1u8; 16]), &secret);
        let skewed = pin_id(&AffinityKey::from_bytes([2u8; 16]), &secret);
        let late = pin_id(&AffinityKey::from_bytes([3u8; 16]), &secret);
        let mut state = ReaderState::default();
        state.warm_up(
            vec![
                pin_entry(
                    "2-0",
                    &skewed.to_hex(),
                    "gpu02",
                    now + PIN_MAX_FUTURE_MS + 1,
                ),
                pin_entry("1-0", &ok.to_hex(), "gpu01", now + PIN_MAX_FUTURE_MS),
            ],
            now,
        );
        assert_eq!(state.pins.len(), 1);
        assert!(state.pins.get(&ok, now).is_some());
        assert_eq!(state.pins.get(&skewed, now), None);
        state.apply_pins(
            vec![pin_entry("3-0", &late.to_hex(), "gpu03", now + 60_000)],
            now,
        );
        assert_eq!(state.pins.get(&late, now + 60_000), None);
        assert_eq!(state.last_pin_id.as_deref(), Some("3-0"));
    }

    #[test]
    fn duplicate_frame_dropped_once_its_key_is_no_longer_attested() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let f = sealed_json(&report(1, t0));
        let targets = ReadTargets::from_registry(&reg);
        let r = raw(vec![Some(f.clone())], t0);
        publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, t0, &metrics).unwrap();
        assert_eq!(slot.load().replicas.len(), 1);

        // The host re-attests with a different key; the old frame is re-read.
        let rotated = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let mut reg2 = registry();
        let hk = &mut reg2.by_host.get_mut(HOST).unwrap()[0];
        hk.key = rotated;
        hk.key_id = frame::key_id(&rotated);
        let targets = ReadTargets::from_registry(&reg2);
        let r = raw(vec![Some(f)], t0 + 500);
        publish_cycle(
            &mut state,
            &slot,
            Ok((targets, r)),
            &reg2,
            t0 + 500,
            &metrics,
        )
        .unwrap();
        assert!(slot.load().replicas.is_empty());
    }

    #[test]
    fn identical_rejected_repeats_are_not_recounted() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let cycle = |state: &mut ReaderState, frames, now| {
            let targets = ReadTargets::from_registry(&reg);
            publish_cycle(
                state,
                &slot,
                Ok((targets, raw(frames, now))),
                &reg,
                now,
                &metrics,
            )
            .unwrap();
        };
        cycle(&mut state, vec![Some(sealed_json(&report(2, t0)))], t0);
        let old = sealed_json(&report(1, t0));
        cycle(&mut state, vec![Some(old.clone())], t0 + 500);
        cycle(&mut state, vec![Some(old)], t0 + 1_000);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:regressed")),
            1
        );
        // The last accepted view survives the repeated regressed read.
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().replicas[0].report.seq, 2);

        cycle(&mut state, vec![Some("junk".into())], t0 + 1_500);
        cycle(&mut state, vec![Some("junk".into())], t0 + 2_000);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            1
        );
        // Once the key disappears the memory clears: the same junk counts again.
        cycle(&mut state, vec![None], t0 + 2_500);
        assert!(state.rejected.is_empty());
        cycle(&mut state, vec![Some("junk".into())], t0 + 3_000);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            2
        );
    }

    fn args(cmd: &redis::Cmd) -> Vec<String> {
        cmd.args_iter()
            .map(|a| match a {
                redis::Arg::Simple(b) => String::from_utf8_lossy(b).into_owned(),
                redis::Arg::Cursor => "<cursor>".to_string(),
            })
            .collect()
    }

    #[test]
    fn write_commands_match_key_layout() {
        let mut pipe = redis::pipe();
        push_write(
            &mut pipe,
            &Write::Routed {
                host: "gpu01".into(),
                replica: None,
                tok: 42,
                sec: 1_700,
            },
        );
        push_write(
            &mut pipe,
            &Write::Pin {
                id_hex: "ab".repeat(16),
                host: "gpu02".into(),
                at_ms: 9,
            },
        );
        let cmds: Vec<Vec<String>> = pipe.cmd_iter().map(args).collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cmds,
            vec![
                s(&["HINCRBY", "routed:gpu01:_host:1700", "req", "1"]),
                s(&["HINCRBY", "routed:gpu01:_host:1700", "tok", "42"]),
                s(&["EXPIRE", "routed:gpu01:_host:1700", "5"]),
                s(&[
                    "XADD",
                    "pins",
                    "MAXLEN",
                    "~",
                    "200000",
                    "*",
                    "k",
                    &"ab".repeat(16),
                    "h",
                    "gpu02",
                    "t",
                    "9"
                ]),
            ]
        );
        assert_eq!(replica_key("gpu01", "r1"), "replica:gpu01:r1");
    }

    #[test]
    fn placeholder_ca_is_a_config_error_not_a_panic() {
        install_crypto_provider();
        assert!(client(VALKEY_ENDPOINT, Some(VALKEY_CA_PEM), Some("pw".into())).is_err());
        assert!(client("not a url", None, None).is_err());
    }

    #[tokio::test]
    async fn start_with_placeholder_config_is_inert() {
        let metrics = Arc::new(FakeMetrics::default());
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let io = PlacementIo::start("secret".into(), hosts, metrics.clone());
        assert_eq!(io.snapshot.load().built_ms, 0);
        io.record(Write::Pin {
            id_hex: "00".repeat(16),
            host: HOST.into(),
            at_ms: 1,
        });
        assert_eq!(metrics.total(METRIC_WRITES_DROPPED, None), 0);
    }

    /// Real Valkey/Redis: `PLACEMENT_TEST_REDIS_URL=redis://127.0.0.1:6379
    /// cargo test -p inference_providers --lib real_valkey -- --ignored`
    /// (optionally `PLACEMENT_TEST_REDIS_CA_CERT` = PEM path for `rediss://`).
    #[tokio::test]
    #[ignore = "needs PLACEMENT_TEST_REDIS_URL"]
    async fn real_valkey_round_trip() {
        let url = std::env::var("PLACEMENT_TEST_REDIS_URL")
            .expect("PLACEMENT_TEST_REDIS_URL must point at a disposable Valkey");
        let ca = std::env::var("PLACEMENT_TEST_REDIS_CA_CERT")
            .ok()
            .map(|p| std::fs::read_to_string(p).unwrap());
        install_crypto_provider();
        let client = client(&url, ca.as_deref(), None).unwrap();
        let held = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let _owner = held.clone();
        let mut conn = connect(&client, &held).await.expect("not orphaned");

        // A host id unique to this run keeps the keys test-owned.
        let host = format!("test-{}", uuid::Uuid::new_v4());
        let pk = signing_key().verifying_key();
        let mut reg = KeyRegistry::default();
        reg.by_host.insert(
            host.clone(),
            vec![HostKey {
                key_id: frame::key_id(&pk),
                key: pk,
                replica_ids: vec![REPLICA.to_string()],
                model: COVERED_MODELS[0].to_string(),
            }],
        );
        let now = now_ms();
        let mut rep = report(1, now);
        rep.host_id = host.clone();
        let _: () = redis::cmd("SET")
            .arg(replica_key(&host, REPLICA))
            .arg(sealed_json(&rep))
            .arg("EX")
            .arg(5)
            .query_async(&mut conn)
            .await
            .unwrap();

        let pin = pin_id(
            &AffinityKey::from_bytes(*uuid::Uuid::new_v4().as_bytes()),
            &[1u8; 32],
        );
        let mut pipe = redis::pipe();
        push_write(
            &mut pipe,
            &Write::Routed {
                host: host.clone(),
                replica: None,
                tok: 77,
                sec: now / 1000,
            },
        );
        push_write(
            &mut pipe,
            &Write::Pin {
                id_hex: pin.to_hex(),
                host: host.clone(),
                at_ms: now,
            },
        );
        pipe.query_async::<()>(&mut conn).await.unwrap();

        let mut state = ReaderState::default();
        state.warm_up(warm_up(&mut conn).await.unwrap(), now);
        let targets = ReadTargets::from_registry(&reg);
        let (targets, raw) = fetch(&mut conn, targets, &state, now).await.unwrap();
        let applied = state.apply(&targets, raw, &reg, now);

        assert_eq!(applied.rejects, Vec::<Reject>::new());
        assert_eq!(applied.snapshot.replicas.len(), 1);
        assert_eq!(applied.snapshot.replicas[0].host_id, host);
        let rc = applied.snapshot.routed[&(host.clone(), HOST_REPLICA.to_string())];
        assert_eq!((rc.req, rc.tok), (1, 77));
        assert_eq!(
            applied.snapshot.pins.get(&pin, now),
            Some((host.as_str(), now))
        );
    }
}
