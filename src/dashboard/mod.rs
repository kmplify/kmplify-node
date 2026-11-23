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

/// The tiny HTTP/1.1 reader and writer the dashboard serves with; the LAN
/// router's node-info surface uses it too.
#[cfg(any(feature = "dashboard", feature = "router"))]
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
/// The KMPLIFY mark on the app-icon tile, as the tab icon. The KMPLIFY name
/// and logo are trademarks and not licensed under Apache-2.0 (TRADEMARKS.md).
#[cfg(feature = "dashboard")]
const FAVICON_SVG: &str = include_str!("assets/favicon.svg");
/// Inter, the KMPLIFY brand text face, Latin subset, variable weight. SIL Open
/// Font License 1.1: `assets/fonts/OFL.txt`, shipped beside it.
#[cfg(feature = "dashboard")]
const INTER_WOFF2: &[u8] = include_bytes!("assets/fonts/inter-latin-wght.woff2");

/// How the node talks to the person at the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMode {
    /// The browser dashboard plus the terminal summary.
    Gui,
    /// The terminal summary only.
    Terminal,
    /// Plain log lines only, for services.
    None,
    /// The terminal dashboard in this console (the terminal view).
    Tui,
    /// The app window: the native window (`native-gui`), or where that is
    /// not built in or cannot start (no display), the interim browser app
    /// window. `native` is accepted as another name for it.
    App,
}

impl UiMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gui" | "web" | "browser" => Ok(UiMode::Gui),
            "terminal" | "console" | "tty" => Ok(UiMode::Terminal),
            "none" | "off" | "service" => Ok(UiMode::None),
            "tui" | "dashboard" => Ok(UiMode::Tui),
            "app" | "native" => Ok(UiMode::App),
            other => Err(format!(
                "KMPLIFY_NODE_UI (--ui) must be tui, gui, app, native, terminal or none, not {other:?}"
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
            UiMode::Tui => "tui",
            UiMode::App => "app",
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
        // The stylesheet names the font with the token, like the page names
        // the stylesheet: every request to this server carries it.
        ("GET", "/app.css") => Response::new(
            200,
            "text/css; charset=utf-8",
            APP_CSS.replace("__KN_TOKEN__", &ctx.token).into_bytes(),
        ),
        ("GET", "/favicon.svg") => Response::new(200, "image/svg+xml", FAVICON_SVG),
        ("GET", "/inter.woff2") => Response::new(200, "font/woff2", INTER_WOFF2),
        ("GET", "/api/state") => Response::json(200, &state(ctx)),
        ("POST", "/api/recheck") => command(ctx, crate::views::Act::Recheck),
        ("POST", "/api/pause") => command(ctx, crate::views::Act::Pause),
        ("POST", "/api/resume") => command(ctx, crate::views::Act::Resume),
        ("POST", "/api/reconnect") => command(ctx, crate::views::Act::Reconnect),
        ("POST", "/api/settings") => set_setting(ctx, &req.body, false),
        ("POST", "/api/settings/clear") => set_setting(ctx, &req.body, true),
        ("POST", "/api/session/stop") => stop_session(ctx, &req.body),
        ("POST", "/api/report") => report(ctx),
        ("POST", "/api/shutdown") => command(ctx, crate::views::Act::StopNode),
        ("POST", "/api/view") => switch_view(ctx, &req.body),
        (_, p) if p.starts_with("/api/") => Response::text(404, "no such call"),
        _ => Response::text(404, "not found"),
    }
}

/// Carry out one action through the action layer every view shares
/// ([`crate::views::perform`]), and answer with its outcome.
#[cfg(feature = "dashboard")]
fn act(ctx: &Ctx, act: crate::views::Act) -> Response {
    let commanded = !matches!(
        act,
        crate::views::Act::Report
            | crate::views::Act::Setting { .. }
            | crate::views::Act::SettingClear { .. }
    );
    let o = crate::views::perform(crate::views::Target::InProcess, &ctx.node_dir, &act);
    let mut body = json!({"ok": o.ok, "message": o.message});
    if commanded {
        body["started"] = json!(o.ok);
    }
    Response::json(o.status, &body)
}

