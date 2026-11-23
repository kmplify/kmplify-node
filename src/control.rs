//! Operator commands: the half of the dashboard that acts instead of watching.
//!
//! A headless provider still needs a hand on the switch — pause sharing before
//! a game, evict a peer's container, reconnect after fixing a gateway URL, stop
//! the node. On a desktop that is a button; here it is `kmplify-node tui`, and
//! it has to work in the two shapes a node actually runs in:
//!
//! * **the dashboard IS the node** (`kmplify-node tui` with nothing else
//!   running) — commands go straight into the worker's control channel;
//! * **the dashboard ATTACHES to a service-managed node** (the systemd or
//!   Docker case, which is most of them) — the two are separate processes, so
//!   the command has to cross the gap.
//!
//! It crosses as a file. This crate promises that a node never listens on a
//! port, and a control socket would be exactly that with a friendlier name, so
//! the attached dashboard drops one small JSON file into `control/` inside the
//! node directory and the running node picks it up within half a second and
//! deletes it. The directory is owner-only; anyone who can write there can
//! already read the node's gateway token, so it grants no new authority.
//!
//! Both paths end in the same place — [`submit`] on the running node — so a
//! command cannot mean one thing locally and another remotely.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Directory inside the node dir where pending commands land.
pub const CONTROL_DIR: &str = "control";

/// How often a running node looks for a dropped command file. Half a second
/// is below the threshold where a keypress feels ignored, and it is one
/// `readdir` of an almost always empty directory.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// A command with no arguments beyond its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Advertise nothing until resumed. The connection stays up, so the node
    /// keeps its place in the registry and its hosted sessions keep running.
    Pause,
    Resume,
    /// Drop the gateway connection and dial again immediately.
    Reconnect,
    /// Re-read the sharing settings and re-advertise with them.
    Reload,
    /// End one peer session running on this machine.
    StopSession(String),
    /// Graceful shutdown: tear down hosted sessions, then exit.
    Shutdown,
    /// Run the trusted node security self-check now. The result goes to the
    /// fabric, signed, with the next heartbeat.
    Recheck,
    /// Bring up one of the views of this node: `web` (the dashboard in the
    /// browser), `app` (the dashboard in an app window) or `terminal` (the
    /// terminal dashboard in the node's console). Carried out by the
    /// process that serves the node (`main::serve`), never by the worker.
    View(String),
}

/// The views a person can switch between while the node runs.
pub const VIEWS: [&str; 3] = ["web", "app", "terminal"];

impl Command {
    pub fn as_frame(&self) -> Value {
        match self {
            Command::Pause => json!({"type": "node_pause"}),
            Command::Resume => json!({"type": "node_resume"}),
            Command::Reconnect => json!({"type": "node_reconnect"}),
            Command::Reload => json!({"type": "node_reload"}),
            Command::StopSession(id) => json!({"type": "workload_stop", "session": id}),
            Command::Shutdown => json!({"type": "node_shutdown"}),
            Command::Recheck => json!({"type": "node_recheck"}),
            Command::View(v) => json!({"type": "node_view", "view": v}),
        }
    }

    pub fn from_frame(v: &Value) -> Option<Self> {
        match v["type"].as_str()? {
            "node_pause" => Some(Command::Pause),
            "node_resume" => Some(Command::Resume),
            "node_reconnect" => Some(Command::Reconnect),
            "node_reload" => Some(Command::Reload),
            "node_shutdown" => Some(Command::Shutdown),
            "node_recheck" => Some(Command::Recheck),
            "node_view" => {
                let v = v["view"].as_str()?;
                VIEWS.contains(&v).then(|| Command::View(v.to_string()))
            }
            "workload_stop" => Some(Command::StopSession(
                v["session"].as_str().unwrap_or_default().to_string(),
            )),
            _ => None,
        }
    }

