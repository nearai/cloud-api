//! Valkey IO for smart placement: a background reader that turns signed
//! host frames, per-replica routed counters, follow pins and the data-plane
//! kill switch into a [`placement::snapshot::Snapshot`], and a bounded,
//! fire-and-forget write queue for routed counters and pins.
//!
//! Nothing here runs on the request path: the reader swaps a fresh snapshot
//! into an [`ArcSwap`] every [`READ_INTERVAL`], and [`PlacementIo::record`]
//! only `try_send`s onto a bounded channel (a full channel drops and counts).
//!
//! Every chat model's Fleet gets a reader, so an idle one must cost nothing:
//! while its key registry holds no attested replica-report key (no host of
//! that model publishes frames) the reader neither connects nor issues any
//! Valkey read, the switches included, and the snapshot stays empty.
//!
//! Fail open: an invalid endpoint or CA, a failed connect, or a failed read
//! never panics. The last snapshot is kept, ages out by `built_ms`, and the
//! placer falls back to the legacy path within `FRESH_MAX_MS`.
//!
//! Privacy: never log the password, the connection URL, a redis error's
//! `Display` (it can echo the URL), frame contents, pin ids or keys. Only
//! error kinds and counts are logged or measured.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use placement::affinity::PinTable;
use placement::frame::Envelope;
use placement::snapshot::{Ingest, Reject, ReplicaView, RoutedCounts, Snapshot};
use placement::{KeyRegistry, SlotId};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::sync::mpsc;

use crate::BackendHosts;

/// The placement Valkey endpoint, from cloud-api's config
/// (`PLACEMENT_REDIS_HOST`, `_PORT`, `_TLS_ENABLED`, `_TLS_CA_CERT`). The
/// client authenticates as ACL user `router` with the password from
/// `PLACEMENT_REDIS_PASSWORD`, which never appears here.
#[derive(Clone)]
pub struct ValkeyEndpoint {
    /// Host or IP literal. With TLS it must match the server certificate's
    /// SAN: the placement Valkey's certificate names its Elastic IP, so use
    /// the IP, not a DNS name.
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// The private CA that signed the server certificate (PEM). Required
    /// with TLS: the platform trust roots are never used instead.
    pub ca_pem: Option<String>,
}

impl ValkeyEndpoint {
    /// `rediss://router@host:port` (`redis://` without TLS).
    fn url(&self) -> String {
        let scheme = if self.tls { "rediss" } else { "redis" };
        if self.host.contains(':') {
            format!("{scheme}://router@[{}]:{}", self.host, self.port)
        } else {
            format!("{scheme}://router@{}:{}", self.host, self.port)
        }
    }
}

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
///
/// Throughput ceiling: one cycle every [`READ_INTERVAL`] (500 ms) reads at
/// most this many entries, so the reader keeps up with at most ~10,000 pin
/// writes/s fleet-wide (summed over every cloud-api node). Above that, pins
/// fall behind (the stream cursor lags) until the write rate drops.
pub const PINS_READ_COUNT: usize = 5_000;
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
/// Per newly received signed frame, accepted or rejected as a future one:
/// this node's clock minus the frame's `reported_at_ms`, signed (a host
/// clock ahead is negative), tagged
/// `host:{host}`. Shows clock skew between cloud-api and a GPU host. The
/// only host-tagged metric: hosts are few (about 10) and not customer data.
pub const METRIC_FRAME_AGE_MS: &str = "cloud_api.placement.frame_age_ms";
/// One per reader cycle that found [`KILL_SWITCH_KEY`] present.
pub const METRIC_KILL_SWITCH_CYCLES: &str = "cloud_api.placement.kill_switch_cycles";
/// One per placement decision, tagged `outcome`, `tier`,
/// `class`, `strategy`, `priority_band` plus `selection:{..}` (place) or
/// `reason:{..}` (legacy), and `model`. Never host or request
/// ids. A Fleet no host of which has ever published records none.
pub const METRIC_DECISIONS: &str = "cloud_api.placement.decisions";
/// Heavy-lane members and cap as seen by each decision, tagged `tier`.
/// Histograms (no gauge primitive); dashboards read the max over the window.
pub const METRIC_LANE_SIZE: &str = "cloud_api.placement.lane_size";
pub const METRIC_LANE_CAP: &str = "cloud_api.placement.lane_cap";
/// Time spent in `Placer::place`, in microseconds, tagged `tier`.
pub const METRIC_PLACE_DURATION_US: &str = "cloud_api.placement.place_duration_us";
/// Request sent to first streamed chunk, tagged `strategy` and `selection`
/// (`legacy` for a request placement did not place), `size` and `model`.
pub const METRIC_TTFT_MS: &str = "cloud_api.placement.ttft_ms";
/// Request sent to end of stream, same tags as [`METRIC_TTFT_MS`].
pub const METRIC_DURATION_MS: &str = "cloud_api.placement.duration_ms";
/// Mean inter-token latency of one streamed request, from its first to its
/// last token chunk (a chunk with at least one choice; a usage-only chunk
/// is not one); recorded only when at least 2 token chunks arrived.
pub const METRIC_ITL_MS: &str = "cloud_api.placement.itl_ms";
/// Replicas excluded per decision, by eligibility rule (`rule:{..}`).
pub const METRIC_EXCLUDED: &str = "cloud_api.placement.excluded";
/// One per decision, tagged `affinity:{client|prefix|none}` and `outcome:{..}`.
pub const METRIC_AFFINITY: &str = "cloud_api.placement.affinity";
/// Prefill backlog (tokens) on the chosen host, per placed request.
pub const METRIC_CHOSEN_BACKLOG: &str = "cloud_api.placement.chosen_backlog_tokens";

/// Everything a provider's `Fleet` needs to place its model's requests.
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
        endpoint: &ValkeyEndpoint,
        placer: Arc<placement::decision::Placer>,
        metrics: Arc<M>,
    ) -> Self {
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let io = PlacementIo::start(
            password,
            endpoint,
            hosts.clone(),
            placer.tuning_handle().clone(),
            metrics,
        );
        Self { placer, io, hosts }
    }

    /// True when a host behind this Fleet has an attested replica-report
    /// key: it has published, so placement has something to read.
    pub fn any_host_publishes(&self) -> bool {
        any_host_publishes(&self.hosts.load().keys)
    }
}

/// The metrics this module emits. Same method shapes as
/// `services::metrics::MetricsServiceTrait` (which this crate cannot depend
/// on); `services` implements it for `dyn MetricsServiceTrait`. Tags are
/// `key:value` strings and must stay low-cardinality (no host ids, except
/// [`METRIC_FRAME_AGE_MS`]'s).
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

/// `replica:{host}`: a host's latest signed frame envelope (JSON string,
/// `EX 5`), matching inference-proxy's `redis_sink::state_key`.
pub fn replica_key(host: &str) -> String {
    format!("replica:{host}")
}

/// `routed:{host}:{replica}:{sec}`: per-second routed counters (`req`,
/// `tok`) for one replica slot.
pub fn routed_key(slot: &SlotId, sec: u64) -> String {
    format!("routed:{}:{}:{sec}", slot.host, slot.replica)
}

/// The data-plane kill switch: while this key exists (any value), every
/// snapshot is published `disabled`, so every placement is Legacy. Only its
/// presence matters. It sits under `routed:*` so the router ACL can read it,
/// but the router may only `HINCRBY`/`EXPIRE` there and the proxies' writer
/// is scoped to `replica:*`, so only admin can set it.
pub const KILL_SWITCH_KEY: &str = "routed:_placement_off";

/// When Valkey acknowledged one routed-count write, shared by the writer
/// (which sets it) and the placing node's ledger entry (which reads it).
/// Unset while the write is queued or in flight, and forever if the write is
/// dropped or fails, so the entry keeps counting locally until it leaves the
/// routed window (see [`placement::score::unseen_by_read`]).
#[derive(Clone, Default)]
pub struct RoutedAck(Arc<AtomicU64>);

