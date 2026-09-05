//! What this node is doing right now, in one snapshot.
//!
//! The worker used to say everything it knew through `println!` and nothing
//! else. That is fine for journald and useless for anything that wants to
//! ANSWER a question: is it connected, what is it serving, who is holding the
//! GPU. This module is the answer surface — a process-global snapshot the
//! worker keeps current as it works, plus the two ways to read it:
//!
//! * **in-process** — [`snapshot`], for the dashboard the node itself renders
//!   (`kmplify-node tui`) and for embedders like the desktop app;
//! * **out-of-process** — [`publish_loop`] writes the same snapshot to
//!   `status.json` in the node directory every couple of seconds, so
//!   `kmplify-node status` and a dashboard ATTACHED to a service-managed node
//!   read exactly what the running process believes.
//!
//! Nothing here listens on a port. That is a property of this crate people
//! rely on (see the crate docs), so the out-of-process path is a file in a
//! directory only the node's user can read, never a socket.
//!
//! # Cost
//!
//! Written on paths that run per job and per log line, so the hot fields are
//! atomics and the log ring has its own lock: a job never waits on the
//! dashboard, and the dashboard never blocks a job. The one `RwLock` holds
//! only slow-moving facts (link state, model list, identity).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// File name inside the node directory. Owner-readable only; it names the
/// gateway and the node id, which is enough to be worth not sharing.
pub const STATUS_FILE: &str = "status.json";

/// How often the running node rewrites `status.json`.
pub const PUBLISH_INTERVAL: Duration = Duration::from_secs(2);

/// A snapshot older than this describes a node that is no longer publishing:
/// killed, wedged, or stopped without cleaning up. Generous next to
/// `PUBLISH_INTERVAL` so a loaded machine is not called dead for missing one
/// write.
pub const STALE_AFTER: Duration = Duration::from_secs(15);

/// How many log lines the ring keeps, and therefore how much scrollback an
/// attached dashboard can show.
const LOG_RING: usize = 400;

/// How many of those travel in `status.json`. The whole ring would make the
/// file several times larger for every write; this is enough for the log pane
/// to be useful the moment it opens.
const LOG_PUBLISHED: usize = 120;

/// Where the node is in its connect/serve/reconnect cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    /// Process is up, first connection not attempted yet.
    #[default]
    Starting,
    Connecting,
    /// Connected and advertising. The only state in which work arrives.
    Online,
    /// Disconnected, waiting out the backoff before dialing again.
    Retrying,
    Stopping,
    Stopped,
}

impl Link {
    pub fn label(self) -> &'static str {
        match self {
            Link::Starting => "STARTING",
            Link::Connecting => "CONNECTING",
            Link::Online => "ONLINE",
            Link::Retrying => "RETRYING",
            Link::Stopping => "STOPPING",
            Link::Stopped => "STOPPED",
        }
    }
}

/// Inference work this node has taken since the process started.
///
/// Counters, not a history: a provider agent that kept a job log would be
/// keeping a record of what its consumers asked, which is exactly the data
/// this ecosystem promises not to accumulate. `last_model` is the one live
/// detail, and it is overwritten by the next job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Jobs {
    pub active: u64,
    /// Jobs that finished, however they finished — `failed` is a subset, not
    /// a second bucket. Anything else and the two counts drift apart the
    /// first time a job fails in a path that also completes.
    pub done: u64,
    pub failed: u64,
    /// Mean wall-clock of completed jobs, in ms.
    pub avg_ms: u64,
    pub last_ms: u64,
    pub last_model: String,
    pub functions: u64,
    pub vector_ops: u64,
}

/// The sharing configuration as the ENVIRONMENT resolved it, before the
/// operator's stored choices were layered on.
///
/// Published so the dashboard can show where each value came from, and so
/// clearing an override can show what it falls back to without waiting for
/// the node to reconnect and republish.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Baseline {
    pub share_inference: bool,
    pub share_cpu: bool,
    pub workloads: Vec<String>,
    pub approval_mode: String,
    pub country: String,
    pub colibri: String,
    pub max_cpus: Option<f64>,
    pub max_vram_mb: Option<u64>,
    pub max_ram_mb: Option<u64>,
    pub max_disk_gb: Option<u64>,
}

