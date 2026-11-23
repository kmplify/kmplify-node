//! The LAN router: the kmplify-nodes on one local network, seen together.
//!
//! The fabric worker lends this machine *outward*, to a gateway, over one
//! outbound socket. The router points the other way: it shows this node's
//! hardware and live meters to the other kmplify-nodes on the same network,
//! and theirs to it, so the app window (and later the web and terminal
//! views, kmplify-desktop and the WebApp's installation admin view) can show
//! the whole personal cluster. Nothing leaves the network.
//!
//! Adapted from the owner's port of NVIDIA's Personal AI Router (PAIR,
//! Apache-2.0, see NOTICE) on the `feature/lan-router-gui` branch. This is
//! its first phase on the current node: the router core (state, local
//! report, discovery, peer reports) and the node-info surface, with the
//! security model the owner set on 2026-11-23 (see [`policy`]):
//!
//! * off unless the operator turns it on (`kmplify-node set router=...`);
//! * `local`: the node-info surface on 127.0.0.1 only;
//! * `lan`: on the network, but only for an allowlist of local addresses,
//!   only with the cluster key ([`auth`]), and only then announced by mDNS.
//!
//! Phase 2 (protocol v2) adds PIN pairing ([`pairing`]: a device joins
//! without the key being shown or typed) and mutual TLS between members
//! ([`tls`]: certificates bound to the cluster key), with v1 still served.
//! The routing proxies, per-GPU telemetry and remote engine control from
//! the branch are the next phases; docs/ROUTER.md and the port plan list
//! them.
//!
//! What a router publishes for other processes on this machine (the app
//! window) is `router.json` in the node directory, owner-only, beside
//! `status.json`. It holds no secret: the key's fingerprint, never the key.

pub mod key;
pub mod policy;

#[cfg(feature = "router")]
pub mod auth;
#[cfg(feature = "router")]
mod discovery;
#[cfg(feature = "router")]
pub mod endpoint;
#[cfg(feature = "router")]
pub mod pairing;
#[cfg(feature = "router")]
mod peers;
#[cfg(feature = "router")]
pub mod tls;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use key::{ClusterKey, FileKeyStore, KeyStore};
pub use policy::Mode;

/// The port of the node-info surface unless `KMPLIFY_ROUTER_PORT` says
/// otherwise. The branch's choice, kept so the two never disagree.
pub const NODE_INFO_PORT: u16 = 14418;
/// The mDNS service every LAN-mode node announces and browses.
pub const SERVICE_TYPE: &str = "_kmplify-node._tcp.local.";
/// File in the node directory the running router publishes.
pub const STATE_FILE: &str = "router.json";
/// Samples kept per meter: one a second, so a minute.
pub const HISTORY: usize = 60;
/// A published state older than this is from a router that stopped.
pub const STALE_AFTER_MS: u64 = 5_000;

/// What a node tells the others about itself: hardware, meters and what it
/// serves. Never a credential, a prompt, a consumer or a log line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeReport {
    /// The node id (public: it is the handle consumers pin).
    pub id: String,
    pub name: String,
    pub version: String,
    pub os: String,
    pub accelerator: String,
    pub gpu_name: String,
    pub vram_total_mb: u64,
    pub vram_used_mb: u64,
    /// `None` where the platform does not report GPU load (Metal).
    pub gpu_percent: Option<u8>,
    pub cpu_model: String,
    pub cpus: f64,
    pub cpu_percent: f32,
    pub ram_total_mb: u64,
    pub ram_used_mb: u64,
    /// The node's fabric link (`ONLINE`, `RETRYING`, ...).
    pub link: String,
    pub paused: bool,
    pub models: Vec<String>,
    pub jobs_active: u64,
    pub jobs_done: u64,
    pub sessions: usize,
}

impl NodeReport {
    /// This node, from what the node's own sampler last read.
    pub fn local(snap: &crate::status::Snapshot, name: &str) -> Self {
        Self {
            id: snap.node_id.clone(),
            name: name.to_string(),
            version: snap.version.clone(),
            os: std::env::consts::OS.to_string(),
            accelerator: snap.accelerator.clone(),
            gpu_name: snap.gpu_name.clone(),
            vram_total_mb: snap.vram_total_mb,
            vram_used_mb: snap.vram_used_mb,
            gpu_percent: snap.gpu_percent,
            cpu_model: snap.cpu_model.clone(),
            cpus: snap.cpus,
            cpu_percent: snap.cpu_percent,
            ram_total_mb: snap.ram_total_mb,
            ram_used_mb: snap.ram_used_mb,
            link: snap.link.label().to_string(),
            paused: snap.paused,
            models: snap.models.clone(),
            jobs_active: snap.jobs.active,
            jobs_done: snap.jobs.done,
            sessions: snap.sessions.len(),
        }
    }

