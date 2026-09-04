//! What a node tells its peers, and how a peer's card is kept current.
//!
//! `GET /v1/node-info` on [`super::NODE_INFO_PORT`] answers with this
//! machine's hardware, live meters, engines with their model names, the
//! work it is holding, and the jobs it has seen. The other half of this
//! module polls every peer's copy: every two seconds while it answers,
//! backing off to thirty while it does not, keeping the last good value so
//! one lost poll dims nothing.
//!
//! Plain HTTP, unauthenticated, and readable by anything on the subnet —
//! the same trade PAIR makes for its telemetry, and the reason the router
//! is opt-in. Nothing here is a request or a response; it is what the
//! machine is and how busy it is.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::{extract::State, routing::get, Json};
use serde::{Deserialize, Serialize};

use super::{lock, Engine, GpuInfo, Job, Node, Router, Shared, Source, NODE_INFO_PORT};

const HEALTHY_POLL: Duration = Duration::from_secs(2);
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// Jobs a report carries: enough for every window to show the cluster's
/// recent work, small enough to poll every two seconds.
const REPORT_JOBS: usize = 50;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuReport {
    pub name: String,
    pub total_mb: u64,
    pub used_mb: u64,
    pub utilization_percent: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CpuReport {
    pub model: String,
    pub cores: usize,
    pub utilization_percent: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MemoryReport {
    pub total_mb: u64,
    pub used_mb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineReport {
    pub id: String,
    pub name: String,
    pub base: String,
    pub models: Vec<String>,
    pub running: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub gpus: Vec<GpuReport>,
    pub cpu: CpuReport,
    pub memory: MemoryReport,
    pub engines: Vec<EngineReport>,
    /// The meters carry a real reading (not the pre-fill).
    pub sampled: bool,
    pub vram_known: bool,
    pub pending: u32,
    pub proxy_ports: [u16; 2],
    pub lan_ingress: bool,
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// This machine's report, from the shared state.
pub fn report(r: &Router) -> Option<NodeInfo> {
    let me = r.local()?;
    let m = &me.metrics;
    Some(NodeInfo {
        id: me.id.clone(),
        name: me.name.clone(),
        version: me.version.clone(),
        gpus: me
            .gpus
            .iter()
            .map(|g| GpuReport {
                name: g.name.clone(),
                total_mb: g.total_mb,
                used_mb: m.vram_used_mb,
                utilization_percent: m.gpu_known.then(|| m.gpu.latest().round() as u8),
            })
            .collect(),
        cpu: CpuReport {
            model: me.cpu_model.clone(),
            cores: me.cpu_cores,
            utilization_percent: m.sampled.then(|| m.cpu.latest()),
        },
        memory: MemoryReport {
            total_mb: me.ram_total_mb,
            used_mb: m.ram_used_mb,
        },
        engines: me
            .engines
            .iter()
            .map(|e| EngineReport {
                id: e.id.clone(),
                name: e.name.clone(),
                base: e.base.clone(),
                models: e.models.clone(),
                running: e.running,
            })
            .collect(),
        sampled: m.sampled,
        vram_known: m.vram_known,
        pending: r.pending_for(&me.id),
        proxy_ports: [me.proxy_ports.0, me.proxy_ports.1],
        lan_ingress: r.lan_ingress,
        jobs: r.jobs.iter().take(REPORT_JOBS).cloned().collect(),
    })
}

async fn handler(State(shared): State<Shared>) -> Result<Json<NodeInfo>, axum::http::StatusCode> {
    report(&lock(&shared))
        .map(Json)
        .ok_or(axum::http::StatusCode::SERVICE_UNAVAILABLE)
}

pub async fn serve(shared: Shared) {
    let app = axum::Router::new()
        .route("/v1/node-info", get(handler))
        .with_state(shared.clone());
    let addr = SocketAddr::from(([0, 0, 0, 0], NODE_INFO_PORT));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let mut r = lock(&shared);
            r.listeners = format!("node-info: cannot bind {addr}: {e}");
            r.push_log(format!("node-info surface unavailable: {e}"));
            return;
        }
    };
    {
        let mut r = lock(&shared);
        r.push_log(format!("node-info on :{NODE_INFO_PORT}"));
    }
    if let Err(e) = axum::serve(listener, app).await {
        lock(&shared).push_log(format!("node-info stopped: {e}"));
    }
}

// ------------------------------------------------------------ the peer side

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_default()
    })
}

