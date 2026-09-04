//! The two routing proxies: one address per API that reaches the whole
//! network.
//!
//! An application on this machine points at `127.0.0.1:11440` (Ollama
//! API) or `127.0.0.1:11441` (OpenAI API) and each request goes to a node
//! that can serve it. The decision is PAIR's: **capability first, then
//! load**. A request naming a model may only go to nodes whose running
//! engine advertises that model; among those, the scheduler's order; a
//! `404` from an advertised owner means its inventory went stale and the
//! next owner is tried, while a `400` or `422` is the request's own fault
//! and is returned as is. Streaming responses pass through untouched.
//!
//! Who may ask: the machine itself on loopback, and — while the operator
//! leaves LAN ingress on — paired nodes over mutual TLS, whose certificate
//! the handshake checked against this node's pins. A peer's request is
//! **served, never re-routed**: it goes to this node's own engine or fails,
//! so a request cannot chain through a third machine. Anything else,
//! plaintext from the network included, is refused. And a request only
//! ever goes *out* to a paired node, over the same mutual TLS.
//!
//! What is logged: model, engine, origin, destination, state, time. Never a
//! prompt, never a response, and there is no flag that changes that.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use super::listen::{self, PeerInfo};
use super::{lock, schedule, Job, JobState, Shared};

/// A request from a peer carries this so the receiving proxy serves it
/// locally and never routes it on.
pub const HOP_HEADER: &str = "x-kmplify-hop";
/// The node name a peer request came from, for the jobs column.
pub const ORIGIN_HEADER: &str = "x-kmplify-origin";

/// Requests up to this size are read and forwarded; a chat with a large
/// document pasted in is normal, a gigabyte is not.
const MAX_BODY: usize = 64 * 1024 * 1024;
const UPSTREAM_CONNECT: Duration = Duration::from_secs(5);
/// Generation can legitimately take minutes; this bounds a hung upstream,
/// not a slow one.
const UPSTREAM_TOTAL: Duration = Duration::from_secs(15 * 60);
const FANOUT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// Ollama's own API (`/api/*`) plus the `/v1/*` it also serves.
    Ollama,
    /// The OpenAI shape every engine speaks.
    OpenAi,
}

impl Api {
    /// Can an engine with this id answer requests of this shape?
    pub fn accepts(self, engine_id: &str) -> bool {
        match self {
            Api::Ollama => engine_id == "ollama",
            Api::OpenAi => true,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Api::Ollama => "ollama-compatible",
            Api::OpenAi => "openai-compatible",
        }
    }
}

#[derive(Clone)]
struct Ctx {
    shared: Shared,
    api: Api,
}

pub async fn serve(shared: Shared, api: Api, port: u16) {
    let ctx = Ctx {
        shared: shared.clone(),
        api,
    };
    let app = axum::Router::new()
        .fallback(handle)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(ctx);
    let label = match api {
        Api::Ollama => "ollama-compatible proxy",
        Api::OpenAi => "openai-compatible proxy",
    };
    listen::serve(shared, port, app, label).await;
}

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(UPSTREAM_CONNECT)
            .timeout(UPSTREAM_TOTAL)
            .build()
            .unwrap_or_default()
    })
}

/// The caller, as the gate sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum Caller {
    /// This machine: may be routed anywhere.
    Local,
    /// A paired node, over mutual TLS: served here, never routed on.
    Peer,
    Refused,
}

pub fn classify(info: &PeerInfo, lan_ingress: bool) -> Caller {
    if info.is_loopback() {
        Caller::Local
    } else if lan_ingress && info.is_member() {
        Caller::Peer
    } else {
        Caller::Refused
    }
}

