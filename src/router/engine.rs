//! Engine lifecycle: install, start, stop, pull — for this machine, and
//! for a paired node that asks over mutual TLS.
//!
//! Adapted from PAIR's engine manager, with its two rules kept whole:
//!
//! - **Find before install.** An engine the operator installed themselves
//!   is used where it is; "install" on a machine that already has one
//!   downloads nothing.
//! - **Adopt, never seize.** An instance already serving its port is used
//!   as it is and reported as running, but this node did not start it, so
//!   it will not stop it or move it. Only a process this node spawned is
//!   *owned*, and only an owned process can be stopped from here.
//!
//! Ollama is installed from its official release archive into the router's
//! own directory (`<node dir>/router/engines/ollama`), extracted with the
//! `tar` every supported platform ships, and started with `OLLAMA_HOST`
//! bound to loopback so the only network-facing listener stays the proxy.
//! LM Studio is a desktop application with its own installer; once it is
//! installed, its `lms` command is what starts, stops and pulls for it.
//!
//! Every step is an [`EngineOp`] on the local node's card, with byte
//! progress where there is a size, and travels in node-info reports so a
//! paired node's window shows it too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use super::{lock, telemetry, OpState, Shared};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Install,
    Start,
    Stop,
    Pull(String),
}

impl Action {
    pub fn parse(action: &str, model: &str) -> Option<Self> {
        match action.trim().to_ascii_lowercase().as_str() {
            "install" => Some(Action::Install),
            "start" => Some(Action::Start),
            "stop" => Some(Action::Stop),
            "pull" if !model.trim().is_empty() => Some(Action::Pull(model.trim().to_string())),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Pull(_) => "pull",
        }
    }

    fn model(&self) -> &str {
        match self {
            Action::Pull(m) => m,
            _ => "",
        }
    }
}

/// Processes this node started, by engine id. Kept out of the shared
/// state because a child handle is neither Clone nor something a frame
/// should copy.
fn owned() -> &'static Mutex<HashMap<String, Child>> {
    static OWNED: OnceLock<Mutex<HashMap<String, Child>>> = OnceLock::new();
    OWNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Started here and still alive.
pub fn is_owned(id: &str) -> bool {
    let mut map = match owned().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    match map.get_mut(id) {
        Some(child) => match child.try_wait() {
            Ok(None) => true,
            _ => {
                map.remove(id);
                false
            }
        },
        None => false,
    }
}

static ENGINES_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where managed engines live. Set once at router start.
pub fn init(node_dir: &Path) {
    let _ = ENGINES_DIR.set(node_dir.join(super::cluster::DIR).join("engines"));
}

