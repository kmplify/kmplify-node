//! Remote settings: the router cluster changes a few settings of this node.
//!
//! `PUT /v2/node-settings` on the node-info port, protocol v2 only: inside
//! mutual TLS with certificates bound to the cluster key ([`super::tls`]),
//! with a body-bound request signature (`v2.` header, nonce and a 30 second
//! window, [`super::auth::Verifier::verify_with_body`]) and a signed answer.
//! A plain v1 connection is refused even with a valid signature.
//!
//! What may be set (the whole list, [`FIELDS`]):
//!
//! | Field | Setting | Values |
//! |---|---|---|
//! | `name` | `router-name` | 1 to 40 letters, digits, `-` or `_` |
//! | `paused` | pause / resume sharing | `true` / `false` |
//! | `marketplace` | `marketplace` | `true` / `false` |
//! | `max_cpus` | `max-cpus` | whole number, 1 to 1024 |
//! | `max_vram_mb` | `max-vram-mb` | 1 to 4,194,304 |
//! | `max_ram_mb` | `max-ram-mb` | 1 to 16,777,216 |
//! | `max_disk_gb` | `max-disk-gb` | 1 to 1,000,000 |
//!
//! Never, whatever the request says ([`NEVER`]): the router mode and the
//! allowlist (a remote request must not widen what the network can reach),
//! the cluster key, credentials, the engine and gateway addresses, and
//! [`OPT_IN`] itself. A ceiling cannot be cleared remotely either: clearing
//! hands it back to the environment, which may mean no ceiling at all.
//!
//! The node accepts none of it unless its operator switched [`OPT_IN`] on,
//! on the machine (`kmplify-node set router-remote-settings=true`, or the
//! LAN router section of a view). Each change then goes through the same
//! action layer as a local one ([`crate::views::perform_with`]): the same
//! checks, the same settings file, the same reload. Every authenticated
//! request, applied or refused, leaves an audit line in the node's log
//! (the Logs view), in the router log, and in `router-audit.log` in the
//! node directory: the cluster key's fingerprint, the request id, the
//! peer address, what was asked and what came of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::views::{self, Act, Outcome};

/// The path of the settings write.
pub const SETTINGS_PATH: &str = "/v2/node-settings";
/// Its method.
pub const SETTINGS_METHOD: &str = "PUT";
/// The local setting that allows it; off by default.
pub const OPT_IN: &str = "router-remote-settings";
/// The fields a request may carry besides `request_id`.
pub const FIELDS: &[&str] = &[
    "name",
    "paused",
    "marketplace",
    "max_cpus",
    "max_vram_mb",
    "max_ram_mb",
    "max_disk_gb",
];
/// What a request may never set, named so a refusal can say why.
pub const NEVER: &[&str] = &[
    "router",
    "mode",
    "router-allow",
    "allow",
    "allowlist",
    "key",
    "cluster_key",
    "router-remote-settings",
    "remote_settings",
    "engine",
    "colibri",
    "colibri-key",
    "functions-pubkey",
];
/// The audit file in the node directory.
pub const AUDIT_FILE: &str = "router-audit.log";
/// Rotated to `router-audit.log.1` past this size.
const AUDIT_MAX_BYTES: u64 = 1024 * 1024;

/// Whatever applies the changes: the headless node ([`NodeHost`]), or an
/// application that embeds the router and keeps its sharing settings its
/// own way (kmplify-desktop).
pub trait RemoteHost: Send + Sync {
    /// The local opt-in ([`OPT_IN`]), read afresh for every request.
    fn allowed(&self) -> bool;
    /// Carry out one change exactly as a local view would. Called on a
    /// blocking thread, never on the async runtime.
    fn perform(&self, act: &Act) -> Outcome;
}

/// The headless node: its settings file and its in-process worker.
pub struct NodeHost {
    node_dir: PathBuf,
    keys: Arc<dyn super::KeyStore>,
}