/// The `model` a request names, if its body is JSON with one.
pub fn model_of(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Should this status send the request to the next advertised owner?
/// `404` is a stale inventory; `5xx` and connection failures are the
/// node's problem; a `4xx` other than 404 would fail identically anywhere.
pub fn retryable(status: StatusCode) -> bool {
    status == StatusCode::NOT_FOUND || status.is_server_error()
}

fn is_listing(method: &Method, path: &str) -> bool {
    method == Method::GET && matches!(path, "/api/tags" | "/v1/models")
}

async fn handle(
    State(ctx): State<Ctx>,
    ConnectInfo(info): ConnectInfo<PeerInfo>,
    req: axum::http::Request<Body>,
) -> Response {
    let caller = classify(&info, lock(&ctx.shared).lan_ingress);
    if caller == Caller::Refused {
        return error(
            StatusCode::FORBIDDEN,
            "this proxy serves the machine it runs on and, over mutual TLS, the nodes it has paired with",
        );
    }
    let (parts, body) = req.into_parts();
    let hop = parts.headers.contains_key(HOP_HEADER);
    let origin = parts
        .headers
        .get(ORIGIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let serve_here = caller == Caller::Peer || hop;
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);

    let listing = is_listing(&parts.method, &path);
    if listing && !serve_here {
        return fan_out(&ctx, &path).await;
    }

    let model = model_of(&body);
    let now = Instant::now();

    // The failover list: (node id, engine base or None for a peer).
    let (order, self_id, self_name) = {
        let r = lock(&ctx.shared);
        let self_id = r.self_id.clone();
        let self_name = r.local().map(|n| n.name.clone()).unwrap_or_default();
        // Only this machine and the nodes it has paired with can take a
        // request: an unpaired node would refuse it anyway, and the
        // routing decision should say so up front.
        let pool: Vec<&super::Node> = if serve_here {
            r.local().into_iter().collect()
        } else {
            r.nodes
                .values()
                .filter(|n| n.online(now) && (n.is_local() || r.is_member(&n.id)))
                .collect()
        };
        let capable: Vec<String> = pool
            .iter()
            .filter(|n| match &model {
                Some(m) => n.serves(m, ctx.api).is_some(),
                None => n.running_engines().any(|e| ctx.api.accepts(&e.id)),
            })
            .map(|n| n.id.clone())
            .collect();
        let order = match &model {
            Some(_) => schedule::rank(&r, &capable, now),
            // No model to gate on (a version check, a status call): this
            // machine's own engine if it has one, else the ranked rest.
            None => {
                let mut o = schedule::rank(&r, &capable, now);
                if let Some(i) = o.iter().position(|id| id == &self_id) {
                    let me = o.remove(i);
                    o.insert(0, me);
                }
                o
            }
        };
        (order, self_id, self_name)
    };

    if order.is_empty() {
        let what = match &model {
            Some(m) if serve_here => format!("this node does not serve {m}"),
            Some(m) => format!("no paired node serves {m}"),
            None => "no engine on this node or a paired one answers this API".to_string(),
        };
        return error(StatusCode::BAD_GATEWAY, &what);
    }
    let tls_client = lock(&ctx.shared).tls_client.clone();

    let requested_from = origin.clone().unwrap_or_else(|| self_name.clone());
    let mut last_err = String::new();
    for node_id in order {
        let Some((target, engine_id, ran_on)) = target_for(&ctx, &node_id, &model, &path, &query)
        else {
            continue;
        };
        let job_id = Job::new_id(&self_id);
        // Only a request that names a model is work. A listing (a peer's
        // fan-out lands here once per refresh) and the version and health
        // calls a client makes on connect are bookkeeping, and would fill
        // the jobs column with entries nobody asked about.
        if !listing && model.is_some() {
            let mut r = lock(&ctx.shared);
            r.push_job(Job {
                id: job_id.clone(),
                model: model.clone().unwrap_or_default(),
                engine: engine_id.clone(),
                requested_from: requested_from.clone(),
                ran_on: ran_on.clone(),
                node_id: node_id.clone(),
                state: JobState::Running,
                at_ms: crate::status::now_ms(),
                error: String::new(),
                local_origin: true,
            });
        }
        let is_peer = node_id != self_id;
        let http = if is_peer {
            match &tls_client {
                Some(c) => c,
                // No certificate here means no way to reach a peer; the
                // next candidate may be this machine.
                None => continue,
            }
        } else {
            client()
        };
        let mut upstream = http
            .request(parts.method.clone(), &target)
            .headers(forward_headers(&parts.headers))
            .timeout(UPSTREAM_TOTAL)
            .body(body.clone());
        if is_peer {
            upstream = upstream
                .header(HOP_HEADER, "1")
                .header(ORIGIN_HEADER, self_name.as_str());
        }
        match upstream.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || !retryable(status) || model.is_none() {
                    return relay(&ctx, resp, job_id);
                }
                last_err = format!("{ran_on} answered {status}");
                lock(&ctx.shared).set_job_state(&job_id, JobState::Failed, last_err.clone());
            }
            Err(e) => {
                last_err = format!("{ran_on}: {e}");
                lock(&ctx.shared).set_job_state(&job_id, JobState::Failed, last_err.clone());
            }
        }
    }
    error(
        StatusCode::BAD_GATEWAY,
        &format!("every capable node failed; last: {last_err}"),
    )
}