#[cfg(feature = "dashboard")]
fn command(ctx: &Ctx, a: crate::views::Act) -> Response {
    act(ctx, a)
}

/// The settings the views may change; one table for all of them.
#[cfg(feature = "dashboard")]
pub use crate::views::EDITABLE;

#[cfg(feature = "dashboard")]
fn body_str(body: &[u8], key: &str) -> Option<String> {
    let v = serde_json::from_slice::<Value>(body).ok()?;
    v[key].as_str().map(str::to_string)
}

/// `/api/session/stop {"id": session}`: the session must be one this node
/// hosts now; anything else is refused before a command is sent.
#[cfg(feature = "dashboard")]
fn stop_session(ctx: &Ctx, body: &[u8]) -> Response {
    let Some(id) = body_str(body, "id") else {
        return Response::json(400, &json!({"ok": false, "message": "no session id"}));
    };
    act(ctx, crate::views::Act::StopSession(id))
}

/// `/api/view {"view": "web" | "app" | "terminal"}`.
#[cfg(feature = "dashboard")]
fn switch_view(ctx: &Ctx, body: &[u8]) -> Response {
    match body_str(body, "view") {
        Some(v) if crate::control::VIEWS.contains(&v.as_str()) => {
            act(ctx, crate::views::Act::View(v))
        }
        _ => Response::json(
            400,
            &json!({"ok": false, "message": "view must be web, app or terminal"}),
        ),
    }
}

#[cfg(feature = "dashboard")]
fn report(ctx: &Ctx) -> Response {
    act(ctx, crate::views::Act::Report)
}

#[cfg(feature = "dashboard")]
fn set_setting(ctx: &Ctx, body: &[u8], clear: bool) -> Response {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, &json!({"ok": false, "message": "not JSON"}));
    };
    let Some(key) = v["key"].as_str() else {
        return Response::json(400, &json!({"ok": false, "message": "no key"}));
    };
    let key = key.to_string();
    if clear {
        return act(ctx, crate::views::Act::SettingClear { key });
    }
    if !EDITABLE.contains(&key.as_str()) {
        // Refused before the value is even looked at, as it always was.
        return act(
            ctx,
            crate::views::Act::Setting {
                key,
                value: String::new(),
            },
        );
    }
    let value = match &v["value"] {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Response::json(400, &json!({"ok": false, "message": "no value"})),
    };
    act(ctx, crate::views::Act::Setting { key, value })
}

