//! Keeping each peer's card current: its signed report every two seconds
//! while it answers, every thirty while it does not.
//!
//! Only a peer discovery marked as allowlisted and of this cluster is asked
//! ([`PeerState::NotAllowed`] and [`PeerState::OtherCluster`] never are),
//! every request is signed with the cluster key, and an answer counts only
//! with a valid signature over this request's nonce: a machine that merely
//! claims to be a peer cannot put numbers on this node's screen.

use std::time::Duration;

use super::auth::{self, ClusterKey};
use super::endpoint::NODE_INFO_PATH;
use super::{lock, Config, NodeReport, PeerState, Shared};

const HEALTHY_EVERY_MS: u64 = 2_000;
const FAILING_EVERY_MS: u64 = 30_000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// A peer neither announced nor answering for this long leaves the list.
const FORGET_AFTER_MS: u64 = 120_000;
/// A report larger than this is not a report.
const MAX_REPORT: usize = 64 * 1024;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // Never follow a redirect: the signed answer must come from the
        // address that was asked.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// The URL of a peer's report. IPv6 addresses go in brackets.
pub fn info_url(address: &str, port: u16) -> String {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => format!("http://[{v6}]:{port}{NODE_INFO_PATH}"),
        _ => format!("http://{address}:{port}{NODE_INFO_PATH}"),
    }
}

/// Ask one peer for its report. `Err` carries the state to show.
pub async fn fetch(
    client: &reqwest::Client,
    key: &ClusterKey,
    address: &str,
    port: u16,
) -> Result<NodeReport, (PeerState, String)> {
    let nonce = auth::new_nonce();
    let header = auth::sign_request(key, "GET", NODE_INFO_PATH, crate::status::now_ms(), &nonce);
    let resp = client
        .get(info_url(address, port))
        .header(auth::AUTH_HEADER, header)
        .send()
        .await
        .map_err(|e| (PeerState::Unreachable, short_error(&e)))?;
    let status = resp.status().as_u16();
    if status == 401 {
        return Err((
            PeerState::Unauthorized,
            "it refused this cluster's key".into(),
        ));
    }
    if status != 200 {
        return Err((PeerState::Unreachable, format!("answered {status}")));
    }
    let sig = resp
        .headers()
        .get(auth::SIG_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_REPORT)
    {
        return Err((PeerState::Unreachable, "the report is too large".into()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| (PeerState::Unreachable, short_error(&e)))?;
    if body.len() > MAX_REPORT {
        return Err((PeerState::Unreachable, "the report is too large".into()));
    }
    if !auth::verify_response(key, &nonce, &body, &sig) {
        return Err((
            PeerState::Unauthorized,
            "its answer is not signed with this cluster's key".into(),
        ));
    }
    serde_json::from_slice::<NodeReport>(&body)
        .map_err(|_| (PeerState::Unreachable, "the report did not parse".into()))
}

fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "no answer within 2 s".into()
    } else if e.is_connect() {
        "connection refused or unreachable".into()
    } else {
        "request failed".into()
    }
}

/// Poll every due peer until aborted.
pub async fn poll(shared: Shared, _config: Config, key: ClusterKey) {
    let client = client();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = crate::status::now_ms();
        let due: Vec<(String, String, u16)> = {
            let mut r = lock(&shared);
            r.peers
                .retain(|p| now.saturating_sub(p.seen_ms.max(p.last_ok_ms)) < FORGET_AFTER_MS);
            let mut due = Vec::new();
            for p in r.peers.iter_mut() {
                let pollable = !matches!(p.state, PeerState::NotAllowed | PeerState::OtherCluster)
                    && !p.address.is_empty();
                if pollable && !p.asking && now >= p.next_poll_ms {
                    p.asking = true;
                    due.push((p.instance.clone(), p.address.clone(), p.port));
                }
            }
            due
        };
        for (instance, address, port) in due {
            let (shared, client, key) = (shared.clone(), client.clone(), key.clone());
            tokio::spawn(async move {
                let result = fetch(&client, &key, &address, port).await;
                let now = crate::status::now_ms();
                let mut r = lock(&shared);
                let Some(p) = r.peers.iter_mut().find(|p| p.instance == instance) else {
                    return;
                };
                p.asking = false;
                let was = p.state;
                match result {
                    Ok(report) => {
                        p.meters.sample(&report);
                        p.report = Some(report);
                        p.state = PeerState::Online;
                        p.detail = String::new();
                        p.last_ok_ms = now;
                        p.next_poll_ms = now + HEALTHY_EVERY_MS;
                    }
                    Err((state, detail)) => {
                        p.state = state;
                        p.detail = detail;
                        p.next_poll_ms = now + FAILING_EVERY_MS;
                    }
                }
                if was != p.state {
                    let line = format!("{} is {}", p.name, p.state.label());
                    r.push_log(line);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_bracket_ipv6() {
        assert_eq!(
            info_url("192.168.1.2", 14418),
            "http://192.168.1.2:14418/v1/node-info"
        );
        assert_eq!(
            info_url("fd00::2", 14418),
            "http://[fd00::2]:14418/v1/node-info"
        );
    }
}