    /// What to tell the operator once it has been sent.
    pub fn confirmation(&self) -> String {
        match self {
            Command::Pause => "paused — advertising no models".into(),
            Command::Resume => "resumed — advertising models again".into(),
            Command::Reconnect => "reconnecting…".into(),
            Command::Reload => "settings saved — re-advertising".into(),
            Command::StopSession(id) => format!("stopping session {}…", short(id)),
            Command::Shutdown => "shutting down…".into(),
            Command::Recheck => {
                "checking now; the result goes to the fabric with the next heartbeat".into()
            }
            Command::View(v) => match v.as_str() {
                "web" => "opening the web view in your browser".into(),
                "app" => "opening the app window".into(),
                _ => "the terminal view is up in the console window kmplify-node runs in \
                      (where you started it); press q there to leave it, the node keeps running"
                    .into(),
            },
        }
    }
}

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}

pub fn control_dir(node_dir: &Path) -> PathBuf {
    node_dir.join(CONTROL_DIR)
}

// ------------------------------------------------------------------ views

static VIEW_SWITCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VIEW_CONSOLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VIEW_TERMINAL_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VIEW_START: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Where the views of this process stand, for the dashboard and the
/// terminal view alike.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ViewState {
    /// Views can be switched: a person started this node interactively.
    /// A service, a container or a redirected run never switches.
    pub switchable: bool,
    /// The node has a console the terminal view can come up in.
    pub console: bool,
    /// The terminal view is on screen now.
    pub terminal_up: bool,
    /// The view this start began with (`--ui`, the chooser, or the default).
    pub start: String,
}

/// Declared by the process serving the node, once, at start.
pub fn set_view_host(switchable: bool, console: bool, start: &str) {
    use std::sync::atomic::Ordering;
    VIEW_SWITCH.store(switchable, Ordering::Relaxed);
    VIEW_CONSOLE.store(console, Ordering::Relaxed);
    if let Ok(mut s) = VIEW_START.lock() {
        *s = start.to_string();
    }
}

/// The terminal view came up or went away.
pub fn set_terminal_up(up: bool) {
    VIEW_TERMINAL_UP.store(up, std::sync::atomic::Ordering::Relaxed);
}

pub fn terminal_up() -> bool {
    VIEW_TERMINAL_UP.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn view_state() -> ViewState {
    use std::sync::atomic::Ordering;
    ViewState {
        switchable: VIEW_SWITCH.load(Ordering::Relaxed),
        console: VIEW_CONSOLE.load(Ordering::Relaxed),
        terminal_up: terminal_up(),
        start: VIEW_START.lock().map(|s| s.clone()).unwrap_or_default(),
    }
}

/// Start the security self-check in this process, unless one runs.
fn start_recheck() -> Result<(), String> {
    if crate::security::rechecking() {
        return Err("a check is already running".into());
    }
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "the node is not running, so there is nothing to check".to_string())?;
    rt.spawn(async {
        if crate::security::recheck().await.is_some() {
            crate::status::push_log(
                "security baseline re-checked on request; the next heartbeat sends it",
            );
            // The gateway receives it with the next pong (pings are seconds
            // apart), then evaluates it; read its view after that.
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            crate::trust::refresh_now();
        }
    });
    Ok(())
}

/// Hand a command to the worker running in THIS process.
pub fn submit(cmd: &Command) -> Result<(), String> {
    // Pause is STATE, not a message. Applied here it holds whether or not a
    // gateway connection happens to exist right now: pausing a node that is
    // between reconnects used to be swallowed, and it came back sharing.
    match cmd {
        Command::Pause => crate::status::set_paused(true),
        Command::Resume => crate::status::set_paused(false),
        Command::Recheck => return start_recheck(),
        Command::View(v) => {
            let st = view_state();
            if !st.switchable {
                return Err(
                    "this node runs without a person at its console (a service or a \
                            redirected start), so it opens no views; open the dashboard link \
                            yourself"
                        .into(),
                );
            }
            if v == "terminal" && !st.console {
                return Err("this node has no console to show the terminal view in".into());
            }
        }
        _ => {}
    }
    let delivered = crate::fabric_worker::send_control(cmd.as_frame());
    match cmd {
        // A live session withdraws the advertised model list on top of the
        // flag; with no session there is nothing advertised to withdraw, so
        // the flag alone is the whole job and this is not a failure.
        Command::Pause | Command::Resume => Ok(()),
        _ => delivered,
    }
}

