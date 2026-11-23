//! The node-info surface: `GET /v1/node-info` on [`super::NODE_INFO_PORT`].
//!
//! The branch served this on every interface, in plaintext, to anyone on
//! the subnet. Here it is restricted (see [`super::policy`]):
//!
//! 1. it binds 127.0.0.1 unless the router is in LAN mode;
//! 2. a connection from an address that may not knock ([`policy::admit`]:
//!    not loopback, or not on the LAN allowlist) is closed before a byte of
//!    it is read, with no answer at all;
//! 3. every request, loopback included, must carry a valid signature made
//!    with the cluster key ([`auth::Verifier`]), else 401;
//! 4. every answer is signed back over the request's nonce, so the caller
//!    knows it came from a holder of the key.
//!
//! One request per connection, small bounded reads and a deadline: the
//! dashboard's HTTP reader, which already holds those limits.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;

use super::auth::{self, ClusterKey, Verifier};
use super::policy::{self, Mode};
use super::{lock, Config, Shared};
use crate::dashboard::http::{self, Request, Response};

/// The one path this surface answers.
pub const NODE_INFO_PATH: &str = "/v1/node-info";
/// Connections handled at once; more are closed unanswered.
const MAX_CONNECTIONS: usize = 16;

/// Answer one request that was allowed to knock. `now_ms` is passed in so
/// the tests can hold the clock still.
pub fn answer(
    req: &Request,
    key: &ClusterKey,
    verifier: &mut Verifier,
    report: Option<&super::NodeReport>,
    now_ms: u64,
) -> Response {
    if req.method != "GET" {
        return Response::text(405, "GET only");
    }
    let nonce = match verifier.verify(
        key,
        req.header(auth::AUTH_HEADER),
        &req.method,
        &req.path,
        now_ms,
    ) {
        Ok(n) => n,
        // One answer for every refusal: the caller learns nothing about
        // which part of its attempt was wrong.
        Err(_) => return Response::text(401, "a signature with this cluster's key is required"),
    };
    let resp = match (req.path.as_str(), report) {
        (NODE_INFO_PATH, Some(r)) => Response::json(200, &json!(r)),
        (NODE_INFO_PATH, None) => Response::text(503, "no report yet"),
        _ => Response::text(404, "not found"),
    };
    let sig = auth::sign_response(key, &nonce, &resp.body);
    resp.with_header(auth::SIG_HEADER, sig)
}

/// Bind for `config` and serve until aborted.
pub async fn serve(shared: Shared, config: Config, key: ClusterKey) {
    let Some(addr) = policy::bind_addr(config.mode, config.port) else {
        return;
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let mut r = lock(&shared);
            r.listening = String::new();
            r.push_log(format!("node-info cannot listen on {addr}: {e}"));
            r.problem = format!("cannot listen on {addr}: {e}");
            return;
        }
    };
    let bound = match listener.local_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    if let Err(e) = policy::check_bound(config.mode, &bound) {
        lock(&shared).push_log(format!("node-info refused to serve: {e}"));
        return;
    }
    {
        let mut r = lock(&shared);
        r.listening = bound.to_string();
        r.push_log(format!(
            "node-info on {bound} ({})",
            if config.mode == Mode::Lan {
                "allowlist and cluster key"
            } else {
                "this computer only, cluster key"
            }
        ));
    }
    let verifier = Arc::new(std::sync::Mutex::new(Verifier::default()));
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let key = Arc::new(key);
    let config = Arc::new(config);
    loop {
        let Ok((mut stream, peer)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        if let Err(why) = policy::admit(config.mode, &config.allow, peer.ip()) {
            note_refusal(&shared, peer, why);
            // Dropped unanswered: an address that may not knock learns
            // nothing, not even that this is a kmplify-node.
            continue;
        }
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let (shared, verifier, key) = (shared.clone(), verifier.clone(), key.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let resp =
                match tokio::time::timeout(http::READ_DEADLINE, http::read_request(&mut stream))
                    .await
                {
                    Ok(Ok(req)) => {
                        let report = lock(&shared).local.clone();
                        let mut v = verifier.lock().unwrap_or_else(|p| p.into_inner());
                        let resp =
                            answer(&req, &key, &mut v, report.as_ref(), crate::status::now_ms());
                        if resp.status == 401 {
                            drop(v);
                            lock(&shared).push_log(format!(
                                "refused an unsigned or foreign request from {}",
                                peer.ip()
                            ));
                        }
                        resp
                    }
                    Ok(Err(code)) => Response::text(code, "bad request"),
                    Err(_) => return,
                };
            http::write_response(&mut stream, &resp).await;
        });
    }
}