/// Where a request for `node_id` goes: this machine's engine directly, a
/// peer's proxy over the LAN. Returns the URL, the engine id for the job
/// card, and the node name.
fn target_for(
    ctx: &Ctx,
    node_id: &str,
    model: &Option<String>,
    path: &str,
    query: &Option<String>,
) -> Option<(String, String, String)> {
    let r = lock(&ctx.shared);
    let n = r.nodes.get(node_id)?;
    let q = query.as_ref().map(|q| format!("?{q}")).unwrap_or_default();
    if n.is_local() {
        let engine = match model {
            Some(m) => n.serves(m, ctx.api)?,
            None => n.running_engines().find(|e| ctx.api.accepts(&e.id))?,
        };
        let base = engine.base.trim_end_matches('/');
        Some((
            format!("{base}{path}{q}"),
            engine.id.clone(),
            n.name.clone(),
        ))
    } else {
        let port = match ctx.api {
            Api::Ollama => n.proxy_ports.0,
            Api::OpenAi => n.proxy_ports.1,
        };
        let engine = match model {
            Some(m) => n
                .serves(m, ctx.api)
                .map(|e| e.id.clone())
                .unwrap_or_default(),
            None => String::new(),
        };
        Some((
            format!("https://{}:{port}{path}{q}", n.address),
            engine,
            n.name.clone(),
        ))
    }
}