impl NodeHost {
    pub fn new(node_dir: &Path, keys: Arc<dyn super::KeyStore>) -> Self {
        Self {
            node_dir: node_dir.to_path_buf(),
            keys,
        }
    }
}

impl RemoteHost for NodeHost {
    fn allowed(&self) -> bool {
        super::remote_settings_allowed(&crate::settings::Settings::load(&self.node_dir))
    }

    fn perform(&self, act: &Act) -> Outcome {
        views::perform_with(
            views::Target::InProcess,
            &self.node_dir,
            act,
            self.keys.as_ref(),
        )
    }
}

/// One validated request: the changes in the order they are applied.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub request_id: String,
    /// `(field, what it does)`; the action layer's [`Act`] for each.
    pub changes: Vec<(&'static str, String, Act)>,
}

/// Why a request was refused before anything changed.
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub error: &'static str,
    pub field: Option<String>,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, error: &'static str, field: Option<&str>, message: String) -> Self {
        Self {
            status,
            error,
            field: field.map(String::from),
            message,
        }
    }

    pub fn body(&self) -> Value {
        json!({
            "ok": false,
            "error": self.error,
            "field": self.field,
            "message": self.message,
        })
    }
}

fn whole(field: &'static str, v: &Value, max: u64) -> Result<u64, Refusal> {
    let n = v
        .as_u64()
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| {
            Refusal::new(
                400,
                "invalid_value",
                Some(field),
                format!("{field} must be a whole number from 1 to {max}"),
            )
        })?;
    Ok(n)
}

fn flag(field: &'static str, v: &Value) -> Result<bool, Refusal> {
    v.as_bool().ok_or_else(|| {
        Refusal::new(
            400,
            "invalid_value",
            Some(field),
            format!("{field} must be true or false"),
        )
    })
}

