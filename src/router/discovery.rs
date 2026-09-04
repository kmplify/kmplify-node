//! Finding the other kmplify-nodes on this network, and being found.
//!
//! One consolidated `_kmplify-node._tcp` record per host, identity and
//! static facts in TXT, exactly the trade PAIR's scanner makes: TXT records
//! are small, so the bulky facts (the model list) travel separately, and
//! the record carries only what a card needs before the first HTTP fetch.
//!
//! The responder is our own rather than the system's, because Windows
//! ships none; `mdns-sd` shares UDP 5353 with Bonjour, Avahi and any other
//! copy of this program on the machine.
//!
//! What crosses the network here is: this machine's hostname, its node id,
//! the GPU model, core and memory counts, which engines are answering and
//! how many models each serves. Nothing about the models' names, nothing
//! about any request. That is the same set the fabric hello carries, and it
//! is readable by anything on the subnet that asks — which is the reason the
//! router is opt-in and says so on its cluster screen.

use std::collections::HashMap;
use std::time::Instant;

use super::{lock, node_info_port, Engine, GpuInfo, Node, Shared, Source, SERVICE_TYPE};

/// TXT keys, short because a TXT record is byte-budgeted.
const K_ID: &str = "id";
const K_NAME: &str = "name";
const K_GPU: &str = "gpu";
const K_VRAM: &str = "vram";
const K_CPU: &str = "cpu";
const K_CORES: &str = "cores";
const K_RAM: &str = "ram";
const K_ENGINES: &str = "eng";
const K_VERSION: &str = "v";
/// The cluster the node belongs to, so a card can say "paired" or "other
/// cluster" before any handshake. Display only; trust is the certificate.
const K_CLUSTER: &str = "cl";

/// The engines part of the record: `id:models,id:models` for the ones that
/// answer, so a peer's card can show badges before any HTTP round trip.
pub fn encode_engines(engines: &[Engine]) -> String {
    engines
        .iter()
        .filter(|e| e.running)
        .map(|e| format!("{}:{}", e.id, e.models.len()))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn decode_engines(s: &str) -> Vec<Engine> {
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (id, n) = p.split_once(':').unwrap_or((p, "0"));
            let known = crate::engines::known(id);
            Engine {
                id: id.to_string(),
                name: known
                    .map(|k| k.name.to_string())
                    .unwrap_or_else(|| id.to_string()),
                base: known.map(|k| k.default_base.to_string()).unwrap_or_default(),
                // The count travels; the names come with the node-info fetch
                // in phase 2. Placeholders keep `model_count()` truthful.
                models: vec![String::new(); n.trim().parse().unwrap_or(0)],
                running: true,
            }
        })
        .collect()
}

/// A peer card from a resolved record. `None` when the record is not one of
/// ours (no id) — a stranger's service that happens to share the type.
pub fn node_from_txt(
    txt: &HashMap<String, String>,
    address: String,
    now: Instant,
) -> Option<Node> {
    let id = txt.get(K_ID)?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let get = |k: &str| txt.get(k).cloned().unwrap_or_default();
    let gpus = {
        let name = get(K_GPU);
        if name.is_empty() {
            vec![]
        } else {
            vec![GpuInfo {
                name,
                total_mb: get(K_VRAM).parse().unwrap_or(0),
            }]
        }
    };
    let name = {
        let n = get(K_NAME);
        if n.is_empty() {
            address.clone()
        } else {
            n
        }
    };
    let mut node = Node::new_peer(id, name, address, Source::Discovered, now);
    node.gpus = gpus;
    node.cpu_model = get(K_CPU);
    node.cpu_cores = get(K_CORES).parse().unwrap_or(0);
    node.ram_total_mb = get(K_RAM).parse().unwrap_or(0);
    node.engines = decode_engines(&get(K_ENGINES));
    node.version = get(K_VERSION);
    node.cluster_id = get(K_CLUSTER);
    Some(node)
}

/// The TXT properties this machine advertises right now.
fn own_txt(shared: &Shared) -> Vec<(String, String)> {
    let r = lock(shared);
    let Some(me) = r.local() else {
        return vec![];
    };
    let gpu = me.gpus.first();
    vec![
        (K_ID.into(), me.id.clone()),
        (K_NAME.into(), me.name.clone()),
        (
            K_GPU.into(),
            gpu.map(|g| g.name.clone()).unwrap_or_default(),
        ),
        (
            K_VRAM.into(),
            gpu.map(|g| g.total_mb.to_string()).unwrap_or_default(),
        ),
        (K_CPU.into(), me.cpu_model.clone()),
        (K_CORES.into(), me.cpu_cores.to_string()),
        (K_RAM.into(), me.ram_total_mb.to_string()),
        (K_ENGINES.into(), encode_engines(&me.engines)),
        (K_VERSION.into(), me.version.clone()),
        (K_CLUSTER.into(), r.cluster.cluster_id.clone()),
    ]
}

/// Advertise and browse on a plain thread: `mdns-sd` hands out a blocking
/// channel, and parking an async worker thread in it would stall every
/// other task on that thread.
pub fn spawn(shared: Shared) {
    std::thread::Builder::new()
        .name("kmplify-mdns".into())
        .spawn(move || run(shared))
        .expect("spawn discovery thread");
}

