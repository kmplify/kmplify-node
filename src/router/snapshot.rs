//! One router, any number of views.
//!
//! The router runs in exactly one process on a machine — the node itself
//! (`kmplify-node run --router`), or the first dashboard or window started
//! with it — and that process publishes what it knows to `router.json` in
//! the node directory once a second, and takes orders as files in
//! `control/router/`, the same two mechanisms the fabric node already uses
//! for `status.json` and its commands (its own orders sit in `control/`;
//! the two are drained by whichever process owns each, which need not be
//! the same one). Every other `gui` or `tui --router` on
//! the machine **attaches**: it draws from the file and writes orders to
//! the directory, and never binds a port of its own. So an operator can
//! close the window and open the terminal dashboard, or the other way
//! round, and the cluster, the proxies and the polls carry on regardless;
//! the router is not a property of whichever screen happens to be open.
//!
//! [`RouterHandle`] is the one thing a screen holds: `view()` returns the
//! state either way, `command()` applies an order either way.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::cluster::{Invite, Member};
use super::{lock, Engine, EngineOp, GpuInfo, Job, Metrics, Node, Router, Series, Shared, Source};
use crate::control::RouterCommand;

pub const FILE: &str = "router.json";
/// A snapshot older than this is a router that stopped; a view then starts
/// its own rather than drawing a dead one.
pub const FRESH: Duration = Duration::from_secs(5);
const PUBLISH_EVERY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricsView {
    pub gpu: Vec<f32>,
    pub vram: Vec<f32>,
    pub cpu: Vec<f32>,
    pub ram: Vec<f32>,
    pub gpu_known: bool,
    pub vram_known: bool,
    pub sampled: bool,
    pub vram_used_mb: u64,
    pub ram_used_mb: u64,
    pub pressure: u8,
    /// Seconds since the last GPU reading, or none.
    pub gpu_age_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub address: String,
    pub source: String,
    pub gpus: Vec<GpuInfo>,
    pub cpu_model: String,
    pub cpu_cores: usize,
    pub ram_total_mb: u64,
    pub engines: Vec<Engine>,
    pub ops: Vec<EngineOp>,
    pub version: String,
    pub cluster_id: String,
    pub reported_pending: u32,
    pub proxy_ports: (u16, u16),
    pub info_port: u16,
    pub poll_failures: u32,
    pub online: bool,
    pub metrics: MetricsView,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InviteView {
    pub pin: String,
    pub remaining_secs: u64,
    pub wrong_attempts: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouterSnapshot {
    pub schema: u32,
    pub published_at_ms: u64,
    pub pid: u32,
    pub self_id: String,
    pub fingerprint: String,
    pub cluster_id: String,
    pub members: Vec<Member>,
    pub invite: Option<InviteView>,
    pub nodes: Vec<NodeView>,
    pub jobs: Vec<Job>,
    pub manual: Vec<String>,
    pub log: Vec<String>,
    pub discovery: String,
    pub listeners: String,
    pub lan_ingress: bool,
}

fn series(s: &Series) -> Vec<f32> {
    s.points().collect()
}

impl Router {
    pub fn snapshot(&self) -> RouterSnapshot {
        let now = Instant::now();
        RouterSnapshot {
            schema: 1,
            published_at_ms: crate::status::now_ms(),
            pid: std::process::id(),
            self_id: self.self_id.clone(),
            fingerprint: self.fingerprint.clone(),
            cluster_id: self.cluster.cluster_id.clone(),
            members: self.cluster.members.values().cloned().collect(),
            invite: self
                .invite
                .as_ref()
                .filter(|i| !i.expired())
                .map(|i| InviteView {
                    pin: i.pin.clone(),
                    remaining_secs: i.remaining().as_secs(),
                    wrong_attempts: i.wrong_attempts,
                }),
            nodes: self
                .nodes
                .values()
                .map(|n| NodeView {
                    id: n.id.clone(),
                    name: n.name.clone(),
                    address: n.address.clone(),
                    source: match n.source {
                        Source::Local => "local",
                        Source::Discovered => "discovered",
                        Source::Manual => "manual",
                        Source::Member => "member",
                    }
                    .into(),
                    gpus: n.gpus.clone(),
                    cpu_model: n.cpu_model.clone(),
                    cpu_cores: n.cpu_cores,
                    ram_total_mb: n.ram_total_mb,
                    engines: n.engines.clone(),
                    ops: n.ops.clone(),
                    version: n.version.clone(),
                    cluster_id: n.cluster_id.clone(),
                    reported_pending: n.reported_pending,
                    proxy_ports: n.proxy_ports,
                    info_port: n.info_port,
                    poll_failures: n.poll_failures,
                    online: n.online(now),
                    metrics: MetricsView {
                        gpu: series(&n.metrics.gpu),
                        vram: series(&n.metrics.vram),
                        cpu: series(&n.metrics.cpu),
                        ram: series(&n.metrics.ram),
                        gpu_known: n.metrics.gpu_known,
                        vram_known: n.metrics.vram_known,
                        sampled: n.metrics.sampled,
                        vram_used_mb: n.metrics.vram_used_mb,
                        ram_used_mb: n.metrics.ram_used_mb,
                        pressure: n.metrics.pressure,
                        gpu_age_secs: n
                            .metrics
                            .gpu_sampled_at
                            .map(|t| now.duration_since(t).as_secs()),
                    },
                })
                .collect(),
            jobs: self.jobs.iter().cloned().collect(),
            manual: self.manual.clone(),
            log: self.log.iter().cloned().collect(),
            discovery: self.discovery.clone(),
            listeners: self.listeners.clone(),
            lan_ingress: self.lan_ingress,
        }
    }

    /// A display-only router rebuilt from a snapshot: no identity, no
    /// listeners, no tasks — what a screen needs and nothing it does not.
    pub fn from_snapshot(s: RouterSnapshot) -> Router {
        let now = Instant::now();
        let mut r = Router {
            self_id: s.self_id,
            fingerprint: s.fingerprint,
            lan_ingress: s.lan_ingress,
            discovery: s.discovery,
            listeners: s.listeners,
            manual: s.manual,
            ..Default::default()
        };
        r.cluster.cluster_id = s.cluster_id;
        for m in s.members {
            r.cluster.members.insert(m.id.clone(), m);
        }
        r.invite = s.invite.map(|i| {
            Invite::restored(
                i.pin,
                Duration::from_secs(i.remaining_secs),
                i.wrong_attempts,
            )
        });
        for v in s.nodes {
            let source = match v.source.as_str() {
                "local" => Source::Local,
                "manual" => Source::Manual,
                "member" => Source::Member,
                _ => Source::Discovered,
            };
            let mut n = Node::new_peer(v.id.clone(), v.name, v.address, source, now);
            if !v.online {
                n.last_seen = now - super::PEER_TIMEOUT - Duration::from_secs(1);
            }
            n.gpus = v.gpus;
            n.cpu_model = v.cpu_model;
            n.cpu_cores = v.cpu_cores;
            n.ram_total_mb = v.ram_total_mb;
            n.engines = v.engines;
            n.ops = v.ops;
            n.version = v.version;
            n.cluster_id = v.cluster_id;
            n.reported_pending = v.reported_pending;
            n.proxy_ports = v.proxy_ports;
            n.info_port = v.info_port;
            n.poll_failures = v.poll_failures;
            n.metrics = Metrics {
                gpu: Series::from_points(v.metrics.gpu),
                vram: Series::from_points(v.metrics.vram),
                cpu: Series::from_points(v.metrics.cpu),
                ram: Series::from_points(v.metrics.ram),
                gpu_known: v.metrics.gpu_known,
                vram_known: v.metrics.vram_known,
                sampled: v.metrics.sampled,
                vram_used_mb: v.metrics.vram_used_mb,
                ram_used_mb: v.metrics.ram_used_mb,
                gpu_sampled_at: v.metrics.gpu_age_secs.map(|a| now - Duration::from_secs(a)),
                gpu_smoothed: 0.0,
                pressure: v.metrics.pressure,
            };
            r.nodes.insert(n.id.clone(), n);
        }
        for j in s.jobs {
            r.jobs.push_back(j);
        }
        for l in s.log {
            r.log.push_back(l);
        }
        r
    }
}

pub fn path(node_dir: &Path) -> PathBuf {
    node_dir.join(FILE)
}

/// Write the snapshot, atomically and owner-only, once a second.
pub async fn publish(shared: Shared, node_dir: PathBuf) {
    loop {
        tokio::time::sleep(PUBLISH_EVERY).await;
        let snap = lock(&shared).snapshot();
        let Ok(bytes) = serde_json::to_vec(&snap) else {
            continue;
        };
        let target = path(&node_dir);
        let tmp = node_dir.join(format!(".{FILE}.tmp"));
        if tokio::fs::write(&tmp, &bytes).await.is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = tokio::fs::rename(&tmp, &target).await;
        }
    }
}

/// The published snapshot, if a router is running here right now.
pub fn read(node_dir: &Path) -> Option<RouterSnapshot> {
    let bytes = std::fs::read(path(node_dir)).ok()?;
    let snap: RouterSnapshot = serde_json::from_slice(&bytes).ok()?;
    let age = crate::status::now_ms().saturating_sub(snap.published_at_ms);
    (age < FRESH.as_millis() as u64).then_some(snap)
}

/// Another process runs the router here.
pub fn running_elsewhere(node_dir: &Path) -> bool {
    read(node_dir).is_some_and(|s| s.pid != std::process::id())
}

// ------------------------------------------------------------------ orders

/// The router running in THIS process, for orders that arrive through
/// `control::submit` — from the control directory or from a screen in the
/// same process.
static ACTIVE: OnceLock<Shared> = OnceLock::new();

pub fn set_active(shared: &Shared) {
    let _ = ACTIVE.set(shared.clone());
}

pub fn active() -> Option<Shared> {
    ACTIVE.get().cloned()
}

/// Apply an order to the router in this process, or explain why not.
pub fn apply(cmd: RouterCommand) -> Result<String, String> {
    let shared = active().ok_or("no LAN router is running in this process")?;
    apply_on(&shared, cmd)
}

pub fn apply_on(shared: &Shared, cmd: RouterCommand) -> Result<String, String> {
    match cmd {
        RouterCommand::Invite => {
            let pin = lock(shared).open_invite();
            Ok(format!("invitation open: PIN {pin}"))
        }
        RouterCommand::CancelInvite => {
            let mut r = lock(shared);
            r.invite = None;
            r.push_log("invitation cancelled");
            Ok("invitation cancelled".into())
        }
        RouterCommand::Join { address, pin } => {
            let shared = shared.clone();
            tokio::spawn(async move {
                let result = super::cluster::join(shared.clone(), address, pin).await;
                let mut r = lock(&shared);
                match result {
                    Ok(m) => r.push_log(m),
                    Err(e) => r.push_log(format!("pairing failed: {e}")),
                }
            });
            Ok("pairing…".into())
        }
        RouterCommand::AddNode { address } => {
            let address = address.trim().to_string();
            if address.is_empty() {
                return Err("an address is needed".into());
            }
            let mut r = lock(shared);
            if r.manual.iter().any(|a| a == &address) {
                return Err(format!("{address} is already on the list"));
            }
            r.manual.push(address.clone());
            let (host, port) = Node::parse_address(&address);
            let mut node = Node::new_peer(
                format!("manual:{address}"),
                address.clone(),
                host,
                Source::Manual,
                Instant::now(),
            );
            node.info_port = port;
            node.last_seen = Instant::now() - super::PEER_TIMEOUT;
            r.upsert_peer(node);
            r.push_log(format!(
                "added {address} by hand; probing its node-info surface"
            ));
            Ok(format!("added {address} — probing"))
        }
        RouterCommand::ForgetNode { address } => {
            let mut r = lock(shared);
            r.manual.retain(|a| a != &address);
            r.nodes.remove(&format!("manual:{address}"));
            Ok(format!("forgot {address}"))
        }
        RouterCommand::RemoveMember { id } => {
            let mut r = lock(shared);
            r.remove_member(&id);
            r.push_log(format!(
                "removed {} from the cluster",
                &id[..8.min(id.len())]
            ));
            Ok("member removed and unpinned".into())
        }
        RouterCommand::Leave => {
            let mut r = lock(shared);
            r.leave_cluster();
            r.push_log("left the cluster; every pin dropped");
            Ok("left the cluster".into())
        }
        RouterCommand::Ingress { on } => {
            let mut r = lock(shared);
            r.lan_ingress = on;
            r.push_log(if on {
                "LAN ingress on: peers may use this node's engines"
            } else {
                "LAN ingress off: this node only consumes the cluster"
            });
            Ok(if on {
                "serving paired nodes"
            } else {
                "not serving paired nodes"
            }
            .into())
        }
        RouterCommand::Engine {
            node,
            engine,
            action,
            model,
        } => {
            let Some(action) = super::engine::Action::parse(&action, &model) else {
                return Err("action is install, start, stop or pull (with a model)".into());
            };
            let (self_id, target) = {
                let r = lock(shared);
                (r.self_id.clone(), r.nodes.get(&node).cloned())
            };
            if node.is_empty() || node == self_id {
                super::engine::launch(
                    shared.clone(),
                    engine.clone(),
                    action.clone(),
                    "an operator".into(),
                );
                return Ok(format!("{} {engine}", action.name()));
            }
            let Some(target) = target else {
                return Err("no such node".into());
            };
            let (client, name) = {
                let r = lock(shared);
                (
                    r.tls_client.clone(),
                    r.local().map(|n| n.name.clone()).unwrap_or_default(),
                )
            };
            let Some(client) = client else {
                return Err("no cluster certificate here; cannot control a remote engine".into());
            };
            let url = format!("https://{}:{}/v1/engine", target.address, target.info_port);
            let body =
                serde_json::json!({ "engine": engine, "action": action.name(), "model": model });
            let shared = shared.clone();
            let target_name = target.name.clone();
            let _ = name;
            tokio::spawn(async move {
                let result = client
                    .post(&url)
                    .json(&body)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
                let mut r = lock(&shared);
                match result {
                    Ok(resp) if resp.status().is_success() => r.push_log(format!(
                        "asked {target_name} to {} {}",
                        body["action"], body["engine"]
                    )),
                    Ok(resp) => r.push_log(format!(
                        "{target_name} refused the engine request: {}",
                        resp.status()
                    )),
                    Err(e) => r.push_log(format!(
                        "cannot reach {target_name} for engine control: {e}"
                    )),
                }
            });
            Ok(format!(
                "asked {} to {} {engine}",
                target.name,
                action.name()
            ))
        }
    }
}

// ------------------------------------------------------------------ the handle

/// What a screen holds: the router in this process, or the published one.
#[allow(clippy::large_enum_variant)]
pub enum RouterHandle {
    Local(Shared),
    Attached {
        dir: PathBuf,
        cache: Mutex<Option<(Instant, Router)>>,
    },
}

impl RouterHandle {
    /// Attach to a router already running here, or start one in this
    /// process. `force_local` starts one regardless (`--standalone`).
    pub fn open(
        dir: &Path,
        gpus: &[crate::gpu::Gpu],
        accel: crate::gpu::Backend,
        force_local: bool,
    ) -> Self {
        if !force_local && running_elsewhere(dir) {
            return RouterHandle::Attached {
                dir: dir.to_path_buf(),
                cache: Mutex::new(None),
            };
        }
        let shared = super::new_shared(dir, gpus, super::lan_address());
        super::spawn(shared.clone(), accel);
        RouterHandle::Local(shared)
    }

    pub fn attached(&self) -> bool {
        matches!(self, RouterHandle::Attached { .. })
    }

    /// The state to draw. Attached, the file is re-read once a second;
    /// between reads the last view is returned.
    pub fn view(&self) -> Router {
        match self {
            RouterHandle::Local(shared) => lock(shared).clone(),
            RouterHandle::Attached { dir, cache } => {
                let mut c = match cache.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                let stale = c
                    .as_ref()
                    .map(|(at, _)| at.elapsed() >= PUBLISH_EVERY)
                    .unwrap_or(true);
                if stale {
                    let router = match read(dir) {
                        Some(snap) => Router::from_snapshot(snap),
                        None => {
                            let mut r = c.as_ref().map(|(_, r)| r.clone()).unwrap_or_default();
                            r.listeners = "the router stopped; start it with `kmplify-node run --router`, `gui` or `tui --router`".into();
                            r
                        }
                    };
                    *c = Some((Instant::now(), router));
                }
                c.as_ref().map(|(_, r)| r.clone()).unwrap_or_default()
            }
        }
    }

    /// An order: applied here, or left for the process that runs the
    /// router. The answer for the attached case is only that it was left.
    pub fn command(&self, cmd: RouterCommand) -> Result<String, String> {
        match self {
            RouterHandle::Local(shared) => apply_on(shared, cmd),
            RouterHandle::Attached { dir, .. } => {
                let what = cmd.confirmation();
                crate::control::request(dir, &crate::control::Command::Router(cmd)).map(|()| what)
            }
        }
    }

    /// The shared state, when the router runs here; a screen that needs
    /// more than a view (the chat's model list is enough from a view).
    pub fn local(&self) -> Option<&Shared> {
        match self {
            RouterHandle::Local(s) => Some(s),
            RouterHandle::Attached { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::new_shared_for_tests;

    #[test]
    fn a_snapshot_round_trips_what_a_screen_draws() {
        let shared = new_shared_for_tests("me");
        {
            let mut r = lock(&shared);
            r.fingerprint = "abcd".into();
            r.cluster.cluster_id = "c1".into();
            r.discovery = "advertising".into();
            let mut peer = Node::new_peer(
                "p".into(),
                "Spark".into(),
                "10.0.0.2".into(),
                Source::Member,
                Instant::now(),
            );
            peer.metrics.cpu.push(42.0);
            peer.metrics.sampled = true;
            peer.metrics.observe_gpu(70.0, Instant::now());
            peer.info_port = 24418;
            r.nodes.insert("p".into(), peer);
            r.push_log("hello");
            r.invite = Some(Invite::new());
        }
        let snap = lock(&shared).snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: RouterSnapshot = serde_json::from_str(&json).unwrap();
        let r = Router::from_snapshot(back);
        assert_eq!(r.self_id, "me");
        assert_eq!(r.fingerprint, "abcd");
        assert_eq!(r.cluster.cluster_id, "c1");
        let p = &r.nodes["p"];
        assert_eq!(p.source, Source::Member);
        assert_eq!(p.info_port, 24418);
        assert!(p.online(Instant::now()));
        assert_eq!(p.metrics.cpu.latest(), 42.0);
        assert_eq!(p.metrics.gpu.latest(), 70.0);
        assert!(p.metrics.gpu_known);
        assert!(r.invite.is_some());
        assert_eq!(r.log.back().map(|l| l.contains("hello")), Some(true));
    }

    #[test]
    fn a_stale_snapshot_is_not_a_running_router() {
        let dir = std::env::temp_dir().join(format!("kmplify-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut snap = lock(&new_shared_for_tests("me")).snapshot();
        snap.published_at_ms -= 60_000;
        std::fs::write(path(&dir), serde_json::to_vec(&snap).unwrap()).unwrap();
        assert!(read(&dir).is_none());
        assert!(!running_elsewhere(&dir));
        let fresh = lock(&new_shared_for_tests("me")).snapshot();
        std::fs::write(path(&dir), serde_json::to_vec(&fresh).unwrap()).unwrap();
        assert!(read(&dir).is_some());
        assert!(!running_elsewhere(&dir), "our own pid is not 'elsewhere'");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn orders_apply_to_the_local_router() {
        let shared = new_shared_for_tests("me");
        lock(&shared).node_dir =
            std::env::temp_dir().join(format!("kmplify-orders-{}", std::process::id()));
        let handle = RouterHandle::Local(shared.clone());
        assert!(handle
            .command(RouterCommand::AddNode {
                address: "10.0.0.9:24418".into()
            })
            .is_ok());
        assert_eq!(lock(&shared).manual, vec!["10.0.0.9:24418"]);
        assert!(
            handle
                .command(RouterCommand::AddNode {
                    address: "10.0.0.9:24418".into()
                })
                .is_err(),
            "twice is refused"
        );
        assert!(handle.command(RouterCommand::Ingress { on: false }).is_ok());
        assert!(!lock(&shared).lan_ingress);
        let pin_msg = handle.command(RouterCommand::Invite).unwrap();
        assert!(pin_msg.contains("PIN"));
        assert!(handle.command(RouterCommand::CancelInvite).is_ok());
        assert!(lock(&shared).invite.is_none());
        assert!(handle
            .command(RouterCommand::ForgetNode {
                address: "10.0.0.9:24418".into()
            })
            .is_ok());
        assert!(lock(&shared).manual.is_empty());
        assert!(handle
            .command(RouterCommand::Engine {
                node: String::new(),
                engine: "ollama".into(),
                action: "nope".into(),
                model: String::new()
            })
            .is_err());
    }
}
