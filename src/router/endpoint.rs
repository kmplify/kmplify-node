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
//!
//! The one request that changes something, `PUT /v2/node-settings`
//! ([`super::remote`]), is answered inside mutual TLS only, with a
//! body-bound signature, and only while the node's operator allows it.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;

use super::auth::{self, ClusterKey, Verifier};
use super::policy::{self, Mode};
use super::{lock, Config, Shared};
use crate::dashboard::http::{self, Request, Response};

/// The path of the node report.
pub const NODE_INFO_PATH: &str = "/v1/node-info";
pub use super::remote::SETTINGS_PATH;

/// What the settings write needs: who applies it, and where the audit file
/// goes.
pub struct RemoteCtx {
    pub host: Arc<dyn super::remote::RemoteHost>,
    pub node_dir: std::path::PathBuf,
}
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

/// Answer `PUT /v2/node-settings`. `secure` is true inside mutual TLS.
/// Returns the answer and the audit line (none for a stranger).
pub fn answer_settings(
    req: &Request,
    key: &ClusterKey,
    verifier: &std::sync::Mutex<Verifier>,
    ctx: &RemoteCtx,
    secure: bool,
    peer: &str,
    now_ms: u64,
) -> (Response, Option<String>) {
    if req.method != super::remote::SETTINGS_METHOD {
        return (Response::text(405, "PUT only"), None);
    }
    let verified = verifier
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .verify_with_body(
            key,
            req.header(auth::AUTH_HEADER),
            &req.method,
            &req.path,
            &req.body,
            now_ms,
        );
    let nonce = match verified {
        Ok(n) => n,
        Err(_) => {
            return (
                Response::text(401, "a signature with this cluster's key is required"),
                None,
            )
        }
    };
    let mut caller = super::remote::Caller {
        fingerprint: key.fingerprint(),
        request_id: String::new(),
        nonce: nonce.clone(),
        peer: peer.to_string(),
    };
    let (status, body, line) = if !secure {
        (
            403,
            json!({
                "ok": false,
                "error": "tls_required",
                "message": "settings are changed over protocol v2 (mutual TLS) only",
            }),
            format!(
                "router cluster (key {}, from {peer}): refused a settings write in plain v1",
                caller.fingerprint
            ),
        )
    } else {
        super::remote::handle(&req.body, ctx.host.as_ref(), &mut caller)
    };
    let resp = Response::json(status, &body);
    let sig = auth::sign_response(key, &nonce, &resp.body);
    (resp.with_header(auth::SIG_HEADER, sig), Some(line))
}

/// Record a settings audit line in the node's log (the Logs view), the
/// router log and `router-audit.log`.
fn audit(shared: &Shared, node_dir: &std::path::Path, line: &str) {
    let line = format!("remote settings: {line}");
    crate::status::push_log(line.clone());
    lock(shared).push_log(line.clone());
    super::remote::append_audit(node_dir, &line);
}

/// Read one request from `stream`, answer it, write the answer.
async fn exchange<S>(
    stream: &mut S,
    shared: &Shared,
    verifier: &Arc<std::sync::Mutex<Verifier>>,
    key: &Arc<ClusterKey>,
    peer: SocketAddr,
    ctx: &Arc<RemoteCtx>,
    secure: bool,
) where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    let resp = match tokio::time::timeout(http::READ_DEADLINE, http::read_request(stream)).await {
        Ok(Ok(req)) if req.path == SETTINGS_PATH => {
            // The action layer writes files and may wait on the embedding
            // application: off the async runtime.
            let (key, verifier, ctx2, shared2) =
                (key.clone(), verifier.clone(), ctx.clone(), shared.clone());
            let ip = peer.ip().to_string();
            let done = tokio::task::spawn_blocking(move || {
                let (resp, line) = answer_settings(
                    &req,
                    &key,
                    &verifier,
                    &ctx2,
                    secure,
                    &ip,
                    crate::status::now_ms(),
                );
                match line {
                    Some(l) => audit(&shared2, &ctx2.node_dir, &l),
                    None if resp.status == 401 => lock(&shared2).push_log(format!(
                        "refused an unsigned or foreign settings request from {ip}"
                    )),
                    None => {}
                }
                resp
            })
            .await;
            match done {
                Ok(r) => r,
                Err(_) => Response::text(500, "internal error"),
            }
        }
        Ok(Ok(req)) => {
            let report = lock(shared).local.clone();
            let mut v = verifier.lock().unwrap_or_else(|p| p.into_inner());
            let resp = answer(&req, key, &mut v, report.as_ref(), crate::status::now_ms());
            if resp.status == 401 {
                drop(v);
                lock(shared).push_log(format!(
                    "refused an unsigned or foreign request from {}",
                    peer.ip()
                ));
            }
            resp
        }
        Ok(Err(code)) => Response::text(code, "bad request"),
        Err(_) => return,
    };
    http::write_response(stream, &resp).await;
}

