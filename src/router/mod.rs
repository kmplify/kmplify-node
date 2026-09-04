//! The LAN router: this node as the hub of a personal inference cluster.
//!
//! The fabric worker lends this machine *outward*, to a gateway, over one
//! outbound socket. The router points the other way: it finds the other
//! kmplify-nodes on the same network, shows what each of them can serve, and
//! (from the second phase on, see docs/ROUTER.md) hands a local application
//! one endpoint that routes every request to whichever machine on the LAN is
//! best placed to answer it. Nothing leaves the network; a prompt travels
//! from the application to a peer's engine and back, and no cloud sees it.
//!
//! The design is adapted from NVIDIA's Personal AI Router (Apache-2.0, see
//! NOTICE), collapsed from thirteen Go processes and an Electron shell into
//! tasks inside this one binary. What PAIR spreads over a broker, workers
//! and JSON-RPC is here one shared [`Router`] behind a mutex, sampled by
//! tasks and drawn by the GUI. The mapping is in docs/ROUTER.md.
//!
//! One deliberate departure from the rest of this crate: the fabric worker
//! never listens on a port, and that stays true. The router is a separate,
//! opt-in mode (`kmplify-node gui`), and only it will bind listeners, only
//! on this machine's own interfaces, and only for the surfaces documented
//! in docs/ROUTER.md. Discovery is the one network activity this phase has,
//! and it is multicast on the local link, nothing more.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod cluster;
pub mod discovery;
pub mod engine;
pub mod listen;
pub mod node_info;
pub mod proxy;
pub mod schedule;
pub mod telemetry;

/// Samples kept per metric: one a second, so a minute of history, which is
/// what a card's chart shows and what the desktop app's charts show too.
pub const HISTORY: usize = 60;

/// The mDNS service every router-mode node advertises and browses. One
/// record per host, identity and ports in TXT, the same shape PAIR chose so
/// a TXT record stays small enough to survive multicast.
pub const SERVICE_TYPE: &str = "_kmplify-node._tcp.local.";

/// How long a peer stays listed after its last announcement or probe. mDNS
/// re-announces well inside this, and a node that is genuinely gone should
/// leave the screen rather than sit there looking routable.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(45);

/// Where the router's own surfaces listen by default. Chosen apart from
/// the engines' defaults and from PAIR's `143xx` band so a machine running
/// both is not a port fight. `KMPLIFY_ROUTER_PORTS=info,ollama,openai`
/// overrides all three, which is what lets two nodes share one machine for
/// a test.
pub const NODE_INFO_PORT: u16 = 14418;
pub const PROXY_OLLAMA_PORT: u16 = 11440;
pub const PROXY_OPENAI_PORT: u16 = 11441;

fn ports() -> (u16, u16, u16) {
    static P: std::sync::OnceLock<(u16, u16, u16)> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let parsed: Option<Vec<u16>> = std::env::var("KMPLIFY_ROUTER_PORTS")
            .ok()
            .map(|s| s.split(',').map(|p| p.trim().parse::<u16>()).collect::<Result<_, _>>().ok())
            .flatten();
        match parsed.as_deref() {
            Some([a, b, c]) => (*a, *b, *c),
            _ => (NODE_INFO_PORT, PROXY_OLLAMA_PORT, PROXY_OPENAI_PORT),
        }
    })
}

pub fn node_info_port() -> u16 {
    ports().0
}

pub fn proxy_ollama_port() -> u16 {
    ports().1
}

pub fn proxy_openai_port() -> u16 {
    ports().2
}

/// A GPU sample older than this is not evidence of anything; the scheduler
/// treats the node as neutral rather than idle.
pub const TELEMETRY_STALE: Duration = Duration::from_secs(10);

/// A minute of one measurement, 0–100.
///
/// Pre-filled with zeros so a card's chart is a flat line from its first
/// frame rather than a dot that grows a tail; the desktop app does the
/// same, and a chart that changes shape for its first minute reads as a
/// bug.
#[derive(Clone, Debug)]
pub struct Series {
    buf: VecDeque<f32>,
}

impl Default for Series {
    fn default() -> Self {
        Self {
            buf: std::iter::repeat(0.0).take(HISTORY).collect(),
        }
    }
}

