//! Finding the other nodes: mDNS on the local link, LAN mode only.
//!
//! The node announces one `_kmplify-node._tcp` record: its label as the
//! instance name, the node-info port, and in TXT only a schema version and
//! the cluster key's fingerprint, so a node of another cluster is recognised
//! without being asked anything. No node id, no hardware, no computer name:
//! those are in the report, which only a holder of the key can read.
//!
//! Its own responder (mdns-sd) rather than the system's, because Windows
//! ships none; it shares UDP 5353 with Bonjour and Avahi where they run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use super::auth::ClusterKey;
use super::{lock, policy, Config, Peer, PeerState, Shared, SERVICE_TYPE};

/// A running announcement and browse.
pub struct Discovery {
    daemon: ServiceDaemon,
    stop: Arc<AtomicBool>,
}

impl Discovery {
    pub fn start(shared: Shared, config: &Config, key: &ClusterKey) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        let host = format!("{}.local.", config.name);
        let fingerprint = key.fingerprint();
        let props = [("v", "1"), ("kf", fingerprint.as_str())];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &config.name,
            &host,
            "",
            config.port,
            &props[..],
        )
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
        let me = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| e.to_string())?;
        let events = daemon.browse(SERVICE_TYPE).map_err(|e| e.to_string())?;
        {
            let mut r = lock(&shared);
            r.discovery = format!("announcing as {}", config.name);
            r.push_log(format!(
                "discovery: announcing as {} on the local network",
                config.name
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let allow = config.allow.clone();
        // The receiver is a blocking channel (mdns-sd's async side is not
        // compiled in), so it gets a thread of its own that checks the stop
        // flag twice a second.
        std::thread::Builder::new()
            .name("router-discovery".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match events.recv_timeout(Duration::from_millis(500)) {
                        Ok(ServiceEvent::ServiceResolved(svc)) => {
                            if svc.fullname == me {
                                continue;
                            }
                            let addrs: Vec<std::net::IpAddr> = svc
                                .addresses
                                .iter()
                                .map(|a| policy::canonical(a.to_ip_addr()))
                                .collect();
                            let found = Found {
                                instance: svc.fullname.clone(),
                                name: svc
                                    .fullname
                                    .strip_suffix(&format!(".{SERVICE_TYPE}"))
                                    .unwrap_or(&svc.fullname)
                                    .to_string(),
                                port: svc.port,
                                key_fingerprint: svc
                                    .txt_properties
                                    .get_property_val_str("kf")
                                    .unwrap_or_default()
                                    .to_string(),
                                addresses: addrs,
                            };
                            upsert(
                                &shared,
                                &allow,
                                &fingerprint,
                                found,
                                crate::status::now_ms(),
                            );
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                            let mut r = lock(&shared);
                            if let Some(p) = r.peers.iter().position(|p| p.instance == fullname) {
                                let gone = r.peers.remove(p);
                                r.push_log(format!("{} left the network", gone.name));
                            }
                        }
                        Ok(_) => {}
                        Err(mdns_sd::RecvTimeoutError::Timeout) => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { daemon, stop })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.daemon.shutdown();
    }
}

/// One resolved announcement.
pub(crate) struct Found {
    pub instance: String,
    pub name: String,
    pub port: u16,
    pub key_fingerprint: String,
    pub addresses: Vec<std::net::IpAddr>,
}

/// Fold an announcement into the peer list. Only an address on the
/// allowlist is ever polled; a peer of another cluster is listed, so the
/// operator sees why it is not in the cluster, but never asked.
pub(crate) fn upsert(
    shared: &Shared,
    allow: &policy::Allowlist,
    my_fingerprint: &str,
    found: Found,
    now_ms: u64,
) {
    let allowed = found
        .addresses
        .iter()
        .copied()
        // IPv4 first: the allowlist is usually written in it, and a
        // link-local IPv6 address needs a scope to be dialled.
        .filter(|a| a.is_ipv4())
        .chain(found.addresses.iter().copied().filter(|a| a.is_ipv6()))
        .find(|a| allow.allows(*a));
    let shown = allowed
        .or_else(|| found.addresses.first().copied())
        .map(|a| a.to_string())
        .unwrap_or_default();
    let (state, detail) = if allowed.is_none() {
        (
            PeerState::NotAllowed,
            "its address is not on this node's allowlist (router-allow)".to_string(),
        )
    } else if !found.key_fingerprint.is_empty() && found.key_fingerprint != my_fingerprint {
        (
            PeerState::OtherCluster,
            format!(
                "holds another cluster key ({}); run kmplify-node router join there to add it",
                found.key_fingerprint
            ),
        )
    } else {
        (PeerState::Found, String::new())
    };
    let mut r = lock(shared);
    match r.peers.iter_mut().find(|p| p.instance == found.instance) {
        Some(p) => {
            p.seen_ms = now_ms;
            p.port = found.port;
            p.key_fingerprint = found.key_fingerprint;
            if p.address != shown
                || matches!(state, PeerState::NotAllowed | PeerState::OtherCluster)
            {
                p.address = shown;
                p.state = state;
                p.detail = detail;
            }
        }
        None => {
            let name = found.name.clone();
            r.peers.push(Peer {
                instance: found.instance,
                name: found.name,
                address: shown,
                port: found.port,
                key_fingerprint: found.key_fingerprint,
                state,
                detail,
                seen_ms: now_ms,
                ..Default::default()
            });
            r.push_log(format!("found {name} on the network"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(addr: &str, kf: &str) -> Found {
        Found {
            instance: format!("studio.{SERVICE_TYPE}"),
            name: "studio".into(),
            port: 14418,
            key_fingerprint: kf.into(),
            addresses: vec![addr.parse().unwrap()],
        }
    }

    #[test]
    fn only_allowlisted_peers_of_this_cluster_are_marked_for_polling() {
        let allow = policy::Allowlist::parse(&["192.168.1.0/24"]).unwrap();
        let shared = super::super::shared_for_tests();
        upsert(
            &shared,
            &allow,
            "aaaa0000",
            found("192.168.1.20", "aaaa0000"),
            1,
        );
        assert_eq!(lock(&shared).peers[0].state, PeerState::Found);

        let shared = super::super::shared_for_tests();
        upsert(
            &shared,
            &allow,
            "aaaa0000",
            found("10.9.9.9", "aaaa0000"),
            1,
        );
        assert_eq!(lock(&shared).peers[0].state, PeerState::NotAllowed);

        let shared = super::super::shared_for_tests();
        upsert(
            &shared,
            &allow,
            "aaaa0000",
            found("192.168.1.20", "bbbb1111"),
            1,
        );
        assert_eq!(lock(&shared).peers[0].state, PeerState::OtherCluster);
    }

    #[test]
    fn a_reannouncement_updates_the_one_card() {
        let allow = policy::Allowlist::parse(&["192.168.1.0/24"]).unwrap();
        let shared = super::super::shared_for_tests();
        upsert(&shared, &allow, "k", found("192.168.1.20", "k"), 1);
        lock(&shared).peers[0].state = PeerState::Online;
        upsert(&shared, &allow, "k", found("192.168.1.20", "k"), 2);
        let r = lock(&shared);
        assert_eq!(r.peers.len(), 1);
        assert_eq!(
            r.peers[0].state,
            PeerState::Online,
            "a heartbeat keeps the state"
        );
        assert_eq!(r.peers[0].seen_ms, 2);
    }
}
