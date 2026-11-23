//! The local web dashboard: the node in a browser, on this computer only.
//!
//! `kmplify-node --ui gui` (the default when a desktop session is detected)
//! serves a small page that shows the whole node: what it is connected to,
//! what it shares, the Trusted node checklist with the exact reason and the
//! fix for every item, sessions, earnings, settings and the log.
//!
//! This is the one place the node listens, so it is locked down:
//!
//! * **Loopback only.** [`check_bind`] refuses any address that is not
//!   127.0.0.1 or ::1, so the page can never be reached from the network.
//! * **A token per start.** 32 random bytes, in the URL the node prints and
//!   opens. Every request without it is refused, the page and its script
//!   included. Compared in constant time.
//! * **No other origin.** The `Host` header must name this loopback port
//!   (DNS rebinding), a browser's `Origin` must be this page, and
//!   `Sec-Fetch-Site` must not be cross-site or same-site (another port on
//!   localhost is same-site). No CORS headers are ever sent.
//! * **Writes need more.** A POST must carry the token in a header and a JSON
//!   body: neither a link nor a cross-site form can produce that.
//! * **Nothing external.** Page, script and style are compiled into the
//!   binary; the content security policy allows this origin and nothing else.
//! * **No secrets on screen.** The credential, the colibri key and the
//!   dashboard token are never in an answer; log lines are redacted.

#[cfg(feature = "dashboard")]
pub mod http;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
#[cfg(feature = "dashboard")]
use std::sync::Arc;
#[cfg(feature = "dashboard")]
use std::time::Duration;

#[cfg(feature = "dashboard")]
use serde_json::{json, Value};

#[cfg(feature = "dashboard")]
use http::{Request, Response};

/// File in the node directory holding the dashboard URL with its token,
/// owner-only, removed when the node stops. `kmplify-node status` prints it.
pub const URL_FILE: &str = "dashboard.url";
/// The header the page sends the token in.
pub const TOKEN_HEADER: &str = "x-kmplify-node-token";
#[cfg(feature = "dashboard")]
/// At most this many requests are handled at once.
const MAX_CONNECTIONS: usize = 16;

#[cfg(feature = "dashboard")]
const INDEX_HTML: &str = include_str!("assets/index.html");
#[cfg(feature = "dashboard")]
const APP_JS: &str = include_str!("assets/app.js");
#[cfg(feature = "dashboard")]
const APP_CSS: &str = include_str!("assets/app.css");

/// How the node talks to the person at the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMode {
    /// The browser dashboard plus the terminal summary.
    Gui,
    /// The terminal summary only.
    Terminal,
    /// Plain log lines only, for services.
    None,
}

impl UiMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gui" | "web" | "browser" => Ok(UiMode::Gui),
            "terminal" | "console" | "tty" => Ok(UiMode::Terminal),
            "none" | "off" | "service" => Ok(UiMode::None),
            other => Err(format!(
                "KMPLIFY_NODE_UI (--ui) must be gui, terminal or none, not {other:?}"
            )),
        }
    }

    /// The mode for this start: the setting when given, otherwise `gui` in a
    /// desktop session and `terminal` everywhere else.
    pub fn resolve(setting: Option<&str>, desktop: bool) -> Result<Self, String> {
        match setting.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Self::parse(s),
            None if desktop => Ok(UiMode::Gui),
            None => Ok(UiMode::Terminal),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UiMode::Gui => "gui",
            UiMode::Terminal => "terminal",
            UiMode::None => "none",
        }
    }
}

/// Is a person sitting at this machine, in a desktop session, who started
/// the node interactively? A service, an SSH login or a container is not.
pub fn desktop_session() -> bool {
    use std::io::IsTerminal;
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") || set("container") {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if cfg!(target_os = "macos") {
        true
    } else if cfg!(windows) {
        // Interactive sessions have a SESSIONNAME (Console, RDP-Tcp#..);
        // services and scheduled tasks without a logon do not.
        set("SESSIONNAME")
    } else {
        set("DISPLAY") || set("WAYLAND_DISPLAY")
    }
}

