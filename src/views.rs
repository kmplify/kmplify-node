//! One feature set for every view of a node.
//!
//! A node is watched and run from three places: the terminal view
//! (`kmplify-node tui`, or the terminal dashboard the node brings up in its
//! own console), the web dashboard in the browser, and the same dashboard in
//! an app window. Owner rule (2026-11-23): all views carry the same
//! information, metrics and options. This module is how that holds:
//!
//! * [`model`] is the ONE data layer. The web dashboard answers `/api/state`
//!   with it, and the terminal view renders its Trusted node, Earnings and
//!   Settings screens from the very same value, built from the same
//!   [`crate::status::Snapshot`] (in this process, or the one a running node
//!   publishes).
//! * Every action goes through [`crate::control::Command`] (in-process
//!   `submit`, or `request` from another process) and the settings file, so
//!   the web buttons and the terminal keys hit the same handlers.
//! * [`SECTIONS`], [`METRICS`] and [`ACTIONS`] are the contract. A parity test
//!   on each side (`tests` in `dashboard` for the web page, in the binary's
//!   `tui` for the terminal) fails when a view drops one of them.
//!
//! The same table is the contract for the native window that will replace
//! the interim app window, and for the provider view of kmplify-desktop.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::settings::Settings;
use crate::status::Snapshot;

/// A section: a tab of the web dashboard, a screen of the terminal view.
#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    /// The keys that open it in the terminal view.
    pub tui_keys: &'static str,
}

pub const SECTIONS: &[Section] = &[
    Section {
        id: "overview",
        title: "Overview",
        tui_keys: "1 h",
    },
    Section {
        id: "trust",
        title: "Trusted node",
        tui_keys: "8 n",
    },
    Section {
        id: "sessions",
        title: "Sessions",
        tui_keys: "2 s",
    },
    Section {
        id: "earnings",
        title: "Earnings",
        tui_keys: "9 $",
    },
    Section {
        id: "settings",
        title: "Settings",
        tui_keys: "0 5 g",
    },
    Section {
        id: "logs",
        title: "Logs",
        tui_keys: "4 l",
    },
];

/// A metric or panel every view shows in that section, by the words it
/// carries (compared without case: the terminal titles its panels in lower
/// case).
#[derive(Clone, Copy, Debug)]
pub struct Metric {
    pub section: &'static str,
    pub label: &'static str,
    /// How the terminal shows what the web page draws as a chart, when it
    /// differs.
    pub terminal_form: &'static str,
}

pub const METRICS: &[Metric] = &[
    Metric {
        section: "overview",
        label: "Trusted node",
        terminal_form: "",
    },
    Metric {
        section: "overview",
        label: "Trust readiness",
        terminal_form: "a text bar [#####.] and n/6 instead of the ring",
    },
    Metric {
        section: "overview",
        label: "What this node shares",
        terminal_form: "",
    },
    Metric {
        section: "overview",
        label: "Connection",
        terminal_form: "",
    },
    Metric {
        section: "overview",
        label: "This machine",
        terminal_form: "",
    },
    Metric {
        section: "overview",
        label: "CPU",
        terminal_form: "a block bar with a one-minute sparkline",
    },
    Metric {
        section: "overview",
        label: "RAM",
        terminal_form: "a block bar with a one-minute sparkline",
    },
    Metric {
        section: "trust",
        label: "Where this node stands",
        terminal_form: "",
    },
    Metric {
        section: "trust",
        label: "Trust readiness",
        terminal_form: "a text bar [#####.] and n/6 instead of the ring",
    },
    Metric {
        section: "trust",
        label: "Missing for a Trusted node",
        terminal_form: "",
    },
    Metric {
        section: "trust",
        label: "What Trusted means",
        terminal_form: "",
    },
    Metric {
        section: "trust",
        label: "Security baseline",
        terminal_form: "",
    },
    Metric {
        section: "sessions",
        label: "Running now",
        terminal_form: "",
    },
    Metric {
        section: "sessions",
        label: "Since this start",
        terminal_form: "",
    },
    Metric {
        section: "earnings",
        label: "This month",
        terminal_form: "",
    },
    Metric {
        section: "earnings",
        label: "In total",
        terminal_form: "",
    },
    Metric {
        section: "earnings",
        label: "Account and rate",
        terminal_form: "",
    },
    Metric {
        section: "settings",
        label: "Settings",
        terminal_form: "",
    },
    Metric {
        section: "settings",
        label: "Starting view",
        terminal_form: "",
    },
    Metric {
        section: "logs",
        label: "Last log lines",
        terminal_form: "",
    },
];