/// Check a request body: JSON object, only [`FIELDS`] and `request_id`,
/// each value in range. Nothing is changed here.
pub fn parse(body: &[u8]) -> Result<Plan, Refusal> {
    let obj: Map<String, Value> = match serde_json::from_slice(body) {
        Ok(Value::Object(m)) => m,
        _ => {
            return Err(Refusal::new(
                400,
                "invalid_body",
                None,
                "the body must be a JSON object".into(),
            ))
        }
    };
    for k in obj.keys() {
        if k == "request_id" || FIELDS.contains(&k.as_str()) {
            continue;
        }
        let why = if NEVER.contains(&k.as_str()) {
            format!(
                "{k} is never changed remotely; only on this machine (allowed: {})",
                FIELDS.join(", ")
            )
        } else {
            format!(
                "{k} is not a remote setting (allowed: {})",
                FIELDS.join(", ")
            )
        };
        return Err(Refusal::new(400, "field_not_allowed", Some(k), why));
    }
    let request_id = match obj.get("request_id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s))
            if s.len() <= 64
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')) =>
        {
            s.clone()
        }
        Some(_) => {
            return Err(Refusal::new(
                400,
                "invalid_value",
                Some("request_id"),
                "request_id must be up to 64 letters, digits, '-', '_' or '.'".into(),
            ))
        }
    };
    let setting = |key: &str, value: String| Act::Setting {
        key: key.into(),
        value,
    };
    let mut changes: Vec<(&'static str, String, Act)> = Vec::new();
    // Settings first, pause or resume last: a request that pauses and
    // lowers a ceiling resumes nothing before the ceiling holds.
    if let Some(v) = obj.get("name") {
        let n = v.as_str().ok_or_else(|| {
            Refusal::new(
                400,
                "invalid_value",
                Some("name"),
                "name must be text".into(),
            )
        })?;
        let n = super::policy::check_name(n)
            .map_err(|e| Refusal::new(400, "invalid_value", Some("name"), e))?;
        changes.push((
            "name",
            format!("router-name={n}"),
            setting("router-name", n),
        ));
    }
    if let Some(v) = obj.get("marketplace") {
        let b = flag("marketplace", v)?;
        changes.push((
            "marketplace",
            format!("marketplace={b}"),
            setting("marketplace", b.to_string()),
        ));
    }
    for (field, key, max) in [
        ("max_cpus", "max-cpus", 1024),
        ("max_vram_mb", "max-vram-mb", 4_194_304),
        ("max_ram_mb", "max-ram-mb", 16_777_216),
        ("max_disk_gb", "max-disk-gb", 1_000_000),
    ] {
        if let Some(v) = obj.get(field) {
            let n = whole(field, v, max)?;
            changes.push((field, format!("{key}={n}"), setting(key, n.to_string())));
        }
    }
    if let Some(v) = obj.get("paused") {
        let p = flag("paused", v)?;
        changes.push((
            "paused",
            if p { "pause" } else { "resume" }.into(),
            if p { Act::Pause } else { Act::Resume },
        ));
    }
    if changes.is_empty() {
        return Err(Refusal::new(
            400,
            "nothing_to_change",
            None,
            format!("name at least one of: {}", FIELDS.join(", ")),
        ));
    }
    // The settings file's own checks, before anything is written: one
    // refused value leaves every other one unwritten too.
    let mut probe = crate::settings::Settings::default();
    for (field, _, act) in &changes {
        if let Act::Setting { key, value } = act {
            probe
                .set(key, value)
                .map_err(|e| Refusal::new(400, "invalid_value", Some(field), e))?;
        }
    }
    Ok(Plan {
        request_id,
        changes,
    })
}

/// Is `act` one a remote request may cause? Checked again right before
/// each change is handed to the host, whatever built the plan.
pub fn act_is_remote(act: &Act) -> bool {
    match act {
        Act::Pause | Act::Resume => true,
        Act::Setting { key, .. } => matches!(
            key.as_str(),
            "router-name"
                | "marketplace"
                | "max-cpus"
                | "max-vram-mb"
                | "max-ram-mb"
                | "max-disk-gb"
        ),
        _ => false,
    }
}

/// Who asked: what an audit line names. No personal data: the key's
/// fingerprint, the caller's request id and the peer address.
#[derive(Clone, Debug)]
pub struct Caller {
    pub fingerprint: String,
    pub request_id: String,
    pub nonce: String,
    pub peer: String,
}

impl Caller {
    fn who(&self) -> String {
        let id = if self.request_id.is_empty() {
            format!("nonce {}", &self.nonce[..self.nonce.len().min(8)])
        } else {
            format!("request {}", self.request_id)
        };
        format!(
            "router cluster (key {}, {id}, from {})",
            self.fingerprint, self.peer
        )
    }
}

/// Apply a checked plan through `host`. The answer's status and body.
pub fn apply(plan: &Plan, host: &dyn RemoteHost) -> (u16, Value, String) {
    let mut applied: Vec<&str> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    for (field, what, act) in &plan.changes {
        if !act_is_remote(act) {
            // Unreachable through `parse`; kept so no future caller can
            // hand the host anything else.
            return (
                403,
                json!({"ok": false, "error": "field_not_allowed", "field": field, "applied": applied}),
                format!("refused {what}: not a remote setting"),
            );
        }
        let o = host.perform(act);
        if !o.ok {
            let line = if done.is_empty() {
                format!("{what} failed: {}", o.message)
            } else {
                format!("applied {}; {what} failed: {}", done.join(", "), o.message)
            };
            return (
                if o.status >= 500 { 500 } else { 409 },
                json!({
                    "ok": false,
                    "error": "apply_failed",
                    "field": field,
                    "applied": applied,
                    "message": o.message,
                }),
                line,
            );
        }
        applied.push(field);
        done.push(what.clone());
    }
    let line = format!("applied {}", done.join(", "));
    (
        200,
        json!({
            "ok": true,
            "applied": applied,
            "message": "applied; the node re-advertises within seconds",
        }),
        line,
    )
}

/// The whole request after its signature was checked: opt-in, parse,
/// apply. Returns the status, the JSON body and the audit line.
pub fn handle(body: &[u8], host: &dyn RemoteHost, caller: &mut Caller) -> (u16, Value, String) {
    if !host.allowed() {
        return (
            403,
            json!({
                "ok": false,
                "error": "remote_settings_off",
                "message": "remote settings are off on this node; its operator can allow them \
                            on the machine (LAN router section, or kmplify-node set \
                            router-remote-settings=true)",
            }),
            format!("{}: refused, remote settings are off here", caller.who()),
        );
    }
    let plan = match parse(body) {
        Ok(p) => p,
        Err(r) => {
            let line = format!(
                "{}: refused{}: {}",
                caller.who(),
                r.field
                    .as_deref()
                    .map(|f| format!(" field {f}"))
                    .unwrap_or_default(),
                r.error
            );
            return (r.status, r.body(), line);
        }
    };
    caller.request_id = plan.request_id.clone();
    let (status, body, what) = apply(&plan, host);
    (status, body, format!("{}: {what}", caller.who()))
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_iso(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant's algorithm), valid for every u64 day
    // count this clock produces.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Append one line to `router-audit.log` in `node_dir`, owner-only, with
/// the time in UTC. Best effort: an audit file that cannot be written does
/// not undo a change, and the line is in the node's log either way.
pub fn append_audit(node_dir: &Path, line: &str) {
    use std::io::Write;
    let path = node_dir.join(AUDIT_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > AUDIT_MAX_BYTES) {
        let _ = std::fs::rename(&path, node_dir.join(format!("{AUDIT_FILE}.1")));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = writeln!(f, "{} {line}", utc_iso(crate::status::now_ms()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A host that records what it was asked to do.
    pub(crate) struct Recorder {
        pub allowed: bool,
        pub done: Mutex<Vec<Act>>,
    }

    impl RemoteHost for Recorder {
        fn allowed(&self) -> bool {
            self.allowed
        }
        fn perform(&self, act: &Act) -> Outcome {
            self.done.lock().unwrap().push(act.clone());
            Outcome {
                status: 200,
                ok: true,
                message: "ok".into(),
            }
        }
    }

    fn caller() -> Caller {
        Caller {
            fingerprint: "abcd1234".into(),
            request_id: String::new(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            peer: "127.0.0.1".into(),
        }
    }

    #[test]
    fn the_allowlisted_fields_become_view_actions_pause_last() {
        let p = parse(
            br#"{"request_id":"r-1","paused":true,"name":"studio","marketplace":false,
                 "max_cpus":8,"max_vram_mb":16000,"max_ram_mb":32000,"max_disk_gb":100}"#,
        )
        .unwrap();
        assert_eq!(p.request_id, "r-1");
        let fields: Vec<&str> = p.changes.iter().map(|c| c.0).collect();
        assert_eq!(
            fields,
            [
                "name",
                "marketplace",
                "max_cpus",
                "max_vram_mb",
                "max_ram_mb",
                "max_disk_gb",
                "paused"
            ]
        );
        assert_eq!(p.changes.last().unwrap().2, Act::Pause);
        assert!(p.changes.iter().all(|c| act_is_remote(&c.2)));
    }

    #[test]
    fn mode_allowlist_key_and_the_opt_in_are_never_remote() {
        for body in [
            r#"{"router":"lan"}"#,
            r#"{"mode":"lan"}"#,
            r#"{"router-allow":"0.0.0.0/0"}"#,
            r#"{"allow":["10.0.0.0/8"]}"#,
            r#"{"key":"00"}"#,
            r#"{"router-remote-settings":true}"#,
            r#"{"remote_settings":true}"#,
            r#"{"engine":"http://evil"}"#,
            r#"{"colibri-key":"x"}"#,
            r#"{"name":"ok","country":"US"}"#,
        ] {
            let r = parse(body.as_bytes()).unwrap_err();
            assert_eq!((r.status, r.error), (400, "field_not_allowed"), "{body}");
            assert!(r.field.is_some());
        }
        for act in [
            Act::Setting {
                key: "router".into(),
                value: "lan".into(),
            },
            Act::Setting {
                key: "router-allow".into(),
                value: "10.0.0.0/8".into(),
            },
            Act::Setting {
                key: OPT_IN.into(),
                value: "true".into(),
            },
            Act::RouterNewKey,
            Act::SettingClear {
                key: "max-cpus".into(),
            },
            Act::StopNode,
        ] {
            assert!(!act_is_remote(&act), "{act:?}");
        }
    }

    #[test]
    fn bad_values_are_refused_before_anything_changes() {
        for (body, field) in [
            (r#"{"name":"has space"}"#, "name"),
            (r#"{"name":7}"#, "name"),
            (r#"{"marketplace":"yes"}"#, "marketplace"),
            (r#"{"max_cpus":0}"#, "max_cpus"),
            (r#"{"max_cpus":2.5}"#, "max_cpus"),
            (r#"{"max_cpus":-1}"#, "max_cpus"),
            (r#"{"max_cpus":null}"#, "max_cpus"),
            (r#"{"max_disk_gb":1000001}"#, "max_disk_gb"),
            (r#"{"paused":1}"#, "paused"),
            (r#"{"request_id":"a b"}"#, "request_id"),
        ] {
            let r = parse(body.as_bytes()).unwrap_err();
            assert_eq!(r.status, 400, "{body}");
            assert_eq!(r.field.as_deref(), Some(field), "{body}");
        }
        assert_eq!(parse(b"{}").unwrap_err().error, "nothing_to_change");
        assert_eq!(parse(b"[]").unwrap_err().error, "invalid_body");
        assert_eq!(parse(b"not json").unwrap_err().error, "invalid_body");
        // One bad value in a valid request: nothing is applied.
        let host = Recorder {
            allowed: true,
            done: Mutex::new(Vec::new()),
        };
        let (status, _, line) = handle(br#"{"name":"studio","max_cpus":0}"#, &host, &mut caller());
        assert_eq!(status, 400);
        assert!(host.done.lock().unwrap().is_empty());
        assert!(line.contains("refused field max_cpus"), "{line}");
    }

    #[test]
    fn with_the_opt_in_off_nothing_is_applied() {
        let host = Recorder {
            allowed: false,
            done: Mutex::new(Vec::new()),
        };
        let (status, body, line) = handle(br#"{"name":"studio"}"#, &host, &mut caller());
        assert_eq!(status, 403);
        assert_eq!(body["error"], "remote_settings_off");
        assert!(host.done.lock().unwrap().is_empty());
        assert!(line.contains("key abcd1234"), "{line}");
        assert!(line.contains("remote settings are off"), "{line}");
    }

    #[test]
    fn an_applied_request_names_who_and_what() {
        let host = Recorder {
            allowed: true,
            done: Mutex::new(Vec::new()),
        };
        let mut c = caller();
        let (status, body, line) = handle(
            br#"{"request_id":"audit-42","name":"studio","paused":false}"#,
            &host,
            &mut c,
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["applied"], json!(["name", "paused"]));
        assert_eq!(
            *host.done.lock().unwrap(),
            vec![
                Act::Setting {
                    key: "router-name".into(),
                    value: "studio".into()
                },
                Act::Resume
            ]
        );
        assert!(line.contains("key abcd1234"), "{line}");
        assert!(line.contains("request audit-42"), "{line}");
        assert!(line.contains("router-name=studio, resume"), "{line}");
    }

    #[test]
    fn utc_iso_is_a_calendar_date() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_iso(1_795_000_000_000), "2026-11-18T11:06:40Z");
        assert_eq!(utc_iso(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn the_audit_file_is_appended_and_owner_only() {
        let dir = std::env::temp_dir().join(format!("kn-remote-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        append_audit(&dir, "first");
        append_audit(&dir, "second");
        let text = std::fs::read_to_string(dir.join(AUDIT_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("Z first"), "{}", lines[0]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(AUDIT_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