/// The port setting (`KMPLIFY_NODE_UI_PORT`): empty or 0 picks a free one.
pub fn parse_port(raw: Option<&str>) -> Result<u16, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(0),
        Some(s) => s.parse::<u16>().map_err(|_| {
            format!("KMPLIFY_NODE_UI_PORT (--ui-port) must be a port number, not {s:?}")
        }),
    }
}

/// Refuse every address a remote machine could reach.
pub fn check_bind(addr: &SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "the dashboard binds to loopback only (127.0.0.1 or ::1), never {}",
            addr.ip()
        ))
    }
}

/// The address the dashboard binds: 127.0.0.1 and the given port.
pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// 32 random bytes from the OS, hex.
pub fn new_token() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Constant-time comparison, so the token cannot be guessed byte by byte
/// from response times.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What the dashboard needs from the node that started it.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub token: String,
    pub node_dir: PathBuf,
    /// The engine the node lends (`OLLAMA_BASE`).
    pub engine: String,
    pub gateway: String,
}

/// A running dashboard.
pub struct Dashboard {
    pub addr: SocketAddr,
    token: String,
    task: tokio::task::JoinHandle<()>,
}

impl Dashboard {
    /// The URL to open, token included.
    pub fn url(&self) -> String {
        format!("http://{}/?t={}", self.addr, self.token)
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

/// Built without the `dashboard` feature: there is no server to start, and
/// the caller carries on without one (the binary falls back to the terminal
/// summary). An embedder of `provider-core` never listens on any port.
#[cfg(not(feature = "dashboard"))]
pub async fn start(
    _addr: SocketAddr,
    _ctx: Ctx,
    _stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Dashboard, String> {
    Err("this build has no local dashboard (built without the `dashboard` feature)".into())
}

#[cfg(feature = "dashboard")]
/// Bind on `addr` (loopback only) and serve until `stop` turns true.
pub async fn start(
    addr: SocketAddr,
    ctx: Ctx,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Dashboard, String> {
    check_bind(&addr)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    // Belt and braces: whatever the OS made of the request, the socket that
    // is actually listening must be a loopback one.
    check_bind(&addr)?;
    let token = ctx.token.clone();
    let ctx = Arc::new(ctx);
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                a = listener.accept() => a,
                _ = stop.changed() => {
                    if *stop.borrow() {
                        return;
                    }
                    continue;
                }
            };
            let Ok((mut stream, peer)) = accepted else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            // Loopback sockets only ever see loopback peers; anything else
            // is dropped without an answer.
            if !peer.ip().is_loopback() {
                continue;
            }
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                continue;
            };
            let ctx = ctx.clone();
            let port = addr.port();
            tokio::spawn(async move {
                let _permit = permit;
                let resp = match tokio::time::timeout(
                    http::READ_DEADLINE,
                    http::read_request(&mut stream),
                )
                .await
                {
                    Ok(Ok(req)) => route(&req, &ctx, port).await,
                    Ok(Err(code)) => Response::text(code, "bad request"),
                    Err(_) => return,
                };
                http::write_response(&mut stream, &resp).await;
            });
        }
    });
    Ok(Dashboard { addr, token, task })
}