impl Series {
    pub fn push(&mut self, v: f32) {
        if self.buf.len() >= HISTORY {
            self.buf.pop_front();
        }
        self.buf.push_back(v.clamp(0.0, 100.0));
    }

    pub fn latest(&self) -> f32 {
        self.buf.back().copied().unwrap_or(0.0)
    }

    pub fn points(&self) -> impl Iterator<Item = f32> + '_ {
        self.buf.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct GpuInfo {
    pub name: String,
    pub total_mb: u64,
}

/// What a node card charts: the four measurements the desktop app charts,
/// plus whether each is a real reading. `gpu_known`/`vram_known` are false
/// where the platform gives no number (Apple Silicon has no distinct VRAM;
/// a CPU-only host has no GPU), and the chart then says so instead of
/// drawing a zero that would read as an idle card.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub gpu: Series,
    pub vram: Series,
    pub cpu: Series,
    pub ram: Series,
    pub gpu_known: bool,
    pub vram_known: bool,
    /// At least one real sample has landed; before that the card says
    /// "no metrics yet" rather than charting the pre-fill.
    pub sampled: bool,
    pub vram_used_mb: u64,
    pub ram_used_mb: u64,
    /// When the GPU figure was last a real reading, for the scheduler's
    /// staleness rule.
    pub gpu_sampled_at: Option<Instant>,
    /// The busiest GPU's utilisation, smoothed, and the pressure unit it
    /// maps to. See [`schedule`].
    pub gpu_smoothed: f32,
    pub pressure: u8,
}

impl Metrics {
    /// Fold in a GPU utilisation reading, keeping the smoothed figure and
    /// pressure unit the scheduler ranks by.
    pub fn observe_gpu(&mut self, pct: f32, now: Instant) {
        self.gpu.push(pct);
        self.gpu_smoothed = if self.gpu_known {
            schedule::smooth(self.gpu_smoothed, pct)
        } else {
            pct
        };
        self.gpu_known = true;
        self.gpu_sampled_at = Some(now);
        self.pressure = schedule::pressure_step(self.pressure, self.gpu_smoothed);
    }
}

/// An inference engine on a node, as the router understands it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    /// Stable id (`ollama`, `lmstudio`, …), the same ids `engines::KNOWN` uses.
    pub id: String,
    pub name: String,
    pub base: String,
    pub models: Vec<String>,
    /// Answered a probe recently. A known engine that is installed but not
    /// serving is listed dimmed, not hidden: the card should say what this
    /// machine *could* run, not just what it happens to be running.
    pub running: bool,
    /// A binary for it was found: on PATH, in a known install location,
    /// or in the router's own engines directory.
    pub installed: bool,
    /// This process started it, so it can also stop it. An instance found
    /// already serving is *adopted*: used, never stopped, never moved.
    pub owned: bool,
}

/// One engine operation in flight or finished, for the card: install,
/// start, stop, pull. Carried in node-info reports so a paired node's card
/// shows its progress too.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct EngineOp {
    pub id: u64,
    pub engine: String,
    pub action: String,
    pub model: String,
    pub state: OpState,
    pub message: String,
    /// Progress in bytes where the step has a size (download, pull).
    pub done: u64,
    pub total: u64,
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpState {
    Running,
    Done,
    Failed,
}

pub const MAX_OPS: usize = 20;

/// How a node entered the directory. Discovered and manual nodes are the
/// same kind of peer once they answer; the distinction is kept so the
/// cluster screen can offer "forget" for one and not the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Local,
    Discovered,
    Manual,
    /// A pinned cluster member reached at the address it was last seen
    /// at. Like a manual node it never expires: a member that is off
    /// tonight is still a member.
    Member,
}