/// Something a person can do from every view.
#[derive(Clone, Copy, Debug)]
pub struct Action {
    pub id: &'static str,
    pub label: &'static str,
    /// The dashboard call that does it (POST, behind the token).
    pub web: &'static str,
    /// The key in the terminal view, as its help names it.
    pub tui_key: &'static str,
}

pub const ACTIONS: &[Action] = &[
    Action {
        id: "pause",
        label: "Pause sharing",
        web: "/api/pause",
        tui_key: "p",
    },
    Action {
        id: "resume",
        label: "Resume sharing",
        web: "/api/resume",
        tui_key: "p",
    },
    Action {
        id: "reconnect",
        label: "Reconnect now",
        web: "/api/reconnect",
        tui_key: "c",
    },
    Action {
        id: "recheck",
        label: "Re-check now",
        web: "/api/recheck",
        tui_key: "R",
    },
    Action {
        id: "setting",
        label: "Change a setting",
        web: "/api/settings",
        tui_key: "5",
    },
    Action {
        id: "setting_clear",
        label: "Use environment",
        web: "/api/settings/clear",
        tui_key: "d",
    },
    Action {
        id: "stop_session",
        label: "Stop session",
        web: "/api/session/stop",
        tui_key: "e",
    },
    Action {
        id: "report",
        label: "Save a report",
        web: "/api/report",
        tui_key: "w",
    },
    Action {
        id: "stop_node",
        label: "Stop the node",
        web: "/api/shutdown",
        tui_key: "x",
    },
    Action {
        id: "view_web",
        label: "Web",
        web: "/api/view",
        tui_key: "b",
    },
    Action {
        id: "view_app",
        label: "App window",
        web: "/api/view",
        tui_key: "a",
    },
    Action {
        id: "view_terminal",
        label: "Terminal",
        web: "/api/view",
        tui_key: "(this view)",
    },
];

/// What the terminal view has beyond this table, and the web page does not
/// yet: listed so nobody mistakes them for parity.
pub const TERMINAL_ONLY: &[&str] = &[
    "Models: the advertised model table (the web page lists the models in What this node shares)",
    "Peers: approve, deny, block, invite, hold and revoke consumers",
    "Activity: five minutes of CPU, GPU, VRAM and RAM history and a bar per core",
];

// ---------------------------------------------------------------- chooser

/// The chooser at an interactive start asks only when nothing chose already:
/// a person at a terminal (stdin AND stdout), no `--ui` / `KMPLIFY_NODE_UI`,
/// nothing remembered. A service, a container without a TTY, systemd or a
/// redirected run never sees it.
pub fn should_prompt(stdin_tty: bool, stdout_tty: bool, chosen: bool) -> bool {
    stdin_tty && stdout_tty && !chosen
}

/// One answer to the chooser: `1`, `2` or `3`, optionally followed by `r`
/// (or ` remember`) to keep it. Empty means "the default". The value is the
/// `--ui` spelling of the choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub ui: &'static str,
    pub remember: bool,
}

pub fn parse_choice(input: &str) -> Result<Option<Choice>, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Ok(None);
    }
    let (head, rest) = s.split_at(1);
    let ui = match head {
        "1" => "tui",
        "2" => "gui",
        "3" => "app",
        _ => return Err(format!("{input:?}: type 1, 2 or 3")),
    };
    let remember = match rest.trim() {
        "" => false,
        "r" | "remember" => true,
        _ => return Err(format!("{input:?}: add r to remember, e.g. 2r")),
    };
    Ok(Some(Choice { ui, remember }))
}