/// Work this node has actually delivered, since this process started.
///
/// For the operator's own eyes, and for a companion that wants to show
/// earnings next to effort. Explicitly **not** an accounting record and
/// explicitly **not** the basis of any payout: what a node claims about
/// itself cannot settle anything. The fabric's own signed receipts are the
/// attestation (see `docs/REWARDS.md`); these are the numbers that let an
/// operator notice when the two disagree.
///
/// Per-process on purpose. A lifetime total would be a ledger, and a ledger a
/// node keeps about its own earnings is exactly the thing nobody should trust
/// or maintain here.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Delivered {
    /// Inference jobs answered.
    pub jobs: u64,
    /// Wall-clock spent answering them, in ms.
    pub job_ms: u64,
    /// Signed Wasm functions run for a peer.
    pub functions: u64,
    /// Wall-clock those calls held this machine — the whole call (verify,
    /// fetch, sandbox), not the guest's own instruction time. What a
    /// provider gave up is the machine.
    pub function_ms: u64,
    /// Vector-collection operations served (upsert, query, delete, drop).
    pub vector_ops: u64,
    pub vector_ms: u64,
    /// Peer sessions hosted to completion.
    pub sessions: u64,
    /// Seconds those sessions held this machine.
    pub session_seconds: u64,
    /// When the count started, unix ms — the process start.
    pub since_ms: u64,
}

impl Delivered {
    /// Every call this node answered, whatever lane it came down.
    ///
    /// The headline number, because a node hosting only functions served
    /// plenty and used to report "0 jobs delivered" — which reads as a node
    /// nobody wanted.
    pub fn calls(&self) -> u64 {
        self.jobs + self.functions + self.vector_ops
    }

    /// Wall-clock spent on other people's work, in ms. Sessions are not in
    /// it: they are counted in seconds HELD, which is a different and much
    /// larger thing than compute time.
    pub fn compute_ms(&self) -> u64 {
        self.job_ms + self.function_ms + self.vector_ms
    }
}

/// A peer's container currently running on this machine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub template: String,
    pub container: String,
    pub state: String,
    pub cpus: f64,
    /// Unix seconds when this node accepted it.
    pub since: i64,
}

/// Everything the dashboard, `status` and an embedder's UI read.
///
/// Every field is `#[serde(default)]` on purpose: a dashboard from a newer
/// build routinely reads a `status.json` written by an older running node
/// (upgrade, then attach), and a missing field must degrade a panel rather
/// than fail the whole read.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// Schema of this file, bumped only on an incompatible change.
    pub schema: u32,
    pub version: String,
    pub pid: u32,
    pub node_id: String,
    pub gateway: String,
    pub link: Link,
    /// Why the link is where it is: the last connect error, or "".
    pub link_detail: String,
    pub started_at_ms: u64,
    pub connected_at_ms: u64,
    pub published_at_ms: u64,
    pub reconnects: u64,

    pub accelerator: String,
    pub gpu_name: String,
    pub vram_total_mb: u64,
    pub vram_used_mb: u64,
    pub cpu_model: String,
    pub cpus: f64,
    pub cpu_percent: f32,
    /// Per-logical-CPU load, 0-100. Empty when the sampler has not warmed up.
    pub per_core: Vec<f32>,
    /// How busy the accelerator is, 0-100, or `None` where the platform will
    /// not say (Metal, oneAPI). Absent is not zero, and a dashboard must not
    /// draw it as an idle card.
    pub gpu_percent: Option<u8>,
    /// Disk the fabric's own volumes are holding, in MB. Sampled rarely: the
    /// answer costs a `docker system df`.
    pub fabric_disk_mb: Option<u64>,
    pub ram_total_mb: u64,
    pub ram_used_mb: u64,
    /// Logical CPUs promised to peer sessions right now.
    pub reserved_cpus: f64,

    pub share_inference: bool,
    pub share_cpu: bool,
    /// Live pause, independent of `share_inference`: the owner hit pause in
    /// the dashboard. Advertises no models until resumed.
    pub paused: bool,
    pub approval_mode: String,
    pub country: String,
    pub workloads: Vec<String>,
    /// The operator's ceilings, as this worker is applying them. `None` is
    /// "no explicit choice", which is not the same as zero — see the fields
    /// on `WorkerConfig`.
    pub max_cpus: Option<f64>,
    pub max_vram_mb: Option<u64>,
    pub max_ram_mb: Option<u64>,
    pub max_disk_gb: Option<u64>,
    /// Colibri upstream, empty when none is configured. The key never
    /// travels in this file.
    pub colibri: String,
    /// The same fields as the environment alone resolved them.
    pub baseline: Baseline,

    pub models: Vec<String>,
    /// Non-default routing only, model -> upstream ("colibri").
    pub engines: BTreeMap<String, String>,
    pub sessions: Vec<Session>,
    pub jobs: Jobs,
    /// What this node has actually served, for the operator and for an
    /// optional rewards companion.
    pub delivered: Delivered,

    pub functions_enabled: bool,
    /// The catalog key this node trusts, or empty. Public by nature — it is a
    /// verification key — and a node that trusts none refuses every function,
    /// which an operator has to be able to SEE.
    pub functions_pubkey: String,
    pub vectors_enabled: bool,
    pub vectors_used_mb: u64,
    pub vectors_max_mb: u64,

    /// Tail of the log ring, oldest first.
    pub logs: Vec<String>,
}