#[cfg(feature = "dashboard")]
fn allowed_hosts(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

#[cfg(feature = "dashboard")]
/// The guards, in order; `Some` is the refusal.
pub fn guard(req: &Request, token: &str, port: u16) -> Option<Response> {
    let hosts = allowed_hosts(port);
    match req.header("host") {
        Some(h) if hosts.iter().any(|a| a.eq_ignore_ascii_case(h)) => {}
        _ => {
            return Some(Response::text(
                403,
                "this dashboard answers on its loopback address only",
            ))
        }
    }
    if let Some(origin) = req.header("origin") {
        if !hosts
            .iter()
            .any(|h| origin.eq_ignore_ascii_case(&format!("http://{h}")))
        {
            return Some(Response::text(403, "requests from other sites are refused"));
        }
    }
    if let Some(site) = req.header("sec-fetch-site") {
        if site != "same-origin" && site != "none" {
            return Some(Response::text(403, "requests from other sites are refused"));
        }
    }
    if req.method != "GET" && req.method != "POST" {
        return Some(Response::text(405, "GET and POST only"));
    }
    let given = if req.method == "POST" {
        req.header(TOKEN_HEADER)
    } else {
        req.header(TOKEN_HEADER).or_else(|| req.query("t"))
    };
    if !given.is_some_and(|g| token_eq(g, token)) {
        return Some(Response::text(
            401,
            "This dashboard needs the link kmplify-node printed when it started (it carries a \
             token). Run `kmplify-node status` on this computer to see it again.",
        ));
    }
    if req.method == "POST"
        && !req
            .header("content-type")
            .is_some_and(|c| c.starts_with("application/json"))
    {
        return Some(Response::text(415, "JSON only"));
    }
    None
}

#[cfg(feature = "dashboard")]
/// Answer one request.
pub async fn route(req: &Request, ctx: &Ctx, port: u16) -> Response {
    if let Some(refused) = guard(req, &ctx.token, port) {
        return refused;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => Response::new(
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.replace("__KN_TOKEN__", &ctx.token).into_bytes(),
        ),
        ("GET", "/app.js") => Response::new(200, "text/javascript; charset=utf-8", APP_JS),
        ("GET", "/app.css") => Response::new(200, "text/css; charset=utf-8", APP_CSS),
        ("GET", "/api/state") => Response::json(200, &state(ctx)),
        ("POST", "/api/recheck") => recheck(),
        ("POST", "/api/pause") => command(crate::control::Command::Pause),
        ("POST", "/api/resume") => command(crate::control::Command::Resume),
        ("POST", "/api/reconnect") => command(crate::control::Command::Reconnect),
        ("POST", "/api/settings") => set_setting(ctx, &req.body, false),
        ("POST", "/api/settings/clear") => set_setting(ctx, &req.body, true),
        (_, p) if p.starts_with("/api/") => Response::text(404, "no such call"),
        _ => Response::text(404, "not found"),
    }
}

#[cfg(feature = "dashboard")]
fn recheck() -> Response {
    if crate::security::rechecking() {
        return Response::json(
            409,
            &json!({"started": false, "message": "a check is already running"}),
        );
    }
    tokio::spawn(async {
        if crate::security::recheck().await.is_some() {
            crate::status::push_log(
                "security baseline re-checked on request; the next heartbeat sends it",
            );
            // The gateway receives it with the next pong (pings are seconds
            // apart), then evaluates it; read its view after that.
            tokio::time::sleep(Duration::from_secs(15)).await;
            crate::trust::refresh_now();
        }
    });
    Response::json(
        202,
        &json!({"started": true, "message": "checking now; the result goes to the fabric with the next heartbeat"}),
    )
}

#[cfg(feature = "dashboard")]
fn command(cmd: crate::control::Command) -> Response {
    let message = cmd.confirmation();
    match crate::control::submit(&cmd) {
        Ok(()) => Response::json(200, &json!({"ok": true, "message": message})),
        Err(e) => Response::json(409, &json!({"ok": false, "message": e})),
    }
}

#[cfg(feature = "dashboard")]
/// The settings the dashboard may change. Each is one the node already
/// applies at runtime (`kmplify-node set` writes the same file and the
/// running node re-advertises). Everything else is shown with how to change it.
pub const EDITABLE: &[&str] = &[
    "share-inference",
    "share-cpu",
    "approval-mode",
    "country",
    "workloads",
    "max-cpus",
    "max-vram-mb",
    "max-ram-mb",
    "max-disk-gb",
];

#[cfg(feature = "dashboard")]
fn set_setting(ctx: &Ctx, body: &[u8], clear: bool) -> Response {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, &json!({"ok": false, "message": "not JSON"}));
    };
    let Some(key) = v["key"].as_str() else {
        return Response::json(400, &json!({"ok": false, "message": "no key"}));
    };
    if !EDITABLE.contains(&key) {
        return Response::json(
            403,
            &json!({"ok": false, "message": format!("{key} is not changed from the dashboard; use kmplify-node set")}),
        );
    }
    let mut stored = crate::settings::Settings::load(&ctx.node_dir);
    let result = if clear {
        stored.clear(key)
    } else {
        let value = match &v["value"] {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Response::json(400, &json!({"ok": false, "message": "no value"})),
        };
        stored.set(key, &value)
    };
    if let Err(e) = result {
        return Response::json(400, &json!({"ok": false, "message": e}));
    }
    if let Err(e) = stored.save(&ctx.node_dir) {
        return Response::json(
            500,
            &json!({"ok": false, "message": format!("cannot save: {e}")}),
        );
    }
    // No live gateway session is fine: the worker re-reads the file before
    // it dials again.
    let _ = crate::control::submit(&crate::control::Command::Reload);
    Response::json(
        200,
        &json!({"ok": true, "message": "saved; the node re-advertises within seconds"}),
    )
}