fn engines_dir() -> PathBuf {
    ENGINES_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("kmplify-node-engines"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The binary that runs an engine, wherever the operator (or this node)
/// put it. PATH first, then the places installers use, then our own.
pub fn installed_binary(id: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    match id {
        "ollama" => {
            let ours = engines_dir().join("ollama");
            candidates.push(ours.join("ollama.exe"));
            candidates.push(ours.join("bin").join("ollama"));
            candidates.push(ours.join("ollama"));
            if let Some(p) = crate::proc::find("ollama") {
                candidates.push(p);
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                candidates.push(PathBuf::from(local).join("Programs").join("Ollama").join("ollama.exe"));
            }
            candidates.push("/Applications/Ollama.app/Contents/Resources/ollama".into());
            candidates.push("/opt/homebrew/bin/ollama".into());
            candidates.push("/usr/local/bin/ollama".into());
            candidates.push("/usr/bin/ollama".into());
        }
        "lmstudio" => {
            if let Some(p) = crate::proc::find("lms") {
                candidates.push(p);
            }
            if let Some(h) = home() {
                candidates.push(h.join(".lmstudio").join("bin").join("lms.exe"));
                candidates.push(h.join(".lmstudio").join("bin").join("lms"));
                candidates.push(h.join(".cache").join("lm-studio").join("bin").join("lms"));
            }
        }
        _ => return None,
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Start an operation and return its id. The work runs on its own task;
/// the card follows it through the op.
pub fn launch(shared: Shared, engine: String, action: Action, who: String) -> u64 {
    let op = lock(&shared).op_start(&engine, action.name(), action.model());
    tokio::spawn(run(shared, engine, action, op, who));
    op
}

async fn run(shared: Shared, engine: String, action: Action, op: u64, who: String) {
    lock(&shared).push_log(format!("{who} asked: {} {engine}", action.name()));
    let result = match (engine.as_str(), &action) {
        ("ollama", Action::Install) => install_ollama(&shared, op).await,
        ("ollama", Action::Start) => start_ollama(&shared, op).await,
        ("ollama", Action::Stop) => stop_owned(&shared, "ollama").await,
        ("ollama", Action::Pull(m)) => pull_ollama(&shared, op, m).await,
        ("lmstudio", Action::Install) => Err(
            "LM Studio is a desktop application: install it from lmstudio.ai, then its `lms` \
             command lets this node start, stop and pull for it"
                .into(),
        ),
        ("lmstudio", Action::Start) => start_lmstudio(&shared).await,
        ("lmstudio", Action::Stop) => stop_lmstudio().await,
        ("lmstudio", Action::Pull(m)) => pull_lmstudio(m).await,
        _ => Err(format!("{engine} is found and used, not managed, by this node")),
    };
    let (state, message) = match &result {
        Ok(m) => (OpState::Done, m.clone()),
        Err(e) => (OpState::Failed, e.clone()),
    };
    {
        let mut r = lock(&shared);
        r.op_update(op, |o| {
            o.state = state;
            o.message = message.clone();
        });
        r.push_log(format!(
            "{} {engine}: {}",
            action.name(),
            if state == OpState::Done { "done" } else { "failed" }
        ));
    }
    // The card should not wait ten seconds to show what just happened.
    telemetry::refresh_roster(&shared).await;
}

fn base_of(shared: &Shared, id: &str) -> String {
    lock(shared)
        .local()
        .and_then(|n| n.engines.iter().find(|e| e.id == id).map(|e| e.base.clone()))
        .or_else(|| crate::engines::known(id).map(|k| k.default_base.to_string()))
        .unwrap_or_default()
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default()
    })
}

async fn answers(base: &str, path: &str) -> bool {
    http()
        .get(format!("{}{path}", base.trim_end_matches('/')))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Poll until the engine serves, up to a minute: a cold start loads
/// libraries and, on the first run, sets up its model directory.
async fn wait_ready(base: &str, path: &str) -> bool {
    for _ in 0..60 {
        if answers(base, path).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

// ------------------------------------------------------------------ ollama

/// The official release archive for this platform.
pub fn ollama_archive() -> Option<(&'static str, &'static str)> {
    const BASE: &str = "https://github.com/ollama/ollama/releases/latest/download/";
    let file = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "ollama-windows-amd64.zip",
        ("windows", "aarch64") => "ollama-windows-arm64.zip",
        ("linux", "x86_64") => "ollama-linux-amd64.tgz",
        ("linux", "aarch64") => "ollama-linux-arm64.tgz",
        ("macos", _) => "ollama-darwin.tgz",
        _ => return None,
    };
    Some((BASE, file))
}

async fn install_ollama(shared: &Shared, op: u64) -> Result<String, String> {
    if let Some(p) = installed_binary("ollama") {
        return Ok(format!("already installed at {}", p.display()));
    }
    let Some((base, file)) = ollama_archive() else {
        return Err(format!(
            "no Ollama release for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };
    let url = format!("{base}{file}");
    let dir = engines_dir();
    let downloads = dir.join("downloads");
    tokio::fs::create_dir_all(&downloads)
        .await
        .map_err(|e| format!("{}: {e}", downloads.display()))?;
    let archive = downloads.join(file);

    lock(shared).op_update(op, |o| o.message = "downloading".into());
    let resp = http()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    lock(shared).op_update(op, |o| o.total = total);
    let mut out = tokio::fs::File::create(&archive)
        .await
        .map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_report = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download: {e}"))?;
        out.write_all(&chunk).await.map_err(|e| format!("write: {e}"))?;
        done += chunk.len() as u64;
        // A progress write per chunk would lock the state thousands of
        // times a second; every few megabytes is what a bar can show.
        if done - last_report >= 4 * 1024 * 1024 {
            last_report = done;
            lock(shared).op_update(op, |o| o.done = done);
        }
    }
    out.flush().await.map_err(|e| format!("write: {e}"))?;
    drop(out);
    lock(shared).op_update(op, |o| {
        o.done = done;
        o.message = "extracting".into();
    });

    let target = dir.join("ollama");
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|e| format!("{}: {e}", target.display()))?;
    // `tar` is on every supported platform (Windows 10+ ships bsdtar, which
    // reads zip as well), so no archive crate is needed for one file.
    let status = command("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&target)
        .status()
        .await
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar exited with {status}"));
    }
    let _ = tokio::fs::remove_file(&archive).await;
    let bin = installed_binary("ollama").ok_or_else(|| {
        format!("archive extracted to {} but no ollama binary was found in it", target.display())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
    }
    Ok(format!("installed {} ({} MB)", bin.display(), done / (1024 * 1024)))
}

async fn start_ollama(shared: &Shared, op: u64) -> Result<String, String> {
    let base = base_of(shared, "ollama");
    if answers(&base, "/api/tags").await {
        return Ok(format!(
            "already serving at {base}; adopted (started outside this node, so it cannot be stopped from here)"
        ));
    }
    let bin = installed_binary("ollama").ok_or("Ollama is not installed; install it first")?;
    let host = base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
    lock(shared).op_update(op, |o| o.message = format!("starting {}", bin.display()));
    let child = command(&bin)
        .arg("serve")
        // Loopback only: the proxy is the one network-facing listener, and
        // an engine reachable from the LAN would bypass every gate.
        .env("OLLAMA_HOST", &host)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    match owned().lock() {
        Ok(mut m) => {
            m.insert("ollama".into(), child);
        }
        Err(p) => {
            p.into_inner().insert("ollama".into(), child);
        }
    }
    if wait_ready(&base, "/api/tags").await {
        Ok(format!("serving at {base}"))
    } else {
        stop_owned(shared, "ollama").await.ok();
        Err("started but did not answer within a minute; stopped again".into())
    }
}

async fn stop_owned(shared: &Shared, id: &str) -> Result<String, String> {
    let child = match owned().lock() {
        Ok(mut m) => m.remove(id),
        Err(p) => p.into_inner().remove(id),
    };
    match child {
        Some(mut child) => {
            child.start_kill().map_err(|e| format!("stop: {e}"))?;
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            Ok("stopped".into())
        }
        None => {
            let base = base_of(shared, id);
            if answers(&base, "/api/tags").await {
                Err("running, but not started by this node; stop it where it was started".into())
            } else {
                Ok("not running".into())
            }
        }
    }
}

/// `POST /api/pull` streams one JSON object per line with `status`,
/// `total` and `completed`; the last says `success`.
async fn pull_ollama(shared: &Shared, op: u64, model: &str) -> Result<String, String> {
    let base = base_of(shared, "ollama");
    if !answers(&base, "/api/tags").await {
        return Err("Ollama is not running".into());
    }
    let resp = http()
        .post(format!("{}/api/pull", base.trim_end_matches('/')))
        .json(&serde_json::json!({ "name": model, "stream": true }))
        .send()
        .await
        .map_err(|e| format!("pull: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("pull: {status} {body}"));
    }
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    let mut last_status = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("pull: {e}"))?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                return Err(format!("pull: {err}"));
            }
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let total = v.get("total").and_then(|t| t.as_u64());
            let completed = v.get("completed").and_then(|c| c.as_u64());
            if status != last_status || completed.is_some() {
                last_status = status.clone();
                lock(shared).op_update(op, |o| {
                    o.message = status.clone();
                    if let Some(t) = total {
                        o.total = t;
                    }
                    if let Some(c) = completed {
                        o.done = c;
                    }
                });
            }
            if status == "success" {
                return Ok(format!("pulled {model}"));
            }
        }
    }
    Err("pull ended without success".into())
}

// ------------------------------------------------------------------ lm studio

async fn lms(args: &[&str]) -> Result<String, String> {
    let bin = installed_binary("lmstudio")
        .ok_or("LM Studio's `lms` command was not found; install LM Studio from lmstudio.ai")?;
    let out = tokio::time::timeout(Duration::from_secs(120), command(&bin).args(args).output())
        .await
        .map_err(|_| "lms did not finish within two minutes".to_string())?
        .map_err(|e| format!("lms: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "lms {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

async fn start_lmstudio(shared: &Shared) -> Result<String, String> {
    let base = base_of(shared, "lmstudio");
    if answers(&base, "/v1/models").await {
        return Ok(format!("already serving at {base}; adopted"));
    }
    let port = base.rsplit(':').next().unwrap_or("1234").trim_end_matches('/').to_string();
    lms(&["server", "start", "--port", &port]).await?;
    if wait_ready(&base, "/v1/models").await {
        Ok(format!("serving at {base}"))
    } else {
        Err("lms reported a start but the server did not answer within a minute".into())
    }
}

/// LM Studio publishes an official stop, so an adopted instance can be
/// stopped too — the one engine where that is not a seizure.
async fn stop_lmstudio() -> Result<String, String> {
    lms(&["server", "stop"]).await.map(|_| "stopped".into())
}

async fn pull_lmstudio(model: &str) -> Result<String, String> {
    lms(&["get", "--yes", model]).await.map(|out| {
        if out.is_empty() {
            format!("downloaded {model}")
        } else {
            out.lines().last().unwrap_or("").to_string()
        }
    })
}

// ------------------------------------------------------------------ helpers

/// A tokio command that opens no console window on Windows.
fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_parse_and_a_pull_needs_a_model() {
        assert_eq!(Action::parse("install", ""), Some(Action::Install));
        assert_eq!(Action::parse(" Start ", ""), Some(Action::Start));
        assert_eq!(Action::parse("stop", ""), Some(Action::Stop));
        assert_eq!(Action::parse("pull", " qwen3:0.6b "), Some(Action::Pull("qwen3:0.6b".into())));
        assert_eq!(Action::parse("pull", ""), None);
        assert_eq!(Action::parse("uninstall", ""), None);
    }

    #[test]
    fn this_platform_has_an_ollama_archive() {
        let (base, file) = ollama_archive().expect("a supported platform");
        assert!(base.starts_with("https://github.com/ollama/ollama/"));
        assert!(file.starts_with("ollama-"));
    }

    #[test]
    fn nothing_is_owned_until_started() {
        assert!(!is_owned("ollama"));
        assert!(!is_owned("lmstudio"));
    }

    #[test]
    fn unknown_engines_have_no_binary() {
        assert!(installed_binary("vllm").is_none());
    }
}