impl Snapshot {
    /// Milliseconds this process has been up, from the reader's clock.
    pub fn uptime(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.started_at_ms))
    }

    /// How long the current connection has held, or `None` when offline.
    pub fn connected_for(&self) -> Option<Duration> {
        if self.link == Link::Online && self.connected_at_ms > 0 {
            Some(Duration::from_millis(
                now_ms().saturating_sub(self.connected_at_ms),
            ))
        } else {
            None
        }
    }

    /// Age of this reading. Zero for an in-process snapshot; grows once the
    /// writing process stops publishing.
    pub fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.published_at_ms))
    }

    /// Is the process that wrote this still publishing?
    pub fn is_fresh(&self) -> bool {
        self.published_at_ms > 0 && self.age() < STALE_AFTER
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- registry

fn slow() -> &'static RwLock<Snapshot> {
    static SLOW: OnceLock<RwLock<Snapshot>> = OnceLock::new();
    SLOW.get_or_init(|| {
        RwLock::new(Snapshot {
            schema: 1,
            version: crate::version_string().to_string(),
            pid: std::process::id(),
            started_at_ms: now_ms(),
            ..Default::default()
        })
    })
}

static JOBS_ACTIVE: AtomicU64 = AtomicU64::new(0);
static JOBS_DONE: AtomicU64 = AtomicU64::new(0);
static JOBS_FAILED: AtomicU64 = AtomicU64::new(0);
static JOBS_TOTAL_MS: AtomicU64 = AtomicU64::new(0);
static JOBS_LAST_MS: AtomicU64 = AtomicU64::new(0);
static FUNCTION_CALLS: AtomicU64 = AtomicU64::new(0);
static FUNCTION_MS: AtomicU64 = AtomicU64::new(0);
static VECTOR_OPS: AtomicU64 = AtomicU64::new(0);
static VECTOR_MS: AtomicU64 = AtomicU64::new(0);
static SESSIONS_HOSTED: AtomicU64 = AtomicU64::new(0);
static SESSION_SECONDS: AtomicU64 = AtomicU64::new(0);
static RECONNECTS: AtomicU64 = AtomicU64::new(0);
static QUIET: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);

fn ring() -> &'static Mutex<std::collections::VecDeque<String>> {
    static RING: OnceLock<Mutex<std::collections::VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(std::collections::VecDeque::with_capacity(LOG_RING)))
}

/// Edit the slow-moving half of the snapshot.
///
/// Poison-tolerant like [`crate::hostcpu::snapshot`], and for the same
/// reason: one panic in a reporting path must not take the node off the
/// fabric. Keep the closure short and never await inside it.
pub fn update(f: impl FnOnce(&mut Snapshot)) {
    let mut guard = match slow().write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    f(&mut guard);
}