/// Sample every peer that is due. Each fetch is its own task so a wedged
/// peer delays nobody else.
pub async fn poll_peers(shared: Shared) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = Instant::now();
        let due: Vec<(String, String)> = {
            let r = lock(&shared);
            r.nodes
                .values()
                .filter(|n| !n.is_local() && n.next_poll <= now && !n.address.is_empty())
                .map(|n| (n.id.clone(), n.address.clone()))
                .collect()
        };
        for (id, address) in due {
            // Push the due time out before the fetch starts, so a slow peer
            // is not fetched again on the next tick while the first is
            // still in flight.
            if let Some(n) = lock(&shared).nodes.get_mut(&id) {
                n.next_poll = now + FETCH_TIMEOUT + Duration::from_millis(500);
            }
            tokio::spawn(fetch(shared.clone(), id, address));
        }
    }
}

/// Where a peer's node-info lives. A typed address may carry its own port;
/// otherwise the default.
pub fn info_url(address: &str) -> String {
    let addr = address.trim().trim_end_matches('/');
    if addr.contains(':') && !addr.starts_with('[') {
        format!("http://{addr}/v1/node-info")
    } else {
        format!("http://{addr}:{NODE_INFO_PORT}/v1/node-info")
    }
}

async fn fetch(shared: Shared, id: String, address: String) {
    let result = async {
        let resp = client().get(info_url(&address)).send().await?;
        resp.error_for_status()?.json::<NodeInfo>().await
    }
    .await;
    let now = Instant::now();
    match result {
        Ok(info) => apply(&shared, &id, address, info, now),
        Err(e) => {
            let mut r = lock(&shared);
            if let Some(n) = r.nodes.get_mut(&id) {
                n.poll_failures = n.poll_failures.saturating_add(1);
                n.next_poll = now + backoff(n.poll_failures);
                if n.poll_failures == 1 && n.source == Source::Manual {
                    let name = n.name.clone();
                    r.push_log(format!("{name} does not answer node-info: {e}"));
                }
            }
        }
    }
}

/// 4, 8, 16 then 30 seconds between attempts on a peer that stopped
/// answering, reset by the first success.
pub fn backoff(failures: u32) -> Duration {
    match failures {
        0 => HEALTHY_POLL,
        1 => Duration::from_secs(4),
        2 => Duration::from_secs(8),
        3 => Duration::from_secs(16),
        _ => Duration::from_secs(30),
    }
}

/// Fold a report into the directory. A manual node answering for the first
/// time is re-keyed from its typed address to its real id, so the card
/// becomes the same node discovery would have produced.
pub fn apply(shared: &Shared, id: &str, address: String, info: NodeInfo, now: Instant) {
    let mut r = lock(shared);
    if info.id == r.self_id {
        // The operator typed this machine's own address. Drop the ghost.
        r.nodes.remove(id);
        return;
    }
    let key = if id != info.id {
        let source = r.nodes.remove(id).map(|n| n.source).unwrap_or(Source::Manual);
        if !r.nodes.contains_key(&info.id) {
            let n = Node::new_peer(info.id.clone(), info.name.clone(), address.clone(), source, now);
            r.nodes.insert(info.id.clone(), n);
        }
        info.id.clone()
    } else {
        id.to_string()
    };
    let self_id = r.self_id.clone();
    let mut jobs_to_merge = Vec::new();
    if let Some(n) = r.nodes.get_mut(&key) {
        n.name = info.name;
        n.address = address;
        n.version = info.version;
        n.last_seen = now;
        n.poll_failures = 0;
        n.next_poll = now + HEALTHY_POLL;
        n.reported_pending = info.pending;
        n.proxy_ports = (info.proxy_ports[0], info.proxy_ports[1]);
        n.cpu_model = info.cpu.model;
        n.cpu_cores = info.cpu.cores;
        n.ram_total_mb = info.memory.total_mb;
        n.gpus = info
            .gpus
            .iter()
            .map(|g| GpuInfo {
                name: g.name.clone(),
                total_mb: g.total_mb,
            })
            .collect();
        n.engines = info
            .engines
            .into_iter()
            .map(|e| Engine {
                id: e.id,
                name: e.name,
                base: e.base,
                models: e.models,
                running: e.running,
            })
            .collect();
        let m = &mut n.metrics;
        if info.sampled {
            m.sampled = true;
            if let Some(c) = info.cpu.utilization_percent {
                m.cpu.push(c);
            }
            m.ram_used_mb = info.memory.used_mb;
            m.ram.push(percent(info.memory.used_mb, info.memory.total_mb));
        }
        if let Some(g) = info.gpus.first() {
            if let Some(u) = g.utilization_percent {
                m.observe_gpu(u as f32, now);
            }
            if info.vram_known && g.total_mb > 0 {
                m.vram_used_mb = g.used_mb;
                m.vram.push(percent(g.used_mb, g.total_mb));
                m.vram_known = true;
            }
        }
        // A peer's jobs are copies: those it dispatched to us we already
        // hold under the same id, those it served elsewhere are news.
        jobs_to_merge = info
            .jobs
            .into_iter()
            .filter(|j| !(j.node_id == self_id && j.requested_from_here()))
            .collect();
    }
    for j in jobs_to_merge {
        r.merge_job(j);
    }
}