/// Log a refused address once a minute at most, so a scanner cannot fill
/// the log.
fn note_refusal(shared: &Shared, peer: SocketAddr, why: policy::Refusal) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::status::now_ms();
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 60_000 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let what = match why {
        policy::Refusal::NotLocal => "the router is on this computer only",
        policy::Refusal::NotAllowed => "not on the allowlist",
    };
    lock(shared).push_log(format!("closed a connection from {} ({what})", peer.ip()));
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn req(method: &str, path: &str, auth_value: Option<String>) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            query: Vec::new(),
            headers: auth_value
                .map(|v| vec![(auth::AUTH_HEADER.to_string(), v)])
                .unwrap_or_default(),
            body: Vec::new(),
        }
    }

    fn report() -> super::super::NodeReport {
        super::super::NodeReport {
            id: "n1".into(),
            name: "studio".into(),
            ..Default::default()
        }
    }

    fn sig(resp: &Response) -> Option<&str> {
        resp.extra
            .iter()
            .find(|(k, _)| *k == auth::SIG_HEADER)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_signed_request_gets_a_signed_report() {
        let key = ClusterKey::generate();
        let mut v = Verifier::default();
        let nonce = auth::new_nonce();
        let h = auth::sign_request(&key, "GET", NODE_INFO_PATH, NOW, &nonce);
        let resp = answer(
            &req("GET", NODE_INFO_PATH, Some(h)),
            &key,
            &mut v,
            Some(&report()),
            NOW,
        );
        assert_eq!(resp.status, 200);
        let body: super::super::NodeReport = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body.name, "studio");
        assert!(auth::verify_response(
            &key,
            &nonce,
            &resp.body,
            sig(&resp).unwrap()
        ));
    }

    #[test]
    fn no_key_no_report() {
        let key = ClusterKey::generate();
        let other = ClusterKey::generate();
        let mut v = Verifier::default();
        // Unsigned.
        let resp = answer(
            &req("GET", NODE_INFO_PATH, None),
            &key,
            &mut v,
            Some(&report()),
            NOW,
        );
        assert_eq!(resp.status, 401);
        assert!(!String::from_utf8_lossy(&resp.body).contains("studio"));
        assert!(sig(&resp).is_none(), "nothing is signed for a stranger");
        // Signed with another cluster's key.
        let h = auth::sign_request(&other, "GET", NODE_INFO_PATH, NOW, &auth::new_nonce());
        let resp = answer(
            &req("GET", NODE_INFO_PATH, Some(h)),
            &key,
            &mut v,
            Some(&report()),
            NOW,
        );
        assert_eq!(resp.status, 401);
        // A replayed capture.
        let h = auth::sign_request(&key, "GET", NODE_INFO_PATH, NOW, &auth::new_nonce());
        assert_eq!(
            answer(
                &req("GET", NODE_INFO_PATH, Some(h.clone())),
                &key,
                &mut v,
                Some(&report()),
                NOW
            )
            .status,
            200
        );
        assert_eq!(
            answer(
                &req("GET", NODE_INFO_PATH, Some(h)),
                &key,
                &mut v,
                Some(&report()),
                NOW
            )
            .status,
            401
        );
        // Only GET.
        let h = auth::sign_request(&key, "POST", NODE_INFO_PATH, NOW, &auth::new_nonce());
        assert_eq!(
            answer(
                &req("POST", NODE_INFO_PATH, Some(h)),
                &key,
                &mut v,
                Some(&report()),
                NOW
            )
            .status,
            405
        );
    }

    #[test]
    fn unknown_paths_are_404_after_authentication() {
        let key = ClusterKey::generate();
        let mut v = Verifier::default();
        let resp = answer(
            &req("GET", "/v1/other", None),
            &key,
            &mut v,
            Some(&report()),
            NOW,
        );
        assert_eq!(resp.status, 401, "a stranger cannot map the surface");
        let h = auth::sign_request(&key, "GET", "/v1/other", NOW, &auth::new_nonce());
        let resp = answer(
            &req("GET", "/v1/other", Some(h)),
            &key,
            &mut v,
            Some(&report()),
            NOW,
        );
        assert_eq!(resp.status, 404);
    }

    /// The listener end to end: loopback in local mode, signed requests,
    /// and the bind never leaves loopback.
    #[tokio::test]
    async fn the_local_surface_answers_signed_requests_on_loopback_only() {
        let shared = super::super::shared_for_tests();
        lock(&shared).local = Some(report());
        let key = ClusterKey::generate();
        // Port 0 picks a free one; read it back from the state.
        let config = Config {
            asked: Mode::Local,
            mode: Mode::Local,
            port: 0,
            allow: policy::Allowlist::default(),
            allow_text: Vec::new(),
            name: "studio".into(),
            problem: String::new(),
        };
        let task = tokio::spawn(serve(shared.clone(), config, key.clone()));
        let mut listening = String::new();
        for _ in 0..100 {
            listening = lock(&shared).listening.clone();
            if !listening.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let addr: SocketAddr = listening.parse().expect("listening");
        assert!(addr.ip().is_loopback(), "{addr}");
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{NODE_INFO_PATH}");
        let unsigned = client.get(&url).send().await.unwrap();
        assert_eq!(unsigned.status().as_u16(), 401);
        let nonce = auth::new_nonce();
        let h = auth::sign_request(&key, "GET", NODE_INFO_PATH, crate::status::now_ms(), &nonce);
        let signed = client
            .get(&url)
            .header(auth::AUTH_HEADER, h)
            .send()
            .await
            .unwrap();
        assert_eq!(signed.status().as_u16(), 200);
        let s = signed
            .headers()
            .get(auth::SIG_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_string();
        let body = signed.bytes().await.unwrap();
        assert!(auth::verify_response(&key, &nonce, &body, &s));
        task.abort();
    }
}