/// Bind for `config` and serve until aborted.
///
/// One port, two protocols: a connection that opens with a TLS handshake
/// record is protocol v2 (mutual TLS, both certificates bound to the
/// cluster key, see [`super::tls`]); anything else is v1, signed plain HTTP,
/// kept for nodes before v2 and for apps on this computer. The same signed
/// request and signed answer travel inside the TLS session.
pub async fn serve(
    shared: Shared,
    config: Config,
    key: ClusterKey,
    identity: Option<super::tls::BoundIdentity>,
    ctx: Arc<RemoteCtx>,
) {
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
    let acceptor = identity.as_ref().and_then(|id| {
        super::tls::server_config(id, &key)
            .map(tokio_rustls::TlsAcceptor::from)
            .map_err(|e| lock(&shared).push_log(format!("TLS unavailable: {e}")))
            .ok()
    });
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
        let (shared, verifier, key, acceptor, ctx) = (
            shared.clone(),
            verifier.clone(),
            key.clone(),
            acceptor.clone(),
            ctx.clone(),
        );
        tokio::spawn(async move {
            let _permit = permit;
            let mut first = [0u8; 1];
            let peeked = tokio::time::timeout(http::READ_DEADLINE, stream.peek(&mut first)).await;
            if !matches!(peeked, Ok(Ok(1))) {
                return;
            }
            if first[0] == super::tls::TLS_HANDSHAKE_BYTE {
                let Some(acceptor) = acceptor else { return };
                match tokio::time::timeout(http::READ_DEADLINE, acceptor.accept(stream)).await {
                    Ok(Ok(mut tls)) => {
                        exchange(&mut tls, &shared, &verifier, &key, peer, &ctx, true).await
                    }
                    Ok(Err(_)) => note_tls_refusal(&shared, peer),
                    Err(_) => {}
                }
            } else {
                exchange(&mut stream, &shared, &verifier, &key, peer, &ctx, false).await;
            }
        });
    }
}