fn run(shared: Shared) {
    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            let mut r = lock(&shared);
            r.discovery = format!("off: {e}");
            r.push_log(format!("discovery unavailable: {e}"));
            return;
        }
    };

    let (self_id, host) = {
        let r = lock(&shared);
        (r.self_id.clone(), super::hostname())
    };
    // Instance name = hostname plus a slice of the id, so two machines with
    // one hostname (the default on a fresh install of anything) stay two
    // records instead of one fighting itself.
    let instance = format!("{host}-{}", &self_id[..8.min(self_id.len())]);
    let host_fqdn = format!("{host}.local.");

    let register = |daemon: &ServiceDaemon, props: Vec<(String, String)>| -> Result<(), String> {
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_fqdn,
            "",
            node_info_port(),
            &props[..],
        )
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
        daemon.register(info).map_err(|e| e.to_string())
    };

    let mut advertised = own_txt(&shared);
    if let Err(e) = register(&daemon, advertised.clone()) {
        let mut r = lock(&shared);
        r.discovery = format!("browsing, not advertising: {e}");
        r.push_log(format!("could not advertise this node: {e}"));
    } else {
        let mut r = lock(&shared);
        r.discovery = "advertising and browsing".into();
        r.push_log(format!("advertising {instance} on the local network"));
    }

    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(rx) => rx,
        Err(e) => {
            let mut r = lock(&shared);
            r.discovery = format!("advertising only: {e}");
            r.push_log(format!("cannot browse for peers: {e}"));
            return;
        }
    };

    let mut last_refresh = Instant::now();
    loop {
        // Re-register when what we advertise changed (an engine came up), so
        // peers' badges follow within a scan or two. A refresh, not a churn:
        // only when the TXT actually differs.
        if last_refresh.elapsed().as_secs() >= 10 {
            last_refresh = Instant::now();
            let now_txt = own_txt(&shared);
            if now_txt != advertised {
                let full = format!("{instance}.{SERVICE_TYPE}");
                let _ = daemon.unregister(&full);
                if register(&daemon, now_txt.clone()).is_ok() {
                    advertised = now_txt;
                }
            }
        }
        let event = match receiver.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(ev) => ev,
            Err(mdns_sd::RecvTimeoutError::Timeout) => continue,
            Err(mdns_sd::RecvTimeoutError::Disconnected) => {
                lock(&shared).discovery = "stopped".into();
                return;
            }
        };
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let txt: HashMap<String, String> = info
                    .get_properties()
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string()))
                    .collect();
                // Prefer a routable IPv4 address: it is what an operator
                // recognises on the card and what the engines bind by
                // default. A host with an idle adapter also announces its
                // 169.254.x link-local address, which nobody else can reach.
                let addrs = info.get_addresses();
                let address = addrs
                    .iter()
                    .find(|a| match a.to_string().parse::<std::net::IpAddr>() {
                        Ok(std::net::IpAddr::V4(v4)) => !v4.is_link_local() && !v4.is_loopback(),
                        _ => false,
                    })
                    .or_else(|| addrs.iter().find(|a| a.is_ipv4()))
                    .or_else(|| addrs.iter().next())
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                let Some(node) = node_from_txt(&txt, address, Instant::now()) else {
                    continue;
                };
                if node.id == self_id {
                    continue;
                }
                let mut r = lock(&shared);
                let fresh = !r.nodes.contains_key(&node.id);
                let name = node.name.clone();
                r.upsert_peer(node);
                if fresh {
                    r.push_log(format!("found {name} on the local network"));
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                // The record names the instance, not the id; the expiry
                // timer does the removal, this only notes the goodbye.
                lock(&shared).push_log(format!("{fullname} said goodbye"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_round_trip_through_txt() {
        let engines = vec![
            Engine {
                id: "ollama".into(),
                name: "Ollama".into(),
                base: "http://127.0.0.1:11434".into(),
                models: vec!["a".into(), "b".into(), "c".into()],
                running: true,
            },
            Engine {
                id: "vllm".into(),
                name: "vLLM".into(),
                base: String::new(),
                models: vec![],
                running: false,
            },
        ];
        let s = encode_engines(&engines);
        assert_eq!(s, "ollama:3", "only running engines travel");
        let back = decode_engines(&s);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "Ollama");
        assert_eq!(back[0].models.len(), 3);
        assert!(back[0].running);
    }

    #[test]
    fn a_record_without_our_id_is_not_a_node() {
        let mut txt = HashMap::new();
        txt.insert("name".to_string(), "printer".to_string());
        assert!(node_from_txt(&txt, "10.0.0.9".into(), Instant::now()).is_none());
    }

    #[test]
    fn a_record_becomes_a_peer_card() {
        let mut txt = HashMap::new();
        txt.insert("id".into(), "abc".into());
        txt.insert("name".into(), "spark".into());
        txt.insert("gpu".into(), "NVIDIA GB10".into());
        txt.insert("vram".into(), "131072".into());
        txt.insert("cores".into(), "20".into());
        txt.insert("ram".into(), "131072".into());
        txt.insert("eng".into(), "ollama:4,lmstudio:2".into());
        let n = node_from_txt(&txt, "192.168.1.25".into(), Instant::now()).unwrap();
        assert_eq!(n.id, "abc");
        assert_eq!(n.name, "spark");
        assert_eq!(n.source, Source::Discovered);
        assert_eq!(n.gpus[0].total_mb, 131072);
        assert_eq!(n.cpu_cores, 20);
        assert_eq!(n.model_count(), 6);
    }
}
