//! KMPLIFY GPU Fabric provider worker.
//!
//! Dials OUT to a fabric gateway (the public fabric, or your own),
//! advertises what the local model server can serve, forwards inference
//! jobs to it, and optionally hosts container sessions on this machine's
//! GPU. Never listens on a port: one outbound WebSocket carries jobs,
//! telemetry and the HTTP relay alike.
//!
//! Runs either as the `kmplify-node` binary or as a background task inside
//! the KMPLIFY desktop app, which is why it is a library task rather than a
//! process of its own: it is pure I/O (one WebSocket, a couple of HTTP
//! calls), not worth a container and a second runtime per install.
//!
//! The trust model is in PROTOCOL.md and summarised in the README. The short
//! version, and the invariant to preserve when editing this file: the
//! gateway schedules, this file decides what the machine actually does.
//! Everything arriving over the socket is input to be validated, clamped or
//! refused, never an instruction to carry out.

use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

const RECONNECT_DELAY: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The gateway pings every 10s, so a healthy link is never silent for long.
/// A link that says nothing for this long is dead — typically a gateway
/// restart behind a proxy edge (Cloudflare) that keeps the client socket
/// open: `read.next()` never errors and never yields, and without this
/// deadline the worker sat "connected" for good while the gateway had long
/// forgotten it. 45s = four missed pings, comfortably past jitter.
const GATEWAY_SILENCE_TIMEOUT: Duration = Duration::from_secs(45);
/// How often a quiet image pull reports "still working" upstream.
const PULL_HEARTBEAT: Duration = Duration::from_secs(20);
/// Docker daemon liveness probe. Short: it runs on the telemetry path, and a
/// daemon that cannot answer in this long is not one that can pull an image.
const DOCKER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Telemetry refreshes between disk/image inventory samples. At a 10s ping
/// that is roughly once a minute — often enough to notice a large download,
/// rare enough that `docker system df` is not scanning a provider's disk
/// every ten seconds.
const SLOW_SAMPLE_EVERY: u8 = 6;
/// Longest silence tolerated from `docker pull`.
///
/// Sized from the real catalog rather than intuition: non-TTY docker prints
/// one line per layer TRANSITION, so a big layer is silent for its entire
/// download. ComfyUI's largest single layer is 0.86 GB, which on an ordinary
/// home uplink (~2.5 MB/s) is nearly six minutes without a word. The old
/// 5-minute limit therefore killed healthy pulls mid-layer — observed, and
/// misread at the time as the provider's Docker being wedged.
const PULL_STALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Cap on the pull as a WHOLE, shared across retries rather than applied per
/// attempt. Per-attempt caps multiplied by the retry count, so the node's
/// real budget was three times its stated one and could outlast the
/// gateway's own deadline — the same class of mistake as counting readiness
/// in loop iterations. ComfyUI is 6.4 GB; an hour covers that at ~1.8 MB/s,
/// and the consumer can abort at any point.
const PULL_TOTAL_TIMEOUT: Duration = Duration::from_secs(3600);

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

// ----- what this machine is hosting for other people, and how to end it ----
//
// The provider is not a bystander on their own hardware. If a peer's
// container is holding VRAM they want back — or a session has clearly gone
// wrong — waiting for that consumer to press Stop is not an answer, and
// killing the container behind the worker's back leaves the gateway
// advertising a session that answers 502 forever.
//
// So: a published snapshot of what is running here, and a control channel
// that feeds the SAME handler the gateway's own `workload_stop` takes. The
// container is removed and the consumer is told the session ended.

/// One container this node is running on a peer's behalf.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostedSession {
    pub session_id: String,
    pub template: String,
    pub container: String,
    /// Unix seconds when this node accepted the session.
    pub since: i64,
    /// pulling | starting | running — as this node last reported it.
    pub state: String,
    /// Cores this session is capped at (`docker run --cpus`), after the
    /// node's clamp. Surfaced so the provider can see what a peer is
    /// actually holding, not just what the gateway asked for.
    pub cpus: f64,
}

static HOSTED: std::sync::OnceLock<Arc<Mutex<Vec<HostedSession>>>> = std::sync::OnceLock::new();
static CONTROL: std::sync::OnceLock<tokio::sync::broadcast::Sender<Value>> =
    std::sync::OnceLock::new();

fn hosted_cell() -> &'static Arc<Mutex<Vec<HostedSession>>> {
    HOSTED.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

fn control() -> &'static tokio::sync::broadcast::Sender<Value> {
    CONTROL.get_or_init(|| tokio::sync::broadcast::channel(16).0)
}

/// The sink of the connection currently talking to the gateway.
///
/// Session tasks are spawned detached and legitimately outlive the socket
/// they were born on: a 28 GB image pull spans minutes, and the gateway link
/// can drop and re-establish underneath it. Each task captures its birth
/// connection's sink, so everything it reported after a reconnect — progress,
/// `running`, the final error — went into a dead socket and was silently
/// dropped, leaving the gateway showing whatever state it last heard.
/// send_frame() falls back to this cell when the captured sink is gone.
fn current_sink_cell() -> &'static tokio::sync::RwLock<Option<Arc<Mutex<WsSink>>>> {
    static CURRENT: std::sync::OnceLock<tokio::sync::RwLock<Option<Arc<Mutex<WsSink>>>>> =
        std::sync::OnceLock::new();
    CURRENT.get_or_init(|| tokio::sync::RwLock::new(None))
}

/// Sessions this PROCESS hosts, shared across gateway connections.
///
/// These were built per-connection, which quietly partitioned the state: a
/// session started before a reconnect was invisible to every handler on the
/// new connection. Relays answered "unknown session" for a container that
/// was up and serving, and `workload_stop` took its "nothing registered yet"
/// branch — a tombstone for a pull that had long finished — so the container
/// was never removed and kept holding the owner's GPU with nothing behind it.
fn sessions_cell() -> &'static Sessions {
    static SESSIONS_CELL: std::sync::OnceLock<Sessions> = std::sync::OnceLock::new();
    SESSIONS_CELL.get_or_init(|| Arc::new(Mutex::new(std::collections::HashMap::new())))
}

/// Stop tombstones, global for the same reason as sessions_cell(): a stop
/// that raced a pull must still be honored when the pull finishes on the
/// far side of a reconnect.
fn stopped_cell() -> &'static Stopped {
    static STOPPED_CELL: std::sync::OnceLock<Stopped> = std::sync::OnceLock::new();
    STOPPED_CELL.get_or_init(|| Arc::new(Mutex::new(std::collections::HashSet::new())))
}

/// What peers are running on this machine right now.
pub async fn hosted_sessions() -> Vec<HostedSession> {
    hosted_cell().lock().await.clone()
}

/// End a session this node is hosting, on the owner's say-so.
///
/// `Err` means no worker is connected, which is itself the answer the caller
/// needs: a `kmplify-fabric-*` container still running with no worker behind
/// it is an orphan, and removing it directly is then the correct repair.
pub fn request_stop(session_id: &str) -> Result<(), String> {
    control()
        .send(json!({"type": "workload_stop", "session": session_id}))
        .map(|_| ())
        .map_err(|_| {
            "GPU sharing is not running, so there is no session to ask it to stop".to_string()
        })
}

/// Resident models as the desktop UI should see them — same call, same
/// pinned-vs-expiring rule the worker publishes to the fabric, so the
/// provider's own screen can never disagree with what consumers are told.
pub async fn local_resident_models(ollama_base: &str) -> Vec<Value> {
    loaded_models(&reqwest::Client::new(), ollama_base).await
}

async fn hosted_add(session: &str, template: &str, container: &str, state: &str, cpus: f64) {
    let mut list = hosted_cell().lock().await;
    list.retain(|h| h.session_id != session);
    list.push(HostedSession {
        session_id: session.to_string(),
        template: template.to_string(),
        container: container.to_string(),
        since: chrono_now_secs(),
        state: state.to_string(),
        cpus,
    });
}

/// Cores currently promised to peer sessions on this machine.
///
/// Only live states count: a session still pulling has already had its cap
/// decided and will hold it the moment the container starts, so counting it
/// is honest; a stopped one holds nothing.
pub async fn reserved_cpus() -> f64 {
    hosted_cell()
        .lock()
        .await
        .iter()
        .filter(|h| matches!(h.state.as_str(), "pulling" | "starting" | "running"))
        .map(|h| h.cpus)
        .sum()
}

/// Logical cores this machine has, as the node sees them.
/// How much of the unified memory the GPU may actually address on Apple
/// Silicon, in MB. macOS caps GPU wired memory below the full pool: the
/// operator-tunable `iogpu.wired_limit_mb` when set, otherwise roughly 75%
/// of RAM (Metal's recommendedMaxWorkingSetSize heuristic — the same figure
/// Ollama plans against). A 64 GB M2 Max is therefore a ~48 GB GPU, and
/// advertising 64 would win it model placements it cannot hold.
pub fn gpu_addressable_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "iogpu.wired_limit_mb"])
            .output()
        {
            if let Ok(mb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                if mb > 0 {
                    return mb;
                }
            }
        }
    }
    host_ram_mb() * 3 / 4
}

/// Total system RAM in MB — on Apple Silicon this is the unified memory
/// pool, of which the GPU may address gpu_addressable_mb().
///
/// Reads through the sysinfo sampler (one OS call per platform) rather than
/// shelling out. The previous per-platform probes spawned `sysctl`,
/// `/proc/meminfo` and `wmic` — and `wmic` is REMOVED from Windows 11 24H2
/// and Server 2025, so every Windows provider on a current build advertised
/// `cpu_share.ram_mb: 0` and showed "0 GB" in the peer grid. Going through
/// sysinfo also drops a subprocess spawn from every hello, which on Windows
/// meant a console window flashing per probe (see proc.rs).
pub fn host_ram_mb() -> u64 {
    let sampled = crate::hostcpu::snapshot().ram_total_mb;
    if sampled > 0 {
        return sampled;
    }
    // Before the sampler's first timed refresh — the window the hello is
    // usually sent in.
    crate::hostcpu::read_ram_total_mb_now()
}

pub fn host_cpus() -> f64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(4.0)
}

async fn hosted_set_state(session: &str, state: &str) {
    if let Some(h) = hosted_cell()
        .lock()
        .await
        .iter_mut()
        .find(|h| h.session_id == session)
    {
        h.state = state.to_string();
    }
}

async fn hosted_remove(session: &str) {
    hosted_cell()
        .lock()
        .await
        .retain(|h| h.session_id != session);
}

/// Sink for user-facing worker events (a peer starting a session on this
/// machine, a session ending). The launcher turns these into OS
/// notifications; `None` keeps the worker usable headlessly, e.g. from
/// examples/fabric_smoke.rs.
///
/// A plain callback rather than a Tauri handle so this module stays
/// independent of the app shell and testable on its own.
pub type EventSink = Arc<dyn Fn(WorkerEvent) + Send + Sync>;

/// Something worth telling the user about. Sharing your GPU means other
/// people's work runs on your hardware — that must never be invisible.
#[derive(Debug, Clone)]
pub struct WorkerEvent {
    pub title: String,
    pub body: String,
}

/// Every field defaulted, so `..Default::default()` in a test or example
/// keeps compiling when a new one is added.
///
/// Three separate commits added a field here (max_shared_cpus,
/// max_shared_vram_mb, max_shared_disk_gb) and each one broke
/// examples/fabric_smoke.rs, because a struct literal must name them all.
/// The example is part of `cargo test`, so the branch failed to build every
/// time until someone noticed. #[non_exhaustive] does not help: it permits
/// omission only OUTSIDE the defining crate, and the example is inside it.
///
/// Written out rather than derived so the defaults are usable rather than
/// merely present: a derived String gateway_url would be "", which dials
/// nowhere and fails deep in a connect rather than at construction.
impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            gateway_url: crate::PUBLIC_FABRIC_URL.to_string(),
            ollama_base: "http://127.0.0.1:11434".to_string(),
            // Empty = no colibri upstream; the node behaves exactly as it
            // did before colibri existed.
            colibri_base: String::new(),
            colibri_api_key: String::new(),
            creds_path: PathBuf::new(),
            country: String::new(),
            // Empty = container sessions off. The safe default: running other
            // people's containers is opt-in everywhere else too.
            workload_templates: Vec::new(),
            max_shared_cpus: None,
            max_shared_vram_mb: None,
            max_shared_ram_mb: None,
            max_shared_disk_gb: None,
            share_inference: true,
            share_cpu: false,
            // None = report this crate's version, which is correct for
            // everything except an embedder with its own release cycle.
            client_version: None,
            approval_mode: "auto".to_string(),
            cuda: false,
            events: None,
        }
    }
}

#[derive(Clone)]
pub struct WorkerConfig {
    pub gateway_url: String,
    pub ollama_base: String,
    /// Optional second local upstream: a colibri `coli serve` gateway
    /// (protocol v2.5). Colibri streams frontier MoE models (284B-2.8T)
    /// from NVMe through RAM/VRAM, so what this node can lend is no longer
    /// bounded by its GPU. Empty = off; the models colibri serves are then
    /// advertised alongside Ollama's and routed there per model.
    pub colibri_base: String,
    /// Bearer for the colibri gateway, matching its COLI_API_KEY. Empty =
    /// no auth (the localhost default).
    pub colibri_api_key: String,
    pub creds_path: PathBuf,
    /// ISO-3166-1 alpha-2 declared on the hello frame, or "" to declare
    /// nothing (the gateway then records "XX"). See PROTOCOL.md — it is
    /// self-reported and unverifiable, a residency preference for consumers
    /// rather than an attestation.
    pub country: String,
    /// Template ids this node agrees to run as container sessions
    /// (vllm-openai, comfyui, ollama, …). Empty = sessions disabled; jobs
    /// (plain chat/embeddings against host Ollama) are unaffected. Only
    /// ever non-empty after the user opted in via the sharing settings —
    /// running other people's containers is a bigger grant than answering
    /// chat requests, so it is a separate switch.
    pub workload_templates: Vec<String>,
    /// Provider's own ceiling on the total logical CPUs peer sessions may
    /// hold, from the sharing settings. None = no explicit choice, so the
    /// node's half-the-host default applies.
    pub max_shared_cpus: Option<f64>,
    /// Provider's ceiling on VRAM lent to peers, in MB. Applied to the
    /// ADVERTISED vram_mb, which is what the gateway's fit checks and free
    /// VRAM accounting use — so lending less genuinely means peers can place
    /// less here, rather than being asked politely not to.
    pub max_shared_vram_mb: Option<u64>,
    /// Ceiling on system RAM advertised in the cpu_share block, from the
    /// sharing settings. None = advertise the machine's total.
    pub max_shared_ram_mb: Option<u64>,
    /// Disk in GB peers may fill with images and downloaded models, or None
    /// when the owner has not agreed to lend any. Unlike VRAM, this is not
    /// handed back when a session ends — a checkpoint downloaded into the
    /// model volume stays until someone deletes it.
    pub max_shared_disk_gb: Option<u64>,
    /// The "Share GPU inference" switch: advertise this node's Ollama
    /// models so the scheduler sends it chat/embedding jobs. Independent of
    /// share_cpu — with only CPU/RAM lending on, the worker connects but
    /// advertises no models and therefore receives no inference work.
    pub share_inference: bool,
    /// Lend spare CPU threads + system RAM to peers (unified memory on
    /// Apple Silicon). Advertised as the hello's `cpu_share` block so the
    /// grid can show general CPU/RAM capacity next to the GPU.
    pub share_cpu: bool,
    /// Peer-consumer admission: "auto" (anyone connects) or "manual" (the
    /// gateway parks unknown consumers until this provider approves them).
    /// Declared on the hello frame; invitations always bypass the gate.
    pub approval_mode: String,
    /// A working NVIDIA stack was detected on this host (nvidia-smi).
    /// Advertised to the gateway so CUDA templates schedule here.
    pub cuda: bool,
    /// What to report as this build's version on the hello frame.
    ///
    /// `None` reports this crate's version, which is what the standalone
    /// node wants. An embedder with its own release cycle (the KMPLIFY
    /// desktop app, whose provider mode is this same worker) sets its own
    /// version here: the question the field answers on the gateway is
    /// "which build is that peer running", and the only useful answer names
    /// the thing that was actually installed.
    pub client_version: Option<String>,
    /// Optional sink for user-facing events (see EventSink).
    pub events: Option<EventSink>,
}