#[derive(Clone, Debug)]
pub struct Node {
    /// The correlation key everywhere: never the hostname, which two
    /// machines can share and one machine can change.
    pub id: String,
    pub name: String,
    pub address: String,
    pub source: Source,
    pub gpus: Vec<GpuInfo>,
    pub cpu_model: String,
    pub cpu_cores: usize,
    pub ram_total_mb: u64,
    pub engines: Vec<Engine>,
    /// Engine operations on that node, newest first.
    pub ops: Vec<EngineOp>,
    pub metrics: Metrics,
    pub version: String,
    pub last_seen: Instant,
    /// The cluster the node says it belongs to (its announcement or its
    /// report), empty for none. Display only; trust is the pinned
    /// certificate, never this string.
    pub cluster_id: String,
    /// Queued plus running work the node itself reports, the scheduler's
    /// first input. Zero for this machine; its own jobs are counted live.
    pub reported_pending: u32,
    /// The proxy ports the node says it listens on, so a peer request goes
    /// where that node actually serves rather than to an assumed default.
    pub proxy_ports: (u16, u16),
    /// Where its node-info answers: the default unless the node said
    /// otherwise (a typed `host:port`, or the port it named when pairing).
    pub info_port: u16,
    /// Peer polling: consecutive failures and when to try next. Healthy
    /// nodes are sampled every two seconds; failures back off to thirty.
    pub poll_failures: u32,
    pub next_poll: Instant,
}

impl Node {
    pub fn is_local(&self) -> bool {
        self.source == Source::Local
    }

    /// The card a peer sees before any HTTP round trip: what the
    /// announcement carried, and polling due immediately.
    pub fn new_peer(id: String, name: String, address: String, source: Source, now: Instant) -> Self {
        Self {
            id,
            name,
            address,
            source,
            gpus: vec![],
            cpu_model: String::new(),
            cpu_cores: 0,
            ram_total_mb: 0,
            engines: vec![],
            ops: vec![],
            metrics: Metrics::default(),
            version: String::new(),
            last_seen: now,
            cluster_id: String::new(),
            reported_pending: 0,
            proxy_ports: (proxy_ollama_port(), proxy_openai_port()),
            info_port: node_info_port(),
            poll_failures: 0,
            next_poll: now,
        }
    }

    /// Split a typed `host[:port]` into the card's address and info port.
    pub fn parse_address(typed: &str) -> (String, u16) {
        let t = typed.trim().trim_end_matches('/');
        match t.rsplit_once(':') {
            Some((host, port)) if !t.starts_with('[') && !host.contains(':') => match port.parse() {
                Ok(p) => (host.to_string(), p),
                Err(_) => (t.to_string(), node_info_port()),
            },
            _ => (t.to_string(), node_info_port()),
        }
    }

    /// Does a running engine here advertise `model`? Ollama's implicit
    /// `:latest` is normalised so `qwen3` and `qwen3:latest` are one model.
    pub fn serves(&self, model: &str, api: proxy::Api) -> Option<&Engine> {
        let want = normalize_model(model);
        self.running_engines().find(|e| {
            api.accepts(&e.id) && e.models.iter().any(|m| normalize_model(m) == want)
        })
    }

    pub fn online(&self, now: Instant) -> bool {
        self.is_local() || now.duration_since(self.last_seen) < PEER_TIMEOUT
    }

    /// The engines answering right now, which is what routing can use.
    pub fn running_engines(&self) -> impl Iterator<Item = &Engine> {
        self.engines.iter().filter(|e| e.running)
    }

    pub fn model_count(&self) -> usize {
        self.running_engines().map(|e| e.models.len()).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
}

impl JobState {
    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => "Queued at",
            JobState::Running => "Started at",
            JobState::Completed => "Completed at",
            JobState::Failed => "Failed at",
        }
    }
}

/// One request the router saw, for the jobs column. Never the prompt, never
/// the answer: model, engine, where it came from, where it ran, and when.
/// That rule is the desktop app's and PAIR's alike, and it has no switch.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: String,
    pub model: String,
    pub engine: String,
    pub requested_from: String,
    pub ran_on: String,
    /// The node it ran on, by id, so pending counts attribute correctly
    /// even when two nodes share a hostname.
    #[serde(default)]
    pub node_id: String,
    pub state: JobState,
    pub at_ms: u64,
    pub error: String,
    /// Dispatched by this machine's proxy (as opposed to learned from a
    /// peer's report). Never on the wire: a peer's copy is a copy.
    #[serde(skip)]
    pub local_origin: bool,
}

impl Job {
    pub fn is_pending(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Running)
    }

    pub fn requested_from_here(&self) -> bool {
        self.local_origin
    }

    /// An id unique across the cluster without coordination: the node id,
    /// the clock, and a counter for two dispatches in one millisecond.
    pub fn new_id(node_id: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}-{}",
            &node_id[..8.min(node_id.len())],
            crate::status::now_ms(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }
}