    pub fn ram_percent(&self) -> f32 {
        if self.ram_total_mb == 0 {
            0.0
        } else {
            (100.0 * self.ram_used_mb as f64 / self.ram_total_mb as f64) as f32
        }
    }
}

/// A minute of one meter, 0-100, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Series(VecDeque<f32>);

impl Series {
    pub fn push(&mut self, v: f32) {
        if self.0.len() >= HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(if v.is_finite() {
            v.clamp(0.0, 100.0)
        } else {
            0.0
        });
    }

    pub fn points(&self) -> Vec<f32> {
        self.0.iter().copied().collect()
    }

    pub fn latest(&self) -> Option<f32> {
        self.0.back().copied()
    }
}

/// The meters a node card charts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meters {
    pub cpu: Series,
    pub ram: Series,
    /// Empty while the platform gives no GPU figure.
    pub gpu: Series,
}

impl Meters {
    pub fn sample(&mut self, r: &NodeReport) {
        self.cpu.push(r.cpu_percent);
        self.ram.push(r.ram_percent());
        if let Some(g) = r.gpu_percent {
            self.gpu.push(f32::from(g));
        }
    }
}

/// Where a peer stands, as this node last found it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Announced; not asked yet.
    #[default]
    Found,
    /// Answered with a correctly signed report.
    Online,
    /// Did not answer.
    Unreachable,
    /// Refused our key or answered without a valid signature.
    Unauthorized,
    /// Announces another cluster key fingerprint: not polled.
    OtherCluster,
    /// Its address is not on the allowlist: not polled.
    NotAllowed,
}

impl PeerState {
    pub fn label(self) -> &'static str {
        match self {
            PeerState::Found => "found",
            PeerState::Online => "online",
            PeerState::Unreachable => "not answering",
            PeerState::Unauthorized => "key refused",
            PeerState::OtherCluster => "another cluster",
            PeerState::NotAllowed => "not on the allowlist",
        }
    }
}

/// Another node on the network.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Peer {
    /// The mDNS instance it announced, which is how it is told apart.
    pub instance: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    /// The cluster key fingerprint it announced.
    pub key_fingerprint: String,
    /// The router protocol it announced: 1 = signed plain HTTP, 2 = mutual
    /// TLS with certificates bound to the cluster key.
    pub proto: u32,
    /// Its last report came over mutual TLS.
    pub secure: bool,
    pub state: PeerState,
    pub detail: String,
    pub report: Option<NodeReport>,
    pub meters: Meters,
    pub last_ok_ms: u64,
    pub seen_ms: u64,
    /// When to ask it next. Internal to the poller.
    #[serde(skip)]
    pub next_poll_ms: u64,
    /// A request to it is in flight. Internal to the poller.
    #[serde(skip)]
    pub asking: bool,
}

/// Everything the router knows, as published in `router.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouterState {
    pub schema: u32,
    /// The mode the operator asked for.
    pub asked: Mode,
    /// The mode in force: `local` instead of `lan` while LAN mode lacks
    /// what it needs, and the reason in `problem`.
    pub mode: Mode,
    pub problem: String,
    pub port: u16,
    /// The address the node-info surface listens on, or "".
    pub listening: String,
    pub key_fingerprint: String,
    /// SHA-256 of this router's certificate for this start (protocol v2).
    pub cert_fingerprint: String,
    pub allow: Vec<String>,
    pub name: String,
    /// What discovery is doing, in words.
    pub discovery: String,
    pub local: Option<NodeReport>,
    pub local_meters: Meters,
    pub peers: Vec<Peer>,
    pub published_at_ms: u64,
    pub log: Vec<String>,
}

impl RouterState {
    pub fn is_fresh(&self) -> bool {
        self.published_at_ms > 0
            && crate::status::now_ms().saturating_sub(self.published_at_ms) < STALE_AFTER_MS
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        let line = format!(
            "{} {}",
            crate::status::clock_hms(crate::status::now_ms()),
            line.into()
        );
        self.log.push(line);
        let over = self.log.len().saturating_sub(50);
        self.log.drain(..over);
    }
}

pub fn state_path(node_dir: &Path) -> PathBuf {
    node_dir.join(STATE_FILE)
}