/// Headers worth forwarding. Hop-by-hop ones and the ones the client
/// library sets itself are dropped; so are our own routing headers, which
/// are re-added only for a peer hop.
fn forward_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        let name = k.as_str();
        if matches!(
            name,
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "keep-alive"
                | "proxy-connection"
                | "upgrade"
                | "te"
        ) || name.starts_with("x-kmplify-")
        {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

/// Stream the upstream answer back. The job closes when the body does —
/// the only point at which a streamed generation is actually finished.
fn relay(ctx: &Ctx, resp: reqwest::Response, job_id: String) -> Response {
    let status = resp.status();
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        if matches!(
            k.as_str(),
            "content-length" | "connection" | "transfer-encoding" | "keep-alive"
        ) {
            continue;
        }
        headers.append(k.clone(), v.clone());
    }
    let shared = ctx.shared.clone();
    if !status.is_success() {
        lock(&shared).set_job_state(
            &job_id,
            JobState::Failed,
            format!("upstream answered {status}"),
        );
    }
    let guard = JobGuard {
        shared,
        job_id,
        outcome: if status.is_success() {
            JobState::Completed
        } else {
            JobState::Failed
        },
        error: String::new(),
    };
    let stream = resp.bytes_stream().map(move |chunk| {
        // The guard lives in this closure; dropping the stream drops it,
        // and that is when the job is over, however it ended.
        let _keep = &guard;
        chunk.map_err(std::io::Error::other)
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

struct JobGuard {
    shared: Shared,
    job_id: String,
    outcome: JobState,
    error: String,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        lock(&self.shared).set_job_state(&self.job_id, self.outcome, self.error.clone());
    }
}

/// `/api/tags` and `/v1/models`: every online node's answer, merged, so the
/// list is the network's inventory rather than one machine's. Peers are
/// asked through their proxy with the hop header, so each answers for its
/// own engines only.
async fn fan_out(ctx: &Ctx, path: &str) -> Response {
    let now = Instant::now();
    let (targets, self_name, tls_client) = {
        let r = lock(&ctx.shared);
        let mut t: Vec<(String, String, bool)> = Vec::new();
        for n in r.nodes.values().filter(|n| n.online(now)) {
            if n.is_local() {
                for e in n.running_engines().filter(|e| ctx.api.accepts(&e.id)) {
                    t.push((
                        format!("{}{path}", e.base.trim_end_matches('/')),
                        n.name.clone(),
                        false,
                    ));
                }
            } else if r.is_member(&n.id) && n.running_engines().any(|e| ctx.api.accepts(&e.id)) {
                let port = match ctx.api {
                    Api::Ollama => n.proxy_ports.0,
                    Api::OpenAi => n.proxy_ports.1,
                };
                t.push((
                    format!("https://{}:{port}{path}", n.address),
                    n.name.clone(),
                    true,
                ));
            }
        }
        (
            t,
            r.local().map(|n| n.name.clone()).unwrap_or_default(),
            r.tls_client.clone(),
        )
    };
    let fetches = targets.into_iter().map(|(url, node, peer)| {
        let self_name = self_name.clone();
        let tls_client = tls_client.clone();
        async move {
            let http = if peer { tls_client.as_ref()? } else { client() };
            let mut req = http.get(&url).timeout(FANOUT_TIMEOUT);
            if peer {
                req = req.header(HOP_HEADER, "1").header(ORIGIN_HEADER, self_name);
            }
            let v = req
                .send()
                .await
                .ok()?
                .json::<serde_json::Value>()
                .await
                .ok()?;
            Some((node, v))
        }
    });
    let answers: Vec<(String, serde_json::Value)> = futures_util::future::join_all(fetches)
        .await
        .into_iter()
        .flatten()
        .collect();
    let merged = merge_listings(path, answers);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        merged.to_string(),
    )
        .into_response()
}

/// One list from many, keyed by model name (`/api/tags`) or id
/// (`/v1/models`), first answer wins for the metadata, every owner named.
pub fn merge_listings(path: &str, answers: Vec<(String, serde_json::Value)>) -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let (list_key, id_key) = if path == "/api/tags" {
        ("models", "name")
    } else {
        ("data", "id")
    };
    let mut seen: Map<String, Value> = Map::new();
    let mut order: Vec<String> = Vec::new();
    for (node, v) in answers {
        let Some(items) = v.get(list_key).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(id) = item.get(id_key).and_then(Value::as_str) else {
                continue;
            };
            match seen.get_mut(id) {
                Some(Value::Object(existing)) => {
                    if let Some(Value::Array(nodes)) = existing.get_mut("kmplify_nodes") {
                        nodes.push(json!(node));
                    }
                }
                _ => {
                    let mut obj = item.as_object().cloned().unwrap_or_default();
                    obj.insert("kmplify_nodes".into(), json!([node]));
                    seen.insert(id.to_string(), Value::Object(obj));
                    order.push(id.to_string());
                }
            }
        }
    }
    let list: Vec<Value> = order
        .into_iter()
        .filter_map(|id| seen.remove(&id))
        .collect();
    if path == "/api/tags" {
        json!({ "models": list })
    } else {
        json!({ "object": "list", "data": list })
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "message": message, "type": "kmplify_router" } });
    let mut resp = (status, body.to_string()).into_response();
    resp.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loopback_routes_paired_nodes_are_served_everyone_else_is_refused() {
        use crate::router::listen::TlsPeer;
        let lan: std::net::SocketAddr = "192.168.1.25:4".parse().unwrap();
        let local = PeerInfo {
            addr: "127.0.0.1:4".parse().unwrap(),
            tls: None,
        };
        let local6 = PeerInfo {
            addr: "[::1]:4".parse().unwrap(),
            tls: None,
        };
        let plain = PeerInfo {
            addr: lan,
            tls: None,
        };
        let member = PeerInfo {
            addr: lan,
            tls: Some(TlsPeer {
                node_id: "n".into(),
                fingerprint: "f".into(),
            }),
        };
        let pinned_unlisted = PeerInfo {
            addr: lan,
            tls: Some(TlsPeer {
                node_id: String::new(),
                fingerprint: "f".into(),
            }),
        };
        assert_eq!(classify(&local, true), Caller::Local);
        assert_eq!(classify(&local6, false), Caller::Local);
        assert_eq!(
            classify(&plain, true),
            Caller::Refused,
            "plaintext from the network is never served"
        );
        assert_eq!(classify(&member, true), Caller::Peer);
        assert_eq!(classify(&member, false), Caller::Refused, "LAN ingress off");
        assert_eq!(classify(&pinned_unlisted, true), Caller::Refused);
    }

    #[test]
    fn the_model_is_read_from_json_and_nothing_else() {
        assert_eq!(
            model_of(br#"{"model":" qwen3 ","messages":[]}"#),
            Some("qwen3".into())
        );
        assert_eq!(model_of(br#"{"model":""}"#), None);
        assert_eq!(model_of(b"not json"), None);
        assert_eq!(model_of(br#"{"prompt":"x"}"#), None);
    }

    #[test]
    fn only_stale_inventory_and_server_faults_fail_over() {
        assert!(retryable(StatusCode::NOT_FOUND));
        assert!(retryable(StatusCode::BAD_GATEWAY));
        assert!(!retryable(StatusCode::BAD_REQUEST));
        assert!(!retryable(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(!retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn hop_by_hop_and_routing_headers_are_not_forwarded() {
        let mut h = HeaderMap::new();
        h.insert("host", "x".parse().unwrap());
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("x-kmplify-hop", "1".parse().unwrap());
        h.insert("authorization", "Bearer k".parse().unwrap());
        let out = forward_headers(&h);
        assert!(out.get("host").is_none());
        assert!(out.get("x-kmplify-hop").is_none());
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("authorization").unwrap(), "Bearer k");
    }

    #[test]
    fn listings_merge_by_name_and_remember_every_owner() {
        let a = json!({"models": [{"name": "qwen3:latest", "size": 1}, {"name": "bge-m3:567m"}]});
        let b = json!({"models": [{"name": "qwen3:latest", "size": 2}, {"name": "gemma4:latest"}]});
        let merged = merge_listings("/api/tags", vec![("king".into(), a), ("spark".into(), b)]);
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0]["name"], "qwen3:latest");
        assert_eq!(models[0]["size"], 1, "first answer keeps its metadata");
        assert_eq!(models[0]["kmplify_nodes"], json!(["king", "spark"]));
        assert_eq!(models[2]["kmplify_nodes"], json!(["spark"]));

        let o = json!({"object": "list", "data": [{"id": "m1", "object": "model"}]});
        let merged = merge_listings("/v1/models", vec![("king".into(), o)]);
        assert_eq!(merged["object"], "list");
        assert_eq!(merged["data"][0]["id"], "m1");
    }

    #[test]
    fn listings_are_recognised_by_method_and_path() {
        assert!(is_listing(&Method::GET, "/api/tags"));
        assert!(is_listing(&Method::GET, "/v1/models"));
        assert!(!is_listing(&Method::POST, "/v1/models"));
        assert!(!is_listing(&Method::GET, "/api/version"));
    }
}