impl WorkerConfig {
    fn emit(&self, title: impl Into<String>, body: impl Into<String>) {
        if let Some(sink) = &self.events {
            sink(WorkerEvent {
                title: title.into(),
                body: body.into(),
            });
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Credentials {
    pub node_id: String,
    pub token: String,
}

fn log(msg: impl std::fmt::Display) {
    println!("[fabric-worker] {msg}");
}

/// Register-or-load this install's ONE fabric identity — shared between
/// provider mode (sharing this GPU) and consumer mode (inference_source
///=peer): a node is its own customer and its own vendor as far as the
/// gateway's accounting is concerned, so there is exactly one
/// node_id/token pair per install,
/// per gateway, regardless of which role(s) it's currently playing.
/// Idempotent: safe to call on every boot.
pub async fn ensure_identity(gateway_url: &str, creds_path: &Path) -> Result<Credentials, String> {
    if let Ok(bytes) = tokio::fs::read(creds_path).await {
        if let Ok(c) = serde_json::from_slice::<Credentials>(&bytes) {
            return Ok(c);
        }
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{gateway_url}/fabric/register"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let creds: Credentials = resp.json().await.map_err(|e| e.to_string())?;
    if let Some(parent) = creds_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(creds_path, serde_json::to_vec(&creds).unwrap()).await;
    log(format!(
        "registered as anonymous node {}…",
        &creds.node_id[..8.min(creds.node_id.len())]
    ));
    Ok(creds)
}

/// Re-register with `gateway_url`, replacing whatever is stored at
/// `creds_path`.
///
/// For when a gateway rejects the stored credential. The stored pair is
/// still PRESENTED to `/fabric/register` (protocol v2.3): a gateway that
/// simply lost its registry re-adopts the old node_id under a fresh token,
/// which is what keeps invitations consumers hold against this node — they
/// are bound to the node_id — reconnectable instead of orphaned forever.
/// Only when the gateway refuses continuity (the id is live under someone
/// else's token) does this fall through to a brand-new identity.
/// `ensure_identity` deliberately does NOT do this: it must not churn a
/// working identity just because the gateway had a bad minute.
pub async fn register_identity(
    gateway_url: &str,
    creds_path: &Path,
) -> Result<Credentials, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let previous: Option<Credentials> = match tokio::fs::read(creds_path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    };
    let body = match &previous {
        Some(c) => serde_json::json!({
            "previous_node_id": c.node_id,
            "previous_token": c.token,
        }),
        None => serde_json::json!({}),
    };
    let creds: Credentials = client
        .post(format!("{gateway_url}/fabric/register"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(parent) = creds_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(creds_path, serde_json::to_vec(&creds).unwrap()).await;
    let kept = previous
        .as_ref()
        .is_some_and(|c| c.node_id == creds.node_id);
    log(format!(
        "re-registered as anonymous node {}… ({})",
        &creds.node_id[..8.min(creds.node_id.len())],
        if kept {
            "gateway re-adopted the previous identity — existing invitations stay valid"
        } else {
            "previous identity was rejected"
        }
    ));
    Ok(creds)
}

/// Local mirror of this node's minted invitations, next to the credential
/// file. The gateway is the source of truth while it remembers this node;
/// the mirror exists for the day it does not.
fn invitations_mirror_path(creds_path: &Path) -> PathBuf {
    creds_path.with_file_name("fabric_invitations.json")
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct MirroredInvitation {
    invitation_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    paused: bool,
}

/// Reconcile the gateway's invitation list with the local mirror, once per
/// connect (protocol v2.3).
///
/// Normal case: the gateway lists this node's invitations (revoked ones
/// included) — refresh the mirror with the live ones and stop. A gateway
/// that reports NO invitations at all while the mirror has some has lost
/// its registry for this node: re-assert each mirrored invitation via the
/// idempotent PUT, so consumers' stored invitation UUIDs keep working
/// instead of dying with the gateway's database.
async fn sync_invitations(
    client: &reqwest::Client,
    gateway_url: &str,
    creds_path: &Path,
    creds: &Credentials,
) -> Result<(), String> {
    let mirror_path = invitations_mirror_path(creds_path);
    let listed: Vec<Value> = client
        .get(format!(
            "{gateway_url}/fabric/invitations?include_revoked=true"
        ))
        .bearer_auth(&creds.token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    if listed.is_empty() {
        let mirrored: Vec<MirroredInvitation> = match tokio::fs::read(&mirror_path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if mirrored.is_empty() {
            return Ok(());
        }
        let mut restored = 0usize;
        for inv in &mirrored {
            let resp = client
                .put(format!(
                    "{gateway_url}/fabric/invitations/{}",
                    inv.invitation_id
                ))
                .bearer_auth(&creds.token)
                .json(&serde_json::json!({ "label": inv.label, "paused": inv.paused }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if resp.status().is_success() {
                restored += 1;
            } else {
                log(format!(
                    "invitation {}… not restored (gateway said {})",
                    &inv.invitation_id[..8.min(inv.invitation_id.len())],
                    resp.status()
                ));
            }
        }
        if restored > 0 {
            log(format!(
                "gateway had no invitations for this node — re-asserted {restored} from the local mirror"
            ));
        }
        return Ok(());
    }

    // Gateway remembers: it is the truth. Mirror the live (non-revoked)
    // contracts for the day it does not.
    let live: Vec<MirroredInvitation> = listed
        .iter()
        .filter(|v| !v["revoked"].as_bool().unwrap_or(false))
        .map(|v| MirroredInvitation {
            invitation_id: v["invitation_id"].as_str().unwrap_or_default().to_string(),
            label: v["label"].as_str().unwrap_or_default().to_string(),
            paused: v["paused"].as_bool().unwrap_or(false),
        })
        .filter(|m| !m.invitation_id.is_empty())
        .collect();
    if let Some(parent) = mirror_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&mirror_path, serde_json::to_vec_pretty(&live).unwrap())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Internal marker: the gateway closed the handshake with 4001 (unknown
/// node id / token). Distinct from every other connect failure because it is
/// the one that never resolves on its own.
const AUTH_REJECTED: &str = "auth-rejected";

async fn credentials(client: &reqwest::Client, cfg: &WorkerConfig) -> Result<Credentials, String> {
    let _ = client; // kept for signature stability at call sites below
    ensure_identity(&cfg.gateway_url, &cfg.creds_path).await
}

/// Model names out of Ollama's native `/api/tags` body.
///
/// A 200 without a `models` key yields an empty list rather than None: that is
/// Ollama answering "nothing pulled here", which is an ANSWER and not a reason
/// to go looking elsewhere. Kept behaviourally identical to the Python
/// reference worker's `local_models` — two implementations of one protocol
/// that disagree about discovery is the drift HEADLESS-NODE.md warns about.
fn models_from_native(body: &Value) -> Vec<String> {
    body.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Model ids out of an OpenAI `/v1/models` body (`{"data":[{"id":…}]}`).
///
/// `id` is taken verbatim: it is the string a consumer puts in a job's `model`
/// field and the string the gateway matches a node on, so rewriting it here —
/// stripping an org prefix, say — would advertise a name nothing can be routed
/// to.
fn models_from_openai(body: &Value) -> Vec<String> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id")?.as_str())
                .filter(|id| !id.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Everything this node can serve, in the names the scheduler matches on.
///
/// Two shapes, because `ollama_base` has quietly meant "any OpenAI-compatible
/// upstream" for a while and only this listing had not caught up: jobs already
/// execute against `/v1/chat/completions` and `/v1/embeddings` (see `run_job`),
/// which vLLM, LiteLLM and TGI all speak. Discovery was still Ollama-native,
/// and none of them serve `/api/tags` — so a node backed by any of them
/// connected happily, advertised nothing, and had every job it was offered
/// refused by the scheduler. Nothing logs that: online_nodes goes up,
/// inference_nodes does not, and the consumer just sees the peer-offline
/// dialog.
///
/// That is not hypothetical here. Dedicated fabric hosts run shared
/// vLLM+LMCache, and this binary is what runs on them.
///
/// Ollama keeps winning when it answers. It serves `/v1/models` too, so
/// preferring the native endpoint is not about capability — it is about
/// leaving every deployed desktop install on byte-for-byte the path it used
/// before this fallback existed.
pub async fn local_models(client: &reqwest::Client, ollama_base: &str) -> Vec<String> {
    let native = client.get(format!("{ollama_base}/api/tags")).send().await;
    let native_err = match native {
        // Status is checked explicitly: vLLM answers /api/tags with a JSON
        // 404 body, which PARSES fine and then has no `models` key — so
        // without this the fallback below was unreachable and the node
        // silently advertised nothing.
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(body) => return models_from_native(&body),
            Err(e) => format!("unreadable body: {e}"),
        },
        Ok(resp) => format!("http {}", resp.status()),
        Err(e) => e.to_string(),
    };

    match client.get(format!("{ollama_base}/v1/models")).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(body) => models_from_openai(&body),
            Err(e) => {
                log(format!(
                    "cannot list models at {ollama_base} — /api/tags: {native_err}; \
                     /v1/models: unreadable body: {e}"
                ));
                Vec::new()
            }
        },
        other => {
            // Both spellings named, because "which one did you mean to be
            // running?" is the actual question when a node advertises nothing.
            let compat_err = match other {
                Ok(resp) => format!("http {}", resp.status()),
                Err(e) => e.to_string(),
            };
            log(format!(
                "cannot list models at {ollama_base} — /api/tags: {native_err}; \
                 /v1/models: {compat_err}"
            ));
            Vec::new()
        }
    }
}

/// Models the local colibri gateway serves, `[]` when unset/unreachable.
///
/// Colibri speaks OpenAI's `/v1/models` only (it is not an Ollama), and an
/// unreachable colibri must not take the node down: the Ollama-served
/// models are still perfectly lendable, so this degrades to "advertise
/// what answers" exactly like `local_models` does.
pub async fn colibri_models(
    client: &reqwest::Client,
    colibri_base: &str,
    api_key: &str,
) -> Vec<String> {
    if colibri_base.is_empty() {
        return Vec::new();
    }
    let mut req = client.get(format!("{colibri_base}/v1/models"));
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(body) => models_from_openai(&body),
            Err(e) => {
                log(format!(
                    "cannot list colibri models at {colibri_base} — unreadable body: {e}"
                ));
                Vec::new()
            }
        },
        Ok(resp) => {
            log(format!(
                "cannot list colibri models at {colibri_base} — http {}",
                resp.status()
            ));
            Vec::new()
        }
        Err(e) => {
            log(format!(
                "cannot list colibri models at {colibri_base} — {e}"
            ));
            Vec::new()
        }
    }
}

/// (all models this node serves, {model: "colibri"} routing overrides).
///
/// The engines map carries ONLY the non-default entries: everything absent
/// routes to the primary upstream as it always has, which is also what
/// keeps the hello frame identical for nodes without a colibri. A model id
/// served by BOTH upstreams stays on the primary — same id, same weights,
/// and the primary path is the one every existing consumer exercised.
pub async fn discover(
    client: &reqwest::Client,
    cfg: &WorkerConfig,
) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let primary = local_models(client, &cfg.ollama_base).await;
    let colibri = colibri_models(client, &cfg.colibri_base, &cfg.colibri_api_key).await;
    let mut engines = std::collections::HashMap::new();
    let mut all = primary.clone();
    for m in colibri {
        if !primary.contains(&m) {
            engines.insert(m.clone(), "colibri".to_string());
            all.push(m);
        }
    }
    (all, engines)
}

/// Total VRAM of GPU 0 in MB via nvidia-smi, or None. The one number the
/// fabric needs for VRAM-fit filtering — without it this node advertised
/// `vram_mb: 0` and could never satisfy any consumer's fit check.
/// The GPU model string ("NVIDIA GeForce RTX 4090"), or None. Shared on the
/// hello so consumers can pick capacity by hardware in the peer GPU grid —
/// a product decision superseding the earlier always-"anonymous" stance:
/// the MODEL is coarse, mass-produced hardware info; node identity and any
/// geo detail beyond the country code stay unpublished.
async fn nvidia_gpu_name() -> Option<String> {
    let out = crate::proc::command("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// Apple Silicon chip name ("Apple M3 Max"), or None.
async fn apple_chip_name() -> Option<String> {
    let out = crate::proc::command("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Total VRAM of the first NVIDIA GPU in MB, or `None` without a working
/// nvidia-smi.
///
/// Public because it is useful to anything sizing work to the local card,
/// not only to the worker: the KMPLIFY desktop app picks a coding-model
/// profile with it.
pub async fn nvidia_vram_mb() -> Option<u64> {
    let out = crate::proc::command("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|v| v as u64)
}

/// VRAM currently in use (nvidia-smi memory.used), or None. Reported with
/// every pong so the gateway's capacity view tracks what this machine's own
/// task manager shows — including VRAM eaten by things that are not fabric
/// sessions at all (games, local inference, browsers).
async fn nvidia_used_mb() -> Option<u64> {
    let out = crate::proc::command("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|v| v as u64)
}

/// Models currently RESIDENT in VRAM on the host Ollama (`/api/ps`), as
/// `[{name, size_vram_mb, expires_in_s}]`.
///
/// Reported with every pong so a 24 GB card showing 8 GB consumed stops
/// being a mystery: without this, neither the provider's own UI nor a
/// consumer could tell "a 7B model is parked with 25 minutes left on its
/// keep_alive" from "some other app ate the card". Names only — the same
/// model list this node already advertises, so it leaks nothing new.
async fn loaded_models(client: &reqwest::Client, ollama_base: &str) -> Vec<Value> {
    let Ok(resp) = client.get(format!("{ollama_base}/api/ps")).send().await else {
        return Vec::new();
    };
    let Ok(body) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let now = chrono_now_secs();
    body.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?.to_string();
                    let vram = m.get("size_vram").and_then(Value::as_u64).unwrap_or(0) / 1_048_576;
                    // expires_at is RFC3339; a plain seconds-remaining value
                    // saves every consumer from parsing timestamps.
                    // Ollama expresses "pinned, never unload" (keep_alive: -1)
                    // as a sentinel far-future timestamp — observed live as
                    // 2318-11-06. Reporting the raw difference produced a
                    // 292-year countdown ("frees in 153722812 min"), so
                    // anything beyond a year means "no expiry" and is sent as
                    // null, which every consumer already renders as no
                    // countdown at all.
                    const PINNED_AFTER_S: i64 = 365 * 24 * 3600;
                    let expires_in = m
                        .get("expires_at")
                        .and_then(Value::as_str)
                        .and_then(rfc3339_secs)
                        .map(|t| (t - now).max(0))
                        .filter(|secs| *secs < PINNED_AFTER_S);
                    Some(json!({
                        "name": name,
                        "size_vram_mb": vram,
                        "expires_in_s": expires_in,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Seconds since the Unix epoch, without pulling in a date crate.
fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Minimal RFC3339 -> epoch seconds. Ollama emits e.g.
/// "2026-07-27T11:04:12.123456789+02:00"; only the fields below matter and
/// a malformed value simply yields None (the UI then omits the countdown).
fn rfc3339_secs(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { t.get(a..z)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days since epoch via the civil-from-days algorithm (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut epoch = days * 86_400 + h * 3600 + mi * 60 + sec;
    // Trailing offset (+02:00 / -05:00 / Z) — ignoring it would make every
    // countdown wrong by the provider's timezone.
    if let Some(idx) = t.rfind(['+', '-']) {
        if idx > 18 {
            if let (Some(oh), Some(om)) = (
                t.get(idx + 1..idx + 3).and_then(|v| v.parse::<i64>().ok()),
                t.get(idx + 4..idx + 6).and_then(|v| v.parse::<i64>().ok()),
            ) {
                let off = oh * 3600 + om * 60;
                epoch += if t.as_bytes()[idx] == b'+' { -off } else { off };
            }
        }
    }
    Some(epoch)
}

/// Real GPU capability probe. The MODEL name ("NVIDIA GeForce RTX 4090",
/// "Apple M3 Max") is shared so consumers can pick capacity by hardware in
/// the peer GPU grid. Node identity stays anonymous — a mass-produced card
/// model plus a country code identifies nobody.
async fn gpu_info(cuda: bool, max_shared_vram_mb: Option<u64>) -> Value {
    if cuda {
        let vram = nvidia_vram_mb().await.unwrap_or(0);
        // Advertise only what the operator agreed to lend. Never MORE than
        // the card holds, whatever the setting says — over-advertising would
        // win the node sessions it then cannot run.
        let vram = match max_shared_vram_mb {
            Some(cap) if cap > 0 => vram.min(cap),
            _ => vram,
        };
        let name = nvidia_gpu_name()
            .await
            .unwrap_or_else(|| "NVIDIA GPU".to_string());
        return json!({ "backend": "cuda", "name": name, "vram_mb": vram });
    }
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        let name = apple_chip_name()
            .await
            .unwrap_or_else(|| "Apple Silicon".to_string());
        // Advertise what the GPU can actually address (iogpu limit / ~75%
        // of unified memory), capped by the operator's ceiling — not the
        // full RAM, which the GPU can never hold.
        let mut vram = gpu_addressable_mb();
        if let Some(cap) = max_shared_vram_mb {
            if cap > 0 {
                vram = vram.min(cap);
            }
        }
        return json!({ "backend": "metal", "name": name, "vram_mb": vram });
    }
    // CPU-only host. The name used to be the literal string "CPU", which is
    // why the peer grid showed a card titled "CPU" with no hardware behind
    // it while every GPU peer showed a real model. A CPU model is the same
    // category of information the fabric already publishes for GPUs
    // ("hardware model and country, never who the provider is"), so there is
    // nothing new disclosed here — only the vagueness removed.
    let model = crate::hostcpu::snapshot().model;
    let name = if model.trim().is_empty() {
        "CPU".to_string()
    } else {
        model
    };
    json!({ "backend": "cpu", "name": name, "vram_mb": 0 })
}

/// Static CPU/RAM facts for the hello frame.
///
/// Sent by EVERY node, not just those lending CPU: a consumer comparing
/// peers wants to know what the machine is, and `cpu_share` only says what
/// was volunteered. Cores do not change at runtime, so this rides the hello
/// rather than the pong; live load/usage go with the pong instead.
fn cpu_info() -> Value {
    let c = crate::hostcpu::snapshot();
    // The sampler publishes model/cores immediately but only fills RAM after
    // its first timed refresh a second later — and hello goes out before
    // that, which advertised `ram_total_mb: 0` and rendered as "0 GB RAM"
    // for the whole session. These synchronous helpers are the same ones
    // cpu_share already uses, so they are known-good at this instant; the
    // sampler's values win once they exist.
    let threads = if c.logical_cores > 0 {
        c.logical_cores as f64
    } else {
        host_cpus()
    };
    let ram_total_mb = if c.ram_total_mb > 0 {
        c.ram_total_mb
    } else {
        host_ram_mb()
    };
    if threads <= 0.0 && ram_total_mb == 0 && c.model.trim().is_empty() {
        // Nothing readable at all — say nothing rather than publishing a row
        // of zeros that would render as "0 cores · 0 GB".
        return Value::Null;
    }
    // Core counts go out as INTEGERS. host_cpus() is an f64 (docker's --cpus
    // takes fractions), and passing it through unrounded published "12.0",
    // which is a count no machine has.
    json!({
        "model": c.model,
        // physical_cores falls back to logical inside the sampler, so it is
        // never 0 once sampled; guard anyway for the pre-sample instant.
        "cores": if c.physical_cores > 0 { c.physical_cores as u64 } else { threads.round() as u64 },
        "threads": threads.round() as u64,
        "ram_total_mb": ram_total_mb,
    })
}

/// The `workloads` capability block of the hello frame — mirrors
/// The `workloads` block of the hello frame (PROTOCOL.md).
/// Is the Docker daemon actually usable right now?
///
/// `docker version` talks to the SERVER, not just the CLI — the failure this
/// catches is Docker Desktop being installed but not running, which reports
/// exactly the error a consumer saw: "failed to connect to the docker API at
/// npipe:////./pipe/dockerDesktopLinux".
async fn docker_ok() -> bool {
    let probe = crate::proc::command("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output();
    matches!(
        tokio::time::timeout(DOCKER_PROBE_TIMEOUT, probe).await,
        Ok(Ok(o)) if o.status.success()
    )
}

/// What this node offers as container sessions.
///
/// Gated on a live Docker daemon. Advertising templates while Docker is down
/// is worse than advertising nothing: the gateway's scheduler prefers the
/// nearest capable peer, so it routes sessions PREFERENTIALLY to a node where
/// every one of them fails at the first pull — and the consumer reads that as
/// the fabric being broken. Plain inference jobs need only Ollama, so they are
/// deliberately unaffected: the machine stays useful for chat while saying
/// honestly that it cannot host containers right now.
fn workload_capability(
    cfg: &WorkerConfig,
    docker_ok: bool,
    disk_used_mb: Option<u64>,
    images: &[String],
    inventory_error: Option<&str>,
) -> Value {
    let usable = docker_ok && !cfg.workload_templates.is_empty();
    let mut caps = json!({
        "enabled": usable,
        "cuda": cfg.cuda,
        "templates": if usable { cfg.workload_templates.clone() } else { Vec::new() },
    });
    // The disk the owner agreed to lend, and what is already spent of it.
    // Absent when they have not agreed to any — the gateway then treats this
    // node as unbounded, which is the pre-existing behaviour, rather than as
    // having zero.
    if let Some(cap_gb) = cfg.max_shared_disk_gb {
        caps["disk_cap_mb"] = json!(cap_gb * 1024);
    }
    if let Some(used) = disk_used_mb {
        caps["disk_used_mb"] = json!(used);
    }
    // Which template images are already here. The scheduler prefers a
    // provider that needs no pull: that is the difference between a session
    // starting in seconds and the consumer waiting out 6.4 GB while the
    // provider pays for the bandwidth.
    if !images.is_empty() {
        caps["images"] = json!(images);
    }
    // Say WHY the numbers above are missing. Absent-because-unset and
    // absent-because-we-could-not-look are different facts, and a provider
    // deserves to see the second one rather than wonder why their cap does
    // nothing. Flows straight through to GET /fabric/nodes, which carries
    // this object verbatim.
    if let Some(err) = inventory_error {
        caps["inventory_error"] = json!(err);
    }
    caps
}

// ----- container sessions (workload_* / http frames) -----------------------
//
// Container sessions + the HTTP relay: the gateway asks this node to
// run a whole container (vLLM / ComfyUI / Ollama) on its GPU, then tunnels
// consumer HTTP requests to it over this same WebSocket — the node never
// opens an inbound port.

/// Live sessions on this node: session id -> (container name, host port).
type Sessions = Arc<Mutex<std::collections::HashMap<String, (String, u16)>>>;

/// Last sampled host telemetry, refreshed off the critical path so the
/// ping/pong round trip stays a measure of the NETWORK (see the ping
/// handler). One sample old at worst.
#[derive(Default)]
struct Telemetry {
    gpu_used_mb: Option<u64>,
    loaded_models: Vec<Value>,
    /// Model names to advertise. Cached here for the same reason as the rest:
    /// the read loop must never wait on Ollama.
    models: Vec<String>,
    /// Whether the Docker daemon answered on the last sample. Cached like
    /// everything else here: the read loop must never wait on a subprocess.
    docker_ok: bool,
    /// Disk the fabric's volumes occupy, MB. None until first sampled.
    disk_used_mb: Option<u64>,
    /// Images already on this machine, so the gateway can prefer a provider
    /// that needs no pull.
    images: Vec<String>,
    /// Ticks since the last heavy sample (disk + images). Both are far more
    /// expensive than the per-ping telemetry and change slowly.
    slow_tick: u8,
    /// Why the last inventory sample failed, if it did. Advertised so the
    /// difference between "shares everything" and "cannot tell you" is
    /// visible to the consumer and to the owner, instead of both arriving
    /// as an absent number.
    inventory_error: Option<String>,
}

/// Sessions stopped BEFORE their container registered in `Sessions` — i.e.
/// a `workload_stop` that lands mid-image-pull. Without this tombstone the
/// stop was a silent no-op (nothing in the map yet to remove), the pull ran
/// to completion and `docker run` started a container nobody tracked until
/// the next disconnect — a leaked GPU container on the provider.
type Stopped = Arc<Mutex<std::collections::HashSet<String>>>;

/// Standard base64 (RFC 4648, with padding) — matches Python's b64encode /
/// b64decode for the gateway relay. Hand-rolled to keep
/// this codebase dependency-light (same reasoning as ecosystem.rs's
/// b64url_decode).
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in input.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Is this a mount this node is willing to make?
///
/// A fabric-namespaced NAMED volume onto an absolute path — never a host
/// path. The gateway is trusted to schedule, not to reach into the
/// provider's filesystem, and `-v /:/host` is one careless template away.
fn is_fabric_volume(name: &str, target: &str) -> bool {
    name.starts_with("kmplify-fabric-")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && target.starts_with('/')
        && !target.contains(':')
}

/// The image REPOSITORY each template may run, decided here rather than by
/// whoever is on the other end of the socket.
///
/// The template id was already re-validated against the operator's opt-in
/// list, but the id and the image travelled in the same frame: enabling
/// `ollama` meant accepting whatever image the gateway attached to that name.
/// `--gpus all` on a stranger's hardware is exactly the wrong place to take
/// something on trust, and cap-drop plus no-new-privileges do not inconvenience
/// a miner in the slightest.
///
/// A repository, not a full reference, because tags legitimately move: the
/// ComfyUI template was repinned cu124 to cu126 in flight, and a node pinned
/// to the exact string would have refused every session until it was rebuilt.
/// The tag and digest are free, the publisher is not.
const IMAGE_PINS: &[(&str, &str)] = &[
    ("vllm-openai", "vllm/vllm-openai"),
    ("vllm-openai-lmcache", "lmcache/vllm-openai"),
    ("comfyui", "yanwk/comfyui-boot"),
    ("ollama", "ollama/ollama"),
    ("ollama-cpu", "ollama/ollama"),
    ("echo-test", "traefik/whoami"),
];

/// Operator-supplied pins, `template=repository` separated by commas.
///
/// Anyone running their own gateway has their own catalog, and the built-in
/// table would otherwise make this node refuse every one of their templates.
/// Deliberately an explicit, greppable opt-in: setting it says "I trust that
/// gateway's catalog", which is a sentence a provider should have to write
/// down rather than inherit by default.
fn extra_image_pins() -> &'static [(String, String)] {
    static EXTRA: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    EXTRA
        .get_or_init(|| {
            std::env::var("KMPLIFY_FABRIC_EXTRA_IMAGE_PINS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|entry| entry.split_once('='))
                .map(|(t, repo)| (t.trim().to_string(), repo.trim().to_string()))
                .filter(|(t, repo)| !t.is_empty() && !repo.is_empty())
                .collect()
        })
        .as_slice()
}

/// An image reference reduced to its repository: no registry host, no tag,
/// no digest.
///
/// Docker Hub is spelled four ways for the same thing (`ollama/ollama`,
/// `docker.io/ollama/ollama`, `index.docker.io/...`, and `library/` for
/// official images), so all of them have to land on one string before any
/// comparison is worth making.
fn image_repository(image: &str) -> String {
    let base = image.split_once('@').map_or(image, |(b, _)| b);
    // A leading segment is a registry only if it looks like a host. Without
    // this, `vllm/vllm-openai` would lose `vllm` as if it were one.
    let (host, rest) = match base.split_once('/') {
        Some((h, r)) if h.contains('.') || h.contains(':') || h == "localhost" => (Some(h), r),
        _ => (None, base),
    };
    // The host is already split off, so any remaining colon starts the tag.
    let repo = rest.rsplit_once(':').map_or(rest, |(r, _)| r);
    match host {
        Some("docker.io") | Some("index.docker.io") | None => {
            repo.strip_prefix("library/").unwrap_or(repo).to_string()
        }
        Some(h) => format!("{h}/{repo}"),
    }
}

/// May this node pull and run `image` for `template`?
///
/// Fails closed: a template with no pin at all is refused rather than waved
/// through, so a gateway cannot introduce a new template id to an old node
/// and have it run whatever comes attached.
fn image_allowed_for(template: &str, image: &str) -> bool {
    let repo = image_repository(image);
    IMAGE_PINS
        .iter()
        .any(|(t, pinned)| *t == template && repo == *pinned)
        || extra_image_pins()
            .iter()
            .any(|(t, pinned)| t == template && &repo == pinned)
}

/// Remove a session container and everything it owns, and CHECK that it went.
///
/// `-v` takes the anonymous volumes with it — an image that declares
/// VOLUME (ComfyUI declares /root) gets a fresh one per launch, and without
/// this they accumulate on the provider's disk forever. Named volumes are
/// deliberately untouched: those are the install and model stores that exist
/// precisely to outlive a session.
///
/// The result used to be discarded. A failed removal then still reported
/// "stopped" upstream, so the consumer and the provider both believed the
/// GPU had been released while the container was still holding it. Ending a
/// session has to be true, not optimistic.
/// `Some(exit_code)` once the container is no longer running; `None` while it
/// is alive (or still being created). A container docker no longer knows at
/// all reports code -1 — dead is dead, even when the code is unknowable.
async fn container_exit(name: &str) -> Option<i64> {
    let out = crate::proc::command("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Status}} {{.State.ExitCode}}",
            name,
        ])
        .output()
        .await
        .ok()?;
    parse_container_state(out.status.success(), &String::from_utf8_lossy(&out.stdout))
}

/// Pure half of `container_exit`, split out for tests.
fn parse_container_state(inspect_ok: bool, stdout: &str) -> Option<i64> {
    if !inspect_ok {
        return Some(-1);
    }
    let mut parts = stdout.split_whitespace();
    let status = parts.next().unwrap_or("");
    let code = parts
        .next()
        .and_then(|c| c.parse::<i64>().ok())
        .unwrap_or(-1);
    match status {
        // "created" is pre-start, "restarting" is docker still trying —
        // neither is a verdict yet.
        "running" | "created" | "restarting" => None,
        _ => Some(code),
    }
}

/// The last log lines of a container, for the error a consumer sees when a
/// session dies before becoming ready. Captured BEFORE the container is
/// removed — afterwards there is nothing to ask.
async fn container_log_tail(name: &str, lines: u32) -> String {
    let out = crate::proc::command("docker")
        .args(["logs", "--tail", &lines.to_string(), name])
        .output()
        .await;
    let combined = match out {
        Ok(o) => {
            // vLLM and friends log almost everything to stderr; take both.
            let mut txt = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !txt.is_empty() {
                    txt.push('\n');
                }
                txt.push_str(err.trim());
            }
            txt
        }
        Err(_) => String::new(),
    };
    trim_log_tail(&combined, 900)
}

/// Keep the END of the log — the crash is at the bottom, and a gateway
/// status message is not the place for 28 MB of startup banner.
fn trim_log_tail(txt: &str, max_chars: usize) -> String {
    let t = txt.trim();
    if t.is_empty() {
        return "(no output captured)".into();
    }
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    let tail: String = t
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

async fn remove_container(name: &str) -> Result<(), String> {
    let out = crate::proc::command("docker")
        .args(["rm", "-f", "-v", name])
        .output()
        .await
        .map_err(|e| format!("docker unavailable: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // Already gone is success, not failure: --rm may have beaten us to it.
    if err.contains("No such container") {
        return Ok(());
    }
    Err(err)
}

/// Sample both inventories, keeping the first failure's reason.
async fn sample_inventory() -> (Option<u64>, Vec<String>, Option<String>) {
    let (disk, disk_err) = match measure_fabric_disk().await {
        Ok(mb) => (Some(mb), None),
        Err(e) => (None, Some(e)),
    };
    let (imgs, img_err) = match cached_images().await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    (disk, imgs, disk_err.or(img_err))
}

/// Template images already pulled on this machine.
///
/// The cheapest possible scheduling signal and by far the most valuable: a
/// provider that already has ComfyUI's 6.4 GB image starts a session in
/// seconds, while a cold one makes the consumer wait out the pull and costs
/// the provider the bandwidth and the disk. `docker images` is a metadata
/// read — no container spawn, no filesystem walk.
async fn cached_images() -> Result<Vec<String>, String> {
    let out = crate::proc::command("docker")
        .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
        .output()
        .await
        .map_err(|e| format!("docker unavailable: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker images failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l != "<none>:<none>")
        .collect())
}

/// How much disk the fabric's own volumes are using, in MB.
///
/// Measured from `docker system df`, which reports per-volume sizes without
/// walking the filesystem ourselves. Only `kmplify-fabric-*` volumes count:
/// the provider's own stack data is theirs and is none of a peer's business.
///
/// Sampled on the slow telemetry path rather than per ping — `system df`
/// scans, and doing that every ten seconds on a provider's machine would be
/// its own kind of rudeness.
pub async fn fabric_disk_used_mb() -> Option<u64> {
    measure_fabric_disk().await.ok()
}

/// The same measurement, with the reason it failed.
///
/// Every failure used to collapse to None, which the gateway and the UI read
/// as "this provider set no limit" — identical to the healthy unset case. So
/// a provider whose docker CLI could not answer looked exactly like one who
/// had chosen to share everything, and there was nothing anywhere to say
/// otherwise. Silence is the one thing a capacity signal must never be.
async fn measure_fabric_disk() -> Result<u64, String> {
    let out = crate::proc::command("docker")
        .args(["system", "df", "-v", "--format", "{{json .Volumes}}"])
        .output()
        .await
        .map_err(|e| format!("docker unavailable: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker system df failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    let vols: Vec<Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("could not read docker's volume report: {e}"))?;
    let mut total = 0u64;
    for v in vols {
        let name = v.get("Name").and_then(|n| n.as_str()).unwrap_or("");
        if !name.starts_with("kmplify-fabric-") {
            continue;
        }
        if let Some(size) = v.get("Size").and_then(|s| s.as_str()) {
            total += parse_docker_size_mb(size);
        }
    }
    Ok(total)
}

/// Docker prints human sizes ("48.15MB", "294.9kB", "1.2GB"). SI units, and
/// anything unrecognised counts as zero rather than as a wild number — a
/// misread here would either hide a full disk or refuse a healthy provider.
fn parse_docker_size_mb(s: &str) -> u64 {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let Ok(n) = num.trim().parse::<f64>() else {
        return 0;
    };
    let mb = match unit.trim() {
        "B" => n / 1_000_000.0,
        "kB" | "KB" => n / 1_000.0,
        "MB" => n,
        "GB" => n * 1_000.0,
        "TB" => n * 1_000_000.0,
        _ => return 0,
    };
    mb.round().max(0.0) as u64
}

/// Fetch a model the consumer asked for into this machine's model store.
///
/// Runs in a throwaway alpine container with ONLY the model volume mounted,
/// not in the session's container: the app image may have no downloader, and
/// a fetch should not be able to touch anything but the store it is aimed at.
///
/// Everything about the request was vetted by the gateway (allowlisted host,
/// allowlisted folder, a filename that cannot climb out of it) and vetted
/// AGAIN here — this machine does not take the gateway's word for what to
/// write to its own disk.
async fn fetch_model(
    sink: Arc<Mutex<WsSink>>,
    client: reqwest::Client,
    cfg: WorkerConfig,
    frame: Value,
) {
    let fetch = frame["fetch"].as_str().unwrap_or_default().to_string();
    let url = frame["url"].as_str().unwrap_or_default().to_string();
    let volume = frame["volume"].as_str().unwrap_or_default().to_string();
    let path = frame["path"].as_str().unwrap_or_default().to_string();

    let report = |state: &str, msg: String| {
        let sink = sink.clone();
        let fetch = fetch.clone();
        let state = state.to_string();
        async move {
            send_frame(
                &sink,
                json!({
                    "type": "model_fetch_status", "fetch": fetch,
                    "state": state, "message": msg,
                }),
            )
            .await;
        }
    };

    // Our own checks, independent of the gateway's.
    let target_ok = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains("..")
        && path.matches('/').count() == 1
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b));
    if !is_fabric_volume(&volume, "/models") || !target_ok {
        report(
            "error",
            format!("refused: {volume:?}/{path:?} is not a fabric model path"),
        )
        .await;
        return;
    }
    if !url.starts_with("https://") {
        report("error", "refused: model URLs must be https".into()).await;
        return;
    }

    // The owner's disk budget, checked BEFORE anything is written. The
    // gateway refuses a fetch onto a node with no headroom, but the
    // authoritative numbers live here: this machine knows what its volumes
    // actually hold and what its owner agreed to lend.
    if let Some(cap_gb) = cfg.max_shared_disk_gb {
        let cap_mb = cap_gb * 1024;
        let used_mb = fabric_disk_used_mb().await.unwrap_or(0);
        let free_mb = cap_mb.saturating_sub(used_mb);
        // Ask the source how big it is. A HEAD is cheap and turns "the disk
        // filled up halfway through a 12 GB download" into a refusal that
        // costs nothing and explains itself.
        let size_mb = match client.head(&url).send().await {
            Ok(resp) => resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|bytes| bytes / 1_048_576),
            Err(_) => None,
        };
        match size_mb {
            Some(need) if need > free_mb => {
                report(
                    "error",
                    format!(
                        "refused: {path} needs {} GB but this provider has {} GB left \
                     of the {} GB they agreed to share",
                        need / 1024,
                        free_mb / 1024,
                        cap_gb,
                    ),
                )
                .await;
                return;
            }
            // Unknown size and almost nothing left: refuse rather than start
            // a download that can only end by breaking the promise.
            None if free_mb < 1024 => {
                report(
                    "error",
                    format!(
                        "refused: {} GB left of the {} GB shared, and the source did \
                     not say how large {path} is",
                        free_mb / 1024,
                        cap_gb,
                    ),
                )
                .await;
                return;
            }
            _ => {}
        }
    }

    report("fetching", format!("downloading {path}")).await;
    let script = format!(
        "set -e; mkdir -p \"$(dirname /m/{path})\"; \
         wget --no-verbose -O /m/{path}.part '{url}'; \
         mv /m/{path}.part /m/{path}"
    );
    let out = crate::proc::command("docker")
        .args([
            "run",
            "--rm",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--memory",
            "512m",
            "--pids-limit",
            "64",
            "-v",
            &format!("{volume}:/m"),
            "alpine:3.20",
            "sh",
            "-c",
            &script,
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            log(format!("fetched model {path} for a peer"));
            report("done", format!("{path} is ready")).await;
        }
        Ok(o) => {
            // .part is left behind on failure by design: a half-file with the
            // real name would look like a working model to ComfyUI.
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            report(
                "error",
                format!(
                    "download failed: {}",
                    err.chars().take(300).collect::<String>()
                ),
            )
            .await;
        }
        Err(e) => report("error", format!("docker unavailable: {e}")).await,
    }
}

fn container_name(session: &str) -> String {
    format!("kmplify-fabric-{}", &session[..12.min(session.len())])
}

async fn send_frame(sink: &Arc<Mutex<WsSink>>, msg: Value) {
    let text = msg.to_string();
    if sink
        .lock()
        .await
        .send(Message::Text(text.clone()))
        .await
        .is_ok()
    {
        return;
    }
    // The captured sink belongs to a connection that no longer exists — a
    // detached session task can outlive the socket it was born on. Deliver
    // over the CURRENT connection instead of silently dropping the frame:
    // the frames that land here are exactly the ones the gateway must not
    // miss (`running`, the final error), because losing them is how a
    // session got stuck showing `starting` until the deadline.
    let current = current_sink_cell().read().await.clone();
    if let Some(current) = current {
        if !Arc::ptr_eq(&current, sink) {
            let _ = current.lock().await.send(Message::Text(text)).await;
        }
    }
}

async fn workload_status(sink: &Arc<Mutex<WsSink>>, session: &str, state: &str, message: &str) {
    send_frame(
        sink,
        json!({"type": "workload_status", "session": session, "state": state, "message": message}),
    )
    .await;
}

/// A `workload_status` frame carrying pull progress (0–100). Progress rides
/// on the existing frame type instead of a new one so an old gateway — which
/// reads only `state`/`message` — keeps working untouched.
async fn workload_progress(sink: &Arc<Mutex<WsSink>>, session: &str, pct: f64, message: &str) {
    send_frame(
        sink,
        json!({"type": "workload_status", "session": session, "state": "pulling",
               "message": message, "progress": pct}),
    )
    .await;
}

/// Pull the template image explicitly, streaming layer-completion progress to
/// the gateway. `docker run` alone also pulls, but silently: the consumer
/// stared at a bare "pulling" badge for minutes (CUDA images are multi-GB)
/// with no way to tell a working pull from a hung one.
///
/// Non-TTY `docker pull` prints one status line per layer transition, so a
/// percentage is derived from layers completed vs layers announced. Coarser
/// than byte-accurate, but it needs no Docker API socket privileges.
async fn pull_image(
    sink: &Arc<Mutex<WsSink>>,
    session: &str,
    image: &str,
    deadline: std::time::Instant,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut child = crate::proc::command("docker")
        .args(["pull", image])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("docker unavailable: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout from docker pull")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut done: std::collections::HashSet<String> = Default::default();
    let mut last_pct = -1.0_f64;
    let mut last_sent = std::time::Instant::now() - Duration::from_secs(10);
    let started = std::time::Instant::now();
    let mut silence = Duration::ZERO;
    loop {
        // Read with a heartbeat tick rather than an open-ended await. Two
        // things depend on it: the consumer needs to see that a quiet pull
        // is still alive, and a pull that has genuinely wedged has to end.
        // `docker pull` never times out by itself — a stalled registry
        // connection or a thrashing Docker Desktop VM just sits there,
        // producing no output and no error, which is precisely how a
        // session used to stay "pulling" until the user gave up.
        let line = match tokio::time::timeout(PULL_HEARTBEAT, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                silence = Duration::ZERO;
                line
            }
            Ok(Ok(None)) => break, // docker finished; exit status checked below
            Ok(Err(e)) => return Err(format!("reading docker pull output: {e}")),
            Err(_) => {
                silence += PULL_HEARTBEAT;
                let waited = started.elapsed();
                if silence >= PULL_STALL_TIMEOUT {
                    let _ = child.kill().await;
                    return Err(format!(
                        "no output from docker pull for {} min — the pull is wedged",
                        PULL_STALL_TIMEOUT.as_secs() / 60
                    ));
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill().await;
                    return Err(format!(
                        "image still not pulled after {} min",
                        PULL_TOTAL_TIMEOUT.as_secs() / 60
                    ));
                }
                // A big single layer is silent for minutes at a time, so an
                // unchanged percentage here is normal — the elapsed count is
                // what tells the consumer the difference between slow and dead.
                workload_progress(
                    sink,
                    session,
                    last_pct.max(0.0),
                    &format!("still downloading — {}s so far", waited.as_secs()),
                )
                .await;
                continue;
            }
        };
        let Some((id, status)) = line.split_once(':') else {
            continue;
        };
        let (id, status) = (id.trim(), status.trim());
        // Layer ids are 12-char short hashes; "Digest:"/"Status:" lines are not layers.
        if id.len() != 12 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        seen.insert(id.to_string());
        if status.starts_with("Pull complete") || status.starts_with("Already exists") {
            done.insert(id.to_string());
        }
        let pct = done.len() as f64 / seen.len().max(1) as f64 * 100.0;
        // Throttled: a big image announces dozens of layer transitions per
        // second mid-pull, and every frame here crosses the gateway.
        if (pct - last_pct).abs() >= 1.0 && last_sent.elapsed() >= Duration::from_millis(750) {
            last_pct = pct;
            last_sent = std::time::Instant::now();
            workload_progress(
                sink,
                session,
                pct,
                &format!("{}/{} layers", done.len(), seen.len()),
            )
            .await;
        }
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    workload_progress(sink, session, 100.0, "image ready").await;
    Ok(())
}

/// Handle a `workload_start` frame: launch the template's container with
/// with the hardening described in the README, discover the ephemeral host port,
/// wait for the app to answer, report `running`.
async fn start_workload(
    sink: Arc<Mutex<WsSink>>,
    sessions: Sessions,
    stopped: Stopped,
    cfg: WorkerConfig,
    frame: Value,
) {
    let session = frame["session"].as_str().unwrap_or_default().to_string();
    let template = frame["template"].as_str().unwrap_or_default().to_string();
    let image = frame["image"].as_str().unwrap_or_default().to_string();
    let port = frame["port"].as_u64().unwrap_or(0);
    let cuda = frame["cuda"].as_bool().unwrap_or(false);
    // How long this template may take to answer. Sent per-template because
    // the flat 120s was fatal for self-bootstrapping images (ComfyUI copies
    // its whole tree into /root on first start): the container was killed
    // mid-setup every time, so it could never finish, and the next attempt
    // began again from nothing. Clamped so a rogue gateway cannot pin a
    // container on this machine indefinitely.
    let ready_timeout_s = frame["ready_timeout_s"]
        .as_u64()
        .unwrap_or(120)
        .clamp(30, 1800);
    if session.is_empty() || image.is_empty() || port == 0 {
        workload_status(&sink, &session, "error", "malformed workload_start").await;
        return;
    }
    // Re-validate against OUR opt-in list — the gateway is honest today,
    // but which images run on this machine is this machine's decision.
    if !cfg.workload_templates.iter().any(|t| t == &template) {
        workload_status(
            &sink,
            &session,
            "error",
            "template not enabled on this node",
        )
        .await;
        return;
    }
    // Opting into a template is not opting into an arbitrary image under its
    // name. Refused before the pull, so a rejected image is never even
    // fetched onto the provider's disk.
    if !image_allowed_for(&template, &image) {
        log(format!(
            "refusing image {image:?} for template {template:?}: not the pinned repository \
             (expected {:?})",
            IMAGE_PINS
                .iter()
                .find(|(t, _)| *t == template)
                .map(|(_, repo)| *repo)
                .unwrap_or("no pin for this template"),
        ));
        workload_status(
            &sink,
            &session,
            "error",
            "image is not the one this node pins for that template",
        )
        .await;
        return;
    }
    if cuda && !cfg.cuda {
        workload_status(&sink, &session, "error", "node has no CUDA GPU").await;
        return;
    }

    let name = container_name(&session);
    // Settled here rather than at `docker run` because the session is
    // announced below and the owner is shown what it holds from that moment:
    // the pull is exactly when they most want to see it.
    let cpus = session_cpus(frame["cpus"].as_f64(), host_cpus(), cfg.max_shared_cpus);
    // Published from the first moment this machine commits to the session,
    // not once it is running: a pull is exactly when the owner most wants to
    // see (and be able to cancel) what their machine took on.
    hosted_add(&session, &template, &name, "pulling", cpus).await;
    workload_status(&sink, &session, "pulling", "").await;
    // Retried: the CUDA images are multi-GB and a single transient hiccup —
    // registry reset, Docker Desktop restarting its VM — surfaced as
    // "image pull failed: unexpected EOF" and killed the whole session even
    // though Docker keeps completed layers, making a retry nearly free.
    let mut pull_err = String::new();
    // One budget for the whole pull, not one per attempt: retries used to
    // multiply the node's real patience by three, past the point where the
    // gateway had already given up on the session.
    let pull_deadline = std::time::Instant::now() + PULL_TOTAL_TIMEOUT;
    for attempt in 1..=3u8 {
        if std::time::Instant::now() >= pull_deadline {
            break;
        }
        match pull_image(&sink, &session, &image, pull_deadline).await {
            Ok(()) => {
                pull_err.clear();
                break;
            }
            Err(e) => {
                pull_err = e;
                if attempt < 3 {
                    log(format!(
                        "session {session}: pull attempt {attempt} failed ({pull_err}); retrying"
                    ));
                    workload_progress(
                        &sink,
                        &session,
                        0.0,
                        &format!(
                            "pull attempt {attempt} failed — retrying ({}/3)",
                            attempt + 1
                        ),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    if !pull_err.is_empty() {
        hosted_remove(&session).await;
        workload_status(
            &sink, &session, "error",
            &format!("image pull failed after 3 attempts: {pull_err} — check the provider's Docker daemon (a crashed Docker Desktop/WSL shows up exactly like this) and free disk space"),
        )
        .await;
        return;
    }
    // A workload_stop that arrived during the pull left nothing to remove
    // from `sessions` yet — honor it here instead of starting a container
    // the consumer already gave up on.
    if stopped.lock().await.remove(&session) {
        hosted_remove(&session).await;
        log(format!(
            "session {session} stopped during image pull — not starting"
        ));
        return;
    }

    // Hardening (README, "Trust model"): no capabilities, no privilege escalation,
    // memory/pid caps, loopback-only ephemeral port.
    //
    // Deliberately NOT --rm. Auto-remove destroyed the only evidence of a
    // crash: the container vanished the moment its process exited, taking
    // its logs with it, and the readiness loop below — which probed only
    // HTTP — read a dead container as "not ready yet" until the full
    // timeout. The consumer saw `starting` for 16 minutes and then a
    // timeout, for a crash that happened in the first 20 seconds (observed
    // with vLLM's WSL2 UVA failure; only racing `docker logs -f` against
    // the removal ever showed why). Every exit path here removes the
    // container explicitly, and the startup sweep (`docker ps -a` on
    // kmplify-fabric-*) catches the case where the worker itself dies
    // between a crash and its cleanup.
    //
    // The memory cap is template-driven (clamped) instead of a flat 8g: an
    // LLM server that spills weights or KV cache past a too-small cgroup cap
    // doesn't fail cleanly, it thrashes — which consumers experienced as a
    // "running" session that took minutes per answer.
    let mem_gb = frame["mem_gb"].as_u64().unwrap_or(8).clamp(1, 64);

    // CPU cap, for the same reason memory has one — and it was missing.
    //
    // A provider donates GPU time. Without this the consumer's container also
    // got every core on the machine: docker's default is unlimited, so a
    // ComfyUI session could saturate all 32 threads of the host while its
    // owner tried to use their own PC. ComfyUI is not GPU-only work — the
    // first-launch bootstrap copies thousands of files, checkpoint and
    // safetensors loading are CPU-bound, PNG encoding of every output is CPU,
    // and plenty of custom nodes never touch the GPU at all. Torch will take
    // whatever cores it can see.
    //
    // Never more than half the host's cores, whatever the gateway asks for:
    // the machine has to stay usable for the person lending it. Falls back to
    // that half-share when the template says nothing, so an older gateway
    // still gets a bounded container instead of an unbounded one.

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.clone(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--memory".into(),
        format!("{mem_gb}g"),
        "--cpus".into(),
        format!("{cpus:.2}"),
        "--pids-limit".into(),
        "512".into(),
        "-p".into(),
        format!("127.0.0.1:0:{port}"),
    ];
    if cuda {
        args.push("--gpus".into());
        args.push("all".into());
    }
    // Optional named-volume mounts from the template (e.g. Ollama's model
    // store, ComfyUI's install and its models), so a re-launched session
    // doesn't re-download gigabytes of weights every time. ONLY
    // fabric-namespaced NAMED volumes are accepted, never a host path: a
    // hostile or buggy gateway must not be able to mount the provider's
    // filesystem into a consumer-driven container.
    //
    // `volumes` (list) supersedes `volume` (single) — a gateway sends both so
    // that a node predating the list still gets the primary mount rather than
    // none at all.
    let mounts: Vec<String> = match frame["volumes"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        None => frame["volume"]
            .as_str()
            .map(str::to_owned)
            .into_iter()
            .collect(),
    };
    for vol in mounts {
        if let Some((vname, target)) = vol.split_once(':') {
            if is_fabric_volume(vname, target) {
                args.push("-v".into());
                args.push(format!("{vname}:{target}"));
            } else {
                log(format!(
                    "refusing volume mount {vol:?} — not a fabric-namespaced named volume"
                ));
            }
        }
    }
    if let Some(env) = frame["env"].as_object() {
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                args.push("-e".into());
                args.push(format!("{k}={v}"));
            }
        }
    }
    args.push(image.clone());
    // Protocol v2.2: template CMD args, appended AFTER the image so they are
    // arguments to the container's entrypoint (e.g. the vLLM+LMCache
    // template's --kv-transfer-config) and can never become docker-run flags
    // on the host side.
    if let Some(extra) = frame["args"].as_array() {
        for a in extra.iter().filter_map(|v| v.as_str()) {
            args.push(a.to_string());
        }
    }

    let out = crate::proc::command("docker").args(&args).output().await;
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            hosted_remove(&session).await;
            workload_status(&sink, &session, "error", &msg).await;
            return;
        }
        Err(e) => {
            hosted_remove(&session).await;
            workload_status(
                &sink,
                &session,
                "error",
                &format!("docker unavailable: {e}"),
            )
            .await;
            return;
        }
    }

    // The ephemeral port docker actually bound (127.0.0.1:0 above).
    let host_port = match crate::proc::command("docker")
        .args(["port", &name, &format!("{port}/tcp")])
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .and_then(|p| p.trim().parse::<u16>().ok()),
        _ => None,
    };
    let Some(host_port) = host_port else {
        let _ = remove_container(&name).await;
        hosted_remove(&session).await;
        workload_status(
            &sink,
            &session,
            "error",
            "could not resolve the container's host port",
        )
        .await;
        return;
    };

    // Narrow race: the stop can also land between the tombstone check above
    // and the container starting. Registering first and re-checking makes
    // that window converge on the same outcome — container removed.
    sessions
        .lock()
        .await
        .insert(session.clone(), (name.clone(), host_port));
    if stopped.lock().await.remove(&session) {
        sessions.lock().await.remove(&session);
        hosted_remove(&session).await;
        let _ = remove_container(&name).await;
        log(format!(
            "session {session} stopped while starting — container removed"
        ));
        return;
    }
    hosted_set_state(&session, "starting").await;
    workload_status(&sink, &session, "starting", "").await;

    // A CUDA template that silently lands on CPU is worse than a refusal:
    // the consumer rents "GPU" time and gets minutes-per-answer inference
    // that reads as a hung session (`--gpus all` succeeds on some setups —
    // notably WSL2 with a half-installed toolkit — while the driver is
    // still invisible INSIDE the container). Verify with nvidia-smi in the
    // container and fail loudly when the GPU is not actually there.
    if cuda {
        let mut gpu_ok = false;
        for _ in 0..5 {
            let probe = crate::proc::command("docker")
                .args(["exec", &name, "nvidia-smi", "-L"])
                .output()
                .await;
            if matches!(&probe, Ok(o) if o.status.success()) {
                gpu_ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !gpu_ok {
            // A container that died instantly also fails every exec probe —
            // blaming the GPU toolkit for an entrypoint crash sent the
            // provider debugging the wrong thing. Tell the two apart.
            let msg = if let Some(code) = container_exit(&name).await {
                let tail = container_log_tail(&name, 15).await;
                format!("container exited (code {code}) before becoming ready — last log lines:\n{tail}")
            } else {
                "NVIDIA GPU not visible inside the container — install/enable the \
                 NVIDIA Container Toolkit on this node. Refusing to run a CUDA \
                 session on CPU."
                    .to_string()
            };
            sessions.lock().await.remove(&session);
            hosted_remove(&session).await;
            let _ = remove_container(&name).await;
            workload_status(&sink, &session, "error", &msg).await;
            return;
        }
    }

    // Readiness: ANY http response counts (vLLM answers 404 on / while
    // being perfectly ready). reqwest::get errors only when nothing is
    // listening yet.
    //
    // Timed against the WALL CLOCK, not a loop counter. Counting iterations
    // meant each one cost a 1s sleep PLUS up to a 2s probe timeout, so a
    // "600s" budget really ran up to 1800s — the error message lied about
    // how long it had waited, and the gateway (whose deadline is derived
    // from this number) gave up first and aborted the session while this
    // node was still calmly waiting. That is why a bootstrapping container
    // showed as `Up` on the provider while the consumer was told the peer
    // never finished.
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("probe client");
    let deadline = std::time::Instant::now() + Duration::from_secs(ready_timeout_s);
    let mut ticks: u32 = 0;
    while std::time::Instant::now() < deadline {
        if !sessions.lock().await.contains_key(&session) {
            hosted_remove(&session).await;
            return; // stopped while starting
        }
        if probe
            .get(format!("http://127.0.0.1:{host_port}/"))
            .send()
            .await
            .is_ok()
        {
            hosted_set_state(&session, "running").await;
            workload_status(&sink, &session, "running", "").await;
            log(format!(
                "session {session} running: {template} on 127.0.0.1:{host_port}"
            ));
            cfg.emit(
                "A peer is using your GPU",
                format!("{template} started on this machine and is now serving a KMPLIFY user."),
            );
            return;
        }
        // Liveness, not just readiness. A crashed container answers no
        // probe, so it used to read as "not ready yet" for the whole
        // timeout — the consumer watched `starting` for 16 minutes to learn
        // about a crash from second 20. Every third tick keeps the docker
        // exec chatter down while still reporting a death within ~3s.
        ticks += 1;
        if ticks % 3 == 0 {
            if let Some(code) = container_exit(&name).await {
                let tail = container_log_tail(&name, 15).await;
                sessions.lock().await.remove(&session);
                hosted_remove(&session).await;
                let _ = remove_container(&name).await;
                workload_status(
                    &sink, &session, "error",
                    &format!("container exited (code {code}) before becoming ready — last log lines:\n{tail}"),
                )
                .await;
                log(format!(
                    "session {session}: container died during startup (exit {code})"
                ));
                cfg.emit(
                    "A peer session failed to start",
                    format!("{template} crashed while starting on this machine (exit code {code}). The session was reported to the consumer and cleaned up."),
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    sessions.lock().await.remove(&session);
    hosted_remove(&session).await;
    let _ = remove_container(&name).await;
    workload_status(
        &sink,
        &session,
        "error",
        &format!("container did not become ready within {ready_timeout_s}s"),
    )
    .await;
}

async fn stop_workload(
    sink: Arc<Mutex<WsSink>>,
    sessions: Sessions,
    stopped: Stopped,
    session: String,
    cfg: WorkerConfig,
) {
    let removed = sessions.lock().await.remove(&session);
    hosted_remove(&session).await;
    if let Some((name, _)) = removed {
        match remove_container(&name).await {
            Ok(()) => {
                log(format!(
                    "session {session} stopped; container {name} removed"
                ));
                cfg.emit(
                    "Peer session ended",
                    "The container a peer was running on your GPU has been removed.",
                );
            }
            Err(e) => {
                // Say so loudly on BOTH surfaces. A container that refuses to
                // die is still holding this machine's GPU, and the owner is
                // the only one who can deal with it.
                log(format!(
                    "session {session}: could not remove container {name}: {e}"
                ));
                cfg.emit(
                    "A peer session did not shut down cleanly",
                    format!("Container {name} could not be removed ({e}). It may still be using your GPU — you can remove it from Running on your GPU."),
                );
            }
        }
    } else {
        // Nothing registered yet: the session is still pulling its image.
        // Tombstone it so start_workload aborts instead of launching a
        // container nobody tracks.
        stopped.lock().await.insert(session.clone());
    }
    workload_status(&sink, &session, "stopped", "").await;
}

/// Relay one tunneled consumer HTTP request into the session's container
/// and answer with an `http_resp` frame.
/// How many CPU cores a session container may use.
///
/// The gateway's number is a REQUEST, never a grant: it is clamped to at most
/// half the host's cores so the person lending the machine keeps enough of it
/// to work. An absent or nonsensical value (missing key, zero, negative, NaN
/// from a malformed frame) falls back to that same half-share, so the
/// container is always bounded — docker's own default is unlimited, which is
/// what let a session take every core on the provider's PC.
fn session_cpus(requested: Option<f64>, host_cpus: f64, operator_max: Option<f64>) -> f64 {
    // Half the host is the floor of last resort; the operator may lend less,
    // never more. Their slider wins over the gateway's request AND over the
    // default, because it is their machine.
    let ceiling = match operator_max {
        Some(m) if m.is_finite() && m > 0.0 => m.min(host_cpus).max(1.0),
        _ => (host_cpus / 2.0).max(1.0),
    };
    match requested {
        Some(c) if c.is_finite() && c > 0.0 => c.min(ceiling),
        _ => ceiling,
    }
}

/// Consumer WebSockets currently bridged into session containers, keyed by
/// the gateway's ws_id. The value is the send half of a channel owned by that
/// socket's writer task — keeping the channel here rather than the sink
/// itself means a slow container cannot block the frame-dispatch loop that
/// every other session shares.
type RelaySockets =
    Arc<Mutex<std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<Message>>>>;

/// Dial a session container's WebSocket on behalf of a consumer.
///
/// HTTP relaying alone leaves ComfyUI broken: its page loads, then the UI
/// drives everything else over a socket — progress, queue state, binary
/// previews, and the `executed` event carrying the finished image. Without
/// this the consumer sees a page that never moves and blames the GPU.
async fn ws_open(sink: Arc<Mutex<WsSink>>, sessions: Sessions, relays: RelaySockets, frame: Value) {
    let ws_id = frame["ws_id"].as_str().unwrap_or_default().to_string();
    let session = frame["session"].as_str().unwrap_or_default().to_string();
    let Some((_, host_port)) = sessions.lock().await.get(&session).cloned() else {
        send_frame(
            &sink,
            json!({"type": "ws_error", "ws_id": ws_id, "message": "session not running on this node"}),
        )
        .await;
        return;
    };

    let path = frame["path"].as_str().unwrap_or("/");
    let query = frame["query"].as_str().unwrap_or("");
    let url = if query.is_empty() {
        format!("ws://127.0.0.1:{host_port}{path}")
    } else {
        format!("ws://127.0.0.1:{host_port}{path}?{query}")
    };

    let (stream, _) = match connect_async(&url).await {
        Ok(ok) => ok,
        Err(e) => {
            send_frame(
                &sink,
                json!({"type": "ws_error", "ws_id": ws_id, "message": e.to_string()}),
            )
            .await;
            return;
        }
    };

    let (mut write, mut read) = stream.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    relays.lock().await.insert(ws_id.clone(), tx);
    send_frame(&sink, json!({"type": "ws_opened", "ws_id": ws_id})).await;

    // Consumer -> container.
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });

    // Container -> consumer. Owns the read half until the socket ends, then
    // tells the gateway so the consumer's socket is closed rather than left
    // hanging on a container that has gone away.
    let sink2 = sink.clone();
    let relays2 = relays.clone();
    let id2 = ws_id.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            let out = match msg {
                Message::Text(t) => {
                    json!({"type": "ws_recv", "ws_id": id2, "data_b64": b64_encode(t.as_bytes()), "binary": false})
                }
                Message::Binary(b) => {
                    json!({"type": "ws_recv", "ws_id": id2, "data_b64": b64_encode(&b), "binary": true})
                }
                Message::Close(_) => break,
                // Ping/Pong are answered by tungstenite itself; forwarding
                // them would only duplicate keepalives on the gateway link.
                _ => continue,
            };
            send_frame(&sink2, out).await;
        }
        relays2.lock().await.remove(&id2);
        send_frame(&sink2, json!({"type": "ws_closed", "ws_id": id2})).await;
    });
}

async fn relay_http(
    sink: Arc<Mutex<WsSink>>,
    sessions: Sessions,
    client: reqwest::Client,
    frame: Value,
) {
    let req_id = frame["req_id"].as_str().unwrap_or_default().to_string();
    let session = frame["session"].as_str().unwrap_or_default().to_string();
    let Some((_, host_port)) = sessions.lock().await.get(&session).cloned() else {
        send_frame(
            &sink,
            json!({"type": "http_resp", "req_id": req_id, "status": 502,
            "headers": {}, "body_b64": b64_encode(b"session not running on this node")}),
        )
        .await;
        return;
    };

    let method = frame["method"].as_str().unwrap_or("GET").to_uppercase();
    let path = frame["path"].as_str().unwrap_or("/");
    let query = frame["query"].as_str().unwrap_or("");
    let url = if query.is_empty() {
        format!("http://127.0.0.1:{host_port}{path}")
    } else {
        format!("http://127.0.0.1:{host_port}{path}?{query}")
    };

    let mut req = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
        &url,
    );
    if let Some(headers) = frame["headers"].as_object() {
        for (k, v) in headers {
            // Hop-by-hop / addressing headers must not leak through a relay.
            let kl = k.to_ascii_lowercase();
            if kl == "host" || kl == "connection" || kl == "content-length" {
                continue;
            }
            if let Some(v) = v.as_str() {
                req = req.header(k, v);
            }
        }
    }
    if let Some(body) = frame["body_b64"].as_str() {
        if !body.is_empty() {
            if let Some(bytes) = b64_decode(body) {
                req = req.body(bytes);
            }
        }
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers = serde_json::Map::new();
            for (k, v) in resp.headers() {
                if let Ok(v) = v.to_str() {
                    headers.insert(k.to_string(), Value::String(v.to_string()));
                }
            }
            // SSE (OpenAI-compatible /v1/*) and NDJSON (Ollama's native
            // /api/*) responses are forwarded chunk-by-chunk as they arrive.
            // Buffering them — the previous behaviour — meant a streamed
            // chat produced NOTHING until the entire generation finished,
            // then dumped every token at once; on a slow model that reads
            // as a hung session and trips the gateway's relay timeout.
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            let streaming =
                ct.starts_with("text/event-stream") || ct.starts_with("application/x-ndjson");
            if streaming {
                send_frame(
                    &sink,
                    json!({"type": "http_resp_start", "req_id": req_id,
                    "status": status, "headers": headers}),
                )
                .await;
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let Ok(chunk) = chunk else { break };
                    if chunk.is_empty() {
                        continue;
                    }
                    send_frame(
                        &sink,
                        json!({"type": "http_resp_chunk", "req_id": req_id,
                        "body_b64": b64_encode(&chunk)}),
                    )
                    .await;
                }
                send_frame(&sink, json!({"type": "http_resp_end", "req_id": req_id})).await;
            } else {
                let body = resp.bytes().await.unwrap_or_default();
                send_frame(
                    &sink,
                    json!({"type": "http_resp", "req_id": req_id, "status": status,
                    "headers": headers, "body_b64": b64_encode(&body)}),
                )
                .await;
            }
        }
        Err(e) => {
            send_frame(
                &sink,
                json!({"type": "http_resp", "req_id": req_id, "status": 502,
                "headers": {}, "body_b64": b64_encode(e.to_string().as_bytes())}),
            )
            .await;
        }
    }
}

/// Remove every container this node started for the fabric — run on
/// disconnect and on stop, so nothing keeps burning the user's GPU after
/// sharing ends. The gateway independently marks the sessions failed.
async fn cleanup_sessions(sessions: &Sessions) {
    let drained: Vec<(String, (String, u16))> = sessions.lock().await.drain().collect();
    for (session, (name, _)) in drained {
        let _ = remove_container(&name).await;
        hosted_remove(&session).await;
        log(format!("cleaned up session {session}"));
    }
    // Anything still listed here was mid-pull when the connection died —
    // start_workload's own removal never ran, and leaving it published would
    // show the owner a session on their GPU that no longer exists.
    hosted_cell().lock().await.clear();
}

/// Translate an OpenAI-shaped chat request into Ollama's NATIVE /api/chat
/// body, preserving `think`.
///
/// Ollama's OpenAI-compatible endpoint accepts `think` in the body and then
/// IGNORES it: a reasoning model still fills the hidden channel, returns
/// `content: ""` with the whole budget spent, and reports
/// `finish_reason: "length"` — verified against qwen3:14b on a live peer.
/// Only the native endpoint honors the flag, which is why it is injected
/// there and nowhere else.
fn openai_to_native_chat(payload: &Value, think: bool) -> Value {
    let mut options = serde_json::Map::new();
    // Native /api/chat takes generation limits under `options`, not at the
    // top level; dropping them would silently ignore the caller's budget.
    if let Some(v) = payload.get("max_tokens").and_then(Value::as_i64) {
        options.insert("num_predict".into(), json!(v));
    }
    for (openai_key, native_key) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("seed", "seed"),
        ("stop", "stop"),
    ] {
        if let Some(v) = payload.get(openai_key) {
            if !v.is_null() {
                options.insert(native_key.into(), v.clone());
            }
        }
    }
    let mut body = json!({
        "model": payload.get("model").cloned().unwrap_or(Value::Null),
        "messages": payload.get("messages").cloned().unwrap_or(json!([])),
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "think": think,
    });
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }
    body
}

/// One native /api/chat NDJSON line -> an OpenAI-shaped streaming chunk, so
/// the gateway and every consumer keep seeing exactly the wire format they
/// already parse.
///
/// Native `message.thinking` maps to `delta.reasoning`, mirroring what
/// Ollama's own OpenAI-compatible endpoint emits. It must NOT be folded into
/// `content` (a caller suppressing reasoning would get it back as the
/// answer) and must NOT be dropped either — with `think: true` the whole
/// reply can live in that field, so discarding it returns an empty response.
fn native_line_to_openai_chunk(v: &Value, model: &str) -> Value {
    let msg = v.get("message");
    let content = msg
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let thinking = msg
        .and_then(|m| m.get("thinking"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let done = v.get("done").and_then(Value::as_bool).unwrap_or(false);
    let mut delta = serde_json::Map::new();
    if !content.is_empty() {
        delta.insert("content".into(), json!(content));
    }
    if !thinking.is_empty() {
        delta.insert("reasoning".into(), json!(thinking));
    }
    json!({
        "id": "chatcmpl-fabric",
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": Value::Object(delta),
            "finish_reason": if done { json!("stop") } else { Value::Null },
        }],
    })
}

/// How long a peer-served model stays resident after answering.
///
/// Ollama's default is 5 minutes, and nothing in the relay overrode it — so
/// a model went cold between conversational turns and the next consumer paid
/// a full load. Measured on this fleet: gemma4 costs ~9.6s to load on a 4090
/// and >20s on a CPU node, against ~0.5s and ~3.5s warm. 30 minutes keeps a
/// chat session responsive across thinking pauses without pinning the card
/// forever — the provider's own pre-warm uses keep_alive:-1 for models THEY
/// chose; a peer's request should not silently claim the same.
const PEER_KEEP_ALIVE: &str = "30m";

/// Make a relayed inference request reuse the model already in VRAM.
///
/// Two separate causes of a cold answer, both fixed here because both are
/// invisible from the consumer's side:
///
/// 1. No `keep_alive` — the model unloads 5 minutes after the last request,
///    so the second question in a conversation reloads it.
/// 2. No `num_ctx` — Ollama keys its resident copy by context size, so a
///    request that omits it does NOT match a model pre-warmed at a larger
///    context and triggers a FULL reload while `ollama ps` still shows the
///    model resident. That is the 9.6s "load" observed on a 4090 with the
///    model supposedly pinned: it was loading a second copy at the default
///    context.
///
/// Deliberately non-destructive: an explicit value from the consumer or the
/// gateway always wins. This only fills in what nobody specified, which is
/// the case that was silently slow.
fn keep_peer_model_warm(mut payload: Value, path: &str) -> Value {
    // Embeddings are short and their models are small; the reload cost that
    // motivates this is a chat-model problem, and touching the embeddings
    // shape risks breaking a request type that is currently fine.
    if !path.contains("chat") {
        return payload;
    }
    let Some(obj) = payload.as_object_mut() else {
        return payload;
    };
    if !obj.contains_key("keep_alive") {
        obj.insert("keep_alive".into(), json!(PEER_KEEP_ALIVE));
    }
    // num_ctx lives under `options` on the native endpoint. The OpenAI-shaped
    // endpoint has no equivalent field, so it is left alone rather than being
    // sent something Ollama would ignore or reject.
    if path == "/api/chat" {
        let opts = obj.entry("options").or_insert_with(|| json!({}));
        if let Some(opts) = opts.as_object_mut() {
            if !opts.contains_key("num_ctx") {
                opts.insert("num_ctx".into(), json!(PEER_NUM_CTX));
            }
        }
    }
    payload
}

/// Context size relayed jobs request when the caller named none.
///
/// Must match what the provider's own pre-warm pins (settings-ollama.yaml's
/// context_window, 16384) — a mismatch is exactly what makes Ollama load a
/// second copy of a model it already holds.
const PEER_NUM_CTX: u32 = 16384;

/// Where a job may be executed: the primary upstream plus the optional
/// colibri gateway, with the per-model routing overrides captured at
/// dispatch time (a mid-refresh change applies to the NEXT job, never to
/// one already running).
#[derive(Clone)]
struct JobUpstreams {
    ollama_base: String,
    colibri_base: String,
    colibri_api_key: String,
    engines: std::collections::HashMap<String, String>,
}

async fn run_job(
    sink: Arc<Mutex<WsSink>>,
    client: reqwest::Client,
    upstreams: JobUpstreams,
    frame: Value,
) {
    let job_id = frame["id"].as_str().unwrap_or_default().to_string();
    let kind = frame["kind"].as_str().unwrap_or_default();
    let payload = frame["payload"].clone();
    // Per-model routing: models the discovery tagged "colibri" go to the
    // colibri gateway; everything else takes the unchanged primary path.
    let requested_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let is_colibri = upstreams.engines.get(&requested_model).map(String::as_str) == Some("colibri");

    let send = |sink: Arc<Mutex<WsSink>>, msg: Value| async move {
        let _ = sink.lock().await.send(Message::Text(msg.to_string())).await;
    };

    if is_colibri && kind != "chat" {
        // Colibri serves chat/completions only. Failing here with a clear
        // message beats forwarding into a 404 whose text names a path the
        // consumer never called.
        send(
            sink,
            json!({"type": "error", "id": job_id, "message": format!(
                "model '{requested_model}' is served by colibri, which does not provide embeddings"
            )}),
        )
        .await;
        return;
    }

    // `think` present => this job must go through the native endpoint, which
    // is the only place the flag actually takes effect. Ollama-only: colibri
    // has no native endpoint and no hidden reasoning channel to suppress.
    let think = payload.get("think").and_then(Value::as_bool);
    let native_think = kind == "chat" && think.is_some() && !is_colibri;
    let path = if kind != "chat" {
        "/v1/embeddings"
    } else if native_think {
        "/api/chat"
    } else {
        "/v1/chat/completions"
    };
    let payload = if native_think {
        openai_to_native_chat(&payload, think.unwrap_or(false))
    } else {
        payload
    };
    // keep_alive/num_ctx are Ollama residency knobs; colibri manages its own
    // expert cache and must not receive fields it never defined.
    let mut payload = if is_colibri {
        payload
    } else {
        keep_peer_model_warm(payload, path)
    };
    if is_colibri {
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("think");
        }
    }
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let streaming = kind == "chat"
        && payload
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);

    let base = if is_colibri {
        upstreams.colibri_base.clone()
    } else {
        upstreams.ollama_base.clone()
    };
    let with_auth = |req: reqwest::RequestBuilder| {
        if is_colibri && !upstreams.colibri_api_key.is_empty() {
            req.bearer_auth(&upstreams.colibri_api_key)
        } else {
            req
        }
    };

    if streaming {
        let resp = with_auth(client.post(format!("{base}{path}")).json(&payload))
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let mut stream = resp.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(chunk) = chunk else { break };
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(nl) = buf.find('\n') {
                        let line = buf[..nl].trim().to_string();
                        buf.drain(..=nl);
                        if native_think {
                            // Native /api/chat streams bare NDJSON objects,
                            // not SSE — translate each into the OpenAI chunk
                            // shape the gateway and consumers already parse.
                            if line.is_empty() {
                                continue;
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                                let out = native_line_to_openai_chunk(&v, &model);
                                send(
                                    sink.clone(),
                                    json!({"type": "chunk", "id": job_id, "data": out}),
                                )
                                .await;
                            }
                            continue;
                        }
                        let Some(data) = line.strip_prefix("data: ") else {
                            continue;
                        };
                        if data == "[DONE]" {
                            break;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            send(
                                sink.clone(),
                                json!({"type": "chunk", "id": job_id, "data": v}),
                            )
                            .await;
                        }
                    }
                }
                send(
                    sink.clone(),
                    json!({"type": "done", "id": job_id, "data": null}),
                )
                .await;
            }
            Ok(resp) => {
                let msg = resp.text().await.unwrap_or_default();
                send(sink, json!({"type": "error", "id": job_id, "message": msg})).await;
            }
            Err(e) => {
                send(
                    sink,
                    json!({"type": "error", "id": job_id, "message": e.to_string()}),
                )
                .await;
            }
        }
    } else {
        match with_auth(client.post(format!("{base}{path}")).json(&payload))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let data: Value = resp.json().await.unwrap_or(Value::Null);
                // Non-streamed native reply -> OpenAI completion shape, so a
                // think-carrying job is indistinguishable from any other on
                // the wire. `thinking` -> `reasoning`, same mapping as the
                // streaming path above.
                let data = if native_think {
                    let msg = data.get("message");
                    let content = msg
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let thinking = msg
                        .and_then(|m| m.get("thinking"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut out_msg = serde_json::Map::new();
                    out_msg.insert("role".into(), json!("assistant"));
                    out_msg.insert("content".into(), json!(content));
                    if !thinking.is_empty() {
                        out_msg.insert("reasoning".into(), json!(thinking));
                    }
                    json!({
                        "id": "chatcmpl-fabric",
                        "object": "chat.completion",
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "message": Value::Object(out_msg),
                            "finish_reason": "stop",
                        }],
                    })
                } else {
                    data
                };
                send(sink, json!({"type": "done", "id": job_id, "data": data})).await;
            }
            Ok(resp) => {
                let msg = resp.text().await.unwrap_or_default();
                send(sink, json!({"type": "error", "id": job_id, "message": msg})).await;
            }
            Err(e) => {
                send(
                    sink,
                    json!({"type": "error", "id": job_id, "message": e.to_string()}),
                )
                .await;
            }
        }
    }
}

async fn session(
    client: &reqwest::Client,
    cfg: &WorkerConfig,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let creds = credentials(client, cfg).await?;
    // No models advertised = no inference jobs scheduled here. That is the
    // OFF state of the "Share GPU inference" switch, not an error — the
    // worker may be connected purely to lend CPU/RAM or run sessions.
    let (models, engines) = if cfg.share_inference {
        discover(client, cfg).await
    } else {
        (Vec::new(), std::collections::HashMap::new())
    };
    if models.is_empty() && cfg.share_inference {
        log("no local models — connecting anyway; jobs will be refused by scheduler");
    }

    // Probed BEFORE the hello so this node never announces container
    // capability it cannot honour. Safe to block here — nothing is connected
    // yet, so there is no read loop to stall.
    let docker_live = docker_ok().await;
    let (disk_live, images_live, inventory_err) = sample_inventory().await;
    if let Some(e) = &inventory_err {
        log(format!("inventory unavailable at connect: {e}"));
    }

    let ws_url = format!(
        "{}/fabric/connect",
        cfg.gateway_url.replacen("http", "ws", 1)
    );
    let (ws, _) = connect_async(&ws_url).await.map_err(|e| e.to_string())?;
    let (write, mut read) = ws.split();
    let sink = Arc::new(Mutex::new(write));
    // Register as THE live connection, so frames from session tasks born on
    // an earlier one still reach the gateway (see current_sink_cell).
    *current_sink_cell().write().await = Some(sink.clone());

    let hello = json!({
        "type": "hello",
        "node_id": creds.node_id,
        "token": creds.token,
        "models": models,
        // Per-model upstream overrides ({model: "colibri"}, protocol v2.5).
        // Empty for single-upstream nodes; older gateways ignore the key.
        "engines": engines,
        "gpu": gpu_info(cfg.cuda, cfg.max_shared_vram_mb).await,
        "workloads": workload_capability(
            cfg, docker_live, disk_live, &images_live, inventory_err.as_deref(),
        ),
        // Which build is this? Compiled in, so it cannot disagree with what
        // was installed. Costs one field and removes a whole class of
        // guesswork: "does this peer speak protocol v2.2 yet?" was answered
        // for weeks by inferring it from which frames the node failed to
        // answer — an inference that was wrong at least once, because the
        // peer had been rebuilt since the last observation.
        "version": cfg.client_version.as_deref().unwrap_or_else(|| crate::version_string()),
        "country": cfg.country,
        // Admission mode (v2.4). Absent/unknown reads as "auto" on the
        // gateway, so older gateways and workers stay compatible both ways.
        "approval": cfg.approval_mode,
        // What this machine actually IS (v2.4): real CPU model, cores and
        // total RAM. Independent of cpu_share, which says only what was
        // volunteered — a GPU peer still has a CPU worth naming.
        "cpu": cpu_info(),
        // General CPU/RAM lending (v2.3): threads bounded by the operator's
        // ceiling, RAM the machine's total. Absent entirely when the
        // operator has not opted in — pre-v2.3 gateways ignore the field.
        "cpu_share": if cfg.share_cpu {
            json!({
                "threads": cfg
                    .max_shared_cpus
                    .unwrap_or_else(|| (host_cpus() / 2.0).max(1.0)),
                "ram_mb": match cfg.max_shared_ram_mb {
                    Some(cap) if cap > 0 => host_ram_mb().min(cap),
                    _ => host_ram_mb(),
                },
            })
        } else {
            Value::Null
        },
    });
    sink.lock()
        .await
        .send(Message::Text(hello.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    let welcome = tokio::time::timeout(HELLO_TIMEOUT, read.next())
        .await
        .map_err(|_| "gateway did not respond to hello".to_string())?
        .ok_or("connection closed during handshake")?
        .map_err(|e| e.to_string())?;
    // The gateway closes with 4001 when `check_token` fails, so a rejected
    // identity arrives as a Close frame rather than a message. Flagged
    // distinctly (`AUTH_REJECTED`) so `run` can re-register instead of
    // retrying the same refused credential every 30s forever — the state an
    // install lands in when its gateway changed, or that gateway lost its
    // registry.
    if let Message::Close(frame) = &welcome {
        if frame.as_ref().map(|f| u16::from(f.code)) == Some(4001) {
            return Err(AUTH_REJECTED.to_string());
        }
    }
    let welcome: Value = serde_json::from_str(&welcome.to_string()).map_err(|e| e.to_string())?;
    if welcome["type"] != "welcome" {
        return Err(format!("gateway refused: {welcome}"));
    }
    log(format!(
        "connected to {} — sharing {} model(s), sessions: {}",
        cfg.gateway_url,
        models.len(),
        if cfg.workload_templates.is_empty() {
            "off".to_string()
        } else {
            cfg.workload_templates.join(",")
        },
    ));

    // Off the session loop: reconcile the gateway's invitation list with
    // the local mirror, re-asserting it when the gateway has forgotten this
    // node's invitations (registry loss) — that is what lets a consumer's
    // stored invitation UUID reconnect automatically once this provider is
    // back online.
    {
        let client = client.clone();
        let gateway_url = cfg.gateway_url.clone();
        let creds_path = cfg.creds_path.clone();
        let creds = creds.clone();
        tokio::spawn(async move {
            if let Err(e) = sync_invitations(&client, &gateway_url, &creds_path, &creds).await {
                log(format!("invitation mirror sync failed: {e}"));
            }
        });
    }

    // Process-global on purpose — a fresh map per connection partitioned the
    // session state across reconnects (see sessions_cell for the fallout).
    let sessions: Sessions = sessions_cell().clone();
    let stopped: Stopped = stopped_cell().clone();
    // Per-connection: a dropped gateway link invalidates every relayed
    // socket, so these die with it rather than outliving the connection that
    // owns their ws_ids.
    let relays: RelaySockets = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let telemetry: Arc<Mutex<Telemetry>> = Arc::new(Mutex::new(Telemetry {
        docker_ok: docker_live,
        ..Default::default()
    }));
    // What the gateway currently believes about this node's session capability.
    let mut advertised_docker = docker_live;
    // The inventory the gateway last heard about, so a change re-advertises.
    // Capability updates used to fire ONLY on a Docker up/down flip, which
    // froze `images` and `disk_used_mb` at hello time for the whole
    // connection: the scheduler kept waiving the disk clause for an image
    // deleted an hour earlier and sized the disk grant against boot-time
    // usage — observed as a session scheduled onto this node to re-pull a
    // 28.5 GB image the fit check should have refused for disk. Disk churns
    // by small amounts every sample, so it re-advertises on ≥1 GB movement
    // rather than every wiggle; the image list re-advertises on any change.
    let mut advertised_images: Vec<String> = Vec::new();
    let mut advertised_disk_gb: Option<u64> = None;
    // Telemetry gets its OWN client with a short timeout. The shared one is
    // built for long generations (600s), and reusing it here meant a wedged
    // Ollama left a 600-second task outstanding for every 10-second ping.
    let probe_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| client.clone());
    // Prime the cache so the first pong already carries real numbers rather
    // than an empty sample.
    {
        let telemetry = telemetry.clone();
        let client = probe_client.clone();
        let base = cfg.ollama_base.clone();
        let cuda = cfg.cuda;
        tokio::spawn(async move {
            let used = if cuda { nvidia_used_mb().await } else { None };
            let resident = loaded_models(&client, &base).await;
            let names = local_models(&client, &base).await;
            let (disk, imgs, err) = sample_inventory().await;
            let mut t = telemetry.lock().await;
            t.inventory_error = err;
            t.gpu_used_mb = used;
            t.loaded_models = resident;
            t.disk_used_mb = disk;
            t.images = imgs;
            if !names.is_empty() {
                t.models = names;
            }
        });
    }
    let mut current_models = models;
    let mut current_engines = engines;
    // The owner's own stop button. Subscribed here rather than globally so a
    // reconnect gets a fresh receiver and cannot replay stale commands.
    let mut control_rx = control().subscribe();
    // Refreshed by EVERY inbound frame (the gateway's 10s pings included, as
    // pre-parse traffic below). When the deadline fires the link is presumed
    // dead and the session ends, handing control back to run()'s reconnect
    // loop — see GATEWAY_SILENCE_TIMEOUT for why read.next() alone can hang
    // forever on a proxied socket.
    let mut last_rx = tokio::time::Instant::now();
    let result = loop {
        tokio::select! {
            ctl = control_rx.recv() => {
                match ctl {
                    Ok(frame) if frame["type"] == "workload_stop" => {
                        let session_id = frame["session"].as_str().unwrap_or_default().to_string();
                        log(format!("session {session_id} stopped by this machine's owner"));
                        tokio::spawn(stop_workload(
                            sink.clone(),
                            sessions.clone(),
                            stopped.clone(),
                            session_id,
                            cfg.clone(),
                        ));
                    }
                    // Lagged means the owner clicked faster than this loop
                    // drained; the sessions they wanted gone are still listed
                    // and still stoppable, so dropping the backlog is safe.
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            _ = stop.changed() => {
                if *stop.borrow() {
                    // Send a real WS close frame so the gateway drops this
                    // node from the live registry immediately instead of
                    // waiting out the ping-timeout window.
                    let _ = sink.lock().await.send(Message::Close(None)).await;
                    break Ok(());
                }
            }
            _ = tokio::time::sleep_until(last_rx + GATEWAY_SILENCE_TIMEOUT) => {
                break Err(format!(
                    "gateway silent for {}s — link presumed dead",
                    GATEWAY_SILENCE_TIMEOUT.as_secs()
                ));
            }
            msg = read.next() => {
                last_rx = tokio::time::Instant::now();
                let Some(msg) = msg else { break Err("connection closed".into()) };
                let msg = match msg { Ok(m) => m, Err(e) => break Err(e.to_string()) };
                let Message::Text(text) = msg else { continue };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                match frame["type"].as_str() {
                    Some("ping") => {
                        // Answer FIRST, measure later. The pong used to be
                        // sent only after spawning nvidia-smi and calling
                        // Ollama's /api/ps, so the gateway's ping->pong
                        // round trip measured this machine's telemetry
                        // collection rather than the network: observed
                        // rtt jumped from ~50ms to ~380ms after the
                        // resident-model probe was added. That number also
                        // drives nearest-peer scheduling, so a provider with
                        // a slow nvidia-smi looked geographically distant.
                        //
                        // The pong now carries the PREVIOUS sample (at most
                        // one ping interval stale, which is well inside the
                        // gateway's own freshness window) and the refresh
                        // runs detached afterwards.
                        let pong = {
                            let t = telemetry.lock().await;
                            let mut p = json!({"type": "pong"});
                            if let Some(used) = t.gpu_used_mb {
                                p["gpu_used_mb"] = json!(used);
                            }
                            // ALWAYS sent, empty included. The empty list is
                            // the news "nothing is resident anymore" — it was
                            // skipped here, so the gateway (which treats an
                            // absent key as "older worker, keep the last
                            // value") kept advertising models long after they
                            // unloaded: consumers saw a frozen "Resident on
                            // peer GPUs" list with entries stuck at
                            // "expiring now" forever.
                            p["loaded_models"] = json!(t.loaded_models);
                            // Live CPU/RAM, so a CPU peer is as current as a
                            // GPU one (which has had vram_used_mb all along).
                            // Read from the background sampler — a snapshot,
                            // never a blocking probe, so this cannot delay
                            // the pong the way the old telemetry did.
                            let c = crate::hostcpu::snapshot();
                            if c.sampled {
                                p["cpu_percent"] = json!(c.percent);
                            }
                            if c.ram_total_mb > 0 {
                                p["ram_used_mb"] = json!(c.ram_used_mb);
                            }
                            p
                        };
                        if let Err(e) = sink.lock().await.send(Message::Text(pong.to_string())).await {
                            break Err(e.to_string());
                        }
                        // Detached so a slow nvidia-smi or a wedged Ollama
                        // can never stall the read loop that also carries
                        // job and session traffic.
                        {
                            let telemetry = telemetry.clone();
                            let client = probe_client.clone();
                            let base = cfg.ollama_base.clone();
                            let cuda = cfg.cuda;
                            tokio::spawn(async move {
                                let used = if cuda { nvidia_used_mb().await } else { None };
                                let resident = loaded_models(&client, &base).await;
                                let names = local_models(&client, &base).await;
                                let dok = docker_ok().await;
                                // Disk and image inventory change slowly and
                                // cost far more to read, so they ride a slow
                                // tick instead of every ping.
                                let due = {
                                    let mut t = telemetry.lock().await;
                                    t.slow_tick = t.slow_tick.wrapping_add(1);
                                    t.slow_tick % SLOW_SAMPLE_EVERY == 0
                                };
                                let heavy = if due { Some(sample_inventory().await) } else { None };
                                let mut t = telemetry.lock().await;
                                t.docker_ok = dok;
                                if let Some((disk, imgs, err)) = heavy {
                                    if disk.is_some() { t.disk_used_mb = disk; }
                                    if !imgs.is_empty() { t.images = imgs; }
                                    // Logged only when it CHANGES: a broken
                                    // docker would otherwise write the same
                                    // line into the provider's log forever.
                                    if err != t.inventory_error {
                                        if let Some(e) = &err {
                                            log(format!("inventory unavailable: {e}"));
                                        } else {
                                            log("inventory readable again");
                                        }
                                    }
                                    t.inventory_error = err;
                                }
                                t.gpu_used_mb = used;
                                t.loaded_models = resident;
                                if !names.is_empty() {
                                    t.models = names;
                                }
                            });
                        }
                        // Read from the SAME cache the pong used. This call
                        // used to be `local_models(...).await` right here,
                        // and it is the reason a peer vanished mid-session:
                        // the shared client's timeout is 600s, so when heavy
                        // disk I/O (a container bootstrapping) made Ollama
                        // slow to answer /api/tags, the read loop blocked on
                        // it, no pong went out, and the gateway reaped the
                        // node after 25s. Every live session then died with
                        // "provider node disconnected". Same class of bug as
                        // the RTT one — telemetry moved off this path, this
                        // one was left behind.
                        // Docker can die (or come back) long after the hello.
                        // Re-advertise from the cache so the scheduler stops
                        // sending sessions to a node that can no longer run
                        // them — and starts again when it can.
                        let docker_now = telemetry.lock().await.docker_ok;
                        let snapshot = {
                            let t = telemetry.lock().await;
                            (t.disk_used_mb, t.images.clone(), t.inventory_error.clone())
                        };
                        let disk_gb_now = snapshot.0.map(|mb| mb / 1024);
                        let docker_flipped = docker_now != advertised_docker;
                        let inventory_moved = snapshot.1 != advertised_images
                            || disk_gb_now != advertised_disk_gb;
                        if docker_flipped {
                            log(format!(
                                "docker {} — {} container sessions",
                                if docker_now { "is back" } else { "is unreachable" },
                                if docker_now { "re-advertising" } else { "withdrawing" },
                            ));
                        }
                        if docker_flipped || inventory_moved {
                            advertised_docker = docker_now;
                            advertised_images = snapshot.1.clone();
                            advertised_disk_gb = disk_gb_now;
                            let frame = json!({
                                "type": "workloads",
                                "workloads": workload_capability(
                                    cfg, docker_now, snapshot.0, &snapshot.1,
                                    snapshot.2.as_deref(),
                                ),
                            });
                            if let Err(e) = sink.lock().await.send(Message::Text(frame.to_string())).await {
                                break Err(e.to_string());
                            }
                        }

                        // Only while inference is actually being shared. This
                        // refresher exists so a model pulled mid-session becomes
                        // schedulable without a reconnect — but it published the
                        // Ollama inventory unconditionally, so a node that said
                        // `models: []` on the hello (because the operator
                        // unticked "Share GPU inference") re-advertised all of
                        // them within a minute and started taking chat jobs
                        // again. The switch has to hold here too, or it only
                        // works until the next refresh.
                        let (fresh, fresh_engines) = if cfg.share_inference {
                            let ollama = telemetry.lock().await.models.clone();
                            // Colibri is probed inline (the sampler is
                            // Ollama-only): one short localhost round-trip
                            // per ping, same cadence as the reference
                            // Python worker. A dead colibri degrades to
                            // the Ollama list, never to a stalled loop.
                            let colibri = colibri_models(
                                client, &cfg.colibri_base, &cfg.colibri_api_key,
                            ).await;
                            let mut engines = std::collections::HashMap::new();
                            let mut all = ollama.clone();
                            for m in colibri {
                                if !ollama.contains(&m) {
                                    engines.insert(m.clone(), "colibri".to_string());
                                    all.push(m);
                                }
                            }
                            (all, engines)
                        } else {
                            (Vec::new(), std::collections::HashMap::new())
                        };
                        if !fresh.is_empty()
                            && (fresh != current_models || fresh_engines != current_engines)
                        {
                            current_models = fresh.clone();
                            current_engines = fresh_engines.clone();
                            if let Err(e) = sink.lock().await.send(Message::Text(
                                json!({"type": "models", "models": fresh, "engines": fresh_engines}).to_string()
                            )).await {
                                break Err(e.to_string());
                            }
                        }
                    }
                    Some("job") => {
                        tokio::spawn(run_job(
                            sink.clone(),
                            client.clone(),
                            JobUpstreams {
                                ollama_base: cfg.ollama_base.clone(),
                                colibri_base: cfg.colibri_base.clone(),
                                colibri_api_key: cfg.colibri_api_key.clone(),
                                engines: current_engines.clone(),
                            },
                            frame,
                        ));
                    }
                    Some("workload_start") => {
                        tokio::spawn(start_workload(sink.clone(), sessions.clone(), stopped.clone(), cfg.clone(), frame));
                    }
                    Some("model_fetch") => {
                        tokio::spawn(fetch_model(
                            sink.clone(), client.clone(), cfg.clone(), frame,
                        ));
                    }
                    Some("workload_stop") => {
                        let session_id = frame["session"].as_str().unwrap_or_default().to_string();
                        tokio::spawn(stop_workload(
                            sink.clone(),
                            sessions.clone(),
                            stopped.clone(),
                            session_id,
                            cfg.clone(),
                        ));
                    }
                    Some("http") => {
                        tokio::spawn(relay_http(sink.clone(), sessions.clone(), client.clone(), frame));
                    }
                    // Protocol v2.2 — see ws_open(). Spawned, never awaited
                    // here: dialling the container can block for seconds and
                    // this loop carries every other session's traffic.
                    Some("ws_open") => {
                        tokio::spawn(ws_open(
                            sink.clone(),
                            sessions.clone(),
                            relays.clone(),
                            frame,
                        ));
                    }
                    Some("ws_send") => {
                        let id = frame["ws_id"].as_str().unwrap_or_default().to_string();
                        if let Some(data) = frame["data_b64"].as_str().and_then(b64_decode) {
                            let msg = if frame["binary"].as_bool().unwrap_or(false) {
                                Message::Binary(data)
                            } else {
                                Message::Text(String::from_utf8_lossy(&data).into_owned())
                            };
                            // A send to a socket that has already closed is
                            // normal (consumer and container can race), so a
                            // failed send just drops the entry.
                            let mut map = relays.lock().await;
                            if let Some(tx) = map.get(&id) {
                                if tx.send(msg).is_err() {
                                    map.remove(&id);
                                }
                            }
                        }
                    }
                    Some("ws_close") => {
                        let id = frame["ws_id"].as_str().unwrap_or_default().to_string();
                        // Dropping the sender ends the writer task, which
                        // closes the socket to the container.
                        relays.lock().await.remove(&id);
                    }
                    _ => {}
                }
            }
        }
    };
    // Whatever ended the session — graceful stop or a dropped gateway —
    // nothing may keep running on the user's GPU afterwards.
    cleanup_sessions(&sessions).await;
    result
}

/// Run the worker until `stop` is signalled true; reconnects with backoff
/// on any error (gateway unreachable, dropped connection, etc).
pub async fn run(cfg: WorkerConfig, mut stop: watch::Receiver<bool>) {
    // The CPU/RAM sampler feeds both the hello's `cpu` block and the pong's
    // live figures. Idempotent, and started HERE rather than only from the
    // GUI setup: the headless `kmplify-node` binary never touched it, so a
    // headless provider would have advertised an empty CPU model and no
    // usage at all — exactly the "CPU peers have no detail" complaint, on
    // the deployment where nobody is watching a window to notice.
    crate::hostcpu::start();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("reqwest client");
    // At most one immediate re-registration per connected session. Without
    // this cap a gateway that rejects even a freshly minted identity would
    // spin: reject -> register -> reject -> register, with no delay between
    // and a POST /fabric/register on every pass. Reset after any session that
    // actually ran, so a genuine rejection later still heals.
    let mut healed_since_connect = false;
    // Drops the current-sink registration however run() exits, so a stopped
    // worker does not leave session tasks a sink to a socket nobody owns.
    struct ClearSinkOnExit;
    impl Drop for ClearSinkOnExit {
        fn drop(&mut self) {
            if let Ok(mut cur) = current_sink_cell().try_write() {
                *cur = None;
            }
        }
    }
    let _clear_on_exit = ClearSinkOnExit;
    loop {
        if *stop.borrow() {
            return;
        }
        match session(&client, &cfg, &mut stop).await {
            Ok(()) => healed_since_connect = false,
            Err(e) => {
                if *stop.borrow() {
                    return;
                }
                if e == AUTH_REJECTED && !healed_since_connect {
                    // The gateway does not know this node id — retrying with
                    // it can only fail identically, forever. Mint a fresh
                    // identity for THIS gateway and reconnect immediately; an
                    // id no reachable gateway recognises carries nothing to
                    // preserve.
                    match register_identity(&cfg.gateway_url, &cfg.creds_path).await {
                        Ok(_) => {
                            healed_since_connect = true;
                            log("identity rejected by the gateway — re-registered, reconnecting");
                            continue;
                        }
                        Err(re) => log(format!(
                            "identity rejected and re-registration failed ({re})"
                        )),
                    }
                } else if e == AUTH_REJECTED {
                    log(format!(
                        "gateway rejected even a freshly registered identity; retrying in {}s",
                        RECONNECT_DELAY.as_secs()
                    ));
                } else {
                    log(format!(
                        "connection lost ({e}); retrying in {}s",
                        RECONNECT_DELAY.as_secs()
                    ));
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return;
                }
            }
        }
    }
}

pub fn default_creds_path(data_dir: &Path) -> PathBuf {
    data_dir.join("fabric_node.json")
}

#[cfg(test)]
mod model_discovery_tests {
    use super::*;

    /// The existing path, unchanged — every deployed desktop install.
    #[test]
    fn ollama_tags_are_read_by_name() {
        let body = json!({"models": [{"name": "llama3.1:8b"}, {"name": "bge-m3"}]});
        assert_eq!(models_from_native(&body), vec!["llama3.1:8b", "bge-m3"]);
    }

    /// A successful empty answer is an ANSWER — "nothing pulled here" — and
    /// the caller must not treat it as a reason to try the other shape.
    /// Identical to the Python reference worker, deliberately.
    #[test]
    fn ollama_with_nothing_pulled_reports_nothing() {
        assert!(models_from_native(&json!({"models": []})).is_empty());
        assert!(models_from_native(&json!({})).is_empty());
    }

    /// vLLM / LiteLLM / TGI: the house-node shape.
    #[test]
    fn openai_models_are_read_by_id() {
        let body = json!({"object": "list", "data": [{"id": "qwen2.5:14b", "object": "model"}]});
        assert_eq!(models_from_openai(&body), vec!["qwen2.5:14b"]);
    }

    /// The id is what a consumer sends as `model` and what the gateway
    /// matches on. Rewriting it would advertise a name nothing routes to.
    #[test]
    fn an_openai_id_is_taken_verbatim() {
        let body = json!({"data": [{"id": "Qwen/Qwen2.5-14B-Instruct-AWQ"}]});
        assert_eq!(
            models_from_openai(&body),
            vec!["Qwen/Qwen2.5-14B-Instruct-AWQ"]
        );
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        let body = json!({"data": [{"id": "good"}, {"object": "model"}, {"id": ""}]});
        assert_eq!(models_from_openai(&body), vec!["good"]);
        assert!(models_from_openai(&json!({"data": null})).is_empty());
        assert!(models_from_openai(&json!({})).is_empty());
    }

    /// The bug the fallback exists for: vLLM answers /api/tags with a JSON
    /// 404 body. It PARSES, so a parse-only check reads it as "Ollama with no
    /// models" and never tries /v1/models — which is exactly how a house node
    /// advertised nothing while looking perfectly healthy.
    #[test]
    fn a_json_404_body_is_not_mistaken_for_an_empty_model_list() {
        assert!(models_from_native(&json!({"detail": "Not Found"})).is_empty());
        // …which is why local_models() gates on status().is_success() before
        // ever calling models_from_native, rather than on the parse alone.
    }
}

#[cfg(test)]
mod peer_warmth_tests {
    use super::*;

    /// The default path: nobody specified warmth, so the relay fills it in.
    /// Without this a peer-served model unloaded after Ollama's 5-minute
    /// default and the next turn of a conversation paid a full reload.
    #[test]
    fn a_bare_chat_request_gets_keep_alive_and_context() {
        let out = keep_peer_model_warm(json!({"model": "gemma4:latest"}), "/api/chat");
        assert_eq!(out["keep_alive"], json!(PEER_KEEP_ALIVE));
        assert_eq!(out["options"]["num_ctx"], json!(PEER_NUM_CTX));
    }

    /// num_ctx must match the provider's pre-warm, or Ollama loads a SECOND
    /// copy at the default context while `ollama ps` still shows the model
    /// resident — the 9.6s "load" measured on a 4090 with the model pinned.
    #[test]
    fn the_context_matches_the_prewarm_so_no_second_copy_loads() {
        assert_eq!(
            PEER_NUM_CTX, 16384,
            "must track settings-ollama.yaml context_window"
        );
    }

    /// Never override what the caller asked for: a consumer that wants a
    /// short-lived model, or a specific context, must still get it.
    #[test]
    fn explicit_values_win() {
        let out = keep_peer_model_warm(
            json!({"model": "m", "keep_alive": "0", "options": {"num_ctx": 2048}}),
            "/api/chat",
        );
        assert_eq!(out["keep_alive"], json!("0"));
        assert_eq!(out["options"]["num_ctx"], json!(2048));
    }

    /// Other options the caller set must survive alongside the injection.
    #[test]
    fn existing_options_are_preserved() {
        let out = keep_peer_model_warm(
            json!({"model": "m", "options": {"temperature": 0.1}}),
            "/api/chat",
        );
        assert_eq!(out["options"]["temperature"], json!(0.1));
        assert_eq!(out["options"]["num_ctx"], json!(PEER_NUM_CTX));
    }

    /// The OpenAI-shaped endpoint has no num_ctx; sending one would be noise
    /// at best. keep_alive still applies — Ollama honours it on both.
    #[test]
    fn openai_shaped_requests_get_keep_alive_but_no_num_ctx() {
        let out = keep_peer_model_warm(json!({"model": "m"}), "/v1/chat/completions");
        assert_eq!(out["keep_alive"], json!(PEER_KEEP_ALIVE));
        assert!(
            out.get("options").is_none(),
            "num_ctx has no meaning on this path"
        );
    }

    /// Embeddings are short and their models small; the reload cost this
    /// addresses is a chat-model problem, and reshaping a request type that
    /// currently works is gratuitous risk.
    #[test]
    fn embeddings_are_left_alone() {
        let out = keep_peer_model_warm(json!({"model": "bge-m3:567m"}), "/v1/embeddings");
        assert!(out.get("keep_alive").is_none());
        assert!(out.get("options").is_none());
    }

    #[test]
    fn a_non_object_payload_is_returned_unchanged() {
        assert_eq!(
            keep_peer_model_warm(json!("garbage"), "/api/chat"),
            json!("garbage")
        );
    }
}

#[cfg(test)]
mod container_death_tests {
    use super::*;

    /// The verdicts that decide whether the ready loop keeps waiting or
    /// reports a death. Getting "restarting" wrong either kills a session
    /// docker is about to save, or waits forever on one it will not.
    #[test]
    fn alive_states_are_not_a_verdict() {
        assert_eq!(parse_container_state(true, "running 0"), None);
        assert_eq!(parse_container_state(true, "created 0"), None);
        assert_eq!(parse_container_state(true, "restarting 1"), None);
    }

    #[test]
    fn dead_states_report_their_exit_code() {
        assert_eq!(parse_container_state(true, "exited 1"), Some(1));
        assert_eq!(parse_container_state(true, "exited 137"), Some(137));
        assert_eq!(parse_container_state(true, "dead 255"), Some(255));
    }

    #[test]
    fn a_container_docker_no_longer_knows_is_dead_not_alive() {
        // inspect fails entirely (e.g. removed by hand, or an older node's
        // --rm beat us to it). Treating that as "alive" recreates the
        // 16-minute silent wait this exists to end.
        assert_eq!(parse_container_state(false, ""), Some(-1));
    }

    #[test]
    fn garbled_inspect_output_is_dead_with_unknown_code() {
        assert_eq!(parse_container_state(true, ""), Some(-1));
        assert_eq!(parse_container_state(true, "exited notanumber"), Some(-1));
    }

    #[test]
    fn log_tail_keeps_the_end_where_the_crash_is() {
        let long = format!("{}THE CRASH", "banner ".repeat(400));
        let trimmed = trim_log_tail(&long, 40);
        assert!(
            trimmed.ends_with("THE CRASH"),
            "lost the crash line: {trimmed}"
        );
        assert!(trimmed.starts_with('…'), "no truncation marker");
        assert!(trimmed.chars().count() <= 41);
        assert_eq!(trim_log_tail("  \n ", 100), "(no output captured)");
        assert_eq!(trim_log_tail("short", 100), "short");
    }
}

#[cfg(test)]
mod volume_rule_tests {
    use super::is_fabric_volume;

    /// The gateway decides WHAT runs here; it does not get to decide which of
    /// the provider's directories a consumer's container can see.
    #[test]
    fn only_fabric_named_volumes_onto_absolute_paths() {
        assert!(is_fabric_volume("kmplify-fabric-comfyui-cu126", "/root"));
        assert!(is_fabric_volume(
            "kmplify-fabric-comfyui-models",
            "/root/ComfyUI/models"
        ));

        // A host path is a host path however it is dressed up.
        assert!(!is_fabric_volume("/", "/host"));
        assert!(!is_fabric_volume("/home/dave", "/data"));
        assert!(!is_fabric_volume("/etc", "/etc"));
        // Someone else's namespace, or none.
        assert!(!is_fabric_volume("postgres_data", "/var/lib/postgresql"));
        assert!(!is_fabric_volume("kmplify-other", "/root"));
        // Relative targets, and colons that could smuggle mount options.
        assert!(!is_fabric_volume("kmplify-fabric-x", "root"));
        assert!(!is_fabric_volume("kmplify-fabric-x", "/root:ro"));
    }
}

#[cfg(test)]
mod image_pin_tests {
    use super::{image_allowed_for, image_repository, IMAGE_PINS};

    #[test]
    fn repository_survives_every_spelling_of_docker_hub() {
        for reference in [
            "ollama/ollama",
            "ollama/ollama:latest",
            "ollama/ollama:0.5.4",
            "docker.io/ollama/ollama:latest",
            "index.docker.io/ollama/ollama",
            "ollama/ollama@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert_eq!(
                image_repository(reference),
                "ollama/ollama",
                "for {reference}"
            );
        }
        // An official image is `library/x` upstream and `x` everywhere else.
        assert_eq!(image_repository("docker.io/library/redis:7"), "redis");
        // A real registry host is kept: it is part of the publisher's identity.
        assert_eq!(
            image_repository("ghcr.io/kmplify/colibri:cpu"),
            "ghcr.io/kmplify/colibri"
        );
        assert_eq!(
            image_repository("localhost:5000/mine/app:dev"),
            "localhost:5000/mine/app"
        );
        // The first segment of a Docker Hub reference is a user, not a host.
        assert_eq!(
            image_repository("vllm/vllm-openai:latest"),
            "vllm/vllm-openai"
        );
    }

    /// The attack this exists to stop: an enabled template id carrying an
    /// image nobody on this machine ever agreed to run.
    #[test]
    fn enabled_template_does_not_imply_arbitrary_image() {
        assert!(image_allowed_for("ollama", "ollama/ollama:latest"));
        assert!(image_allowed_for("ollama", "docker.io/ollama/ollama:0.5.4"));

        assert!(!image_allowed_for("ollama", "attacker/miner:latest"));
        // Same image name under a different publisher.
        assert!(!image_allowed_for("ollama", "evil/ollama:latest"));
        // A registry the pin never mentioned.
        assert!(!image_allowed_for("ollama", "ghcr.io/ollama/ollama:latest"));
        // Right image, wrong template: pins are per template, not a global set.
        assert!(!image_allowed_for("comfyui", "ollama/ollama:latest"));
    }

    /// Tags move (the ComfyUI template went cu124 to cu126 in flight); a node
    /// that refused the new tag would have gone dark until it was rebuilt.
    #[test]
    fn tags_and_digests_are_free_to_move() {
        assert!(image_allowed_for(
            "comfyui",
            "yanwk/comfyui-boot:cu124-slim"
        ));
        assert!(image_allowed_for(
            "comfyui",
            "yanwk/comfyui-boot:cu126-slim"
        ));
        assert!(image_allowed_for(
            "comfyui",
            "yanwk/comfyui-boot@sha256:1111111111111111111111111111111111111111111111111111111111111111"
        ));
    }

    /// Fail closed: an id this build has never heard of gets nothing, so a
    /// gateway cannot invent a template to escape the table.
    #[test]
    fn unknown_template_is_refused_not_waved_through() {
        assert!(!image_allowed_for(
            "brand-new-template",
            "vllm/vllm-openai:latest"
        ));
        assert!(!image_allowed_for("", "vllm/vllm-openai:latest"));
        assert!(!image_allowed_for("ollama", ""));
    }

    /// Every pin is a bare repository. A tag here would be silently
    /// unmatchable, since the left side of the comparison never has one.
    #[test]
    fn pin_table_holds_repositories_only() {
        for (template, repo) in IMAGE_PINS {
            assert!(!repo.contains('@'), "{template} pins a digest: {repo}");
            assert!(
                !repo.rsplit('/').next().unwrap_or(repo).contains(':'),
                "{template} pins a tag: {repo}"
            );
            assert_eq!(
                &image_repository(repo),
                repo,
                "{template} pin is not normalized"
            );
        }
    }
}

#[cfg(test)]
mod rfc3339_tests {
    use super::rfc3339_secs;

    // Hand-computed epochs — the civil-from-days arithmetic is exactly the
    // kind of code that returns a plausible-but-wrong number, which here
    // would show every consumer a wrong keep_alive countdown.
    #[test]
    fn parses_utc_instants() {
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_secs("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(rfc3339_secs("2026-07-27T09:00:00Z"), Some(1_785_142_800));
    }

    #[test]
    fn applies_the_timezone_offset() {
        // Same instant, three spellings: ignoring the offset would put these
        // hours apart and make the countdown wrong by the provider's zone.
        let utc = rfc3339_secs("2026-07-27T09:00:00Z").unwrap();
        assert_eq!(rfc3339_secs("2026-07-27T11:00:00+02:00"), Some(utc));
        assert_eq!(rfc3339_secs("2026-07-27T04:00:00-05:00"), Some(utc));
    }

    #[test]
    fn tolerates_fractional_seconds() {
        // Ollama emits nanosecond precision; the fraction is ignored, not fatal.
        assert_eq!(
            rfc3339_secs("2026-07-27T11:04:12.123456789+02:00"),
            rfc3339_secs("2026-07-27T11:04:12+02:00")
        );
    }

    #[test]
    fn leap_day_is_not_off_by_one() {
        // 2024-02-29 exists; 2100 is NOT a leap year (the /100 rule).
        assert_eq!(rfc3339_secs("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(rfc3339_secs("2100-03-01T00:00:00Z"), Some(4_107_542_400));
    }

    #[test]
    fn malformed_input_yields_none_rather_than_a_wrong_time() {
        assert_eq!(rfc3339_secs(""), None);
        assert_eq!(rfc3339_secs("not-a-timestamp"), None);
        assert_eq!(rfc3339_secs("2026-07-27"), None);
    }
}

#[cfg(test)]
mod session_cpus_tests {
    use super::session_cpus;

    #[test]
    fn a_greedy_request_is_clamped_to_half_the_host() {
        // The safety property: a buggy or hostile gateway must not be able to
        // hand a consumer's container the whole machine.
        assert_eq!(session_cpus(Some(999.0), 32.0, None), 16.0);
        assert_eq!(session_cpus(Some(32.0), 32.0, None), 16.0);
    }

    #[test]
    fn a_modest_request_is_honoured() {
        assert_eq!(session_cpus(Some(8.0), 32.0, None), 8.0);
        assert_eq!(session_cpus(Some(4.0), 32.0, None), 4.0);
    }

    #[test]
    fn an_absent_or_nonsense_value_still_bounds_the_container() {
        // Pre-cpus gateways send no key at all; these must not fall through
        // to docker's unlimited default.
        for bad in [None, Some(0.0), Some(-4.0), Some(f64::NAN)] {
            assert_eq!(
                session_cpus(bad, 32.0, None),
                16.0,
                "bad input {bad:?} was not bounded"
            );
        }
    }

    #[test]
    fn a_single_core_host_still_gets_one_core() {
        // Half of 1 rounds to 0, and `--cpus 0` is rejected by docker.
        assert_eq!(session_cpus(None, 1.0, None), 1.0);
        assert_eq!(session_cpus(Some(8.0), 1.0, None), 1.0);
    }

    #[test]
    fn the_operators_slider_wins_over_the_gateway() {
        // Their machine, their ceiling — below the default and above it.
        assert_eq!(session_cpus(Some(8.0), 32.0, Some(4.0)), 4.0);
        assert_eq!(session_cpus(None, 32.0, Some(2.0)), 2.0);
        // Lending MORE than half is allowed if they explicitly chose it,
        // but never more than the machine actually has.
        assert_eq!(session_cpus(Some(999.0), 32.0, Some(24.0)), 24.0);
        assert_eq!(session_cpus(Some(999.0), 32.0, Some(999.0)), 32.0);
    }

    #[test]
    fn a_nonsense_ceiling_falls_back_to_the_default() {
        for bad in [Some(0.0), Some(-1.0), Some(f64::NAN)] {
            assert_eq!(
                session_cpus(Some(8.0), 32.0, bad),
                8.0,
                "ceiling {bad:?} broke the default"
            );
        }
    }
}

#[cfg(test)]
mod host_memory_tests {
    use super::{gpu_addressable_mb, host_ram_mb};

    /// The regression this guards: RAM used to be probed by shelling out per
    /// platform, and the Windows probe (`wmic`) is removed from Windows 11
    /// 24H2 / Server 2025. It returned 0 there, so `cpu_share.ram_mb` — and
    /// with it every RAM figure a consumer sees for that provider — was 0.
    /// Any host running this test has memory, so 0 means the probe is broken
    /// on this platform.
    #[test]
    fn reports_real_host_memory() {
        let mb = host_ram_mb();
        assert!(
            mb > 0,
            "host RAM probe returned 0 — broken on this platform"
        );
        // Sanity bounds rather than an exact figure: ≥256 MB rules out a
        // unit slip (bytes/KB read as MB), ≤16 TB rules out the inverse.
        assert!(
            (256..=16 * 1024 * 1024).contains(&mb),
            "host RAM {mb} MB is outside any plausible range — unit error?"
        );
    }

    /// A GPU may never be told it can address more than the machine has.
    #[test]
    fn gpu_ceiling_never_exceeds_installed_ram() {
        assert!(gpu_addressable_mb() <= host_ram_mb());
    }
}
