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

/// Subdirectory of it where router orders land. The node process drains
/// `control/` itself, and the router can live in a different process (a
/// window attached to a headless node), so its orders wait where only the
/// process running the router looks.
pub const ROUTER_CONTROL_DIR: &str = "router";

/// How often a running node looks for a dropped command file. Half a second
/// is below the threshold where a keypress feels ignored, and it is one
/// `readdir` of an almost always empty directory.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// An order for the LAN router (docs/ROUTER.md), from a screen in another
/// process. Plain data here so the file format exists in every build; the
/// `router` feature is what can act on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterCommand {
    Invite,
    CancelInvite,
    Join {
        address: String,
        pin: String,
    },
    AddNode {
        address: String,
    },
    ForgetNode {
        address: String,
    },
    RemoveMember {
        id: String,
    },
    Leave,
    Ingress {
        on: bool,
    },
    /// `node` empty means this machine; otherwise a paired node's id.
    Engine {
        node: String,
        engine: String,
        action: String,
        model: String,
    },
}

impl RouterCommand {
    pub fn as_frame(&self) -> Value {
        match self {
            RouterCommand::Invite => json!({"type": "router_invite"}),
            RouterCommand::CancelInvite => json!({"type": "router_cancel_invite"}),
            RouterCommand::Join { address, pin } => {
                json!({"type": "router_join", "address": address, "pin": pin})
            }
            RouterCommand::AddNode { address } => {
                json!({"type": "router_add_node", "address": address})
            }
            RouterCommand::ForgetNode { address } => {
                json!({"type": "router_forget_node", "address": address})
            }
            RouterCommand::RemoveMember { id } => json!({"type": "router_remove_member", "id": id}),
            RouterCommand::Leave => json!({"type": "router_leave"}),
            RouterCommand::Ingress { on } => json!({"type": "router_ingress", "on": on}),
            RouterCommand::Engine {
                node,
                engine,
                action,
                model,
            } => {
                json!({"type": "router_engine", "node": node, "engine": engine, "action": action, "model": model})
            }
        }
    }

    pub fn from_frame(v: &Value) -> Option<Self> {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Some(match v["type"].as_str()? {
            "router_invite" => RouterCommand::Invite,
            "router_cancel_invite" => RouterCommand::CancelInvite,
            "router_join" => RouterCommand::Join {
                address: s("address"),
                pin: s("pin"),
            },
            "router_add_node" => RouterCommand::AddNode {
                address: s("address"),
            },
            "router_forget_node" => RouterCommand::ForgetNode {
                address: s("address"),
            },
            "router_remove_member" => RouterCommand::RemoveMember { id: s("id") },
            "router_leave" => RouterCommand::Leave,
            "router_ingress" => RouterCommand::Ingress {
                on: v["on"].as_bool().unwrap_or(true),
            },
            "router_engine" => RouterCommand::Engine {
                node: s("node"),
                engine: s("engine"),
                action: s("action"),
                model: s("model"),
            },
            _ => return None,
        })
    }

    /// What to tell the operator once it has been left for the router.
    pub fn confirmation(&self) -> String {
        match self {
            RouterCommand::Invite => "invitation requested — the PIN appears in a moment".into(),
            RouterCommand::CancelInvite => "invitation cancelled".into(),
            RouterCommand::Join { address, .. } => format!("pairing with {address}…"),
            RouterCommand::AddNode { address } => format!("adding {address}…"),
            RouterCommand::ForgetNode { address } => format!("forgetting {address}"),
            RouterCommand::RemoveMember { id } => format!("removing {}…", &id[..8.min(id.len())]),
            RouterCommand::Leave => "leaving the cluster…".into(),
            RouterCommand::Ingress { on } => if *on {
                "serving paired nodes"
            } else {
                "not serving paired nodes"
            }
            .into(),
            RouterCommand::Engine { engine, action, .. } => format!("{action} {engine}…"),
        }
    }
}

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
    /// An order for the LAN router running in the node's process.
    Router(RouterCommand),
}

impl Command {
    pub fn as_frame(&self) -> Value {
        match self {
            Command::Pause => json!({"type": "node_pause"}),
            Command::Resume => json!({"type": "node_resume"}),
            Command::Reconnect => json!({"type": "node_reconnect"}),
            Command::Reload => json!({"type": "node_reload"}),
            Command::StopSession(id) => json!({"type": "workload_stop", "session": id}),
            Command::Shutdown => json!({"type": "node_shutdown"}),
            Command::Router(rc) => rc.as_frame(),
        }
    }