fn read_slow() -> Snapshot {
    match slow().read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

/// The current state of this process, assembled from every source.
pub fn snapshot() -> Snapshot {
    let mut s = read_slow();
    let done = JOBS_DONE.load(Ordering::Relaxed);
    s.jobs = Jobs {
        active: JOBS_ACTIVE.load(Ordering::Relaxed),
        done,
        failed: JOBS_FAILED.load(Ordering::Relaxed),
        avg_ms: JOBS_TOTAL_MS
            .load(Ordering::Relaxed)
            .checked_div(done)
            .unwrap_or(0),
        last_ms: JOBS_LAST_MS.load(Ordering::Relaxed),
        last_model: s.jobs.last_model,
        functions: FUNCTION_CALLS.load(Ordering::Relaxed),
        vector_ops: VECTOR_OPS.load(Ordering::Relaxed),
    };
    s.delivered = Delivered {
        jobs: done,
        job_ms: JOBS_TOTAL_MS.load(Ordering::Relaxed),
        functions: FUNCTION_CALLS.load(Ordering::Relaxed),
        function_ms: FUNCTION_MS.load(Ordering::Relaxed),
        vector_ops: VECTOR_OPS.load(Ordering::Relaxed),
        vector_ms: VECTOR_MS.load(Ordering::Relaxed),
        sessions: SESSIONS_HOSTED.load(Ordering::Relaxed),
        session_seconds: SESSION_SECONDS.load(Ordering::Relaxed),
        since_ms: s.started_at_ms,
    };
    s.reconnects = RECONNECTS.load(Ordering::Relaxed);
    s.paused = paused();
    s.published_at_ms = now_ms();
    s
}

/// Move the link to `link`, recording `detail` as the reason.
pub fn set_link(link: Link, detail: impl Into<String>) {
    let detail = detail.into();
    update(|s| {
        if link == Link::Online && s.link != Link::Online {
            s.connected_at_ms = now_ms();
        }
        if link == Link::Retrying && s.link == Link::Online {
            RECONNECTS.fetch_add(1, Ordering::Relaxed);
        }
        s.link = link;
        s.link_detail = detail;
    });
}

/// What this node currently advertises.
pub fn set_models(models: &[String], engines: &std::collections::HashMap<String, String>) {
    update(|s| {
        s.models = models.to_vec();
        s.engines = engines
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    });
}

pub fn set_sessions(sessions: Vec<Session>) {
    update(|s| s.sessions = sessions);
}

/// Live pause: advertise nothing until resumed. Survives reconnects within
/// the process and is deliberately NOT persisted — a restarted node comes
/// back sharing, because the switch that outlives a restart is the config.
pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

pub fn set_paused(v: bool) {
    PAUSED.store(v, Ordering::Relaxed);
    update(|s| s.paused = v);
}

/// True while a full-screen dashboard owns the terminal, so [`crate::fabric_worker`]
/// logs into the ring instead of scribbling over it.
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn set_quiet(v: bool) {
    QUIET.store(v, Ordering::Relaxed);
}

/// Record one log line. Called from the worker's `log()`; keep it cheap.
pub fn push_log(line: impl Into<String>) {
    let line = format!("{} {}", clock_hms(now_ms()), line.into());
    let mut r = match ring().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if r.len() == LOG_RING {
        r.pop_front();
    }
    r.push_back(line);
}

/// The log ring, oldest first.
pub fn logs() -> Vec<String> {
    let r = match ring().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    r.iter().cloned().collect()
}

/// `HH:MM:SS` in UTC from a unix-ms stamp.
///
/// Hand-rolled rather than pulling in a date library for eight characters:
/// the log pane and the "refreshed" line are the only consumers and neither
/// needs a calendar.
pub fn clock_hms(ms: u64) -> String {
    let secs = ms / 1000 % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Which lane a piece of work came down. All three are work this machine did
/// for someone else, and all three belong in what it delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Chat and embeddings against the local model server.
    Inference,
    /// A signed Wasm function, in the sandbox.
    Function,
    /// An operation on a vector collection held here.
    Vector,
}

/// One piece of work, counted for as long as this guard lives.
///
/// RAII because every lane has early returns (bad model, dead upstream,
/// refused kind, unreadable manifest) and a counter that is only decremented
/// on the happy path reads as "12 jobs running" on an idle node forever.
/// A call that failed still held the machine, so it still counts as served;
/// `Jobs::failed` is what says how many went wrong.
pub struct JobGuard {
    started: std::time::Instant,
    lane: Lane,
}

impl JobGuard {
    /// An inference job for `model`.
    pub fn start(model: &str) -> Self {
        if !model.is_empty() {
            let model = model.to_string();
            update(|s| s.jobs.last_model = model);
        }
        Self::lane(Lane::Inference)
    }

    /// Work in one of the other lanes.
    pub fn lane(lane: Lane) -> Self {
        JOBS_ACTIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            started: std::time::Instant::now(),
            lane,
        }
    }
}