impl RoutedAck {
    /// This node's clock when Valkey acknowledged the write, if it has.
    pub fn acked_ms(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Marks the write acknowledged at `at_ms` (0 is reserved for unset).
    pub(crate) fn set(&self, at_ms: u64) {
        self.0.store(at_ms.max(1), Ordering::Release);
    }
}

/// Marks every routed write of a batch acknowledged at `at_ms`, but only
/// when Valkey accepted the batch: a failed batch leaves them unset.
fn acknowledge(acks: &[RoutedAck], result: &redis::RedisResult<()>, at_ms: u64) {
    if result.is_ok() {
        for ack in acks {
            ack.set(at_ms);
        }
    }
}

/// A fire-and-forget write. Holds a pin id, so it intentionally has no
/// `Debug`.
pub enum Write {
    /// One routed request of `tok` tokens to `slot` in unix second `sec`.
    /// `ack` is set once Valkey acknowledges the write.
    Routed {
        slot: SlotId,
        tok: u64,
        sec: u64,
        ack: RoutedAck,
    },
    /// A follow pin: `id_hex` (32 hex chars) now points at `slot`, whose
    /// host is on `boot` (`None`: unknown, so no `b` field is written).
    Pin {
        id_hex: String,
        slot: SlotId,
        at_ms: u64,
        boot: Option<String>,
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
        Write::Routed { slot, tok, sec, .. } => {
            let key = routed_key(slot, *sec);
            pipe.cmd("HINCRBY").arg(&key).arg("req").arg(1).ignore();
            pipe.cmd("HINCRBY").arg(&key).arg("tok").arg(*tok).ignore();
            pipe.cmd("EXPIRE").arg(&key).arg(ROUTED_TTL_SECS).ignore();
        }
        Write::Pin {
            id_hex,
            slot,
            at_ms,
            boot,
        } => {
            let cmd = pipe.cmd("XADD");
            cmd.arg(PINS_STREAM)
                .arg("MAXLEN")
                .arg("~")
                .arg(PINS_MAXLEN)
                .arg("*")
                .arg("k")
                .arg(id_hex)
                .arg("h")
                .arg(&slot.host)
                .arg("r")
                .arg(slot.replica)
                .arg("t")
                .arg(*at_ms);
            if let Some(boot) = boot {
                cmd.arg("b").arg(boot);
            }
            cmd.ignore();
        }
    }
}

/// Set once the "client not configured" warning has been logged: every chat
/// endpoint starts a handle set, and the cause (the configured endpoint or
/// CA) is process-wide, so one line says it all.
static UNCONFIGURED_WARNED: AtomicBool = AtomicBool::new(false);

/// `true` for exactly the first caller on `warned`.
fn claim_unconfigured_warning(warned: &AtomicBool) -> bool {
    !warned.swap(true, Ordering::AcqRel)
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
    /// Spawns the reader and writer against `endpoint` with `password`.
    /// Never panics and never blocks: if the endpoint or CA is invalid it
    /// logs the error kind and returns an inert handle whose snapshot stays
    /// empty. Must be called inside a Tokio runtime.
    ///
    /// The reader and writer tasks are detached: the writer ends once every
    /// handle (and so the channel sender) is dropped, and the reader once
    /// only it still holds `hosts`. Start one per placement-enabled provider
    /// (see [`PlacementHandles::start`]), never per request.
    pub fn start<M: PlacementMetrics + ?Sized + 'static>(
        password: String,
        endpoint: &ValkeyEndpoint,
        hosts: Arc<ArcSwap<BackendHosts>>,
        tuning: Arc<ArcSwap<placement::Tuning>>,
        metrics: Arc<M>,
    ) -> Arc<Self> {
        let metrics: Arc<dyn PlacementMetrics> = Arc::new(ErasedMetrics(metrics));
        let (io, rx) = Self::new(metrics.clone());
        install_crypto_provider();
        match endpoint_client(endpoint, password) {
            Ok(client) => {
                tokio::spawn(run(client, hosts, io.snapshot.clone(), tuning, rx, metrics));
            }
            Err(kind) => {
                if claim_unconfigured_warning(&UNCONFIGURED_WARNED) {
                    tracing::warn!(
                        error_kind = kind,
                        "Placement Valkey client not configured; placement stays on the legacy path"
                    );
                }
            }
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

/// Builds the client for `endpoint`. With TLS it trusts only the endpoint's
/// CA, which is then required (`ca_missing` otherwise). Errors carry only a
/// kind, never the host or password.
fn endpoint_client(endpoint: &ValkeyEndpoint, password: String) -> Result<redis::Client, String> {
    let ca_pem = match (endpoint.tls, endpoint.ca_pem.as_deref()) {
        (true, None) => return Err("ca_missing".to_string()),
        (true, ca_pem) => ca_pem,
        (false, _) => None,
    };
    client(&endpoint.url(), ca_pem, Some(password))
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
        // Nothing to read until a host of this Fleet publishes: stay
        // unconnected rather than hold a connection per idle model.
        if !any_host_publishes(&hosts.load().keys) {
            tokio::time::sleep(READ_INTERVAL).await;
            continue;
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
    tuning: Arc<ArcSwap<placement::Tuning>>,
    rx: mpsc::Receiver<Write>,
    metrics: Arc<dyn PlacementMetrics>,
) {
    let Some(conn) = connect(&client, &hosts).await else {
        return;
    };
    tokio::spawn(writer(conn.clone(), rx, metrics.clone()));
    reader(conn, hosts, slot, tuning, metrics).await;
}

/// One writer batch: `first` plus whatever is already queued, up to
/// [`WRITE_BATCH`] writes, in one pipeline. `acks` is refilled with the
/// acknowledgement handle of every routed write in the batch, in order.
/// Returns the pipeline and its write count.
fn build_batch(
    first: Write,
    rx: &mut mpsc::Receiver<Write>,
    acks: &mut Vec<RoutedAck>,
) -> (redis::Pipeline, usize) {
    let mut pipe = redis::pipe();
    acks.clear();
    let mut add = |w: Write, pipe: &mut redis::Pipeline| {
        push_write(pipe, &w);
        if let Write::Routed { ack, .. } = w {
            acks.push(ack);
        }
    };
    add(first, &mut pipe);
    let mut n = 1;
    while n < WRITE_BATCH {
        match rx.try_recv() {
            Ok(w) => {
                add(w, &mut pipe);
                n += 1;
            }
            Err(_) => break,
        }
    }
    (pipe, n)
}

async fn writer(
    mut conn: ConnectionManager,
    mut rx: mpsc::Receiver<Write>,
    metrics: Arc<dyn PlacementMetrics>,
) {
    let mut last_warn: Option<Instant> = None;
    let mut acks: Vec<RoutedAck> = Vec::with_capacity(WRITE_BATCH);
    while let Some(first) = rx.recv().await {
        let (pipe, n) = build_batch(first, &mut rx, &mut acks);
        let result = pipe.query_async::<()>(&mut conn).await;
        acknowledge(&acks, &result, now_ms());
        if let Err(e) = result {
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
    tuning: Arc<ArcSwap<placement::Tuning>>,
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
        state.set_pin_ttl_ms(tuning.load().pin_ttl_ms);
        let hosts_now = hosts.load();
        let reg = &hosts_now.keys;
        let Some((result, now)) =
            reader_cycle(&mut conn, &mut state, &slot, reg, metrics.as_ref()).await
        else {
            continue;
        };
        if let Err(e) = result {
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

/// True when at least one host of this Fleet has an attested
/// replica-report key, i.e. publishes frames placement can read.
fn any_host_publishes(reg: &KeyRegistry) -> bool {
    reg.by_host.values().any(|keys| !keys.is_empty())
}

/// One reader cycle's Valkey reads: the connection in production, a fake in
/// tests.
trait CycleSource {
    fn read_cycle(
        &mut self,
        state: &mut ReaderState,
        reg: &KeyRegistry,
        metrics: &dyn PlacementMetrics,
    ) -> impl std::future::Future<Output = redis::RedisResult<(ReadTargets, RawRead)>> + Send;
}

impl CycleSource for ConnectionManager {
    fn read_cycle(
        &mut self,
        state: &mut ReaderState,
        reg: &KeyRegistry,
        metrics: &dyn PlacementMetrics,
    ) -> impl std::future::Future<Output = redis::RedisResult<(ReadTargets, RawRead)>> + Send {
        read_cycle(self, state, reg, metrics)
    }
}

/// One reader cycle: the result of reading and publishing, with the time it
/// was published at, or `None` when there is nothing to place. With no host
/// publishing (see [`any_host_publishes`]) it issues no Valkey read at all,
/// not even the kill switch, and clears a snapshot left
/// over from hosts that stopped publishing. Otherwise the switch is read
/// once, in the same pipeline as the frames.
async fn reader_cycle(
    src: &mut impl CycleSource,
    state: &mut ReaderState,
    slot: &ArcSwap<Snapshot>,
    reg: &KeyRegistry,
    metrics: &dyn PlacementMetrics,
) -> Option<(redis::RedisResult<()>, u64)> {
    if !any_host_publishes(reg) {
        if !slot.load().replicas.is_empty() {
            slot.store(Arc::new(Snapshot::default()));
        }
        return None;
    }
    let result = src.read_cycle(state, reg, metrics).await;
    let now = now_ms();
    Some((publish_cycle(state, slot, result, reg, now, metrics), now))
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
    let targets = ReadTargets::new(reg, state);
    // Taken just before the read is issued, never at completion: a routed
    // write acknowledged while the read is in flight may be missing from it.
    let read_ms = now_ms();
    fetch(conn, targets, state, read_ms).await
}

/// What one reader cycle reads: the frame key of every attested host (from
/// the key registry), and the routed counters of every replica slot those
/// hosts' last accepted frames carried. Sorted for a stable pipeline layout.
pub(crate) struct ReadTargets {
    hosts: Vec<String>,
    slots: Vec<SlotId>,
}

impl ReadTargets {
    fn new(reg: &KeyRegistry, state: &ReaderState) -> Self {
        let mut hosts: Vec<String> = reg.by_host.keys().cloned().collect();
        hosts.sort();
        let mut slots: Vec<SlotId> = state
            .hosts
            .iter()
            .filter(|(host, _)| reg.by_host.contains_key(*host))
            .flat_map(|(_, acc)| acc.views.iter().map(|v| v.slot.clone()))
            .collect();
        slots.sort();
        Self { hosts, slots }
    }
}

/// A follow-pin stream entry's fields, as read from Valkey.
pub(crate) struct PinEntry {
    id: String,
    fields: HashMap<String, String>,
}

/// Everything one reader cycle fetched, aligned with its [`ReadTargets`].
pub(crate) struct RawRead {
    /// Raw `MGET` element per `targets.hosts` entry (`Nil` when the key is
    /// gone). Decoded one by one, so a single bad value (a compromised proxy
    /// can write anything to its own key) never fails the whole cycle.
    frames: Vec<redis::Value>,
    /// [`KILL_SWITCH_KEY`] exists.
    kill_switch: bool,
    /// `(req, tok)` summed over the last two seconds per `targets.slots` entry.
    routed: Vec<(u64, u64)>,
    /// New pins stream entries, oldest first.
    pins: Vec<PinEntry>,
    /// Unix second the routed counters were read for.
    now_s: u64,
    /// This node's clock when the read was issued (the snapshot's
    /// `routed_read_ms`).
    read_ms: u64,
}

/// Reader-owned state that persists across cycles: monotonic ingest, the
/// last accepted frame per host, and the pin table.
#[derive(Default)]
pub(crate) struct ReaderState {
    ingest: Ingest,
    /// Last accepted frame per host.
    hosts: HashMap<String, Accepted>,
    /// The slots of the last accepted frame of each host whose frame key
    /// has since expired (not in the snapshot), so the host's next accepted
    /// frame still drops the pins of slots it no longer carries.
    expired_slots: HashMap<String, Vec<SlotId>>,
    /// Last rejected raw value per host and why (`None`: not a valid
    /// envelope), so identical repeats are neither re-verified nor re-counted.
    rejected: HashMap<String, (Vec<u8>, Option<Reject>)>,
    pins: Arc<PinTable>,
    /// Last pins stream id read; `None` until warm-up has run.
    last_pin_id: Option<String>,
    cycles: u64,
}

/// A host's last accepted frame: its raw JSON, the envelope's key id, and
/// the verified view of every replica it carried.
#[derive(Clone)]
struct Accepted {
    json: String,
    key_id: String,
    views: Vec<ReplicaView>,
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
    /// `(host, now - reported_at_ms)` per newly received signed frame that was
    /// accepted or rejected as Future.
    frame_ages: Vec<(String, f64)>,
    rejects: Vec<Reject>,
    bad_envelopes: u32,
    bad_pins: u32,
}

impl ReaderState {
    /// Applies the live pin TTL to the table. Copies the shared table only
    /// when the TTL actually changed.
    fn set_pin_ttl_ms(&mut self, ttl_ms: u64) {
        if self.pins.ttl_ms() != ttl_ms {
            Arc::make_mut(&mut self.pins).set_ttl_ms(ttl_ms);
        }
    }

    fn warmed(&self) -> bool {
        self.last_pin_id.is_some()
    }

    /// Loads the warm-up pins (`XREVRANGE`, newest first), ignoring entries
    /// already older than the pin TTL, and resumes `XREAD` after the newest
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
                Some((id, slot, at_ms, boot)) => {
                    let live = now_ms < at_ms.saturating_add(self.pins.ttl_ms());
                    let skewed = at_ms > now_ms.saturating_add(PIN_MAX_FUTURE_MS);
                    if live && !skewed {
                        Arc::make_mut(&mut self.pins).insert_on_boot(id, slot, at_ms, boot);
                    }
                }
                None => bad += 1,
            }
            self.last_pin_id = Some(entry.id);
        }
        bad
    }

    /// Builds this cycle's snapshot from `raw`: the replicas of every host
    /// whose key is present (a newly accepted frame replaces all of that
    /// host's views; a duplicate or regressed frame keeps the last accepted
    /// ones), per-slot routed counts, the pins, and the two switches.
    fn apply(
        &mut self,
        targets: &ReadTargets,
        raw: RawRead,
        reg: &KeyRegistry,
        now_ms: u64,
    ) -> Applied {
        let mut rejects = Vec::new();
        let mut frame_ages = Vec::new();
        let mut bad_envelopes = 0;
        let mut hosts = HashMap::with_capacity(targets.hosts.len());
        let mut rejected = HashMap::new();
        // Slots a host's newly accepted frame no longer carries.
        let mut removed: Vec<SlotId> = Vec::new();
        for (host, value) in targets.hosts.iter().zip(raw.frames) {
            let prev = self.hosts.remove(host);
            let last_reject = self.rejected.remove(host);
            let json = match decode_frame(value) {
                Ok(Some(json)) => json,
                Ok(None) => {
                    // Key expired: the host's views drop out, but its slots
                    // are remembered for the removed-slot pin check.
                    if let Some(acc) = prev {
                        self.expired_slots.insert(
                            host.clone(),
                            acc.views.into_iter().map(|v| v.slot).collect(),
                        );
                    }
                    continue;
                }
                Err(()) => {
                    // Not a string: an unusable envelope, dropped (not
                    // remembered, it is rare and cheap to recount).
                    bad_envelopes += 1;
                    continue;
                }
            };
            // The same frame read again (the reader outpaces the publisher):
            // reuse the views, provided its signing key is still attested.
            if let Some(acc) = prev.as_ref() {
                let still_attested = reg
                    .by_host
                    .get(host)
                    .is_some_and(|keys| keys.iter().any(|k| k.key_id == acc.key_id));
                if acc.json == json && still_attested {
                    hosts.insert(host.clone(), acc.clone());
                    continue;
                }
            }
            // The same rejected value again: keep its outcome without
            // re-verifying or re-counting.
            if let Some((bytes, reason)) = last_reject {
                if bytes == json.as_bytes() {
                    if reason == Some(Reject::Regressed) {
                        if let Some(acc) = prev {
                            hosts.insert(host.clone(), acc);
                        }
                    }
                    rejected.insert(host.clone(), (bytes, reason));
                    continue;
                }
            }
            let Ok(env) = serde_json::from_str::<Envelope>(&json) else {
                bad_envelopes += 1;
                rejected.insert(host.clone(), (json.into_bytes(), None));
                continue;
            };
            match self.ingest.accept(host, &env, reg, now_ms) {
                Ok(views) => {
                    if let Some(reported) = self.ingest.reported_at_ms(host) {
                        frame_ages.push((host.clone(), now_ms as f64 - reported as f64));
                    }
                    let old_slots: Vec<SlotId> = match prev.as_ref() {
                        Some(acc) => acc.views.iter().map(|v| v.slot.clone()).collect(),
                        None => self.expired_slots.remove(host).unwrap_or_default(),
                    };
                    self.expired_slots.remove(host);
                    removed.extend(
                        old_slots
                            .into_iter()
                            .filter(|old| !views.iter().any(|v| &v.slot == old)),
                    );
                    let key_id = env.key_id;
                    hosts.insert(
                        host.clone(),
                        Accepted {
                            json,
                            key_id,
                            views,
                        },
                    );
                }
                Err(r) => {
                    rejects.push(r);
                    // A skewed host's frame is still signed: record how far
                    // ahead it is (a negative age) so the worst skew shows.
                    if r == Reject::Future {
                        if let Some(reported) = self.ingest.future_reported_at_ms(host) {
                            frame_ages.push((host.clone(), now_ms as f64 - reported as f64));
                        }
                    }
                    if r == Reject::Regressed {
                        if let Some(acc) = prev {
                            hosts.insert(host.clone(), acc);
                        }
                    }
                    rejected.insert(host.clone(), (json.into_bytes(), Some(r)));
                }
            }
        }
        self.hosts = hosts;
        self.rejected = rejected;
        // A host no longer in the key registry is never read again.
        self.expired_slots
            .retain(|host, _| targets.hosts.contains(host));

        let mut replicas: Vec<ReplicaView> = self
            .hosts
            .values()
            .flat_map(|acc| acc.views.iter().cloned())
            .collect();
        replicas.sort_by(|a, b| a.slot.cmp(&b.slot));

        // Counts only for slots still in the snapshot: a slot the latest
        // frame dropped takes its counters with it.
        let since_ms = raw.now_s.saturating_sub(1).saturating_mul(1000);
        let routed = targets
            .slots
            .iter()
            .zip(raw.routed)
            .filter(|(_, (req, tok))| *req > 0 || *tok > 0)
            .filter(|(slot, _)| replicas.iter().any(|v| &v.slot == *slot))
            .map(|(slot, (req, tok))| {
                (
                    slot.clone(),
                    RoutedCounts {
                        req: u32::try_from(req).unwrap_or(u32::MAX),
                        tok,
                        since_ms,
                    },
                )
            })
            .collect();

        // A rebooted host's caches are cold: drop its pins at once, before
        // this cycle's new pins (a pin written since the reboot carries the
        // new boot and is kept; an older one is ignored by its boot).
        let rebooted = self.ingest.take_rebooted_hosts();
        if !rebooted.is_empty() {
            let pins = Arc::make_mut(&mut self.pins);
            for host in &rebooted {
                pins.drop_host(host);
            }
        }
        let host_boots = self
            .hosts
            .keys()
            .filter_map(|host| {
                self.ingest
                    .boot_id(host)
                    .map(|boot| (host.clone(), boot.to_string()))
            })
            .collect();
        let host_reported_ms = self
            .hosts
            .keys()
            .filter_map(|host| {
                self.ingest
                    .reported_at_ms(host)
                    .map(|reported| (host.clone(), reported))
            })
            .collect();
        let bad_pins = self.apply_pins(raw.pins, now_ms);
        if !removed.is_empty() {
            Arc::make_mut(&mut self.pins).retain_slots(|slot| !removed.contains(slot));
        }
        self.cycles = self.cycles.wrapping_add(1);
        if self.cycles.is_multiple_of(PRUNE_EVERY_CYCLES) {
            Arc::make_mut(&mut self.pins).prune(now_ms);
        }

        Applied {
            snapshot: Snapshot {
                built_ms: now_ms,
                replicas,
                routed,
                routed_read_ms: raw.read_ms,
                pins: self.pins.clone(),
                disabled: raw.kill_switch,
                host_boots,
                host_reported_ms,
            },
            frame_ages,
            rejects,
            bad_envelopes,
            bad_pins,
        }
    }
}

/// A parsed pins stream entry: id, slot, written-at ms and host boot.
type ParsedPin = ([u8; 16], SlotId, u64, Option<String>);

/// Parses a pins stream entry: `k` (32 hex chars), `h` (host), `r` (replica
/// index), `t` (ms) and the optional `b` (the host's `boot_id` when the pin
/// was written; absent or empty from an older node, i.e. unknown). An entry
/// without a valid `r` is malformed.
fn parse_pin(fields: &HashMap<String, String>) -> Option<ParsedPin> {
    let id: [u8; 16] = hex::decode(fields.get("k")?).ok()?.try_into().ok()?;
    let host = fields.get("h").filter(|h| !h.is_empty())?.clone();
    let replica = fields.get("r")?.parse().ok()?;
    let at_ms = fields.get("t")?.parse().ok()?;
    let boot = fields.get("b").filter(|b| !b.is_empty()).cloned();
    Some((id, SlotId { host, replica }, at_ms, boot))
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
    for (host, age_ms) in &applied.frame_ages {
        let tag = format!("host:{host}");
        metrics.record_histogram(METRIC_FRAME_AGE_MS, *age_ms, &[&tag]);
    }
    if applied.snapshot.disabled {
        metrics.record_count(METRIC_KILL_SWITCH_CYCLES, 1, &[]);
    }
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

/// One pipeline: `EXISTS` [`KILL_SWITCH_KEY`]
/// (presence of any value type trips it), `MGET` every host's frame key
/// (skipped with no hosts),
/// `HGETALL` each slot's routed hashes for `now_s-1` and `now_s`, and `XREAD`
/// new pins.
fn read_pipeline(targets: &ReadTargets, last_id: &str, now_s: u64) -> redis::Pipeline {
    let mut pipe = redis::pipe();
    pipe.cmd("EXISTS").arg(KILL_SWITCH_KEY);
    if !targets.hosts.is_empty() {
        let keys: Vec<String> = targets.hosts.iter().map(|h| replica_key(h)).collect();
        pipe.cmd("MGET").arg(keys);
    }
    for slot in &targets.slots {
        pipe.cmd("HGETALL")
            .arg(routed_key(slot, now_s.saturating_sub(1)));
        pipe.cmd("HGETALL").arg(routed_key(slot, now_s));
    }
    pipe.cmd("XREAD")
        .arg("COUNT")
        .arg(PINS_READ_COUNT)
        .arg("STREAMS")
        .arg(PINS_STREAM)
        .arg(last_id);
    pipe
}

/// A data-plane switch from an `EXISTS` reply: on when the key exists,
/// whatever its value type.
fn switch_from_exists(reply: redis::Value) -> redis::RedisResult<bool> {
    Ok(redis::from_owned_redis_value::<i64>(reply)? > 0)
}

/// Issues one read pipeline. `read_ms` is this node's clock just before it
/// is sent, recorded as the snapshot's `routed_read_ms`.
async fn fetch(
    conn: &mut ConnectionManager,
    targets: ReadTargets,
    state: &ReaderState,
    read_ms: u64,
) -> redis::RedisResult<(ReadTargets, RawRead)> {
    let now_s = read_ms / 1000;
    let last_id = state.last_pin_id.as_deref().unwrap_or("0-0");
    let values: Vec<redis::Value> = read_pipeline(&targets, last_id, now_s)
        .query_async(conn)
        .await?;
    let mut it = values.into_iter();
    let mut next = || {
        it.next().ok_or_else(|| {
            redis::RedisError::from((redis::ErrorKind::TypeError, "short pipeline reply"))
        })
    };

    let kill_switch = switch_from_exists(next()?)?;
    let frames: Vec<redis::Value> = if targets.hosts.is_empty() {
        Vec::new()
    } else {
        redis::from_owned_redis_value(next()?)?
    };
    if frames.len() != targets.hosts.len() {
        return Err(redis::RedisError::from((
            redis::ErrorKind::TypeError,
            "MGET reply length mismatch",
        )));
    }
    let mut routed = Vec::with_capacity(targets.slots.len());
    for _ in &targets.slots {
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
            kill_switch,
            routed,
            pins,
            now_s,
            read_ms,
        },
    ))
}

/// Runs reader cycles' `apply` over what Valkey would hold: `frames` maps a
/// host to the raw envelope JSON its proxy wrote to `replica:{host}`,
/// `routed` maps a slot to the `(req, tok)` summed over its two routed
/// hashes. Targets come from `reg` and the reader state exactly as in
/// `read_cycle`, so this exercises the production decode/verify/snapshot
/// path without a Valkey connection. Two cycles run, the way the reader
/// learns a host's slots from its frame before it reads their counters.
/// For cross-module contract tests.
#[cfg(test)]
pub(crate) fn snapshot_from_valkey_values(
    reg: &KeyRegistry,
    frames: &HashMap<String, String>,
    routed: &HashMap<SlotId, (u64, u64)>,
    now_ms: u64,
) -> Snapshot {
    let mut state = ReaderState::default();
    let mut snapshot = Snapshot::default();
    for _ in 0..2 {
        let targets = ReadTargets::new(reg, &state);
        let raw = RawRead {
            frames: targets
                .hosts
                .iter()
                .map(|host| {
                    frames.get(host).map_or(redis::Value::Nil, |json| {
                        redis::Value::BulkString(json.clone().into_bytes())
                    })
                })
                .collect(),
            kill_switch: false,
            routed: targets
                .slots
                .iter()
                .map(|slot| routed.get(slot).copied().unwrap_or((0, 0)))
                .collect(),
            pins: Vec::new(),
            now_s: now_ms / 1000,
            read_ms: now_ms,
        };
        snapshot = state.apply(&targets, raw, reg, now_ms).snapshot;
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use placement::affinity::{pin_id, AffinityKey};
    use placement::consts::FRESH_MAX_MS;
    use placement::decision::{AffinitySource, Decision, LegacyReason, PlaceInput, Placer};
    use placement::frame::{self, HostReport, Lifecycle, Load, ReplicaState, SIGNING_DOMAIN};
    use placement::policy::Tier;
    use placement::snapshot::HostKey;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::sync::Mutex;

    const HOST: &str = "glm53-gpu03";
    const HOST_B: &str = "glm53-gpu04";

    #[derive(Default)]
    struct FakeMetrics {
        counts: Mutex<Vec<(String, i64, Vec<String>)>>,
        histograms: Mutex<Vec<(String, f64, Vec<String>)>>,
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
        fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
            self.histograms.lock().unwrap().push((
                name.to_string(),
                value,
                tags.iter().map(|t| t.to_string()).collect(),
            ));
        }
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn host_key(sk: &SigningKey) -> HostKey {
        let pk = sk.verifying_key();
        HostKey {
            key_id: frame::key_id(&pk),
            key: pk,
        }
    }

    /// Every host in `hosts` attested with [`signing_key`].
    fn registry_for(hosts: &[&str]) -> KeyRegistry {
        let by_host = hosts
            .iter()
            .map(|h| (h.to_string(), vec![host_key(&signing_key())]))
            .collect();
        KeyRegistry { by_host }
    }

    fn registry() -> KeyRegistry {
        registry_for(&[HOST])
    }

    fn sid(host: &str, replica: u32) -> SlotId {
        SlotId {
            host: host.into(),
            replica,
        }
    }

    /// A ready replica. `proxy_inflight` carries the frame's `seq`, so a test
    /// can tell which frame a view came from.
    fn replica(index: u32, seq: u64, sampled_ms: u64) -> ReplicaState {
        ReplicaState {
            index,
            engine_sampled_at_ms: Some(sampled_ms),
            lifecycle_state: Lifecycle::Ready,
            engine_version: None,
            limits: Default::default(),
            load: Load {
                running: Some(0),
                queued: Some(0),
                prefill_backlog_tokens: Some(0),
                ..Load::default()
            },
            proxy_inflight: u32::try_from(seq).unwrap(),
        }
    }

    /// A frame for `host` carrying replica `indices`.
    fn host_report(host: &str, seq: u64, sampled_ms: u64, indices: &[u32]) -> HostReport {
        HostReport {
            schema: 1,
            host_id: host.into(),
            boot_id: "boot-a".into(),
            seq,
            reported_at_ms: sampled_ms,
            engine: "sglang".into(),
            report_key_id: frame::key_id(&signing_key().verifying_key()),
            replicas: indices
                .iter()
                .map(|i| replica(*i, seq, sampled_ms))
                .collect(),
        }
    }

    /// A one-replica frame for [`HOST`].
    fn report(seq: u64, sampled_ms: u64) -> HostReport {
        host_report(HOST, seq, sampled_ms, &[0])
    }

    /// Envelope JSON as inference-proxy writes it to `replica:{host}`.
    fn sealed_json(r: &HostReport) -> String {
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

    /// One cycle's raw read for `targets`: `frames` per target host, no
    /// routed counts, no kill switch.
    fn raw(targets: &ReadTargets, frames: Vec<Option<String>>, now_ms: u64) -> RawRead {
        RawRead {
            frames: frames
                .into_iter()
                .map(|f| {
                    f.map_or(redis::Value::Nil, |s| {
                        redis::Value::BulkString(s.into_bytes())
                    })
                })
                .collect(),
            kill_switch: false,
            routed: vec![(0, 0); targets.slots.len()],
            pins: Vec::new(),
            now_s: now_ms / 1000,
            read_ms: now_ms,
        }
    }

    fn pin_entry(id: &str, k: &str, h: &str, r: &str, t: u64) -> PinEntry {
        PinEntry {
            id: id.to_string(),
            fields: [
                ("k".to_string(), k.to_string()),
                ("h".to_string(), h.to_string()),
                ("r".to_string(), r.to_string()),
                ("t".to_string(), t.to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn pid(seed: u8) -> placement::affinity::PinId {
        pin_id(Tier::Base, &AffinityKey::from_bytes([seed; 16]), &[3u8; 32])
    }

    fn place(snap: &Snapshot, now_ms: u64) -> Decision {
        let input = PlaceInput {
            model: "z-ai/glm-5.3-flash".into(),
            prompt_tokens: 100,
            prefill_heavy: false,
            priority: 0,
            affinity: None,
            affinity_source: AffinitySource::None,
            now_ms,
        };
        let mut rng = StdRng::seed_from_u64(1);
        Placer::new([0u8; 32], Tier::Base).place(&input, snap, &HashMap::new(), &mut rng)
    }

    fn legacy_reason(d: Decision) -> LegacyReason {
        match d {
            Decision::Legacy { reason, .. } => reason,
            Decision::Place { .. } => panic!("expected legacy, placed"),
        }
    }

    /// One `publish_cycle` for `reg` against `frames` (one per target host).
    fn cycle(
        state: &mut ReaderState,
        slot: &ArcSwap<Snapshot>,
        reg: &KeyRegistry,
        frames: Vec<Option<String>>,
        now: u64,
        metrics: &FakeMetrics,
    ) {
        let targets = ReadTargets::new(reg, state);
        let r = raw(&targets, frames, now);
        publish_cycle(state, slot, Ok((targets, r)), reg, now, metrics).unwrap();
    }

    /// Every chat endpoint starts a handle set, so the "not configured"
    /// warning is claimed once per process, not once per endpoint.
    #[test]
    fn unconfigured_client_warns_once_per_process() {
        let warned = AtomicBool::new(false);
        assert!(claim_unconfigured_warning(&warned));
        for _ in 0..5 {
            assert!(!claim_unconfigured_warning(&warned));
        }
    }

    #[test]
    fn reader_is_orphaned_once_the_handles_are_dropped() {
        let metrics: Arc<dyn PlacementMetrics> = Arc::new(FakeMetrics::default());
        let (io, _rx) = PlacementIo::for_test(metrics);
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let handles = PlacementHandles {
            placer: Arc::new(Placer::new([0u8; 32], Tier::Base)),
            io,
            hosts: hosts.clone(),
        };
        assert!(!orphaned(&hosts));
        drop(handles);
        assert!(orphaned(&hosts));
    }

    #[test]
    fn routed_read_ms_is_the_issue_time_and_survives_a_failed_read() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();

        // The read was issued 300 ms before this cycle applied it.
        let targets = ReadTargets::new(&reg, &state);
        let mut r = raw(&targets, vec![Some(sealed_json(&report(1, t0)))], t0);
        r.read_ms = t0 - 300;
        publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, t0, &metrics).unwrap();
        assert_eq!(slot.load().routed_read_ms, t0 - 300);

        // A failed read keeps the old routed counts, and with them the old
        // read time.
        let err = redis::RedisError::from((redis::ErrorKind::IoError, "down"));
        assert!(publish_cycle(&mut state, &slot, Err(err), &reg, t0 + 500, &metrics).is_err());
        assert_eq!(slot.load().routed_read_ms, t0 - 300);
    }

    #[test]
    fn batch_acks_routed_writes_only_on_pipeline_success() {
        let (tx, mut rx) = mpsc::channel(WRITE_BATCH + 8);
        let a = RoutedAck::default();
        let b = RoutedAck::default();
        let routed = |ack: &RoutedAck| Write::Routed {
            slot: sid(HOST, 0),
            tok: 1,
            sec: 1,
            ack: ack.clone(),
        };
        let pin = || Write::Pin {
            id_hex: "00".repeat(16),
            slot: sid(HOST, 0),
            at_ms: 1,
            boot: None,
        };
        tx.try_send(pin()).unwrap();
        tx.try_send(routed(&b)).unwrap();
        // More than one batch holds: the tail waits for the next batch.
        let tail: Vec<RoutedAck> = (0..WRITE_BATCH).map(|_| RoutedAck::default()).collect();
        for ack in &tail {
            tx.try_send(routed(ack)).unwrap();
        }

        let mut acks = vec![RoutedAck::default()];
        let (pipe, n) = build_batch(routed(&a), &mut rx, &mut acks);
        assert_eq!(n, WRITE_BATCH);
        // 3 commands per routed write, 1 per pin.
        assert_eq!(pipe.cmd_iter().count(), 3 * (WRITE_BATCH - 1) + 1);
        // Only the routed writes' acks, refilled (the stale entry is gone).
        assert_eq!(acks.len(), WRITE_BATCH - 1);

        let failed: redis::RedisResult<()> =
            Err(redis::RedisError::from((redis::ErrorKind::IoError, "down")));
        acknowledge(&acks, &failed, 1_234);
        assert_eq!(a.acked_ms(), None);
        assert_eq!(b.acked_ms(), None);

        acknowledge(&acks, &Ok(()), 1_234);
        assert_eq!(a.acked_ms(), Some(1_234));
        assert_eq!(b.acked_ms(), Some(1_234));
        // The 3 routed writes past the cap were not in the batch.
        let (in_batch, left) = tail.split_at(WRITE_BATCH - 3);
        assert!(in_batch.iter().all(|ack| ack.acked_ms() == Some(1_234)));
        assert!(left.iter().all(|ack| ack.acked_ms().is_none()));

        let (_, rest) = build_batch(pin(), &mut rx, &mut acks);
        assert_eq!(rest, 4);
        assert_eq!(acks.len(), 3);
    }

    #[test]
    fn routed_write_is_acknowledged_only_when_valkey_accepts_it() {
        let acks = [RoutedAck::default(), RoutedAck::default()];
        assert_eq!(acks[0].acked_ms(), None);

        let failed: redis::RedisResult<()> =
            Err(redis::RedisError::from((redis::ErrorKind::IoError, "down")));
        acknowledge(&acks, &failed, 1_234);
        assert!(acks.iter().all(|a| a.acked_ms().is_none()));

        acknowledge(&acks, &Ok(()), 1_234);
        assert!(acks.iter().all(|a| a.acked_ms() == Some(1_234)));
    }

    #[test]
    fn stale_after_error_ages_out() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();

        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&report(1, t0)))],
            t0,
            &metrics,
        );
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
        assert_eq!(
            legacy_reason(place(&slot.load(), t0 + FRESH_MAX_MS + 1)),
            LegacyReason::Stale
        );
    }

    #[test]
    fn duplicate_or_regressed_frame_keeps_last_view_and_gone_key_drops_it() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let f2 = sealed_json(&report(2, t0));

        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(f2.clone())],
            t0,
            &metrics,
        );
        // The same frame read again (reader runs faster than the publisher)
        // is a duplicate, not a reject.
        cycle(&mut state, &slot, &reg, vec![Some(f2)], t0 + 500, &metrics);
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().built_ms, t0 + 500);
        assert_eq!(metrics.total(METRIC_FRAMES_REJECTED, None), 0);