/// Leave a command for the node running in ANOTHER process.
///
/// Written to a unique name and never overwritten, so two commands issued in
/// the same instant both survive; the node deletes each one as it applies it.
pub fn request(node_dir: &Path, cmd: &Command) -> Result<(), String> {
    let dir = control_dir(node_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    restrict_dir(&dir);
    // Unique without a random source: the clock, this process, and a counter
    // that makes two commands in the same millisecond distinct.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!(
        "{:013}-{}-{}.json",
        crate::status::now_ms(),
        std::process::id(),
        n
    );
    let body = serde_json::to_vec(&cmd.as_frame()).map_err(|e| e.to_string())?;
    // Write beside the target and rename in, so the poller never reads a
    // half-written command.
    let tmp = dir.join(format!(".{name}"));
    std::fs::write(&tmp, &body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(&name)).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

/// Apply commands other processes drop into `control/`, until stopped.
///
/// Deletes each file BEFORE acting on it: a command that somehow panics the
/// handler must not be replayed on every tick forever.
pub async fn watch(node_dir: PathBuf, mut stop: tokio::sync::watch::Receiver<bool>) {
    let dir = control_dir(&node_dir);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return;
                }
            }
        }
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        let mut pending: Vec<PathBuf> = Vec::new();
        while let Ok(Some(e)) = entries.next_entry().await {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                pending.push(path);
            }
        }
        // Name-sorted: the names lead with a zero-padded millisecond stamp,
        // so this is issue order, and "pause then resume" cannot land as
        // "resume then pause".
        pending.sort();
        for path in pending {
            let body = tokio::fs::read(&path).await;
            let _ = tokio::fs::remove_file(&path).await;
            let Ok(body) = body else { continue };
            let Ok(frame) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            let Some(cmd) = Command::from_frame(&frame) else {
                continue;
            };
            if let Err(e) = submit(&cmd) {
                crate::status::push_log(format!("control: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_survives_the_wire() {
        for cmd in [
            Command::Pause,
            Command::Resume,
            Command::Reconnect,
            Command::Reload,
            Command::Shutdown,
            Command::StopSession("s-1".into()),
            Command::Recheck,
            Command::View("web".into()),
            Command::View("app".into()),
            Command::View("terminal".into()),
        ] {
            assert_eq!(Command::from_frame(&cmd.as_frame()), Some(cmd));
        }
    }

    #[test]
    fn a_stop_session_frame_is_the_one_the_worker_already_understood() {
        // The owner's stop button predates this module; its frame shape is
        // load-bearing in the session loop and must not drift.
        let f = Command::StopSession("abc".into()).as_frame();
        assert_eq!(f["type"], "workload_stop");
        assert_eq!(f["session"], "abc");
    }

    #[test]
    fn unknown_frames_are_not_commands() {
        assert_eq!(Command::from_frame(&json!({"type": "ping"})), None);
        assert_eq!(Command::from_frame(&json!({})), None);
        // A view nobody can show is not a command.
        assert_eq!(
            Command::from_frame(&json!({"type": "node_view", "view": "calc.exe"})),
            None
        );
        assert_eq!(Command::from_frame(&json!({"type": "node_view"})), None);
    }

    #[test]
    fn a_view_switch_needs_a_person_at_the_console() {
        // Never declared interactive (a service): no switch, no frame sent.
        set_view_host(false, false, "none");
        assert!(submit(&Command::View("web".into())).is_err());
        set_view_host(true, false, "web");
        assert!(submit(&Command::View("terminal".into())).is_err());
        set_view_host(false, false, "");
    }

    #[test]
    fn a_request_lands_as_one_readable_file() {
        let dir = std::env::temp_dir().join(format!("kmplify-node-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        request(&dir, &Command::Pause).unwrap();
        let files: Vec<_> = std::fs::read_dir(control_dir(&dir))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(files.len(), 1);
        let body: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(Command::from_frame(&body), Some(Command::Pause));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