/// Ollama's implicit tag, made explicit so inventories compare.
pub fn normalize_model(name: &str) -> String {
    let n = name.trim();
    if n.contains(':') {
        n.to_string()
    } else {
        format!("{n}:latest")
    }
}

/// Everything the GUI draws, behind one lock. Samplers write, the frame
/// reads a clone, and neither holds the lock across an await or a paint.
#[derive(Clone, Debug, Default)]
pub struct Router {
    pub self_id: String,
    pub nodes: BTreeMap<String, Node>,
    /// Newest first, capped so a busy week does not become a memory leak.
    pub jobs: VecDeque<Job>,
    /// Addresses the operator typed, kept even while they do not answer so
    /// a machine that is switched off at night is still on the list.
    pub manual: Vec<String>,
    pub log: VecDeque<String>,
    /// Discovery's own state, for the cluster screen: browsing, or why not.
    pub discovery: String,
    /// Accept requests from paired nodes at the proxies. On by default
    /// while the router runs; off makes this machine a consumer of the
    /// cluster that serves nothing to it.
    pub lan_ingress: bool,
    /// What the listeners report: bound, or why not.
    pub listeners: String,
    /// Where cluster.json and the certificate live.
    pub node_dir: PathBuf,
    /// This node's certificate; None when it could not be minted, in which
    /// case the peer surfaces stay plaintext-only and pairing is refused.
    pub identity: Option<Arc<cluster::Identity>>,
    pub cluster: cluster::ClusterFile,
    /// The live pin set every TLS verifier consults.
    pub pins: cluster::Pins,
    pub tls_server: Option<Arc<rustls::ServerConfig>>,
    /// An HTTP client that speaks the cluster's mutual TLS.
    pub tls_client: Option<reqwest::Client>,
    /// The invitation open on this node, if any.
    pub invite: Option<cluster::Invite>,
}

pub const MAX_JOBS: usize = 500;
const MAX_LOG: usize = 200;

impl Router {
    pub fn local(&self) -> Option<&Node> {
        self.nodes.get(&self.self_id)
    }

    pub fn local_mut(&mut self) -> Option<&mut Node> {
        self.nodes.get_mut(&self.self_id)
    }

    /// This machine first, then the rest by name: the order a card list
    /// should never shuffle.
    pub fn ordered(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.nodes.values().collect();
        v.sort_by(|a, b| {
            b.is_local()
                .cmp(&a.is_local())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        v
    }

    pub fn push_job(&mut self, job: Job) {
        self.jobs.push_front(job);
        while self.jobs.len() > MAX_JOBS {
            self.jobs.pop_back();
        }
    }

    /// A job reported by a peer: update the entry if it is known, else add
    /// it, so every window shows work running anywhere in the cluster.
    pub fn merge_job(&mut self, job: Job) {
        match self.jobs.iter_mut().find(|j| j.id == job.id) {
            Some(existing) => {
                existing.state = job.state;
                existing.error = job.error;
                existing.at_ms = job.at_ms;
            }
            None => self.push_job(job),
        }
    }

    pub fn set_job_state(&mut self, id: &str, state: JobState, error: impl Into<String>) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.state = state;
            j.error = error.into();
            j.at_ms = crate::status::now_ms();
        }
    }

    /// Queued plus running work attributed to a node: what it reports about
    /// itself, plus what this machine has dispatched to it and not yet seen
    /// reported back — the reservation that spreads a burst.
    pub fn pending_for(&self, node_id: &str) -> u32 {
        let local: u32 = self
            .jobs
            .iter()
            .filter(|j| j.is_pending() && j.node_id == node_id && j.requested_from_here())
            .count() as u32;
        let reported = self.nodes.get(node_id).map(|n| n.reported_pending).unwrap_or(0);
        if node_id == self.self_id {
            // This machine's own count is live; the reported figure is what
            // it tells peers, derived from the same jobs.
            self.jobs
                .iter()
                .filter(|j| j.is_pending() && j.node_id == node_id)
                .count() as u32
        } else {
            reported.max(local)
        }
    }