#[cfg(feature = "dashboard")]
fn opt<T: serde::Serialize>(v: Option<T>) -> Value {
    v.map(|v| json!(v)).unwrap_or(Value::Null)
}

#[cfg(feature = "dashboard")]
/// Everything the page shows, in one answer.
pub fn state(ctx: &Ctx) -> Value {
    let snap = crate::status::snapshot();
    let report = crate::security::latest();
    let fabric = crate::trust::latest();
    let now_s = crate::identity::now_s();
    let stored = crate::settings::Settings::load(&ctx.node_dir);
    let standing = crate::trust::standing(report.as_ref(), fabric.as_ref(), now_s);
    let readiness = crate::trust::readiness(
        report.as_ref(),
        fabric.as_ref(),
        &crate::security::progress(),
        now_s,
    );
    let checklist = report
        .as_ref()
        .map(|r| crate::trust::checklist(r, fabric.as_ref()))
        .unwrap_or_default();
    let b = &snap.baseline;
    let setting = |key: &str,
                   label: &str,
                   value: Value,
                   env: Value,
                   overridden: bool,
                   kind: &str,
                   var: &str| {
        json!({
            "key": key, "label": label, "value": value, "env": env,
            "overridden": overridden, "kind": kind, "var": var,
            "editable": EDITABLE.contains(&key),
            "how": format!("kmplify-node set {key}=VALUE (or {var} in the environment)"),
        })
    };
    let settings: Vec<Value> = vec![
        setting(
            "share-inference",
            "Share chat models (GPU inference)",
            json!(snap.share_inference),
            json!(b.share_inference),
            stored.share_inference.is_some(),
            "bool",
            "PROVIDER_SHARE_INFERENCE",
        ),
        setting(
            "share-cpu",
            "Lend CPU threads and RAM",
            json!(snap.share_cpu),
            json!(b.share_cpu),
            stored.share_cpu.is_some(),
            "bool",
            "PROVIDER_SHARE_CPU",
        ),
        setting(
            "workloads",
            "Serverless app templates (comma separated)",
            json!(snap.workloads.join(",")),
            json!(b.workloads.join(",")),
            stored.workloads.is_some(),
            "text",
            "PROVIDER_WORKLOADS",
        ),
        setting(
            "approval-mode",
            "Admission of new consumers",
            json!(snap.approval_mode),
            json!(b.approval_mode),
            stored.approval_mode.is_some(),
            "choice",
            "PROVIDER_APPROVAL_MODE",
        ),
        setting(
            "country",
            "Country (ISO alpha-2, self-declared)",
            json!(snap.country),
            json!(b.country),
            stored.country.is_some(),
            "text",
            "PROVIDER_COUNTRY",
        ),
        setting(
            "max-cpus",
            "Ceiling: CPU threads lent",
            opt(snap.max_cpus),
            opt(b.max_cpus),
            stored.max_cpus.is_some(),
            "number",
            "PROVIDER_MAX_CPUS",
        ),
        setting(
            "max-vram-mb",
            "Ceiling: VRAM (MB)",
            opt(snap.max_vram_mb),
            opt(b.max_vram_mb),
            stored.max_vram_mb.is_some(),
            "number",
            "PROVIDER_MAX_VRAM_MB",
        ),
        setting(
            "max-ram-mb",
            "Ceiling: RAM (MB)",
            opt(snap.max_ram_mb),
            opt(b.max_ram_mb),
            stored.max_ram_mb.is_some(),
            "number",
            "PROVIDER_MAX_RAM_MB",
        ),
        setting(
            "max-disk-gb",
            "Ceiling: disk for sessions (GB)",
            opt(snap.max_disk_gb),
            opt(b.max_disk_gb),
            stored.max_disk_gb.is_some(),
            "number",
            "PROVIDER_MAX_DISK_GB",
        ),
        {
            let mut v = setting(
                "engine",
                "Model engine (OpenAI-compatible URL)",
                json!(ctx.engine),
                Value::Null,
                stored.engine.is_some(),
                "text",
                "OLLAMA_BASE",
            );
            v["how"] = json!("kmplify-node set engine=NAME_OR_URL (ollama, lmstudio, llamacpp, ...); kmplify-node engines lists what runs here");
            v
        },
        {
            let mut v = setting(
                "gateway",
                "Fabric gateway",
                json!(snap.gateway),
                Value::Null,
                false,
                "text",
                "PROVIDER_GATEWAY_URL",
            );
            v["how"] = json!("set PROVIDER_GATEWAY_URL or --gateway, then restart the node");
            v
        },
        {
            let mut v = setting(
                "functions",
                "Host signed functions",
                json!(snap.functions_enabled),
                Value::Null,
                stored.functions.is_some(),
                "bool",
                "PROVIDER_FUNCTIONS",
            );
            v["how"] = json!("kmplify-node set functions=true functions-pubkey=HEX (the key from GET /v1/functions)");
            v
        },
        {
            let mut v = setting(
                "share-vectors",
                "Host vector collections",
                json!(snap.vectors_enabled),
                Value::Null,
                stored.share_vectors.is_some(),
                "bool",
                "PROVIDER_SHARE_VECTORS",
            );
            v["how"] = json!("kmplify-node set share-vectors=true");
            v
        },
        {
            let mut v = setting(
                "ui",
                "This dashboard",
                json!("gui"),
                Value::Null,
                false,
                "text",
                "KMPLIFY_NODE_UI",
            );
            v["how"] = json!("kmplify-node --ui gui | terminal | none, --ui-port PORT (KMPLIFY_NODE_UI, KMPLIFY_NODE_UI_PORT); restart to change");
            v
        },
    ];
    let sessions: Vec<Value> = snap
        .sessions
        .iter()
        .map(|s| {
            json!({
                "id": &s.session_id[..12.min(s.session_id.len())],
                "template": s.template,
                "state": s.state,
                "cpus": s.cpus,
                "since": s.since,
            })
        })
        .collect();
    let logs: Vec<String> = crate::status::logs()
        .iter()
        .rev()
        .take(200)
        .rev()
        .map(|l| crate::trust::redact(l))
        .collect();
    json!({
        "now_ms": crate::status::now_ms(),
        "version": snap.version,
        "os": std::env::consts::OS,
        "node": {
            "id_short": &snap.node_id[..8.min(snap.node_id.len())],
            "gateway": snap.gateway,
            "link": snap.link.label(),
            "link_detail": snap.link_detail,
            "uptime_s": snap.uptime().as_secs(),
            "connected_s": snap.connected_for().map(|d| d.as_secs()),
            "reconnects": snap.reconnects,
            "paused": snap.paused,
        },
        "machine": {
            "accelerator": snap.accelerator,
            "gpu_name": snap.gpu_name,
            "vram_total_mb": snap.vram_total_mb,
            "vram_used_mb": snap.vram_used_mb,
            "gpu_percent": snap.gpu_percent,
            "cpu_model": snap.cpu_model,
            "cpus": snap.cpus,
            "cpu_percent": snap.cpu_percent,
            "ram_total_mb": snap.ram_total_mb,
            "ram_used_mb": snap.ram_used_mb,
            "fabric_disk_mb": snap.fabric_disk_mb,
            "reserved_cpus": snap.reserved_cpus,
        },
        "sharing": {
            "inference": snap.share_inference,
            "models": snap.models,
            "models_text": crate::summary::models_text(&snap, &ctx.engine),
            "engine": ctx.engine,
            "workloads": snap.workloads,
            "cpu": snap.share_cpu,
            "functions": snap.functions_enabled,
            "vectors": snap.vectors_enabled,
            "max_cpus": snap.max_cpus,
            "max_vram_mb": snap.max_vram_mb,
            "max_ram_mb": snap.max_ram_mb,
            "max_disk_gb": snap.max_disk_gb,
            "approval_mode": snap.approval_mode,
            "country": snap.country,
        },
        "trust": {
            "standing": standing,
            "readiness": readiness,
            "checklist": checklist,
            "checked_at": report.as_ref().map(|r| r.checked_at),
            "checker_version": report.as_ref().map(|r| r.checker_version.clone()),
            "hints": report.as_ref().map(|r| r.hints.clone()).unwrap_or_default(),
            "elevated": report.as_ref().and_then(|r| r.elevated),
            "rechecking": crate::security::rechecking(),
            "fabric": fabric,
            "fabric_line": fabric.as_ref().map(crate::trust::fabric_line),
        },
        "sessions": {
            "current": sessions,
            "hosted": snap.delivered.sessions,
            "session_seconds": snap.delivered.session_seconds,
        },
        "jobs": snap.jobs,
        "delivered": snap.delivered,
        "earnings": snap.credits,
        "settings": settings,
        "logs": logs,
    })
}

