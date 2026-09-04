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
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod discovery;
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

/// Where the router's own surfaces will listen (phase 2). Chosen apart from
/// the engines' defaults and from PAIR's `143xx` band so a machine running
/// both is not a port fight.
pub const NODE_INFO_PORT: u16 = 14418;
pub const PROXY_OLLAMA_PORT: u16 = 11440;
pub const PROXY_OPENAI_PORT: u16 = 11441;

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
}

/// How a node entered the directory. Discovered and manual nodes are the
/// same kind of peer once they answer; the distinction is kept so the
/// cluster screen can offer "forget" for one and not the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Local,
    Discovered,
    Manual,
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
    pub metrics: Metrics,
    pub version: String,
    pub last_seen: Instant,
}

impl Node {
    pub fn is_local(&self) -> bool {
        self.source == Source::Local
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub model: String,
    pub engine: String,
    pub requested_from: String,
    pub ran_on: String,
    pub state: JobState,
    pub at_ms: u64,
    pub error: String,
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

    pub fn push_log(&mut self, line: impl Into<String>) {
        let stamp = crate::status::clock_hms(crate::status::now_ms());
        self.log.push_back(format!("{stamp} {}", line.into()));
        while self.log.len() > MAX_LOG {
            self.log.pop_front();
        }
    }

    /// Fold a peer announcement in: refresh an existing card in place so its
    /// chart history survives, or create one.
    pub fn upsert_peer(&mut self, peer: Node) {
        match self.nodes.get_mut(&peer.id) {
            Some(existing) => {
                existing.name = peer.name;
                existing.address = peer.address;
                existing.gpus = peer.gpus;
                existing.cpu_model = peer.cpu_model;
                existing.cpu_cores = peer.cpu_cores;
                existing.ram_total_mb = peer.ram_total_mb;
                existing.engines = peer.engines;
                existing.version = peer.version;
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

    /// Drop discovered peers that have gone quiet. Manual ones stay, and
    /// merely read as offline.
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
/// the key.
pub fn hostname() -> String {
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
    let local = Node {
        id: id.clone(),
        name: hostname(),
        address,
        source: Source::Local,
        gpus: gpus
            .iter()
            .map(|g| GpuInfo {
                name: g.name.clone(),
                total_mb: g.total_mb,
            })
            .collect(),
        cpu_model: cpu.model,
        cpu_cores: cpu.logical_cores,
        ram_total_mb: crate::hostcpu::read_ram_total_mb_now(),
        engines: Vec::new(),
        metrics: Metrics::default(),
        version: crate::version_string().to_string(),
        last_seen: Instant::now(),
    };
    let mut router = Router {
        self_id: id,
        ..Default::default()
    };
    router.nodes.insert(local.id.clone(), local);
    router.discovery = "starting".into();
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
    tokio::spawn(telemetry::sample_local(shared.clone(), accel));
    tokio::spawn(telemetry::scan_engines(shared.clone()));
    tokio::spawn(telemetry::expire_peers(shared.clone()));
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
        Node {
            id: id.into(),
            name: name.into(),
            address: "10.0.0.1".into(),
            source,
            gpus: vec![],
            cpu_model: String::new(),
            cpu_cores: 0,
            ram_total_mb: 0,
            engines: vec![],
            metrics: Metrics::default(),
            version: String::new(),
            last_seen: seen,
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
            r.push_job(Job {
                id: i.to_string(),
                model: "m".into(),
                engine: "ollama".into(),
                requested_from: String::new(),
                ran_on: String::new(),
                state: JobState::Completed,
                at_ms: i as u64,
                error: String::new(),
            });
        }
        assert_eq!(r.jobs.len(), MAX_JOBS);
        assert_eq!(r.jobs.front().unwrap().id, (MAX_JOBS + 4).to_string());
    }
}