    /// Every model some online node can serve through `api`, with the
    /// nodes that have it. The cluster's inventory, for the fan-out lists
    /// and the chat pane's model picker.
    pub fn inventory(&self, api: proxy::Api, now: Instant) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for n in self.nodes.values().filter(|n| n.online(now)) {
            for e in n.running_engines().filter(|e| api.accepts(&e.id)) {
                for m in &e.models {
                    if m.is_empty() {
                        continue;
                    }
                    out.entry(m.clone()).or_default().push(n.name.clone());
                }
            }
        }
        out
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        let stamp = crate::status::clock_hms(crate::status::now_ms());
        self.log.push_back(format!("{stamp} {}", line.into()));
        while self.log.len() > MAX_LOG {
            self.log.pop_front();
        }
    }

    /// Record a new engine operation on this machine and return its id.
    pub fn op_start(&mut self, engine: &str, action: &str, model: &str) -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(1);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let op = EngineOp {
            id,
            engine: engine.into(),
            action: action.into(),
            model: model.into(),
            state: OpState::Running,
            message: String::new(),
            done: 0,
            total: 0,
            at_ms: crate::status::now_ms(),
        };
        if let Some(me) = self.local_mut() {
            me.ops.insert(0, op);
            me.ops.truncate(MAX_OPS);
        }
        id
    }

    pub fn op_update(&mut self, id: u64, f: impl FnOnce(&mut EngineOp)) {
        if let Some(op) = self
            .local_mut()
            .and_then(|me| me.ops.iter_mut().find(|o| o.id == id))
        {
            f(op);
            op.at_ms = crate::status::now_ms();
        }
    }

    /// Fold a peer announcement in: refresh an existing card in place so its
    /// chart history survives, or create one. An announcement never
    /// overwrites what node-info already told us in more detail (model
    /// names), only what it knows better (address, liveness).
    pub fn upsert_peer(&mut self, peer: Node) {
        match self.nodes.get_mut(&peer.id) {
            Some(existing) => {
                existing.name = peer.name;
                // An announcement names every interface the node has and
                // this side guessed which one to use. A card whose polls
                // succeed keeps the address that works; only one with no
                // address, or one that is failing, takes the announced one.
                // Otherwise a guess that happened to be wrong would be
                // re-applied on every announcement, undoing the address the
                // node was actually reached at.
                if existing.address.is_empty()
                    || (existing.poll_failures > 0
                        && (existing.address != peer.address || existing.info_port != peer.info_port))
                {
                    existing.address = peer.address;
                    existing.info_port = peer.info_port;
                    existing.next_poll = Instant::now();
                }
                if existing.gpus.is_empty() {
                    existing.gpus = peer.gpus;
                }
                if existing.cpu_model.is_empty() {
                    existing.cpu_model = peer.cpu_model;
                    existing.cpu_cores = peer.cpu_cores;
                    existing.ram_total_mb = peer.ram_total_mb;
                }
                let announced_more = existing.engines.iter().all(|e| e.models.iter().all(String::is_empty));
                if announced_more {
                    existing.engines = peer.engines;
                }
                existing.version = peer.version;
                existing.cluster_id = peer.cluster_id;
                existing.last_seen = peer.last_seen;
                if existing.source == Source::Manual && peer.source == Source::Discovered {
                    // A typed address that then announces itself is simply a
                    // discovered node; keep the stronger identity.
                    existing.source = Source::Discovered;
                }
            }
            None => {
                self.nodes.insert(peer.id.clone(), peer);
            }
        }
    }

    /// Drop discovered peers that have gone quiet. Manual nodes and cluster
    /// members stay, and merely read as offline.
    pub fn expire(&mut self, now: Instant) {
        let gone: Vec<String> = self
            .nodes
            .values()
            .filter(|n| n.source == Source::Discovered && !n.online(now))
            .map(|n| n.id.clone())
            .collect();
        for id in gone {
            if let Some(n) = self.nodes.remove(&id) {
                self.push_log(format!("{} left the network", n.name));
            }
        }
    }
}

pub type Shared = Arc<Mutex<Router>>;