    pub fn from_frame(v: &Value) -> Option<Self> {
        match v["type"].as_str()? {
            "node_pause" => Some(Command::Pause),
            "node_resume" => Some(Command::Resume),
            "node_reconnect" => Some(Command::Reconnect),
            "node_reload" => Some(Command::Reload),
            "node_shutdown" => Some(Command::Shutdown),
            "workload_stop" => Some(Command::StopSession(
                v["session"].as_str().unwrap_or_default().to_string(),
            )),
            t if t.starts_with("router_") => RouterCommand::from_frame(v).map(Command::Router),
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
            Command::Router(rc) => rc.confirmation(),
        }
    }
}

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}

pub fn control_dir(node_dir: &Path) -> PathBuf {
    node_dir.join(CONTROL_DIR)
}

pub fn router_control_dir(node_dir: &Path) -> PathBuf {
    control_dir(node_dir).join(ROUTER_CONTROL_DIR)
}

/// Hand a command to the worker running in THIS process.
pub fn submit(cmd: &Command) -> Result<(), String> {
    // Router orders go to the router in this process, never to the worker.
    if let Command::Router(rc) = cmd {
        #[cfg(feature = "router")]
        {
            return crate::router::snapshot::apply(rc.clone()).map(|msg| {
                crate::status::push_log(format!("router: {msg}"));
            });
        }
        #[cfg(not(feature = "router"))]
        {
            let _ = rc;
            return Err("this build has no LAN router".into());
        }
    }
    // Pause is STATE, not a message. Applied here it holds whether or not a
    // gateway connection happens to exist right now: pausing a node that is
    // between reconnects used to be swallowed, and it came back sharing.
    match cmd {
        Command::Pause => crate::status::set_paused(true),
        Command::Resume => crate::status::set_paused(false),
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

/// Leave a command for the node — or, for a router order, the router —
/// running in ANOTHER process.
///
/// Written to a unique name and never overwritten, so two commands issued in
/// the same instant both survive; the reader deletes each one as it applies it.
pub fn request(node_dir: &Path, cmd: &Command) -> Result<(), String> {
    let dir = match cmd {
        Command::Router(_) => router_control_dir(node_dir),
        _ => control_dir(node_dir),
    };
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
        for cmd in take_pending(&dir).await {
            if let Err(e) = submit(&cmd) {
                crate::status::push_log(format!("control: {e}"));
            }
        }
    }
}

/// Apply router orders other processes drop into `control/router/`, for as
/// long as this process runs the router. Started by `router::spawn`, so a
/// headless `run --router` node and a window each answer for their own.
#[cfg(feature = "router")]
pub async fn watch_router(node_dir: PathBuf) {
    let dir = router_control_dir(&node_dir);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        for cmd in take_pending(&dir).await {
            if let Err(e) = submit(&cmd) {
                crate::status::push_log(format!("router: {e}"));
            }
        }
    }
}

/// Every command file in `dir`, in issue order, each deleted as it is read.
async fn take_pending(dir: &Path) -> Vec<Command> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
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
    let mut commands = Vec::new();
    for path in pending {
        let body = tokio::fs::read(&path).await;
        let _ = tokio::fs::remove_file(&path).await;
        let Ok(body) = body else { continue };
        let Ok(frame) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        if let Some(cmd) = Command::from_frame(&frame) {
            commands.push(cmd);
        }
    }
    commands
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
            Command::Router(RouterCommand::Invite),
            Command::Router(RouterCommand::CancelInvite),
            Command::Router(RouterCommand::Join {
                address: "10.0.0.5:14418".into(),
                pin: "123456".into(),
            }),
            Command::Router(RouterCommand::AddNode {
                address: "10.0.0.6".into(),
            }),
            Command::Router(RouterCommand::ForgetNode {
                address: "10.0.0.6".into(),
            }),
            Command::Router(RouterCommand::RemoveMember { id: "abc".into() }),
            Command::Router(RouterCommand::Leave),
            Command::Router(RouterCommand::Ingress { on: false }),
            Command::Router(RouterCommand::Engine {
                node: String::new(),
                engine: "ollama".into(),
                action: "pull".into(),
                model: "qwen3:0.6b".into(),
            }),
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

    #[test]
    fn a_router_order_waits_where_the_node_never_looks() {
        // The node drains control/*.json; an order for a router in another
        // process must not be eaten (or refused) by it.
        let dir = std::env::temp_dir().join(format!("kmplify-node-rctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        request(&dir, &Command::Router(RouterCommand::Invite)).unwrap();
        let in_node_dir = std::fs::read_dir(control_dir(&dir))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .count();
        assert_eq!(in_node_dir, 0);
        let in_router_dir = std::fs::read_dir(router_control_dir(&dir))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .count();
        assert_eq!(in_router_dir, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