fn percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{new_shared_for_tests, JobState};
    use std::sync::{Arc, Mutex};

    fn info(id: &str, name: &str) -> NodeInfo {
        NodeInfo {
            id: id.into(),
            name: name.into(),
            version: "t".into(),
            gpus: vec![GpuReport {
                name: "GPU".into(),
                total_mb: 1000,
                used_mb: 250,
                utilization_percent: Some(50),
            }],
            cpu: CpuReport {
                model: "cpu".into(),
                cores: 8,
                utilization_percent: Some(12.5),
            },
            memory: MemoryReport {
                total_mb: 2000,
                used_mb: 1000,
            },
            engines: vec![EngineReport {
                id: "ollama".into(),
                name: "Ollama".into(),
                base: "http://127.0.0.1:11434".into(),
                models: vec!["qwen3:latest".into()],
                running: true,
            }],
            sampled: true,
            vram_known: true,
            pending: 2,
            proxy_ports: [11440, 11441],
            lan_ingress: true,
            jobs: vec![],
        }
    }

    #[test]
    fn a_manual_node_is_rekeyed_to_its_real_id_when_it_answers() {
        let shared: Shared = Arc::new(Mutex::new(Router {
            self_id: "me".into(),
            ..Default::default()
        }));
        let now = Instant::now();
        lock(&shared).nodes.insert(
            "manual:10.0.0.5".into(),
            Node::new_peer("manual:10.0.0.5".into(), "10.0.0.5".into(), "10.0.0.5".into(), Source::Manual, now),
        );
        apply(&shared, "manual:10.0.0.5", "10.0.0.5".into(), info("real", "spark"), now);
        let r = lock(&shared);
        assert!(!r.nodes.contains_key("manual:10.0.0.5"));
        let n = &r.nodes["real"];
        assert_eq!(n.source, Source::Manual, "typed by hand stays forgettable");
        assert_eq!(n.name, "spark");
        assert_eq!(n.cpu_cores, 8);
        assert_eq!(n.reported_pending, 2);
        assert_eq!(n.metrics.gpu.latest(), 50.0);
        assert_eq!(n.metrics.vram.latest(), 25.0);
        assert_eq!(n.metrics.ram.latest(), 50.0);
        assert!(n.metrics.sampled && n.metrics.vram_known && n.metrics.gpu_known);
        assert_eq!(n.engines[0].models, vec!["qwen3:latest"]);
    }

    #[test]
    fn typing_this_machines_own_address_adds_nothing() {
        let shared = new_shared_for_tests("me");
        let now = Instant::now();
        lock(&shared).nodes.insert(
            "manual:127.0.0.1".into(),
            Node::new_peer("manual:127.0.0.1".into(), "127.0.0.1".into(), "127.0.0.1".into(), Source::Manual, now),
        );
        apply(&shared, "manual:127.0.0.1", "127.0.0.1".into(), info("me", "me"), now);
        assert_eq!(lock(&shared).nodes.len(), 1);
    }

    #[test]
    fn peer_jobs_are_merged_except_copies_of_our_own_dispatches() {
        let shared = new_shared_for_tests("me");
        let now = Instant::now();
        lock(&shared).nodes.insert(
            "p".into(),
            Node::new_peer("p".into(), "p".into(), "10.0.0.7".into(), Source::Discovered, now),
        );
        let mut i = info("p", "p");
        i.jobs = vec![
            Job {
                id: "served-there".into(),
                model: "m".into(),
                engine: "ollama".into(),
                requested_from: "p".into(),
                ran_on: "p".into(),
                node_id: "p".into(),
                state: JobState::Running,
                at_ms: 1,
                error: String::new(),
                local_origin: false,
            },
        ];
        apply(&shared, "p", "10.0.0.7".into(), i, now);
        let r = lock(&shared);
        assert_eq!(r.jobs.len(), 1);
        assert!(!r.jobs[0].local_origin);
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(0), HEALTHY_POLL);
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(16));
        assert_eq!(backoff(9), Duration::from_secs(30));
    }

    #[test]
    fn info_url_respects_a_typed_port() {
        assert_eq!(info_url("10.0.0.5"), "http://10.0.0.5:14418/v1/node-info");
        assert_eq!(info_url("10.0.0.5:9000"), "http://10.0.0.5:9000/v1/node-info");
        assert_eq!(info_url("spark.local/"), "http://spark.local:14418/v1/node-info");
    }
}