/// One job answered with an error frame.
///
/// Counted where the error is SENT rather than through the guard: the job
/// paths fail from a dozen places (refused kind, dead upstream, upstream
/// non-2xx) and all of them funnel through one send, so that is the only
/// place a count cannot drift from what the consumer was actually told.
pub fn count_job_error() {
    JOBS_FAILED.fetch_add(1, Ordering::Relaxed);
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let ms = self.started.elapsed().as_millis() as u64;
        JOBS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
        match self.lane {
            Lane::Inference => {
                JOBS_DONE.fetch_add(1, Ordering::Relaxed);
                JOBS_TOTAL_MS.fetch_add(ms, Ordering::Relaxed);
                // "Last job" stays the inference one: it is the figure a
                // provider reads to see their model server responding.
                JOBS_LAST_MS.store(ms, Ordering::Relaxed);
            }
            Lane::Function => {
                FUNCTION_CALLS.fetch_add(1, Ordering::Relaxed);
                FUNCTION_MS.fetch_add(ms, Ordering::Relaxed);
            }
            Lane::Vector => {
                VECTOR_OPS.fetch_add(1, Ordering::Relaxed);
                VECTOR_MS.fetch_add(ms, Ordering::Relaxed);
            }
        }
    }
}

/// One peer session ended after holding this machine for `seconds`.
///
/// Counted where the session is REMOVED rather than where it is asked to
/// stop: a container that died on its own, or that outlived a reconnect,
/// still held the hardware for that long.
pub fn count_session(seconds: u64) {
    SESSIONS_HOSTED.fetch_add(1, Ordering::Relaxed);
    SESSION_SECONDS.fetch_add(seconds, Ordering::Relaxed);
}

// ------------------------------------------------------------- publishing

pub fn status_path(node_dir: &Path) -> PathBuf {
    node_dir.join(STATUS_FILE)
}

/// How many publish ticks pass between accelerator readings.
///
/// Everything else in [`sample_host`] is an in-memory read; the GPU is a
/// subprocess (`nvidia-smi`, `rocm-smi`). Every four seconds is fast enough
/// for a graph to be worth looking at and slow enough that a node lending its
/// cycles is not spending them on being watched. A dashboard rendering the
/// node in its OWN process samples faster, because someone is looking at it.
const GPU_EVERY: u32 = 2;

/// Ticks between `docker system df` readings. Minutes, not seconds: it walks
/// the volume set, and the number moves when a model is downloaded rather
/// than continuously.
const DISK_EVERY: u32 = 30;

/// Fill in the live host figures the worker does not otherwise sample.
///
/// Separate from [`snapshot`] because these cost more than a lock and a
/// dashboard repainting at 4 Hz must not pay that on every frame.
pub async fn sample_host(accel: crate::gpu::Backend, with_gpu: bool) {
    let cpu = crate::hostcpu::snapshot();
    let (gpu_busy, vram_used) = if with_gpu {
        crate::gpu::utilization(accel).await
    } else {
        (None, None)
    };
    let reserved = crate::fabric_worker::reserved_cpus().await;
    let sessions: Vec<Session> = crate::fabric_worker::hosted_sessions()
        .await
        .into_iter()
        .map(|h| Session {
            session_id: h.session_id,
            template: h.template,
            container: h.container,
            state: h.state,
            cpus: h.cpus,
            since: h.since,
        })
        .collect();
    update(|s| {
        s.cpu_model = cpu.model;
        s.cpus = cpu.logical_cores as f64;
        s.cpu_percent = cpu.percent;
        if !cpu.per_core.is_empty() {
            s.per_core = cpu.per_core;
        }
        s.ram_total_mb = cpu.ram_total_mb;
        s.ram_used_mb = cpu.ram_used_mb;
        // Only a real reading replaces the last one: a skipped GPU tick must
        // leave the previous figures standing, not blank the graph.
        if let Some(v) = vram_used {
            s.vram_used_mb = v;
        }
        if gpu_busy.is_some() {
            s.gpu_percent = gpu_busy;
        }
        // Never below zero: the accounting lands on -0.0 when the last
        // session releases, and "-0 of 12 cores lent" reads as a bug. Not
        // `max(0.0)` — for two zeros that is allowed to return either one,
        // and it returns the negative.
        s.reserved_cpus = if reserved > 0.0 { reserved } else { 0.0 };
        s.sessions = sessions;
    });
}