pub const CHOOSER_TEXT: &str = "How do you want to watch this node?\n\
     \x20 1  Terminal view   the dashboard here, in this console\n\
     \x20 2  Web view        the dashboard in your browser\n\
     \x20 3  App window      the dashboard in its own window\n\
     Type 1, 2 or 3 and press Enter; add r to remember the choice (e.g. 2r).\n\
     You can switch at any time: keys in the terminal view, the View buttons on the dashboard.";

// ------------------------------------------------------------------ model

/// Where a model is built: in the node's own process, or from what another
/// process published.
pub struct Source<'a> {
    pub snap: &'a Snapshot,
    pub stored: &'a Settings,
    /// The engine the node lends (`OLLAMA_BASE`).
    pub engine: &'a str,
    /// The log lines to show, oldest first.
    pub logs: Vec<String>,
    /// A security check runs now.
    pub rechecking: bool,
    pub view: crate::control::ViewState,
}

/// The settings the views may change. Each is one the node already applies
/// at runtime (`kmplify-node set` writes the same file and the running node
/// re-advertises). Everything else is shown with how to change it.
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

fn opt<T: serde::Serialize>(v: Option<T>) -> Value {
    v.map(|v| json!(v)).unwrap_or(Value::Null)
}

/// The starting view as the "This dashboard" row says it.
fn start_view_text(stored: &Settings, view: &crate::control::ViewState) -> String {
    let set = std::env::var("KMPLIFY_NODE_UI")
        .ok()
        .filter(|v| !v.trim().is_empty());
    match (set, stored.ui.as_deref(), view.start.as_str()) {
        (Some(v), _, _) => format!("{v} (from --ui / KMPLIFY_NODE_UI)"),
        (None, Some(v), _) => format!("{v} (remembered from the chooser)"),
        (None, None, "") => "asked at an interactive start".into(),
        (None, None, s) => format!("{s} (this start)"),
    }
}

/// Everything a view shows, in one value.
pub fn model(src: &Source) -> Value {
    let snap = src.snap;
    let stored = src.stored;
    let report = snap.security.as_ref();
    let fabric = snap.fabric_trust.as_ref();
    let now_s = crate::identity::now_s();
    let progress = snap.security_progress.clone().unwrap_or_default();
    let standing = crate::trust::standing(report, fabric, now_s);
    let readiness = crate::trust::readiness(report, fabric, &progress, now_s);
    let checklist = report
        .map(|r| crate::trust::checklist(r, fabric))
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
    let with_how = |mut v: Value, how: &str| {
        v["how"] = json!(how);
        v
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
        with_how(
            setting(
                "engine",
                "Model engine (OpenAI-compatible URL)",
                json!(src.engine),
                Value::Null,
                stored.engine.is_some(),
                "text",
                "OLLAMA_BASE",
            ),
            "kmplify-node set engine=NAME_OR_URL (ollama, lmstudio, llamacpp, ...); kmplify-node engines lists what runs here",
        ),
        with_how(
            setting(
                "gateway",
                "Fabric gateway",
                json!(snap.gateway),
                Value::Null,
                false,
                "text",
                "PROVIDER_GATEWAY_URL",
            ),
            "set PROVIDER_GATEWAY_URL or --gateway, then restart the node",
        ),
        with_how(
            setting(
                "functions",
                "Host signed functions",
                json!(snap.functions_enabled),
                Value::Null,
                stored.functions.is_some(),
                "bool",
                "PROVIDER_FUNCTIONS",
            ),
            "kmplify-node set functions=true functions-pubkey=HEX (the key from GET /v1/functions)",
        ),
        with_how(
            setting(
                "share-vectors",
                "Host vector collections",
                json!(snap.vectors_enabled),
                Value::Null,
                stored.share_vectors.is_some(),
                "bool",
                "PROVIDER_SHARE_VECTORS",
            ),
            "kmplify-node set share-vectors=true",
        ),
        with_how(
            setting(
                "ui",
                "Starting view",
                json!(start_view_text(stored, &src.view)),
                Value::Null,
                stored.ui.is_some(),
                "text",
                "KMPLIFY_NODE_UI",
            ),
            "Only how a start begins: kmplify-node --ui tui | gui | app | terminal | none \
             (KMPLIFY_NODE_UI), or the chooser at an interactive start, which can remember \
             it (kmplify-node set ui=gui; kmplify-node set --clear ui asks again). Switch \
             while the node runs with the View buttons at the top, or b (web) and a (app \
             window) in the terminal view; no restart.",
        ),
    ];
    let sessions: Vec<Value> = snap
        .sessions
        .iter()
        .map(|s| {
            json!({
                "id": &s.session_id[..12.min(s.session_id.len())],
                "session_id": s.session_id,
                "template": s.template,
                "state": s.state,
                "cpus": s.cpus,
                "since": s.since,
            })
        })
        .collect();
    let logs: Vec<String> = src
        .logs
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
            "models_text": crate::summary::models_text(snap, src.engine),
            "engine": src.engine,
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
            "checked_at": report.map(|r| r.checked_at),
            "checker_version": report.map(|r| r.checker_version.clone()),
            "hints": report.map(|r| r.hints.clone()).unwrap_or_default(),
            "elevated": report.and_then(|r| r.elevated),
            "rechecking": src.rechecking || progress.running,
            "fabric": fabric,
            "fabric_line": fabric.map(crate::trust::fabric_line),
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
        "view": src.view,
        "logs": logs,
    })
}