/// Write the dashboard URL (with its token) where only this user can read
/// it, so `kmplify-node status` can show it again.
pub fn write_url_file(dir: &std::path::Path, url: &str) -> std::io::Result<()> {
    let path = dir.join(URL_FILE);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(url.as_bytes())
    }
    #[cfg(not(unix))]
    {
        // The node directory is per user; it inherits that ACL, like
        // status.json.
        std::fs::write(&path, url)
    }
}

/// The URL a running dashboard wrote, if any.
pub fn read_url_file(dir: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(dir.join(URL_FILE)).ok()?;
    let s = s.trim();
    (s.starts_with("http://127.0.0.1:") || s.starts_with("http://[::1]:")).then(|| s.to_string())
}

pub fn remove_url_file(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join(URL_FILE));
}

/// Open `url` in the default browser, without a shell. Never elevates.
pub fn open_browser(url: &str) -> Result<(), String> {
    // Built by proc::command like every launch in this crate: resolved to a
    // path, and no console window on Windows.
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = crate::proc::command("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = crate::proc::command("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    } else {
        let mut c = crate::proc::command("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(feature = "dashboard")]
#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        let (path, q) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.into(),
            path: path.into(),
            query: q
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (k.into(), v.into())
                })
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    const T: &str = "abc123";

    #[test]
    fn only_loopback_addresses_may_be_bound() {
        assert!(check_bind(&"127.0.0.1:0".parse().unwrap()).is_ok());
        assert!(check_bind(&"[::1]:8080".parse().unwrap()).is_ok());
        for bad in ["0.0.0.0:0", "192.168.1.5:80", "[::]:0", "10.0.0.1:9"] {
            assert!(check_bind(&bad.parse().unwrap()).is_err(), "{bad}");
        }
        assert!(loopback(0).ip().is_loopback());
    }

    #[test]
    fn the_token_is_random_long_and_compared_exactly() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, new_token());
        assert!(token_eq(&a, &a.clone()));
        assert!(!token_eq(&a, &a[..63]));
        assert!(!token_eq("", ""));
        assert!(!token_eq("abcd", "abce"));
    }

    #[test]
    fn requests_without_the_token_or_from_elsewhere_are_refused() {
        let host = ("host", "127.0.0.1:4000");
        let ok = |r: &Request| guard(r, T, 4000).is_none();
        let code = |r: &Request| guard(r, T, 4000).map(|r| r.status);
        assert!(ok(&req("GET", "/?t=abc123", &[host])));
        assert!(ok(&req("GET", "/api/state", &[host, (TOKEN_HEADER, T)])));
        assert_eq!(code(&req("GET", "/", &[host])), Some(401));
        assert_eq!(code(&req("GET", "/?t=wrong1", &[host])), Some(401));
        // DNS rebinding: a page on another name resolving to 127.0.0.1.
        assert_eq!(
            code(&req("GET", "/?t=abc123", &[("host", "evil.example:4000")])),
            Some(403)
        );
        assert_eq!(
            code(&req("GET", "/?t=abc123", &[("host", "127.0.0.1:4001")])),
            Some(403)
        );
        assert_eq!(code(&req("GET", "/?t=abc123", &[])), Some(403));
        assert_eq!(
            code(&req(
                "GET",
                "/?t=abc123",
                &[host, ("origin", "https://evil.example")]
            )),
            Some(403)
        );
        assert_eq!(
            code(&req(
                "GET",
                "/?t=abc123",
                &[host, ("sec-fetch-site", "same-site")]
            )),
            Some(403)
        );
        assert_eq!(code(&req("OPTIONS", "/", &[host])), Some(405));
        // Writes: the token only in a header, and JSON only.
        let json = ("content-type", "application/json");
        assert_eq!(
            code(&req("POST", "/api/pause?t=abc123", &[host, json])),
            Some(401)
        );
        assert_eq!(
            code(&req("POST", "/api/pause", &[host, (TOKEN_HEADER, T)])),
            Some(415)
        );
        assert!(ok(&req(
            "POST",
            "/api/pause",
            &[
                host,
                (TOKEN_HEADER, T),
                json,
                ("origin", "http://127.0.0.1:4000"),
                ("sec-fetch-site", "same-origin")
            ]
        )));
    }

    #[test]
    fn the_ui_mode_follows_the_setting_then_the_session() {
        assert_eq!(UiMode::resolve(None, true), Ok(UiMode::Gui));
        assert_eq!(UiMode::resolve(None, false), Ok(UiMode::Terminal));
        assert_eq!(UiMode::resolve(Some(""), false), Ok(UiMode::Terminal));
        assert_eq!(UiMode::resolve(Some("none"), true), Ok(UiMode::None));
        assert_eq!(UiMode::resolve(Some("GUI"), false), Ok(UiMode::Gui));
        assert!(UiMode::resolve(Some("window"), true).is_err());
        assert_eq!(parse_port(None), Ok(0));
        assert_eq!(parse_port(Some("8765")), Ok(8765));
        assert!(parse_port(Some("http")).is_err());
    }

    #[test]
    fn the_page_and_its_assets_are_self_contained() {
        for asset in [INDEX_HTML, APP_JS, APP_CSS] {
            for external in ["http://", "https://cdn", "//fonts", "<style", "onclick="] {
                assert!(!asset.contains(external), "{external} in an embedded asset");
            }
        }
        assert!(INDEX_HTML.contains("__KN_TOKEN__"));
        assert!(
            !APP_JS.contains("innerHTML"),
            "data is written with textContent only"
        );
    }
}