/// Write `status.json` for as long as the node runs.
///
/// Owner-only, and atomic: readers poll this file, and a half-written
/// snapshot would show up as "node offline" for the duration of the write.
pub async fn publish_loop(
    node_dir: PathBuf,
    accel: crate::gpu::Backend,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let path = status_path(&node_dir);
    let tmp = path.with_extension("json.tmp");
    let mut tick: u32 = 0;
    loop {
        sample_host(accel, tick.is_multiple_of(GPU_EVERY)).await;
        if tick.is_multiple_of(DISK_EVERY) {
            let used = crate::fabric_worker::fabric_disk_used_mb().await;
            update(move |s| s.fabric_disk_mb = used);
        }
        tick = tick.wrapping_add(1);
        let snap = snapshot();
        write_snapshot(&path, &tmp, &snap).await;
        tokio::select! {
            _ = tokio::time::sleep(PUBLISH_INTERVAL) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    let mut last = snapshot();
                    last.link = Link::Stopped;
                    last.link_detail = "stopped".into();
                    write_snapshot(&path, &tmp, &last).await;
                    return;
                }
            }
        }
    }
}

async fn write_snapshot(path: &Path, tmp: &Path, snap: &Snapshot) {
    let mut snap = snap.clone();
    let all = logs();
    snap.logs = all
        .iter()
        .skip(all.len().saturating_sub(LOG_PUBLISHED))
        .cloned()
        .collect();
    let Ok(bytes) = serde_json::to_vec(&snap) else {
        return;
    };
    if write_owner_only(tmp, &bytes).await.is_err() {
        return;
    }
    let _ = tokio::fs::rename(tmp, path).await;
}

/// Write the snapshot so other local accounts cannot read it.
///
/// The file names the gateway, the node id and the last hundred log lines.
/// None of that is a credential — the node's token lives in `fabric_node.json`
/// and never appears here — but it is this machine's business and not the
/// business of every account on it.
async fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await?;
        f.write_all(bytes).await?;
        f.flush().await
    }
    #[cfg(not(unix))]
    {
        // Windows inherits the directory's ACL, which for the default
        // per-user node directory is already this user only. Deliberately
        // not the credential file's stricter treatment: nothing here is
        // usable to impersonate the node.
        tokio::fs::write(path, bytes).await
    }
}

/// Read a node's published status, or `None` when nothing has run here.
pub fn read_published(node_dir: &Path) -> Option<Snapshot> {
    read_published_result(node_dir).ok().flatten()
}