/// Log a refused TLS handshake once a minute at most.
fn note_tls_refusal(shared: &Shared, peer: SocketAddr) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::status::now_ms();
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 60_000 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    lock(shared).push_log(format!(
        "refused a TLS connection from {} (no certificate bound to this cluster's key)",
        peer.ip()
    ));
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
        let task = tokio::spawn(serve(
            shared.clone(),
            config,
            key.clone(),
            None,
            recorder_ctx(false),
        ));
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

    struct Recorder {
        allowed: bool,
        done: std::sync::Mutex<Vec<crate::views::Act>>,
    }

    impl super::super::remote::RemoteHost for Recorder {
        fn allowed(&self) -> bool {
            self.allowed
        }
        fn perform(&self, act: &crate::views::Act) -> crate::views::Outcome {
            self.done.lock().unwrap().push(act.clone());
            crate::views::Outcome {
                status: 200,
                ok: true,
                message: "ok".into(),
            }
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kn-endpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn recorder_ctx(allowed: bool) -> Arc<RemoteCtx> {
        Arc::new(RemoteCtx {
            host: Arc::new(Recorder {
                allowed,
                done: std::sync::Mutex::new(Vec::new()),
            }),
            node_dir: temp_dir(&format!("rec-{allowed}")),
        })
    }

    fn put(body: &str, header: Option<String>) -> Request {
        Request {
            method: "PUT".into(),
            path: SETTINGS_PATH.into(),
            query: Vec::new(),
            headers: header
                .map(|v| vec![(auth::AUTH_HEADER.to_string(), v)])
                .unwrap_or_default(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn signed_put(key: &ClusterKey, body: &str) -> (Request, String) {
        let nonce = auth::new_nonce();
        let h =
            auth::sign_request_with_body(key, "PUT", SETTINGS_PATH, NOW, &nonce, body.as_bytes());
        (put(body, Some(h)), nonce)
    }

    /// The settings write, request by request: a signed write over TLS is
    /// applied and answered with a signature; a replay, another key, a v1
    /// header, a body swapped under a signature, plain v1 transport and the
    /// opt-in off are all refused, and nothing reaches the host.
    #[test]
    fn the_settings_write_is_v2_signed_and_opt_in_only() {
        let key = ClusterKey::generate();
        let v = std::sync::Mutex::new(Verifier::default());
        let rec = Arc::new(Recorder {
            allowed: true,
            done: std::sync::Mutex::new(Vec::new()),
        });
        let ctx = RemoteCtx {
            host: rec.clone(),
            node_dir: temp_dir("write"),
        };
        let body = r#"{"request_id":"t-1","name":"studio"}"#;
        let (req, nonce) = signed_put(&key, body);
        let (resp, line) = answer_settings(&req, &key, &v, &ctx, true, "127.0.0.1", NOW);
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert!(auth::verify_response(
            &key,
            &nonce,
            &resp.body,
            sig(&resp).unwrap()
        ));
        let line = line.unwrap();
        assert!(
            line.contains(&key.fingerprint()) && line.contains("request t-1"),
            "{line}"
        );
        assert_eq!(rec.done.lock().unwrap().len(), 1);

        // The same request again: a replay.
        let (resp, line) = answer_settings(&req, &key, &v, &ctx, true, "127.0.0.1", NOW);
        assert_eq!((resp.status, line), (401, None));
        // Another cluster's key; unsigned; a v1 header.
        let (req, _) = signed_put(&ClusterKey::generate(), body);
        assert_eq!(
            answer_settings(&req, &key, &v, &ctx, true, "x", NOW)
                .0
                .status,
            401
        );
        assert_eq!(
            answer_settings(&put(body, None), &key, &v, &ctx, true, "x", NOW)
                .0
                .status,
            401
        );
        let v1 = auth::sign_request(&key, "PUT", SETTINGS_PATH, NOW, &auth::new_nonce());
        assert_eq!(
            answer_settings(&put(body, Some(v1)), &key, &v, &ctx, true, "x", NOW)
                .0
                .status,
            401
        );
        // A body swapped under a valid signature.
        let (mut req, _) = signed_put(&key, body);
        req.body = br#"{"name":"other"}"#.to_vec();
        assert_eq!(
            answer_settings(&req, &key, &v, &ctx, true, "x", NOW)
                .0
                .status,
            401
        );
        // Too old.
        let h = auth::sign_request_with_body(
            &key,
            "PUT",
            SETTINGS_PATH,
            NOW - 60_000,
            &auth::new_nonce(),
            body.as_bytes(),
        );
        assert_eq!(
            answer_settings(&put(body, Some(h)), &key, &v, &ctx, true, "x", NOW)
                .0
                .status,
            401
        );
        // Valid, but in plain v1: refused, signed, audited.
        let (req, _) = signed_put(&key, body);
        let (resp, line) = answer_settings(&req, &key, &v, &ctx, false, "127.0.0.1", NOW);
        assert_eq!(resp.status, 403);
        assert!(String::from_utf8_lossy(&resp.body).contains("tls_required"));
        assert!(line.unwrap().contains("plain v1"));
        // A refused field.
        let (req, _) = signed_put(&key, r#"{"router":"lan"}"#);
        let (resp, line) = answer_settings(&req, &key, &v, &ctx, true, "127.0.0.1", NOW);
        assert_eq!(resp.status, 400);
        assert!(String::from_utf8_lossy(&resp.body).contains("field_not_allowed"));
        assert!(line.unwrap().contains("refused field router"));
        assert_eq!(
            rec.done.lock().unwrap().len(),
            1,
            "nothing more was applied"
        );

        // The opt-in off.
        let off = RemoteCtx {
            host: Arc::new(Recorder {
                allowed: false,
                done: std::sync::Mutex::new(Vec::new()),
            }),
            node_dir: temp_dir("off"),
        };
        let (req, nonce) = signed_put(&key, body);
        let (resp, _) = answer_settings(&req, &key, &v, &off, true, "127.0.0.1", NOW);
        assert_eq!(resp.status, 403);
        assert!(String::from_utf8_lossy(&resp.body).contains("remote_settings_off"));
        assert!(auth::verify_response(
            &key,
            &nonce,
            &resp.body,
            sig(&resp).unwrap()
        ));
    }

    async fn listening(shared: &Shared) -> SocketAddr {
        for _ in 0..100 {
            let l = lock(shared).listening.clone();
            if !l.is_empty() {
                return l.parse().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("not listening");
    }

    /// Protocol v2 on loopback: a member with a certificate bound to the
    /// cluster key gets the signed report over mutual TLS; a certificate
    /// bound to another key, or none, is refused in the handshake; v1 plain
    /// still answers on the same port.
    #[tokio::test]
    async fn mutual_tls_admits_bound_certificates_only() {
        use super::super::tls::{client_config, BoundIdentity};
        let shared = super::super::shared_for_tests();
        lock(&shared).local = Some(report());
        let key = ClusterKey::generate();
        let server_id = BoundIdentity::mint(&key).unwrap();
        let config = Config {
            asked: Mode::Local,
            mode: Mode::Local,
            port: 0,
            allow: policy::Allowlist::default(),
            allow_text: Vec::new(),
            name: "studio".into(),
            problem: String::new(),
        };
        let task = tokio::spawn(serve(
            shared.clone(),
            config,
            key.clone(),
            Some(server_id),
            recorder_ctx(false),
        ));
        let addr = listening(&shared).await;
        assert!(addr.ip().is_loopback());

        // A member: bound certificate, signed request.
        let member = BoundIdentity::mint(&key).unwrap();
        let client = super::super::peers::tls_client(&member, &key).unwrap();
        let report = super::super::peers::fetch(&client, &key, "127.0.0.1", addr.port(), true)
            .await
            .expect("member gets the report");
        assert_eq!(report.name, "studio");

        // A stranger with a certificate bound to another cluster's key, who
        // even trusts our server: refused in the handshake.
        let other = ClusterKey::generate();
        let stranger = BoundIdentity::mint(&other).unwrap();
        let cfg = client_config(&stranger, &other).unwrap();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let refused = match connector.connect(name.clone(), tcp).await {
            Err(_) => true,
            // TLS 1.3: the server's verdict on the client certificate can
            // arrive after the client considers the handshake done; it must
            // then fail on the first exchange.
            Ok(mut s) => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let _ = s
                    .write_all(b"GET /v1/node-info HTTP/1.1\r\nHost: x\r\n\r\n")
                    .await;
                let mut buf = Vec::new();
                s.read_to_end(&mut buf).await.is_err() || buf.is_empty()
            }
        };
        assert!(refused, "a certificate of another cluster must be refused");

        // The other direction: a client whose own certificate the server
        // accepts, but which checks the server against another cluster's
        // key, refuses the server (a node never reports to a stranger).
        let cfg = client_config(&member, &other).unwrap();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(connector.connect(name, tcp).await.is_err());

        // v1, signed plain HTTP, still answers on the same port.
        let plain = reqwest::Client::new();
        let v1 = super::super::peers::fetch(&plain, &key, "127.0.0.1", addr.port(), false)
            .await
            .expect("v1 still answers");
        assert_eq!(v1.name, "studio");
        task.abort();
    }

    /// End to end on loopback with the node's own host: over mutual TLS a
    /// signed write changes `settings.json` through the views action layer
    /// once the opt-in is on, leaves an audit line in the node log, the
    /// router log and `router-audit.log`; with the opt-in off it is
    /// refused; plain v1 is refused.
    #[tokio::test]
    async fn a_member_changes_settings_over_mutual_tls_through_the_action_layer() {
        use super::super::tls::BoundIdentity;
        let dir = temp_dir("e2e");
        let shared = super::super::shared_for_tests();
        lock(&shared).local = Some(report());
        let key = ClusterKey::generate();
        let keys: Arc<dyn super::super::KeyStore> = Arc::new(super::super::FileKeyStore::new(&dir));
        let ctx = Arc::new(RemoteCtx {
            host: Arc::new(super::super::remote::NodeHost::new(&dir, keys)),
            node_dir: dir.clone(),
        });
        let config = Config {
            asked: Mode::Local,
            mode: Mode::Local,
            port: 0,
            allow: policy::Allowlist::default(),
            allow_text: Vec::new(),
            name: "studio".into(),
            problem: String::new(),
        };
        let task = tokio::spawn(serve(
            shared.clone(),
            config,
            key.clone(),
            Some(BoundIdentity::mint(&key).unwrap()),
            ctx,
        ));
        let addr = listening(&shared).await;
        let member = BoundIdentity::mint(&key).unwrap();
        let tls = super::super::peers::tls_client(&member, &key).unwrap();
        let send = |client: reqwest::Client, scheme: &'static str, body: &'static str| {
            let key = key.clone();
            async move {
                let nonce = auth::new_nonce();
                let h = auth::sign_request_with_body(
                    &key,
                    "PUT",
                    SETTINGS_PATH,
                    crate::status::now_ms(),
                    &nonce,
                    body.as_bytes(),
                );
                let r = client
                    .put(format!(
                        "{scheme}://127.0.0.1:{}{SETTINGS_PATH}",
                        addr.port()
                    ))
                    .header(auth::AUTH_HEADER, h)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                let status = r.status().as_u16();
                let s = r
                    .headers()
                    .get(auth::SIG_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let b = r.bytes().await.unwrap();
                assert!(auth::verify_response(&key, &nonce, &b, &s), "signed answer");
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&b).unwrap(),
                )
            }
        };
        let body = r#"{"request_id":"e2e-1","name":"remote-name","max_cpus":3}"#;
        // Off by default.
        let (status, b) = send(tls.clone(), "https", body).await;
        assert_eq!(
            (status, b["error"].as_str()),
            (403, Some("remote_settings_off"))
        );
        assert_eq!(crate::settings::Settings::load(&dir).router_name, None);
        // Allowed on the machine.
        let mut s = crate::settings::Settings::load(&dir);
        s.set(super::super::remote::OPT_IN, "true").unwrap();
        s.save(&dir).unwrap();
        let (status, b) = send(tls.clone(), "https", body).await;
        assert_eq!(status, 200, "{b}");
        let s = crate::settings::Settings::load(&dir);
        assert_eq!(s.router_name.as_deref(), Some("remote-name"));
        assert_eq!(s.max_cpus, Some(3.0));
        assert_eq!(s.router_remote_settings, Some(true));
        // The mode and the allowlist stay out of reach.
        let (status, b) = send(tls.clone(), "https", r#"{"router-allow":"10.0.0.0/8"}"#).await;
        assert_eq!((status, b["field"].as_str()), (400, Some("router-allow")));
        assert_eq!(crate::settings::Settings::load(&dir).router_allow, None);
        // Plain v1 on the same port: refused.
        let (status, b) = send(reqwest::Client::new(), "http", body).await;
        assert_eq!((status, b["error"].as_str()), (403, Some("tls_required")));
        // The audit: node log (the Logs view), router log, audit file.
        let fp = key.fingerprint();
        assert!(crate::status::logs()
            .iter()
            .any(|l| l.contains("remote settings")
                && l.contains(&fp)
                && l.contains("request e2e-1")));
        assert!(lock(&shared)
            .log
            .iter()
            .any(|l| l.contains("applied router-name=remote-name")));
        let audit = std::fs::read_to_string(dir.join(super::super::remote::AUDIT_FILE)).unwrap();
        assert_eq!(audit.lines().count(), 4, "{audit}");
        assert!(!audit.contains(&key.reveal()), "never the key");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