/// What a running router published, if one runs (or ran) here.
pub fn read_published(node_dir: &Path) -> Option<RouterState> {
    let bytes = std::fs::read(state_path(node_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// How the router is configured: the stored setting first, then the
/// environment, then off. Settings and environment are the same pair every
/// other node setting uses (`kmplify-node set` writes the first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub asked: Mode,
    pub mode: Mode,
    pub port: u16,
    pub allow: policy::Allowlist,
    pub allow_text: Vec<String>,
    pub name: String,
    /// Why `mode` is not `asked`, or a setting was ignored.
    pub problem: String,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The label a node gets when nobody named it: never the computer's name,
/// which often carries a person's.
pub fn default_name(node_id: &str) -> String {
    let short: String = node_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if short.is_empty() {
        "node".into()
    } else {
        format!("node-{short}")
    }
}

impl Config {
    /// Resolve from the settings file, the environment and whether a
    /// cluster key exists.
    pub fn resolve(stored: &crate::settings::Settings, node_id: &str, has_key: bool) -> Self {
        let mut problems: Vec<String> = Vec::new();
        let asked = match stored.router.clone().or_else(|| env("KMPLIFY_ROUTER")) {
            None => Mode::Off,
            Some(v) => Mode::parse(&v).unwrap_or_else(|e| {
                problems.push(e);
                Mode::Off
            }),
        };
        let allow_text: Vec<String> = stored
            .router_allow
            .clone()
            .or_else(|| {
                env("KMPLIFY_ROUTER_ALLOW").map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                        .collect()
                })
            })
            .unwrap_or_default();
        let allow = policy::Allowlist::parse(&allow_text).unwrap_or_else(|e| {
            problems.push(format!("router-allow ignored: {e}"));
            policy::Allowlist::default()
        });
        let name = match stored
            .router_name
            .clone()
            .or_else(|| env("KMPLIFY_ROUTER_NAME"))
        {
            None => default_name(node_id),
            Some(n) => policy::check_name(&n).unwrap_or_else(|e| {
                problems.push(e);
                default_name(node_id)
            }),
        };
        let port = match env("KMPLIFY_ROUTER_PORT") {
            None => NODE_INFO_PORT,
            Some(p) => p
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .unwrap_or_else(|| {
                    problems.push(format!(
                    "KMPLIFY_ROUTER_PORT must be a port number, not {p:?}; using {NODE_INFO_PORT}"
                ));
                    NODE_INFO_PORT
                }),
        };
        let mode = match asked {
            Mode::Lan if allow.is_empty() => {
                problems.push(
                    "LAN mode needs an allowlist (kmplify-node set router-allow=192.168.1.0/24); \
                     staying on this computer until it has one"
                        .into(),
                );
                Mode::Local
            }
            Mode::Lan if !has_key => {
                problems.push(
                    "LAN mode needs the cluster key (kmplify-node router key, or router join \
                     on a second machine); staying on this computer until it has one"
                        .into(),
                );
                Mode::Local
            }
            m => m,
        };
        Self {
            asked,
            mode,
            port,
            allow,
            allow_text,
            name,
            problem: problems.join("; "),
        }
    }
}

#[cfg(feature = "router")]
pub use running::{run, run_with};

#[cfg(feature = "router")]
mod running {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub(crate) type Shared = Arc<Mutex<RouterState>>;

    pub(crate) fn lock(s: &Shared) -> std::sync::MutexGuard<'_, RouterState> {
        // A panic while holding the lock leaves the state as it was; the
        // router keeps going rather than poisoning every later frame.
        s.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The tasks of one configuration.
    struct Active {
        config: Config,
        key: Option<auth::ClusterKey>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        discovery: Option<discovery::Discovery>,
    }

    impl Active {
        fn stop(self) {
            for t in self.tasks {
                t.abort();
            }
            if let Some(d) = self.discovery {
                d.stop();
            }
        }
    }

    /// Run the router for the node in `node_dir` until `stop` turns true,
    /// with the cluster key in `router.key` there ([`FileKeyStore`]).
    ///
    /// Re-reads its configuration every second, so `kmplify-node set
    /// router=...` (or a view's change) applies without a restart: the
    /// surfaces of the old configuration are torn down and the new one's
    /// started.
    pub async fn run(node_dir: PathBuf, stop: tokio::sync::watch::Receiver<bool>) {
        let store = Arc::new(FileKeyStore::new(&node_dir));
        run_with(node_dir, stop, store).await
    }

    /// [`run`] with the cluster key in `keys` instead of `router.key`: for an
    /// application that embeds the router and keeps credentials its own way
    /// (kmplify-desktop). Settings (`settings.json`) and the published state
    /// (`router.json`) still live in `node_dir`; neither holds a secret.
    pub async fn run_with(
        node_dir: PathBuf,
        mut stop: tokio::sync::watch::Receiver<bool>,
        keys: Arc<dyn KeyStore>,
    ) {
        let shared: Shared = Arc::new(Mutex::new(RouterState {
            schema: 2,
            ..Default::default()
        }));
        let mut active: Option<Active> = None;
        let mut reported_key_error = String::new();
        loop {
            let snap = crate::status::snapshot();
            let stored = crate::settings::Settings::load(&node_dir);
            let loaded = keys.load();
            let key_error = loaded.as_ref().err().cloned().unwrap_or_default();
            let mut key = loaded.unwrap_or(None);
            let mut config = Config::resolve(&stored, &snap.node_id, key.is_some());
            if !key_error.is_empty() {
                // Neither a new key over one that could not be read, nor a
                // surface without one.
                if key_error != reported_key_error {
                    lock(&shared).push_log(format!("cluster key unreadable: {key_error}"));
                }
                config.mode = Mode::Off;
                config.problem = format!("the cluster key cannot be read: {key_error}");
            } else if config.mode != Mode::Off && key.is_none() {
                // Any active mode authenticates every request, loopback
                // included, so it needs a key: the first start makes one.
                let fresh = auth::ClusterKey::generate();
                match keys.save(&fresh) {
                    Ok(()) => {
                        lock(&shared).push_log(format!(
                            "created this node's cluster key ({}, fingerprint {})",
                            keys.place(),
                            fresh.fingerprint()
                        ));
                        key = Some(fresh);
                        config = Config::resolve(&stored, &snap.node_id, true);
                    }
                    Err(e) => {
                        config.mode = Mode::Off;
                        config.problem = format!("cannot store the cluster key: {e}");
                    }
                }
            }
            reported_key_error = key_error;
            let changed = active
                .as_ref()
                .is_none_or(|a| a.config != config || a.key != key);
            if changed {
                if let Some(a) = active.take() {
                    a.stop();
                }
                active = Some(start(&shared, config.clone(), key.clone()));
            }
            sample(&shared, &config, key.as_ref(), &snap);
            publish(&node_dir, &shared).await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = stop.changed() => {
                    if *stop.borrow() {
                        break;
                    }
                }
            }
        }
        if let Some(a) = active.take() {
            a.stop();
        }
        let _ = std::fs::remove_file(state_path(&node_dir));
    }

    fn start(shared: &Shared, config: Config, key: Option<auth::ClusterKey>) -> Active {
        // Remembered, so a changed key file restarts the surfaces.
        let started_with = key.clone();
        let mut tasks = Vec::new();
        let mut discovery = None;
        {
            let mut r = lock(shared);
            r.asked = config.asked;
            r.mode = config.mode;
            r.problem = config.problem.clone();
            r.port = config.port;
            r.allow = config.allow_text.clone();
            r.name = config.name.clone();
            r.listening = String::new();
            r.key_fingerprint = key.as_ref().map(|k| k.fingerprint()).unwrap_or_default();
            r.discovery = if config.mode == Mode::Lan {
                "starting".into()
            } else {
                "off (LAN mode only)".into()
            };
            r.peers.clear();
            if !config.problem.is_empty() {
                let p = config.problem.clone();
                r.push_log(p);
            }
            let m = config.mode;
            r.push_log(format!("router {}", m.as_str()));
        }
        let identity = match (&key, config.mode != Mode::Off) {
            (Some(k), true) => match tls::BoundIdentity::mint(k) {
                Ok(id) => Some(id),
                Err(e) => {
                    lock(shared).push_log(format!("no TLS certificate ({e}); v1 only"));
                    None
                }
            },
            _ => None,
        };
        lock(shared).cert_fingerprint = identity
            .as_ref()
            .map(|i| i.fingerprint.clone())
            .unwrap_or_default();
        if let (Some(key), true) = (key, config.mode != Mode::Off) {
            tasks.push(tokio::spawn(endpoint::serve(
                shared.clone(),
                config.clone(),
                key.clone(),
                identity.clone(),
            )));
            if config.mode == Mode::Lan {
                match discovery::Discovery::start(shared.clone(), &config, &key) {
                    Ok(d) => discovery = Some(d),
                    Err(e) => {
                        let mut r = lock(shared);
                        r.discovery = format!("unavailable: {e}");
                        r.push_log(format!("discovery unavailable: {e}"));
                    }
                }
                tasks.push(tokio::spawn(peers::poll(
                    shared.clone(),
                    config.clone(),
                    key,
                    identity,
                )));
            }
        }
        Active {
            config,
            key: started_with,
            tasks,
            discovery,
        }
    }

    fn sample(
        shared: &Shared,
        config: &Config,
        key: Option<&auth::ClusterKey>,
        snap: &crate::status::Snapshot,
    ) {
        let mut r = lock(shared);
        r.key_fingerprint = key.map(|k| k.fingerprint()).unwrap_or_default();
        if config.mode == Mode::Off {
            r.local = None;
            r.local_meters = Meters::default();
            return;
        }
        let me = NodeReport::local(snap, &config.name);
        r.local_meters.sample(&me);
        r.local = Some(me);
    }

    async fn publish(node_dir: &Path, shared: &Shared) {
        let bytes = {
            let mut r = lock(shared);
            r.published_at_ms = crate::status::now_ms();
            serde_json::to_vec(&*r)
        };
        let Ok(bytes) = bytes else { return };
        let path = state_path(node_dir);
        let tmp = path.with_extension("json.tmp");
        if crate::fabric_worker::write_private(&tmp, bytes)
            .await
            .is_ok()
        {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    /// A shared state for the tests of the router's parts.
    #[cfg(test)]
    pub(crate) fn shared_for_tests() -> Shared {
        Arc::new(Mutex::new(RouterState::default()))
    }
}

#[cfg(feature = "router")]
pub(crate) use running::{lock, Shared};

#[cfg(all(test, feature = "router"))]
pub(crate) use running::shared_for_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    fn stored(router: Option<&str>, allow: &[&str]) -> Settings {
        Settings {
            router: router.map(String::from),
            router_allow: (!allow.is_empty())
                .then(|| allow.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn the_router_is_off_unless_chosen() {
        let c = Config::resolve(&stored(None, &[]), "abcdef0123456789", false);
        // The environment of the test process may set KMPLIFY_ROUTER; the
        // stored setting is what this test controls.
        if std::env::var_os("KMPLIFY_ROUTER").is_none() {
            assert_eq!(c.mode, Mode::Off);
        }
        let c = Config::resolve(&stored(Some("off"), &[]), "x", true);
        assert_eq!(c.mode, Mode::Off);
    }

    #[test]
    fn lan_mode_needs_an_allowlist_and_a_key() {
        let c = Config::resolve(&stored(Some("lan"), &[]), "x", true);
        assert_eq!((c.asked, c.mode), (Mode::Lan, Mode::Local));
        assert!(c.problem.contains("allowlist"), "{}", c.problem);
        let c = Config::resolve(&stored(Some("lan"), &["192.168.1.0/24"]), "x", false);
        assert_eq!(c.mode, Mode::Local);
        assert!(c.problem.contains("cluster key"), "{}", c.problem);
        let c = Config::resolve(&stored(Some("lan"), &["192.168.1.0/24"]), "x", true);
        assert_eq!(c.mode, Mode::Lan);
        assert!(c.allow.allows("192.168.1.4".parse().unwrap()));
    }

    #[test]
    fn a_node_is_named_after_its_id_never_the_computer() {
        assert_eq!(default_name("0123456789abcdef"), "node-01234567");
        assert_eq!(default_name(""), "node");
        let c = Config::resolve(&stored(Some("local"), &[]), "0123456789abcdef", true);
        if std::env::var_os("KMPLIFY_ROUTER_NAME").is_none() {
            assert_eq!(c.name, "node-01234567");
        }
    }

    #[test]
    fn meters_keep_a_minute_and_clamp() {
        let mut s = Series::default();
        for i in 0..(HISTORY + 10) {
            s.push(i as f32);
        }
        assert_eq!(s.points().len(), HISTORY);
        assert_eq!(s.latest(), Some(69.0));
        s.push(250.0);
        assert_eq!(s.latest(), Some(100.0));
        s.push(f32::NAN);
        assert_eq!(s.latest(), Some(0.0));
    }

    #[test]
    fn the_local_report_carries_no_secret_and_no_log() {
        let mut snap = crate::status::Snapshot {
            node_id: "nid".into(),
            ..Default::default()
        };
        snap.logs = vec!["token=supersecret".into()];
        let r = NodeReport::local(&snap, "studio");
        let text = serde_json::to_string(&r).unwrap();
        assert!(!text.contains("supersecret"));
        assert!(text.contains("\"name\":\"studio\""));
    }
}