/// [`read_published`], keeping the reason it failed.
///
/// The reason matters in one specific, common case: a node installed as a
/// service publishes into its own state directory as its own user, and an
/// operator reading it as themselves gets `PermissionDenied` rather than
/// "nothing here". Telling those apart is what stops a dashboard from
/// cheerfully starting a SECOND node next to the one it could not see.
pub fn read_published_result(node_dir: &Path) -> std::io::Result<Option<Snapshot>> {
    match std::fs::read(status_path(node_dir)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Is ANOTHER process publishing from this directory right now?
///
/// The file is rewritten once a second by whoever publishes, and a read
/// that lands mid-replace fails (or parses as nothing) for a moment,
/// which on Windows is easy to hit with two writers. One transient miss
/// must not turn into a second node with the same identity, so the read
/// is tried a few times before "nobody is running here" is believed.
pub fn other_node_running(node_dir: &Path) -> Option<Snapshot> {
    for attempt in 0..5 {
        match read_published_result(node_dir) {
            Ok(Some(s)) if s.is_fresh() && s.pid != std::process::id() => return Some(s),
            Ok(Some(_)) => return None,
            Ok(None) if attempt == 0 && !status_path(node_dir).exists() => return None,
            _ => std::thread::sleep(Duration::from_millis(120)),
        }
    }
    None
}

/// Remove a stale `status.json` on a clean exit, so the next reader is not
/// told a node is running when none is.
pub fn clear_published(node_dir: &Path) {
    let _ = std::fs::remove_file(status_path(node_dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters are process-global, so the tests that assert deltas on
    /// them run one at a time. Without this they pass alone and fail
    /// together, which is the most expensive kind of green.
    fn counters() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    #[test]
    fn clock_formats_utc_hms() {
        assert_eq!(clock_hms(0), "00:00:00");
        assert_eq!(clock_hms(3_661_000), "01:01:01");
        // Wraps by day rather than growing a fourth field.
        assert_eq!(clock_hms(86_400_000 + 1000), "00:00:01");
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let mut s = Snapshot {
            schema: 1,
            node_id: "abc".into(),
            link: Link::Online,
            ..Default::default()
        };
        s.models.push("llama3".into());
        let text = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(back.node_id, "abc");
        assert_eq!(back.link, Link::Online);
        assert_eq!(back.models, vec!["llama3".to_string()]);
    }

    #[test]
    fn an_older_nodes_file_still_reads() {
        // Every field defaulted: a dashboard must not fail to attach because
        // the running node predates a field it knows about.
        let back: Snapshot = serde_json::from_str(r#"{"node_id":"old"}"#).unwrap();
        assert_eq!(back.node_id, "old");
        assert_eq!(back.link, Link::Starting);
        assert!(back.models.is_empty());
    }

    #[test]
    fn every_lane_lands_in_delivered() {
        let _serial = counters();
        // The regression this guards: a node hosting only functions reported
        // "0 jobs delivered", which reads as a node nobody wanted.
        let before = snapshot().delivered;
        drop(JobGuard::lane(Lane::Function));
        drop(JobGuard::lane(Lane::Vector));
        let after = snapshot().delivered;
        assert_eq!(after.functions, before.functions + 1);
        assert_eq!(after.vector_ops, before.vector_ops + 1);
        assert_eq!(after.calls(), before.calls() + 2);
        // Inference is untouched by the other lanes: the split still means
        // something.
        assert_eq!(after.jobs, before.jobs);
    }

    #[test]
    fn a_lane_that_is_not_inference_leaves_the_last_job_alone() {
        let _serial = counters();
        // "last model, in N ms" is the line a provider reads to see their
        // model server answering; a function call must not overwrite it.
        drop(JobGuard::start("llama3"));
        let after_inference = snapshot().jobs.last_ms;
        drop(JobGuard::lane(Lane::Function));
        assert_eq!(snapshot().jobs.last_ms, after_inference);
        assert_eq!(snapshot().jobs.last_model, "llama3");
    }

    #[test]
    fn work_in_flight_counts_whatever_lane_it_is_in() {
        let _serial = counters();
        let idle = snapshot().jobs.active;
        let guard = JobGuard::lane(Lane::Function);
        assert_eq!(
            snapshot().jobs.active,
            idle + 1,
            "a running function is work"
        );
        drop(guard);
        assert_eq!(snapshot().jobs.active, idle);
    }

    #[test]
    fn compute_time_adds_up_but_held_sessions_do_not() {
        // Sessions are counted in seconds HELD, which is a much larger thing
        // than compute; mixing them would flatter every provider.
        let d = Delivered {
            jobs: 2,
            job_ms: 300,
            functions: 3,
            function_ms: 60,
            vector_ops: 5,
            vector_ms: 40,
            sessions: 1,
            session_seconds: 3_600,
            since_ms: 0,
        };
        assert_eq!(d.calls(), 10);
        assert_eq!(d.compute_ms(), 400);
    }

    #[test]
    fn a_missing_publish_stamp_is_never_fresh() {
        let s = Snapshot::default();
        assert!(!s.is_fresh());
    }

    #[test]
    fn the_log_ring_keeps_the_tail() {
        for i in 0..(LOG_RING + 25) {
            push_log(format!("line {i}"));
        }
        let lines = logs();
        assert_eq!(lines.len(), LOG_RING);
        assert!(lines
            .last()
            .unwrap()
            .ends_with(&format!("line {}", LOG_RING + 24)));
    }
}