        // An older seq is rejected as regressed, but the last view stays.
        let f1 = sealed_json(&report(1, t0));
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(f1)],
            t0 + 1_000,
            &metrics,
        );
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().replicas[0].state.proxy_inflight, 2);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:regressed")),
            1
        );

        // The key expired: the host's views drop out.
        cycle(&mut state, &slot, &reg, vec![None], t0 + 1_500, &metrics);
        assert!(slot.load().replicas.is_empty());

        // A garbage value is counted, never trusted.
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some("not json".into())],
            t0 + 2_000,
            &metrics,
        );
        assert!(slot.load().replicas.is_empty());
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            1
        );
    }

    #[test]
    fn host_frame_expanding_to_three_replicas_creates_three_views() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();

        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&host_report(HOST, 1, t0, &[0])))],
            t0,
            &metrics,
        );
        assert_eq!(slot.load().replicas.len(), 1);

        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&host_report(HOST, 2, t0, &[2, 0, 1])))],
            t0 + 500,
            &metrics,
        );
        let snap = slot.load();
        let slots: Vec<SlotId> = snap.replicas.iter().map(|v| v.slot.clone()).collect();
        assert_eq!(slots, vec![sid(HOST, 0), sid(HOST, 1), sid(HOST, 2)]);
        // The next cycle reads each new slot's routed counters.
        let targets = ReadTargets::new(&reg, &state);
        assert_eq!(targets.slots, slots);
        assert_eq!(targets.hosts, vec![HOST.to_string()]);
    }

    #[test]
    fn frame_shrinking_drops_removed_slots_and_their_routed_targets() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let pin_kept = pid(1);
        let pin_gone = pid(2);
        state.warm_up(
            vec![
                pin_entry("2-0", &pin_gone.to_hex(), HOST, "2", t0),
                pin_entry("1-0", &pin_kept.to_hex(), HOST, "0", t0),
            ],
            t0,
        );

        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&host_report(HOST, 1, t0, &[0, 1, 2])))],
            t0,
            &metrics,
        );
        // The next cycle reads routed counters for all three slots, and the
        // host's frame shrinks to replica 0 only.
        let targets = ReadTargets::new(&reg, &state);
        assert_eq!(targets.slots.len(), 3);
        let mut r = raw(
            &targets,
            vec![Some(sealed_json(&host_report(HOST, 2, t0, &[0])))],
            t0,
        );
        r.routed = vec![(1, 10), (2, 20), (3, 30)];
        publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, t0, &metrics).unwrap();

        let snap = slot.load();
        assert_eq!(snap.replicas.len(), 1);
        assert_eq!(snap.replicas[0].slot, sid(HOST, 0));
        // Only the surviving slot keeps its routed counts...
        assert_eq!(snap.routed.len(), 1);
        let rc = snap.routed[&sid(HOST, 0)];
        assert_eq!((rc.req, rc.tok, rc.since_ms), (1, 10, 9_999_000));
        // ...only it is read from now on...
        assert_eq!(ReadTargets::new(&reg, &state).slots, vec![sid(HOST, 0)]);
        // ...and a pin to a removed slot is dropped.
        assert_eq!(
            snap.pins.get(&pin_kept, t0).map(|(s, _)| s.clone()),
            Some(sid(HOST, 0))
        );
        assert!(snap.pins.get(&pin_gone, t0).is_none());
    }

    #[test]
    fn expired_host_pins_are_dropped() {
        // A host's frame key expires, then the host returns with fewer
        // replicas: pins to the slots it no longer carries are dropped,
        // measured against its last frame before the expiry.
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let pin_kept = pid(1);
        let pin_gone = pid(2);
        state.warm_up(
            vec![
                pin_entry("2-0", &pin_gone.to_hex(), HOST, "2", t0),
                pin_entry("1-0", &pin_kept.to_hex(), HOST, "0", t0),
            ],
            t0,
        );
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&host_report(HOST, 1, t0, &[0, 1, 2])))],
            t0,
            &metrics,
        );
        assert_eq!(slot.load().replicas.len(), 3);

        // Expired for two cycles: no views, pins untouched.
        for t in [t0 + 500, t0 + 1_000] {
            cycle(&mut state, &slot, &reg, vec![None], t, &metrics);
            assert!(slot.load().replicas.is_empty());
            assert!(slot.load().pins.get(&pin_gone, t).is_some());
        }

        // Back with replica 0 only.
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&host_report(HOST, 2, t0 + 1_500, &[0])))],
            t0 + 1_500,
            &metrics,
        );
        let snap = slot.load();
        assert_eq!(snap.replicas.len(), 1);
        assert!(snap.pins.get(&pin_kept, t0 + 1_500).is_some());
        assert!(
            snap.pins.get(&pin_gone, t0 + 1_500).is_none(),
            "a pin to a slot gone across the expiry is dropped"
        );
    }

    #[test]
    fn routed_counts_are_per_slot() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let f = sealed_json(&host_report(HOST, 1, t0, &[0, 1]));
        cycle(&mut state, &slot, &reg, vec![Some(f.clone())], t0, &metrics);

        let targets = ReadTargets::new(&reg, &state);
        let mut r = raw(&targets, vec![Some(f)], t0);
        r.routed = vec![(0, 0), (3, 900)];
        let applied = state.apply(&targets, r, &reg, t0);
        assert!(!applied.snapshot.routed.contains_key(&sid(HOST, 0)));
        let rc = applied.snapshot.routed[&sid(HOST, 1)];
        assert_eq!((rc.req, rc.tok, rc.since_ms), (3, 900, 9_999_000));
    }

    #[test]
    fn kill_switch_trips_for_non_string_value() {
        // `EXISTS` counts a key of any type (a hash from `HINCRBY`, a list,
        // a stream), where `MGET` would read nil for a non-string value.
        assert!(switch_from_exists(redis::Value::Int(1)).unwrap());
        assert!(!switch_from_exists(redis::Value::Int(0)).unwrap());
        assert!(switch_from_exists(redis::Value::Nil).is_err());

        let targets = ReadTargets {
            hosts: vec!["gpu01".to_string()],
            slots: Vec::new(),
        };
        let packed =
            String::from_utf8_lossy(&read_pipeline(&targets, "0-0", 7).get_packed_pipeline())
                .into_owned();
        let exists = packed.find("EXISTS").expect("EXISTS in the pipeline");
        let mget = packed.find("MGET").expect("MGET in the pipeline");
        assert!(exists < mget, "EXISTS is the first reply");
        assert_eq!(
            packed.matches(KILL_SWITCH_KEY).count(),
            1,
            "kill key only in EXISTS"
        );

        // No hosts: no MGET (it would be an error with zero keys).
        let empty = ReadTargets {
            hosts: Vec::new(),
            slots: Vec::new(),
        };
        let packed =
            String::from_utf8_lossy(&read_pipeline(&empty, "0-0", 7).get_packed_pipeline())
                .into_owned();
        assert!(!packed.contains("MGET"));
    }

    #[test]
    fn kill_switch_key_disables_snapshot() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let f = sealed_json(&report(1, t0));

        for i in 0..3u64 {
            let now = t0 + i * 500;
            let targets = ReadTargets::new(&reg, &state);
            let mut r = raw(&targets, vec![Some(f.clone())], now);
            r.kill_switch = true;
            publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, now, &metrics).unwrap();
        }
        // One count per cycle while the key is present; frames still ingest.
        assert_eq!(metrics.total(METRIC_KILL_SWITCH_CYCLES, None), 3);
        let snap = slot.load();
        assert!(snap.disabled);
        assert_eq!(snap.replicas.len(), 1);
        assert_eq!(
            legacy_reason(place(&snap, t0 + 1_000)),
            LegacyReason::Disabled
        );

        // The key is deleted: the next cycle places again, uncounted.
        cycle(&mut state, &slot, &reg, vec![Some(f)], t0 + 1_500, &metrics);
        assert!(!slot.load().disabled);
        assert!(matches!(
            place(&slot.load(), t0 + 1_600),
            Decision::Place { .. }
        ));
        assert_eq!(metrics.total(METRIC_KILL_SWITCH_CYCLES, None), 3);
    }

    /// Counts the reads a reader cycle issues and serves one frame per target
    /// host, with the kill switch as set.
    struct FakeSource {
        reads: usize,
        frame: String,
        kill_switch: bool,
    }

    impl CycleSource for FakeSource {
        async fn read_cycle(
            &mut self,
            state: &mut ReaderState,
            reg: &KeyRegistry,
            _metrics: &dyn PlacementMetrics,
        ) -> redis::RedisResult<(ReadTargets, RawRead)> {
            self.reads += 1;
            let targets = ReadTargets::new(reg, state);
            let frames = vec![Some(self.frame.clone()); targets.hosts.len()];
            let mut r = raw(&targets, frames, now_ms());
            r.kill_switch = self.kill_switch;
            Ok((targets, r))
        }
    }

    #[tokio::test]
    async fn reader_skips_valkey_when_no_host_publishes() {
        let metrics = FakeMetrics::default();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let mut src = FakeSource {
            reads: 0,
            frame: sealed_json(&report(1, now_ms())),
            kill_switch: false,
        };

        // No attested replica-report key (a host attested without one counts
        // the same): nothing to place, so no read at all, switches included.
        let mut keyless = KeyRegistry::default();
        keyless.by_host.insert(HOST.to_string(), Vec::new());
        for reg in [KeyRegistry::default(), keyless] {
            for _ in 0..3 {
                let cycle = reader_cycle(&mut src, &mut state, &slot, &reg, &metrics).await;
                assert!(cycle.is_none());
            }
        }
        assert_eq!(src.reads, 0);
        assert!(slot.load().replicas.is_empty());
        assert!(metrics.counts.lock().unwrap().is_empty());

        // A publishing host: one read per cycle, and the kill switch still
        // reaches the snapshot.
        let reg = registry();
        src.kill_switch = true;
        let (result, _) = reader_cycle(&mut src, &mut state, &slot, &reg, &metrics)
            .await
            .expect("read");
        result.unwrap();
        assert_eq!(src.reads, 1);
        assert_eq!(slot.load().replicas.len(), 1);
        assert!(slot.load().disabled);
        assert_eq!(metrics.total(METRIC_KILL_SWITCH_CYCLES, None), 1);

        src.kill_switch = false;
        let (result, _) = reader_cycle(&mut src, &mut state, &slot, &reg, &metrics)
            .await
            .expect("read");
        result.unwrap();
        assert_eq!(src.reads, 2);
        assert!(!slot.load().disabled);

        // Its key goes away: reads stop and the stale picture is cleared.
        let none = KeyRegistry::default();
        assert!(reader_cycle(&mut src, &mut state, &slot, &none, &metrics)
            .await
            .is_none());
        assert_eq!(src.reads, 2);
        assert!(slot.load().replicas.is_empty());
        assert!(!slot.load().disabled);
    }

    #[test]
    fn kill_switch_is_read_first_in_the_pipeline() {
        let targets = ReadTargets {
            hosts: vec!["gpu01".to_string()],
            slots: Vec::new(),
        };
        let cmds: Vec<Vec<String>> = read_pipeline(&targets, "0-0", 7)
            .cmd_iter()
            .map(args)
            .collect();
        assert_eq!(
            cmds[0],
            vec!["EXISTS".to_string(), KILL_SWITCH_KEY.to_string()]
        );
        assert_eq!(cmds[1][0], "MGET");
        // Under the router's readable `routed:*` prefix, never a counter key.
        assert!(KILL_SWITCH_KEY.starts_with("routed:"));
        assert_eq!(KILL_SWITCH_KEY.split(':').count(), 2);
    }

    #[test]
    fn pins_older_than_ttl_ignored_on_warmup() {
        let now = 10_000_000u64;
        let fresh_id = pid(1);
        let old_id = pid(2);
        let mut state = ReaderState::default();
        let bad = state.warm_up(
            vec![
                pin_entry("3-0", &fresh_id.to_hex(), "gpu02", "1", now - 1_000),
                pin_entry("2-0", "zz-not-hex", "gpu05", "0", now - 1_000),
                pin_entry(
                    "1-0",
                    &old_id.to_hex(),
                    "gpu01",
                    "0",
                    now - placement::Tuning::default().pin_ttl_ms,
                ),
            ],
            now,
        );
        assert_eq!(bad, 1);
        assert_eq!(state.pins.len(), 1);
        assert_eq!(
            state.pins.get(&fresh_id, now),
            Some((&sid("gpu02", 1), now - 1_000))
        );
        assert_eq!(state.pins.get(&old_id, now), None);
        // XREAD resumes after the newest warm-up entry.
        assert_eq!(state.last_pin_id.as_deref(), Some("3-0"));
    }

    #[test]
    fn tuned_pin_ttl_applies_to_warmup_and_existing_pins() {
        let now = 10_000_000u64;
        let id = pid(1);
        let mut state = ReaderState::default();
        state.warm_up(
            vec![pin_entry("1-0", &id.to_hex(), "gpu01", "0", now - 90_000)],
            now,
        );
        assert!(state.pins.get(&id, now).is_some());
        // A shorter TTL expires the same entry; a pin that old is not
        // loaded at warm-up either.
        state.set_pin_ttl_ms(60_000);
        assert!(state.pins.get(&id, now).is_none());
        let mut fresh = ReaderState::default();
        fresh.set_pin_ttl_ms(60_000);
        fresh.warm_up(
            vec![pin_entry("1-0", &id.to_hex(), "gpu01", "0", now - 90_000)],
            now,
        );
        assert!(fresh.pins.is_empty());
    }

    /// A pin written by an older node has no `b` field: its host boot is
    /// unknown, so it is accepted (rolling-deploy compatibility), while a
    /// pin carrying a boot is checked against the host's current one.
    #[test]
    fn pin_without_boot_field_is_accepted() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let (legacy, current, old) = (pid(1), pid(2), pid(3));
        let with_boot = |id: &str, k: &str, boot: &str| {
            let mut e = pin_entry(id, k, HOST, "0", t0);
            e.fields.insert("b".to_string(), boot.to_string());
            e
        };
        let bad = state.warm_up(
            vec![
                with_boot("3-0", &old.to_hex(), "boot-z"),
                with_boot("2-0", &current.to_hex(), "boot-a"),
                pin_entry("1-0", &legacy.to_hex(), HOST, "0", t0),
            ],
            t0,
        );
        assert_eq!(bad, 0);
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&report(1, t0)))],
            t0,
            &metrics,
        );
        let snap = slot.load();
        assert_eq!(
            snap.host_boots.get(HOST).map(String::as_str),
            Some("boot-a")
        );
        let stands = |id: &placement::affinity::PinId| {
            let (s, _, boot) = snap.pins.get_with_boot(id, t0).expect("live pin");
            snap.pin_boot_current(&s.host, boot)
        };
        assert!(stands(&legacy), "a pin without a boot is accepted");
        assert!(stands(&current));
        assert!(!stands(&old), "a pin from another boot is not");
    }

    /// A host whose frame shows a new boot has its pins dropped from this
    /// node's table at once; other hosts' pins stay.
    #[test]
    fn rebooted_host_pins_are_dropped_locally() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry_for(&[HOST, HOST_B]);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let (on_host, elsewhere) = (pid(1), pid(2));
        state.warm_up(
            vec![
                pin_entry("2-0", &elsewhere.to_hex(), HOST_B, "0", t0),
                pin_entry("1-0", &on_host.to_hex(), HOST, "0", t0),
            ],
            t0,
        );
        let frames = |boot: &str, seq: u64, t: u64| {
            [HOST, HOST_B]
                .iter()
                .map(|h| {
                    let mut r = host_report(h, seq, t, &[0]);
                    if *h == HOST {
                        r.boot_id = boot.into();
                    }
                    Some(sealed_json(&r))
                })
                .collect::<Vec<_>>()
        };
        cycle(
            &mut state,
            &slot,
            &reg,
            frames("boot-a", 1, t0),
            t0,
            &metrics,
        );
        assert!(slot.load().pins.get(&on_host, t0).is_some());

        // HOST reboots: its first frame of the new boot drops its pins.
        let t1 = t0 + 500;
        cycle(
            &mut state,
            &slot,
            &reg,
            frames("boot-b", 1, t1),
            t1,
            &metrics,
        );
        let snap = slot.load();
        assert_eq!(
            snap.host_boots.get(HOST).map(String::as_str),
            Some("boot-b")
        );
        assert!(snap.pins.get(&on_host, t1).is_none());
        assert!(snap.pins.get(&elsewhere, t1).is_some());
    }

    /// Each newly accepted frame records its age at ingest (this node's
    /// clock minus the host's `reported_at_ms`, signed, so a host clock
    /// ahead shows as negative), tagged by host. A frame read again is not
    /// a new ingest.
    #[test]
    fn frame_age_metric_recorded() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry_for(&[HOST, HOST_B]);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let mut behind = host_report(HOST, 1, t0 - 1_200, &[0]);
        behind.reported_at_ms = t0 - 1_234;
        let mut ahead = host_report(HOST_B, 1, t0, &[0]);
        ahead.reported_at_ms = t0 + 300;
        let frames = vec![Some(sealed_json(&behind)), Some(sealed_json(&ahead))];
        cycle(&mut state, &slot, &reg, frames.clone(), t0, &metrics);
        cycle(&mut state, &slot, &reg, frames, t0 + 500, &metrics);

        let mut ages: Vec<(f64, Vec<String>)> = metrics
            .histograms
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _, _)| n == METRIC_FRAME_AGE_MS)
            .map(|(_, v, tags)| (*v, tags.clone()))
            .collect();
        ages.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(
            ages,
            vec![
                (-300.0, vec![format!("host:{HOST_B}")]),
                (1_234.0, vec![format!("host:{HOST}")]),
            ]
        );
    }

    /// A frame rejected as Future (a host clock far ahead) is still a
    /// signed frame: its age is recorded (negative) so the worst positive
    /// skew shows, once per frame and not again when the same frame is read
    /// again.
    #[test]
    fn frame_age_recorded_for_future_rejected_frames() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry_for(&[HOST]);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let skewed = report(1, t0 + 60_000);
        let frames = vec![Some(sealed_json(&skewed))];
        cycle(&mut state, &slot, &reg, frames.clone(), t0, &metrics);
        cycle(&mut state, &slot, &reg, frames, t0 + 500, &metrics);

        let ages: Vec<(f64, Vec<String>)> = metrics
            .histograms
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _, _)| n == METRIC_FRAME_AGE_MS)
            .map(|(_, v, tags)| (*v, tags.clone()))
            .collect();
        assert_eq!(ages, vec![(-60_000.0, vec![format!("host:{HOST}")])]);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:future")),
            1
        );
    }

    /// The snapshot carries each accepted host's `reported_at_ms`.
    #[test]
    fn snapshot_carries_host_reported_ms() {
        let t0 = 10_000_500u64;
        let metrics = FakeMetrics::default();
        let reg = registry_for(&[HOST]);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let mut r = report(1, t0);
        r.reported_at_ms = t0 - 700;
        cycle(
            &mut state,
            &slot,
            &reg,
            vec![Some(sealed_json(&r))],
            t0,
            &metrics,
        );
        assert_eq!(slot.load().host_reported_ms.get(HOST), Some(&(t0 - 700)));
    }

    #[test]
    fn pin_entry_without_replica_is_malformed() {
        let now = 10_000_000u64;
        let id = pid(4);
        let mut no_replica = pin_entry("1-0", &id.to_hex(), "gpu01", "0", now);
        no_replica.fields.remove("r");
        let entries = vec![
            no_replica,
            pin_entry("2-0", &id.to_hex(), "gpu01", "", now),
            pin_entry("3-0", &id.to_hex(), "gpu01", "-1", now),
            pin_entry("4-0", &id.to_hex(), "gpu01", "r1", now),
        ];
        let metrics = FakeMetrics::default();
        let mut state = ReaderState::default();
        count_malformed_pins(&metrics, state.warm_up(Vec::new(), now));
        count_malformed_pins(&metrics, state.apply_pins(entries, now));
        assert_eq!(metrics.total(METRIC_PINS_MALFORMED, None), 4);
        assert!(state.pins.is_empty());
        // Skipped entries still advance the stream cursor.
        assert_eq!(state.last_pin_id.as_deref(), Some("4-0"));
    }

    #[test]
    fn empty_warmup_reads_from_stream_start_and_pins_apply_incrementally() {
        let now = 10_000_000u64;
        let id = pid(4);
        let mut state = ReaderState::default();
        assert_eq!(state.warm_up(Vec::new(), now), 0);
        assert_eq!(state.last_pin_id.as_deref(), Some("0-0"));

        let before = state.pins.clone();
        assert_eq!(
            state.apply_pins(
                vec![
                    pin_entry("5-0", &id.to_hex(), "gpu01", "0", now),
                    pin_entry("6-0", &id.to_hex(), "gpu07", "3", now + 10),
                ],
                now + 10,
            ),
            0
        );
        assert_eq!(
            state.pins.get(&id, now + 10),
            Some((&sid("gpu07", 3), now + 10))
        );
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
            slot: sid(HOST, 0),
            tok: 10,
            sec: 1,
            ack: RoutedAck::default(),
        };
        for _ in 0..WRITE_QUEUE_CAPACITY {
            io.record(routed());
        }
        assert_eq!(metrics.total(METRIC_WRITES_DROPPED, None), 0);
        io.record(routed());
        io.record(Write::Pin {
            id_hex: "00".repeat(16),
            slot: sid(HOST, 0),
            at_ms: 1,
            boot: None,
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
    fn non_utf8_value_is_isolated_to_its_host() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry_for(&[HOST, HOST_B]);
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let targets = ReadTargets::new(&reg, &state);
        assert_eq!(targets.hosts, vec![HOST.to_string(), HOST_B.to_string()]);
        let mut r = raw(&targets, vec![Some(sealed_json(&report(1, t0)))], t0);
        // HOST_B's key holds bytes that are not a UTF-8 string.
        r.frames
            .push(redis::Value::BulkString(vec![0xff, 0xfe, 0x00]));
        publish_cycle(&mut state, &slot, Ok((targets, r)), &reg, t0, &metrics).unwrap();
        let snap = slot.load();
        assert_eq!(snap.built_ms, t0);
        assert_eq!(snap.replicas.len(), 1);
        assert_eq!(snap.replicas[0].slot, sid(HOST, 0));
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
        let ok = pid(1);
        let skewed = pid(2);
        let late = pid(3);
        let mut state = ReaderState::default();
        state.warm_up(
            vec![
                pin_entry(
                    "2-0",
                    &skewed.to_hex(),
                    "gpu02",
                    "0",
                    now + PIN_MAX_FUTURE_MS + 1,
                ),
                pin_entry("1-0", &ok.to_hex(), "gpu01", "0", now + PIN_MAX_FUTURE_MS),
            ],
            now,
        );
        assert_eq!(state.pins.len(), 1);
        assert!(state.pins.get(&ok, now).is_some());
        assert_eq!(state.pins.get(&skewed, now), None);
        state.apply_pins(
            vec![pin_entry("3-0", &late.to_hex(), "gpu03", "0", now + 60_000)],
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
        cycle(&mut state, &slot, &reg, vec![Some(f.clone())], t0, &metrics);
        assert_eq!(slot.load().replicas.len(), 1);

        // The host re-attests with a different key; the old frame is re-read.
        let rotated = SigningKey::from_bytes(&[9u8; 32]);
        let mut reg2 = registry();
        reg2.by_host
            .insert(HOST.to_string(), vec![host_key(&rotated)]);
        cycle(&mut state, &slot, &reg2, vec![Some(f)], t0 + 500, &metrics);
        assert!(slot.load().replicas.is_empty());
    }

    #[test]
    fn identical_rejected_repeats_are_not_recounted() {
        let t0 = 10_000_000u64;
        let metrics = FakeMetrics::default();
        let reg = registry();
        let slot = ArcSwap::from_pointee(Snapshot::default());
        let mut state = ReaderState::default();
        let run = |state: &mut ReaderState, frame: Option<String>, now| {
            cycle(state, &slot, &reg, vec![frame], now, &metrics)
        };
        run(&mut state, Some(sealed_json(&report(2, t0))), t0);
        let old = sealed_json(&report(1, t0));
        run(&mut state, Some(old.clone()), t0 + 500);
        run(&mut state, Some(old), t0 + 1_000);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:regressed")),
            1
        );
        // The last accepted view survives the repeated regressed read.
        assert_eq!(slot.load().replicas.len(), 1);
        assert_eq!(slot.load().replicas[0].state.proxy_inflight, 2);

        run(&mut state, Some("junk".into()), t0 + 1_500);
        run(&mut state, Some("junk".into()), t0 + 2_000);
        assert_eq!(
            metrics.total(METRIC_FRAMES_REJECTED, Some("reason:envelope")),
            1
        );
        // Once the key disappears the memory clears: the same junk counts again.
        run(&mut state, None, t0 + 2_500);
        assert!(state.rejected.is_empty());
        run(&mut state, Some("junk".into()), t0 + 3_000);
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
                slot: sid("gpu01", 3),
                tok: 42,
                sec: 1_700,
                ack: RoutedAck::default(),
            },
        );
        push_write(
            &mut pipe,
            &Write::Pin {
                id_hex: "ab".repeat(16),
                slot: sid("gpu02", 1),
                at_ms: 9,
                boot: Some("boot-a".to_string()),
            },
        );
        // A pin whose host boot is unknown carries no `b` field.
        push_write(
            &mut pipe,
            &Write::Pin {
                id_hex: "cd".repeat(16),
                slot: sid("gpu02", 0),
                at_ms: 9,
                boot: None,
            },
        );
        let cmds: Vec<Vec<String>> = pipe.cmd_iter().map(args).collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cmds,
            vec![
                s(&["HINCRBY", "routed:gpu01:3:1700", "req", "1"]),
                s(&["HINCRBY", "routed:gpu01:3:1700", "tok", "42"]),
                s(&["EXPIRE", "routed:gpu01:3:1700", "5"]),
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
                    "r",
                    "1",
                    "t",
                    "9",
                    "b",
                    "boot-a"
                ]),
                s(&[
                    "XADD",
                    "pins",
                    "MAXLEN",
                    "~",
                    "200000",
                    "*",
                    "k",
                    &"cd".repeat(16),
                    "h",
                    "gpu02",
                    "r",
                    "0",
                    "t",
                    "9"
                ]),
            ]
        );
        assert_eq!(replica_key("gpu01"), "replica:gpu01");
        // The kill switch sits under the router's readable `routed:*` prefix
        // and can never collide with a slot's counter key.
        assert!(KILL_SWITCH_KEY.starts_with("routed:"));
        assert_eq!(KILL_SWITCH_KEY.split(':').count(), 2);
    }

    /// A self-signed CA certificate, PEM.
    fn test_ca_pem() -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap().pem()
    }

    fn endpoint(tls: bool, ca_pem: Option<String>) -> ValkeyEndpoint {
        ValkeyEndpoint {
            host: "203.0.113.7".to_string(),
            port: 6379,
            tls,
            ca_pem,
        }
    }

    #[test]
    fn endpoint_url_names_the_router_user_and_scheme() {
        assert_eq!(
            endpoint(true, None).url(),
            "rediss://router@203.0.113.7:6379"
        );
        assert_eq!(
            endpoint(false, None).url(),
            "redis://router@203.0.113.7:6379"
        );
        let v6 = ValkeyEndpoint {
            host: "2001:db8::7".to_string(),
            ..endpoint(true, None)
        };
        assert_eq!(v6.url(), "rediss://router@[2001:db8::7]:6379");
    }

    #[test]
    fn configured_endpoint_builds_a_client() {
        install_crypto_provider();
        assert!(endpoint_client(&endpoint(true, Some(test_ca_pem())), "pw".into()).is_ok());
        assert!(endpoint_client(&endpoint(false, None), "pw".into()).is_ok());
    }

    /// A bad CA is a config error (placement stays off), never a panic and
    /// never a fallback to the platform's trust roots.
    #[test]
    fn bad_ca_is_a_config_error_not_a_panic() {
        install_crypto_provider();
        let err = |e: &ValkeyEndpoint| endpoint_client(e, "pw".into()).unwrap_err();
        assert_eq!(err(&endpoint(true, None)), "ca_missing");
        assert_eq!(
            err(&endpoint(true, Some("not a certificate".into()))),
            "ca_pem_empty"
        );
        let garbled = "-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n";
        assert!(endpoint_client(&endpoint(true, Some(garbled.into())), "pw".into()).is_err());
        assert!(client("not a url", None, None).is_err());
    }

    #[tokio::test]
    async fn start_with_bad_ca_is_inert() {
        let metrics = Arc::new(FakeMetrics::default());
        let hosts = Arc::new(ArcSwap::from_pointee(BackendHosts::default()));
        let io = PlacementIo::start(
            "secret".into(),
            &endpoint(true, Some("not a certificate".into())),
            hosts,
            Arc::new(ArcSwap::from_pointee(placement::Tuning::default())),
            metrics.clone(),
        );
        assert_eq!(io.snapshot.load().built_ms, 0);
        io.record(Write::Pin {
            id_hex: "00".repeat(16),
            slot: sid(HOST, 0),
            at_ms: 1,
            boot: None,
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
        let reg = registry_for(&[host.as_str()]);
        let now = now_ms();
        let _: () = redis::cmd("SET")
            .arg(replica_key(&host))
            .arg(sealed_json(&host_report(&host, 1, now, &[0, 1])))
            .arg("EX")
            .arg(5)
            .query_async(&mut conn)
            .await
            .unwrap();

        let pin = pin_id(
            Tier::Base,
            &AffinityKey::from_bytes(*uuid::Uuid::new_v4().as_bytes()),
            &[1u8; 32],
        );
        let routed_slot = sid(&host, 1);
        let mut pipe = redis::pipe();
        push_write(
            &mut pipe,
            &Write::Routed {
                slot: routed_slot.clone(),
                tok: 77,
                sec: now / 1000,
                ack: RoutedAck::default(),
            },
        );
        push_write(
            &mut pipe,
            &Write::Pin {
                id_hex: pin.to_hex(),
                slot: routed_slot.clone(),
                at_ms: now,
                boot: None,
            },
        );
        pipe.query_async::<()>(&mut conn).await.unwrap();

        let mut state = ReaderState::default();
        state.warm_up(warm_up(&mut conn).await.unwrap(), now);
        // Cycle 1 learns the host's slots from its frame; cycle 2 reads
        // their routed counters.
        let mut applied = None;
        for _ in 0..2 {
            let targets = ReadTargets::new(&reg, &state);
            let (targets, raw) = fetch(&mut conn, targets, &state, now).await.unwrap();
            applied = Some(state.apply(&targets, raw, &reg, now));
        }
        let applied = applied.unwrap();

        assert_eq!(applied.rejects, Vec::<Reject>::new());
        assert_eq!(applied.snapshot.replicas.len(), 2);
        // A disposable Valkey never holds the switch.
        assert!(!applied.snapshot.disabled);
        let rc = applied.snapshot.routed[&routed_slot];
        assert_eq!((rc.req, rc.tok), (1, 77));
        assert_eq!(
            applied.snapshot.pins.get(&pin, now),
            Some((&routed_slot, now))
        );
    }
}