/// Apply the fields an editor changed (`draft` against the `saved` it
/// started from) onto the settings file as it is NOW, so a change made
/// meanwhile from another view (or `kmplify-node set`, or the chooser's
/// remembered view) is kept instead of overwritten.
pub fn merge_changes(current: &Settings, saved: &Settings, draft: &Settings) -> Settings {
    let to_map = |s: &Settings| match serde_json::to_value(s) {
        Ok(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    let (saved, draft) = (to_map(saved), to_map(draft));
    let mut out = to_map(current);
    let keys: std::collections::BTreeSet<&String> = saved.keys().chain(draft.keys()).collect();
    for k in keys {
        if saved.get(k) != draft.get(k) {
            match draft.get(k) {
                Some(v) => {
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
    }
    serde_json::from_value(Value::Object(out)).unwrap_or_else(|_| current.clone())
}

// ----------------------------------------------------------------- report

fn human_s(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// A plain-text report of what the views show: the first thing anyone is
/// asked for when a node misbehaves. Log lines are redacted like on screen.
pub fn report_text(s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("kmplify-node {}\n", s.version));
    out.push_str(&format!(
        "link      : {} {}\n",
        s.link.label(),
        if s.paused { "(paused)" } else { "" }
    ));
    out.push_str(&format!("node      : {}\n", s.node_id));
    out.push_str(&format!("gateway   : {}\n", s.gateway));
    out.push_str(&format!("uptime    : {}\n", human_s(s.uptime().as_secs())));
    out.push_str(&format!(
        "accel     : {} {} ({} MB)\n",
        s.accelerator, s.gpu_name, s.vram_total_mb
    ));
    out.push_str(&format!(
        "cpu/ram   : {} · {:.0}% · {} / {} MB\n",
        s.cpu_model, s.cpu_percent, s.ram_used_mb, s.ram_total_mb
    ));
    out.push_str(&format!("models    : {}\n", s.models.join(", ")));
    out.push_str(&format!(
        "jobs      : {} active, {} finished, {} errors, avg {} ms\n",
        s.jobs.active, s.jobs.done, s.jobs.failed, s.jobs.avg_ms
    ));
    for sess in &s.sessions {
        out.push_str(&format!(
            "session   : {} {} {} cpus={}\n",
            sess.session_id, sess.template, sess.state, sess.cpus
        ));
    }
    if let Some(r) = &s.security {
        let rd = crate::trust::readiness(
            Some(r),
            s.fabric_trust.as_ref(),
            &s.security_progress.clone().unwrap_or_default(),
            crate::identity::now_s(),
        );
        out.push_str(&format!(
            "trust     : {} {}\n",
            crate::trust::readiness_bar(&rd),
            rd.verdict
        ));
    }
    out.push_str("\nlog\n");
    for line in &s.logs {
        out.push_str(&crate::trust::redact(line));
        out.push('\n');
    }
    out
}

/// Write [`report_text`] into the node directory; the path written.
pub fn write_report(node_dir: &Path, s: &Snapshot) -> Result<PathBuf, String> {
    let path = node_dir.join(format!("snapshot-{}.txt", crate::status::now_ms()));
    std::fs::write(&path, report_text(s))
        .map(|()| path)
        .map_err(|e| format!("could not write the report: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chooser_reads_a_digit_and_an_optional_remember() {
        assert_eq!(parse_choice(""), Ok(None));
        assert_eq!(parse_choice("  \n"), Ok(None));
        assert_eq!(
            parse_choice("1"),
            Ok(Some(Choice {
                ui: "tui",
                remember: false
            }))
        );
        assert_eq!(
            parse_choice("2r"),
            Ok(Some(Choice {
                ui: "gui",
                remember: true
            }))
        );
        assert_eq!(
            parse_choice("3 remember\r\n"),
            Ok(Some(Choice {
                ui: "app",
                remember: true
            }))
        );
        assert!(parse_choice("4").is_err());
        assert!(parse_choice("2x").is_err());
        assert!(parse_choice("web").is_err());
        // Every choice is a starting view `--ui` accepts.
        for c in ["1", "2", "3"] {
            let ui = parse_choice(c).unwrap().unwrap().ui;
            assert!(crate::dashboard::UiMode::parse(ui).is_ok(), "{ui}");
        }
    }

    #[test]
    fn only_a_person_at_a_terminal_is_asked() {
        assert!(should_prompt(true, true, false));
        // Already chosen by --ui, KMPLIFY_NODE_UI or a remembered answer.
        assert!(!should_prompt(true, true, true));
        // systemd, a container without -t, a pipe, a redirected log.
        assert!(!should_prompt(false, true, false));
        assert!(!should_prompt(true, false, false));
        assert!(!should_prompt(false, false, false));
    }

    #[test]
    fn the_contract_is_well_formed() {
        for m in METRICS {
            assert!(
                SECTIONS.iter().any(|s| s.id == m.section),
                "{} names no section",
                m.label
            );
        }
        for s in SECTIONS {
            assert!(
                METRICS.iter().any(|m| m.section == s.id),
                "{} shows nothing",
                s.id
            );
        }
        for a in ACTIONS {
            assert!(a.web.starts_with("/api/"), "{}", a.id);
            assert!(!a.tui_key.is_empty(), "{}", a.id);
        }
    }

    #[test]
    fn an_editor_keeps_what_another_view_changed_meanwhile() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        draft.set("max-cpus", "4").unwrap();
        // Meanwhile the web dashboard set the country and the chooser
        // remembered a view.
        let mut current = saved.clone();
        current.set("country", "DE").unwrap();
        current.set("ui", "gui").unwrap();
        let merged = merge_changes(&current, &saved, &draft);
        assert_eq!(merged.max_cpus, Some(4.0));
        assert_eq!(merged.country.as_deref(), Some("DE"));
        assert_eq!(merged.ui.as_deref(), Some("gui"));
        // Handing a field back to the environment in the editor clears it.
        let mut cleared = draft.clone();
        cleared.clear("max-cpus").unwrap();
        let merged = merge_changes(&merged, &draft, &cleared);
        assert_eq!(merged.max_cpus, None);
        assert_eq!(merged.country.as_deref(), Some("DE"));
    }

    #[test]
    fn the_model_carries_every_setting_and_the_view_state() {
        let snap = Snapshot::default();
        let stored = Settings::default();
        let m = model(&Source {
            snap: &snap,
            stored: &stored,
            engine: "http://127.0.0.1:11434",
            logs: vec!["12:00:00 token=abcdef0123456789abcdef0123456789 x".into()],
            rechecking: false,
            view: crate::control::ViewState::default(),
        });
        let labels: Vec<&str> = m["settings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"Starting view"));
        assert_eq!(m["view"]["switchable"], false);
        assert!(m["trust"]["readiness"].is_object());
    }
}