/// Lock without propagating poison: a panic in one sampler must not take
/// the whole window with it. Same reasoning as `status::snapshot`.
pub fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Router> {
    match shared.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// What this machine calls itself on the network. Display only; the id is
/// the key. `KMPLIFY_NODE_NAME` overrides the hostname, for a machine whose
/// hostname is not what its owner calls it — or for two nodes on one box.
pub fn hostname() -> String {
    if let Ok(n) = std::env::var("KMPLIFY_NODE_NAME") {
        let n = n.trim().to_string();
        if !n.is_empty() {
            return n;
        }
    }
    sysinfo::System::host_name()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".to_string())
}

/// The node id this install already has on the fabric, or a stable one
/// minted from the hostname when it has never joined. Reusing the fabric id
/// is deliberate: one machine, one identity, wherever it shows up.
pub fn self_id(node_dir: &Path) -> String {
    if let Some(ident) = crate::identity::Identity::read(node_dir) {
        if !ident.node_id.is_empty() {
            return ident.node_id;
        }
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kmplify-node lan router ");
    h.update(hostname().as_bytes());
    let out = h.finalize();
    out.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Build the shared state with this machine's card in it.
pub fn new_shared(
    node_dir: &Path,
    gpus: &[crate::gpu::Gpu],
    address: String,
) -> Shared {
    let id = self_id(node_dir);
    // The static CPU facts are published by start(); reading the snapshot
    // before it has run gives an empty model and 0 cores (the wizard had
    // exactly this bug). Idempotent, so the sampler task calling it again
    // costs nothing.
    crate::hostcpu::start();
    let cpu = crate::hostcpu::snapshot();
    let mut local = Node::new_peer(id.clone(), hostname(), address, Source::Local, Instant::now());
    local.gpus = gpus
        .iter()
        .map(|g| GpuInfo {
            name: g.name.clone(),
            total_mb: g.total_mb,
        })
        .collect();
    local.cpu_model = cpu.model;
    local.cpu_cores = cpu.logical_cores;
    local.ram_total_mb = crate::hostcpu::read_ram_total_mb_now();
    local.version = crate::version_string().to_string();
    let cluster = cluster::ClusterFile::load(node_dir);
    local.cluster_id = cluster.cluster_id.clone();
    let mut router = Router {
        self_id: id.clone(),
        lan_ingress: true,
        node_dir: node_dir.to_path_buf(),
        cluster,
        ..Default::default()
    };
    router.nodes.insert(local.id.clone(), local);
    router.discovery = "starting".into();
    router.listeners = "starting".into();
    router.pins.set(router.cluster.fingerprints());
    match cluster::Identity::load_or_create(node_dir, &id) {
        Ok(identity) => {
            let identity = Arc::new(identity);
            match (
                cluster::server_config(&identity, &router.pins),
                cluster::tls_client(&identity, &router.pins),
            ) {
                (Ok(server), Ok(client)) => {
                    router.tls_server = Some(server);
                    router.tls_client = Some(client);
                }
                (Err(e), _) | (_, Err(e)) => {
                    router.push_log(format!("cluster TLS unavailable: {e}"));
                }
            }
            router.identity = Some(identity);
        }
        Err(e) => router.push_log(format!("no node certificate, pairing is off: {e}")),
    }
    if router.cluster.is_clustered() {
        let n = router.cluster.members.len();
        router.push_log(format!("in a cluster with {n} pinned node(s)"));
        router.seed_member_cards();
    }
    Arc::new(Mutex::new(router))
}

/// A router with only a bare local node, for tests that need `self_id`
/// resolved without touching the filesystem or probing hardware.
#[cfg(test)]
pub fn new_shared_for_tests(self_id: &str) -> Shared {
    let now = Instant::now();
    let mut router = Router {
        self_id: self_id.to_string(),
        lan_ingress: true,
        node_dir: std::env::temp_dir().join(format!("kmplify-router-test-{}", std::process::id())),
        ..Default::default()
    };
    router.nodes.insert(
        self_id.to_string(),
        Node::new_peer(self_id.to_string(), "me".into(), "127.0.0.1".into(), Source::Local, now),
    );
    Arc::new(Mutex::new(router))
}

/// The address peers should use for this machine: the interface that
/// routes toward the LAN, learned the portable way, by asking the OS which
/// source address it would pick for an outbound packet. No packet is sent.
pub fn lan_address() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Start every background task the router mode needs. Each is independent
/// and none holds the lock across an await, so one stalling probe cannot
/// freeze a card.
pub fn spawn(shared: Shared, accel: crate::gpu::Backend) {
    engine::init(&lock(&shared).node_dir);
    tokio::spawn(telemetry::sample_local(shared.clone(), accel));
    tokio::spawn(telemetry::scan_engines(shared.clone()));
    tokio::spawn(telemetry::expire_peers(shared.clone()));
    tokio::spawn(node_info::serve(shared.clone()));
    tokio::spawn(node_info::poll_peers(shared.clone()));
    tokio::spawn(proxy::serve(shared.clone(), proxy::Api::Ollama, proxy_ollama_port()));
    tokio::spawn(proxy::serve(shared.clone(), proxy::Api::OpenAi, proxy_openai_port()));
    discovery::spawn(shared);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_is_a_flat_minute_until_told_otherwise() {
        let s = Series::default();
        assert_eq!(s.len(), HISTORY);
        assert!(s.points().all(|p| p == 0.0));
    }

    #[test]
    fn a_series_keeps_one_minute_and_clamps() {
        let mut s = Series::default();
        for i in 0..(HISTORY * 2) {
            s.push(i as f32 * 10.0);
        }
        assert_eq!(s.len(), HISTORY);
        assert_eq!(s.latest(), 100.0, "values past 100 are clamped");
    }

    fn node(id: &str, name: &str, source: Source, seen: Instant) -> Node {
        Node::new_peer(id.into(), name.into(), "10.0.0.1".into(), source, seen)
    }

    fn job(id: &str, node_id: &str, state: JobState, local: bool) -> Job {
        Job {
            id: id.into(),
            model: "m".into(),
            engine: "ollama".into(),
            requested_from: String::new(),
            ran_on: String::new(),
            node_id: node_id.into(),
            state,
            at_ms: 0,
            error: String::new(),
            local_origin: local,
        }
    }

    #[test]
    fn the_local_node_sorts_first_then_names() {
        let now = Instant::now();
        let mut r = Router {
            self_id: "me".into(),
            ..Default::default()
        };
        r.nodes.insert("z".into(), node("z", "alpha", Source::Discovered, now));
        r.nodes.insert("me".into(), node("me", "zulu", Source::Local, now));
        r.nodes.insert("b".into(), node("b", "Bravo", Source::Manual, now));
        let names: Vec<&str> = r.ordered().iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["zulu", "alpha", "Bravo"]);
    }

    #[test]
    fn quiet_discovered_peers_expire_but_manual_ones_stay() {
        let now = Instant::now();
        let old = now - PEER_TIMEOUT - Duration::from_secs(1);
        let mut r = Router {
            self_id: "me".into(),
            ..Default::default()
        };
        r.nodes.insert("me".into(), node("me", "me", Source::Local, old));
        r.nodes.insert("d".into(), node("d", "d", Source::Discovered, old));
        r.nodes.insert("m".into(), node("m", "m", Source::Manual, old));
        r.expire(now);
        assert!(r.nodes.contains_key("me"), "the local node never expires");
        assert!(!r.nodes.contains_key("d"));
        assert!(r.nodes.contains_key("m"));
        assert!(!r.nodes["m"].online(now));
    }

    #[test]
    fn an_announcement_does_not_move_a_card_that_is_being_reached() {
        let now = Instant::now();
        let mut r = Router::default();
        let mut first = node("p", "peer", Source::Discovered, now);
        first.address = "192.168.2.171".into();
        r.upsert_peer(first);
        let mut again = node("p", "peer", Source::Discovered, now);
        again.address = "172.23.240.1".into();
        r.upsert_peer(again.clone());
        assert_eq!(r.nodes["p"].address, "192.168.2.171", "polls are fine: keep the working address");
        r.nodes.get_mut("p").unwrap().poll_failures = 2;
        r.upsert_peer(again);
        assert_eq!(r.nodes["p"].address, "172.23.240.1", "failing: try what was announced");

        // The announced port travels with the address: a node on another
        // port than ours is polled where it actually answers.
        r.nodes.get_mut("p").unwrap().poll_failures = 1;
        let mut other_port = node("p", "peer", Source::Discovered, now);
        other_port.address = "172.23.240.1".into();
        other_port.info_port = 24418;
        r.upsert_peer(other_port);
        assert_eq!(r.nodes["p"].info_port, 24418);
    }

    #[test]
    fn a_reannouncing_peer_keeps_its_chart_history() {
        let now = Instant::now();
        let mut r = Router::default();
        let mut first = node("p", "peer", Source::Discovered, now);
        first.metrics.cpu.push(77.0);
        r.upsert_peer(first);
        let mut again = node("p", "peer-renamed", Source::Discovered, now);
        again.metrics = Metrics::default();
        r.upsert_peer(again);
        let n = &r.nodes["p"];
        assert_eq!(n.name, "peer-renamed");
        assert_eq!(n.metrics.cpu.latest(), 77.0);
    }

    #[test]
    fn jobs_are_newest_first_and_capped() {
        let mut r = Router::default();
        for i in 0..(MAX_JOBS + 5) {
            r.push_job(job(&i.to_string(), "n", JobState::Completed, true));
        }
        assert_eq!(r.jobs.len(), MAX_JOBS);
        assert_eq!(r.jobs.front().unwrap().id, (MAX_JOBS + 4).to_string());
    }

    #[test]
    fn a_peer_report_updates_a_known_job_and_adds_an_unknown_one() {
        let mut r = Router::default();
        r.push_job(job("a", "n", JobState::Running, true));
        r.merge_job(job("a", "n", JobState::Completed, false));
        r.merge_job(job("b", "n", JobState::Running, false));
        assert_eq!(r.jobs.len(), 2);
        let a = r.jobs.iter().find(|j| j.id == "a").unwrap();
        assert_eq!(a.state, JobState::Completed);
        assert!(a.local_origin, "a merge never demotes a local job to a copy");
    }

    #[test]
    fn pending_counts_own_dispatches_and_trusts_the_larger_figure_for_peers() {
        let now = Instant::now();
        let mut r = Router {
            self_id: "me".into(),
            ..Default::default()
        };
        r.nodes.insert("me".into(), node("me", "me", Source::Local, now));
        let mut peer = node("p", "p", Source::Discovered, now);
        peer.reported_pending = 3;
        r.nodes.insert("p".into(), peer);
        r.push_job(job("1", "me", JobState::Running, true));
        r.push_job(job("2", "me", JobState::Completed, true));
        r.push_job(job("3", "p", JobState::Running, true));
        assert_eq!(r.pending_for("me"), 1);
        assert_eq!(r.pending_for("p"), 3, "the peer's own count wins while it is larger");
        for i in 4..9 {
            r.push_job(job(&i.to_string(), "p", JobState::Running, true));
        }
        assert_eq!(r.pending_for("p"), 6, "a burst dispatched here is reserved before the peer reports it");
    }

    #[test]
    fn a_typed_address_may_carry_its_info_port() {
        assert_eq!(Node::parse_address("10.0.0.5"), ("10.0.0.5".into(), NODE_INFO_PORT));
        assert_eq!(Node::parse_address("10.0.0.5:24418/"), ("10.0.0.5".into(), 24418));
        assert_eq!(Node::parse_address("spark.local:x"), ("spark.local:x".into(), NODE_INFO_PORT));
    }

    #[test]
    fn serves_normalises_ollamas_implicit_tag() {
        let now = Instant::now();
        let mut n = node("n", "n", Source::Local, now);
        n.engines.push(Engine {
            id: "ollama".into(),
            name: "Ollama".into(),
            base: "http://127.0.0.1:11434".into(),
            models: vec!["qwen3:latest".into(), "bge-m3:567m".into()],
            running: true,
            installed: true,
            owned: false,
        });
        assert!(n.serves("qwen3", proxy::Api::Ollama).is_some());
        assert!(n.serves("qwen3:latest", proxy::Api::OpenAi).is_some());
        assert!(n.serves("bge-m3", proxy::Api::Ollama).is_none(), "a different tag is a different model");
        n.engines[0].running = false;
        assert!(n.serves("qwen3", proxy::Api::Ollama).is_none(), "a model on a stopped engine does not count");
    }

    #[test]
    fn the_inventory_lists_who_has_what() {
        let now = Instant::now();
        let mut r = Router::default();
        for (id, model) in [("a", "x:latest"), ("b", "x:latest"), ("c", "y:7b")] {
            let mut n = node(id, id, Source::Discovered, now);
            n.engines.push(Engine {
                id: "ollama".into(),
                name: "Ollama".into(),
                base: String::new(),
                models: vec![model.into()],
                running: true,
                installed: true,
                owned: false,
            });
            r.nodes.insert(id.into(), n);
        }
        let inv = r.inventory(proxy::Api::Ollama, now);
        assert_eq!(inv["x:latest"], vec!["a", "b"]);
        assert_eq!(inv["y:7b"], vec!["c"]);
    }
}