/// Everything the page shows, in one answer: [`crate::views::model`] of this
/// process, the value the terminal view renders too.
#[cfg(feature = "dashboard")]
pub fn state(ctx: &Ctx) -> Value {
    let snap = crate::status::snapshot();
    let stored = crate::settings::Settings::load(&ctx.node_dir);
    crate::views::model(&crate::views::Source {
        snap: &snap,
        stored: &stored,
        engine: &ctx.engine,
        logs: crate::status::logs(),
        rechecking: crate::security::rechecking(),
        view: crate::control::view_state(),
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

/// Chromium-family browsers that can show a page as an app window
/// (`--app=URL`: no tabs, no address bar), in the order they are tried.
fn app_browsers() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut out: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        // Edge ships with every Windows 10 and 11, so this always finds one.
        for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                out.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
                out.push(base.join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
        for root in roots {
            for (app, bin) in [
                ("Google Chrome.app", "Google Chrome"),
                ("Microsoft Edge.app", "Microsoft Edge"),
                ("Chromium.app", "Chromium"),
                ("Brave Browser.app", "Brave Browser"),
            ] {
                out.push(root.join(app).join("Contents/MacOS").join(bin));
            }
        }
    } else {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            if let Some(p) = crate::proc::find(name) {
                out.push(p);
            }
        }
    }
    out.into_iter().filter(|p| p.is_file()).collect()
}

/// Open the dashboard in an app window of its own.
///
/// The FALLBACK of the app view: the native window (`crate::native`) is
/// tried first, and this is used where it is not built in or cannot start.
/// A Chromium-family browser in app mode (`--app=URL`).
/// The URL goes over as one argument, never through a shell; it carries the
/// per-start token, which is therefore visible in this user's process list,
/// exactly as it is when the default browser is started with it. Without
/// such a browser the normal one opens instead, and the answer says so.
pub fn open_app_window(url: &str) -> Result<String, String> {
    if let Some(browser) = app_browsers().into_iter().next() {
        let spawned = crate::proc::command(&browser)
            .arg(format!("--app={url}"))
            .arg("--new-window")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return Ok("opened the app window".into());
        }
    }
    open_browser(url).map(|()| {
        "no Edge, Chrome or Chromium found for an app window; opened your browser instead".into()
    })
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
        // The native window is the app view; `native` names it too.
        assert_eq!(UiMode::resolve(Some("native"), false), Ok(UiMode::App));
        assert_eq!(UiMode::resolve(Some("app"), false), Ok(UiMode::App));
        assert_eq!(parse_port(None), Ok(0));
        assert_eq!(parse_port(Some("8765")), Ok(8765));
        assert!(parse_port(Some("http")).is_err());
    }

    /// The view parity contract (`crate::views`), web side: every section is
    /// a tab, every metric is rendered by the page (or carried by the model
    /// it renders), every action has a control and a route.
    #[test]
    fn the_web_page_keeps_the_view_contract() {
        use crate::views::{ACTIONS, METRICS, SECTIONS};
        const SRC: &str = include_str!("mod.rs");
        for s in SECTIONS {
            assert!(
                INDEX_HTML.contains(&format!("data-tab=\"{}\"", s.id))
                    && INDEX_HTML.contains(&format!(">{}</span>", s.title)),
                "section {} has no tab",
                s.id
            );
        }
        let model = crate::views::model(&crate::views::Source {
            snap: &crate::status::Snapshot::default(),
            stored: &crate::settings::Settings::default(),
            engine: "",
            logs: Vec::new(),
            rechecking: false,
            view: crate::control::ViewState::default(),
        });
        let setting_labels: Vec<String> = model["settings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap().to_string())
            .collect();
        for m in METRICS {
            assert!(
                APP_JS.contains(&format!("\"{}", m.label))
                    || setting_labels.iter().any(|l| l == m.label),
                "the page does not show {:?} ({})",
                m.label,
                m.section
            );
        }
        for a in ACTIONS {
            assert!(
                SRC.contains(&format!("(\"POST\", \"{}\")", a.web)),
                "no route for {} ({})",
                a.id,
                a.web
            );
            let offered = match a.id {
                "view_web" => INDEX_HTML.contains("data-view=\"web\""),
                "view_app" => INDEX_HTML.contains("data-view=\"app\""),
                "view_terminal" => INDEX_HTML.contains("data-view=\"terminal\""),
                // A control per editable setting: Save, Turn on/off, a choice.
                "setting" => APP_JS.contains("act(\"/api/settings\""),
                _ => APP_JS.contains(&format!("\"{}\"", a.label)) && APP_JS.contains(a.web),
            };
            assert!(offered, "the page offers no control for {}", a.id);
        }
    }

    #[test]
    fn the_page_and_its_assets_are_self_contained() {
        for asset in [INDEX_HTML, APP_JS, APP_CSS] {
            for external in ["http://", "https://cdn", "//fonts", "<style", "onclick="] {
                assert!(!asset.contains(external), "{external} in an embedded asset");
            }
        }
        assert!(INDEX_HTML.contains("__KN_TOKEN__"));
        assert!(APP_CSS.contains("/inter.woff2?t=__KN_TOKEN__"));
        // The icon and the font are this crate's own bytes.
        assert!(FAVICON_SVG.starts_with("<svg") && !FAVICON_SVG.contains("href"));
        assert_eq!(&INTER_WOFF2[..4], b"wOF2");
        assert!(
            !APP_JS.contains("innerHTML"),
            "data is written with textContent only"
        );
    }
}
