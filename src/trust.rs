//! Where this node stands on the way to Trusted, in plain words.
//!
//! Two views meet here:
//!
//! * **this machine's**: the baseline the node checks itself
//!   ([`crate::security`]), with the exact reason per item and how to fix it;
//! * **the fabric's**: the trust level, site and baseline as the gateway
//!   evaluated them, read from the gateway's PUBLIC `/fabric/nodes` list,
//!   which names each node by the first 8 characters of its id.
//!
//! The node never decides its own trust level (PROTOCOL.md v5.5); this
//! module only reads what the gateway publishes and explains it. When the
//! node cannot be found in that list, or the gateway publishes no trust
//! fields, it says so instead of guessing.

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{Item, Report, Verdict};

/// How often the gateway's public list is read while the node runs. The list
/// is public and cheap, but it is a list of every node, so not every ping.
pub const FABRIC_POLL_INTERVAL: Duration = Duration::from_secs(120);

/// How the node was (or was not) found in `/fabric/nodes`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// Not read yet.
    #[default]
    Pending,
    /// Exactly one entry has this node's id prefix.
    Matched,
    /// No entry has it: not connected right now, or not listed yet.
    NotListed,
    /// More than one entry shares the 8-character prefix, so the public
    /// list cannot say which one is this node.
    Ambiguous,
    /// The entry exists but carries no trust fields (an older gateway).
    NoTrustFields,
    /// The list could not be read.
    Unreachable,
    /// This node has no identity yet, so there is nothing to look for.
    NoIdentity,
}

/// This node in the gateway's public list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FabricView {
    pub state: Match,
    /// `cloud`, `trusted` or `community`, as the gateway computed it.
    pub trust_level: Option<String>,
    /// `datacenter`, `office` or `home`.
    pub site: Option<String>,
    /// The baseline as the gateway evaluated the signed facts.
    pub baseline_ok: Option<bool>,
    /// The gateway's names of what fails (`disk_encrypted`, `stale`, ...).
    pub failing: Vec<String>,
    /// When this was read, unix ms.
    pub read_at_ms: u64,
    /// Why the state is what it is, for `Unreachable` (the error).
    pub detail: String,
}

/// Find `node_id` in a `/fabric/nodes` answer. Pure, so every branch is
/// tested without a gateway.
pub fn match_entry(body: &Value, node_id: &str, now_ms: u64) -> FabricView {
    let mut view = FabricView {
        read_at_ms: now_ms,
        ..FabricView::default()
    };
    if node_id.len() < 8 {
        view.state = Match::NoIdentity;
        return view;
    }
    let Some(list) = body.as_array() else {
        view.state = Match::Unreachable;
        view.detail = "the gateway's node list is not a list".into();
        return view;
    };
    let prefix = &node_id[..8];
    let hits: Vec<&Value> = list
        .iter()
        .filter(|e| e["node"].as_str() == Some(prefix))
        .collect();
    let entry = match hits.as_slice() {
        [] => {
            view.state = Match::NotListed;
            return view;
        }
        [one] => *one,
        _ => {
            view.state = Match::Ambiguous;
            return view;
        }
    };
    let Some(level) = entry["trust_level"].as_str() else {
        view.state = Match::NoTrustFields;
        return view;
    };
    view.state = Match::Matched;
    view.trust_level = Some(level.to_string());
    view.site = entry["site"].as_str().map(str::to_string);
    view.baseline_ok = entry["baseline"]["ok"].as_bool();
    view.failing = entry["baseline"]["failing"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    view
}

fn cell() -> &'static RwLock<Option<FabricView>> {
    static LATEST: OnceLock<RwLock<Option<FabricView>>> = OnceLock::new();
    LATEST.get_or_init(|| RwLock::new(None))
}

/// The latest reading of the gateway's view, if one was made.
pub fn latest() -> Option<FabricView> {
    match cell().read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

fn store(v: FabricView) {
    match cell().write() {
        Ok(mut g) => *g = Some(v),
        Err(p) => *p.into_inner() = Some(v),
    }
}

fn wake() -> &'static tokio::sync::Notify {
    static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
    WAKE.get_or_init(tokio::sync::Notify::new)
}

/// Read the gateway's view again now rather than at the next interval.
pub fn refresh_now() {
    wake().notify_one();
}

/// Read `/fabric/nodes` once and find this node in it.
pub async fn read_once(client: &reqwest::Client, gateway: &str, node_id: &str) -> FabricView {
    let now = crate::status::now_ms();
    if node_id.len() < 8 {
        return match_entry(&Value::Null, node_id, now);
    }
    let url = format!("{}/fabric/nodes", gateway.trim_end_matches('/'));
    let answer = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.map_err(|e| e.to_string()),
        Ok(r) => Err(format!("http {}", r.status().as_u16())),
        Err(e) => Err(e.to_string()),
    };
    match answer {
        Ok(body) => match_entry(&body, node_id, now),
        Err(e) => FabricView {
            state: Match::Unreachable,
            read_at_ms: now,
            detail: e,
            ..FabricView::default()
        },
    }
}

/// Keep [`latest`] current for as long as the node runs: soon after it is
/// online, then every [`FABRIC_POLL_INTERVAL`] or when [`refresh_now`] asks.
/// Logs a line when the fabric's verdict changes, so the terminal says it too.
pub async fn poll_loop(gateway: String, mut stop: tokio::sync::watch::Receiver<bool>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    // The gateway lists a node only while it is connected, and evaluates the
    // baseline from the hello or a pong: give the first one a moment.
    let mut delay = Duration::from_secs(15);
    let mut last: Option<(Option<String>, Vec<String>)> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = wake().notified() => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return;
                }
            }
        }
        delay = FABRIC_POLL_INTERVAL;
        let node_id = crate::status::snapshot().node_id;
        let view = read_once(&client, &gateway, &node_id).await;
        if view.state == Match::Matched {
            let now = (view.trust_level.clone(), view.failing.clone());
            if last.as_ref() != Some(&now) {
                crate::status::push_log(format!("fabric: {}", fabric_line(&view)));
                if !crate::status::quiet() {
                    println!("[kmplify-node] fabric: {}", fabric_line(&view));
                }
                last = Some(now);
            }
        }
        store(view);
    }
}

/// The gateway's check names in the operator's words.
pub fn failing_label(name: &str) -> String {
    for item in Item::ALL {
        if item.key() == name {
            return item.label().to_string();
        }
    }
    match name {
        "missing" => "no security check received yet".into(),
        "malformed" => "the security check could not be read".into(),
        "signature" => "the security check is not signed by this node's key".into(),
        "stale" => "the last security check is older than 6 hours".into(),
        other => other.to_string(),
    }
}

/// One line: the level and, when it is not Trusted, the main reason.
pub fn fabric_line(v: &FabricView) -> String {
    match v.state {
        Match::Matched => {
            let level = v.trust_level.as_deref().unwrap_or("community");
            let site = v.site.as_deref().unwrap_or("home");
            let failing: Vec<String> = v.failing.iter().map(|f| failing_label(f)).collect();
            match (level, v.baseline_ok) {
                ("cloud", _) => "cloud node (KMPLIFY data centre)".into(),
                ("trusted", _) => format!("TRUSTED (site {site})"),
                (_, Some(false)) if !failing.is_empty() => format!(
                    "community; baseline fails at the fabric: {}",
                    failing.join(", ")
                ),
                (_, Some(true)) => "community; the baseline passes, a trust grant for this \
                                    node is still missing"
                    .into(),
                (l, _) => l.to_string(),
            }
        }
        Match::Pending => "not read yet".into(),
        Match::NotListed => "this node is not in the gateway's public list right now \
                             (it is listed while connected)"
            .into(),
        Match::Ambiguous => "cannot be matched: more than one node in the gateway's list \
                             starts with this node's id"
            .into(),
        Match::NoTrustFields => "the gateway does not publish trust levels (older gateway)".into(),
        Match::Unreachable => format!("the gateway's node list could not be read ({})", v.detail),
        Match::NoIdentity => "this node has no identity yet".into(),
    }
}

/// One baseline item, ready to show.
#[derive(Clone, Debug, Serialize)]
pub struct CheckItem {
    pub key: &'static str,
    pub label: &'static str,
    /// `pass`, `fail`, `unknown` or `not_applicable`.
    pub state: &'static str,
    /// The reason in words ("off", "unknown: counts as failing", ...).
    pub text: String,
    /// What the check saw, on this screen only (may name a drive).
    pub detail: Option<String>,
    /// Steps to fix it on this OS; empty when it passes.
    pub steps: Vec<String>,
    /// The same steps for the other systems, for reference.
    pub other_os: Vec<(&'static str, Vec<String>)>,
    /// How the gateway evaluated this item: `pass`, `fail`, or `None` when
    /// the fabric's view is not known.
    pub fabric: Option<&'static str>,
}

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::Unknown => "unknown",
        Verdict::NotApplicable => "not_applicable",
    }
}

/// The checklist for the dashboard and the summary, item by item.
pub fn checklist(r: &Report, fabric: Option<&FabricView>) -> Vec<CheckItem> {
    let fabric = fabric.filter(|f| f.state == Match::Matched && f.baseline_ok.is_some());
    Item::ALL
        .iter()
        .map(|&item| {
            let v = r.verdict(item);
            let bad = matches!(v, Verdict::Fail | Verdict::Unknown);
            let os = r.os.as_str();
            CheckItem {
                key: item.key(),
                label: item.label(),
                state: verdict_word(v),
                text: crate::security::state_text(r, item),
                detail: r.detail(item).map(str::to_string),
                steps: if bad {
                    crate::security::steps(item, os)
                } else {
                    Vec::new()
                },
                other_os: ["windows", "macos", "linux"]
                    .into_iter()
                    .filter(|o| *o != os)
                    .map(|o| (o, crate::security::steps(item, o)))
                    .collect(),
                fabric: fabric.map(|f| {
                    if f.failing.iter().any(|n| n == item.key()) {
                        "fail"
                    } else {
                        "pass"
                    }
                }),
            }
        })
        .collect()
}

/// Where the node stands overall, with what is missing to become Trusted.
#[derive(Clone, Debug, Serialize)]
pub struct Standing {
    /// The fabric's level, or `unknown` when it could not be read.
    pub level: String,
    /// One sentence.
    pub headline: String,
    /// What is still needed, in order, each one sentence.
    pub missing: Vec<String>,
    /// The local baseline verdict (the gateway's rule, applied here).
    pub baseline_ok: Option<bool>,
}

/// Combine this machine's check and the fabric's view into one answer.
pub fn standing(r: Option<&Report>, f: Option<&FabricView>, now_s: u64) -> Standing {
    let matched = f.filter(|f| f.state == Match::Matched);
    let level = matched
        .and_then(|f| f.trust_level.clone())
        .unwrap_or_else(|| "unknown".into());
    let mut missing = Vec::new();
    let baseline_ok = r.map(|r| r.baseline_ok(now_s));
    if let Some(r) = r {
        for item in Item::ALL {
            match r.verdict(item) {
                Verdict::Fail => missing.push(format!(
                    "{}: {}",
                    item.label(),
                    crate::security::state_text(r, item)
                )),
                Verdict::Unknown => missing.push(format!(
                    "{}: could not be read on this machine, which counts as failing",
                    item.label()
                )),
                _ => {}
            }
        }
        if now_s.saturating_sub(r.checked_at) > crate::security::MAX_CHECK_AGE_S {
            missing.push("The last check is older than 6 hours; it runs again soon.".into());
        }
    } else {
        missing.push("The security check is still running on this machine.".into());
    }
    if let Some(f) = matched {
        for name in &f.failing {
            match Item::ALL.iter().find(|i| i.key() == name) {
                // Failing here too: already listed from the local check.
                Some(item)
                    if r.is_none_or(|r| {
                        matches!(r.verdict(*item), Verdict::Fail | Verdict::Unknown)
                    }) => {}
                // Passing here, failing in the check the fabric holds: an
                // older check. Say so, or the two views look contradictory.
                Some(item) => missing.push(format!(
                    "At the fabric: {} still fails in the check it holds. This machine passes \
                     now; the next heartbeat sends the newer result (Re-check now sends it \
                     sooner).",
                    item.label()
                )),
                // What only the fabric sees: missing, signature, stale.
                None => missing.push(format!("At the fabric: {}.", failing_label(name))),
            }
        }
    }
    let headline = match level.as_str() {
        "cloud" => "This is a KMPLIFY cloud node.".to_string(),
        "trusted" => "This node is Trusted: it may run personal and confidential work.".into(),
        "community" => {
            if baseline_ok == Some(true) && matched.is_some_and(|f| f.baseline_ok == Some(true)) {
                missing.push(
                    "A trust grant for this node: the provider's KMPLIFY account passes the \
                     identity check (trader verified, 2FA on), then KMPLIFY grants Trusted for \
                     this node."
                        .into(),
                );
                "Community node. The security baseline passes; the account side is still missing."
                    .into()
            } else {
                "Community node. To become Trusted, fix the items below; Trusted also needs a \
                 verified KMPLIFY provider account."
                    .into()
            }
        }
        _ => match f {
            Some(v) if !matches!(v.state, Match::Matched | Match::Pending) => {
                format!("The fabric's trust level is not known: {}.", fabric_line(v))
            }
            _ => "The fabric's trust level is not known yet.".into(),
        },
    };
    if level == "trusted" || level == "cloud" {
        missing.clear();
    }
    Standing {
        level,
        headline,
        missing,
        baseline_ok,
    }
}

/// Mask what could be a secret in a log line before it leaves the process
/// for a screen: bearer tokens, `key=`/`token=`/`secret=`/`password=`
/// values, and long unbroken runs of hex or base64 (keys, signatures).
pub fn redact(line: &str) -> String {
    let mut out = Vec::new();
    let mut mask_next = false;
    for word in line.split(' ') {
        if mask_next {
            out.push("[redacted]".to_string());
            mask_next = false;
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if lower == "bearer" {
            out.push(word.to_string());
            mask_next = true;
            continue;
        }
        let mut masked = None;
        for k in ["token=", "key=", "secret=", "password=", "sig=", "t="] {
            if let Some(pos) = lower.find(k) {
                if pos == 0 || !lower.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                    masked = Some(format!("{}[redacted]", &word[..pos + k.len()]));
                    break;
                }
            }
        }
        if let Some(m) = masked {
            out.push(m);
            continue;
        }
        let core: &str = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let long_secret_like = core.len() >= 40
            && core
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '_' || c == '-');
        if long_secret_like {
            out.push(word.replace(core, "[redacted]"));
        } else {
            out.push(word.to_string());
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "8f2c1a5b90de4a17b3c9d0e5f6a7b8c9";

    fn report(now: u64) -> Report {
        Report {
            os: "windows".into(),
            disk_encrypted: Some(false),
            patch_age_days: Some(3),
            pending_security_updates: Some(0),
            firewall_on: Some(true),
            screen_lock: Some(false),
            ssh_password_auth: None,
            ssh_root_login: None,
            checked_at: now,
            checker_version: "2".into(),
            ..Report::default()
        }
    }

    #[test]
    fn the_node_is_found_by_its_prefix_and_only_unambiguously() {
        let entry = json!({"node": "8f2c1a5b", "trust_level": "community", "site": "home",
                           "baseline": {"ok": false, "failing": ["disk_encrypted", "screen_lock"]}});
        let v = match_entry(&json!([{"node": "aaaaaaaa"}, entry.clone()]), ID, 1);
        assert_eq!(v.state, Match::Matched);
        assert_eq!(v.trust_level.as_deref(), Some("community"));
        assert_eq!(v.baseline_ok, Some(false));
        assert_eq!(v.failing, ["disk_encrypted", "screen_lock"]);

        assert_eq!(
            match_entry(&json!([entry.clone(), entry]), ID, 1).state,
            Match::Ambiguous
        );
        assert_eq!(
            match_entry(&json!([{"node": "aaaaaaaa"}]), ID, 1).state,
            Match::NotListed
        );
        assert_eq!(
            match_entry(&json!([{"node": "8f2c1a5b"}]), ID, 1).state,
            Match::NoTrustFields
        );
        assert_eq!(
            match_entry(&json!({"error": 1}), ID, 1).state,
            Match::Unreachable
        );
        assert_eq!(match_entry(&json!([]), "", 1).state, Match::NoIdentity);
    }

    #[test]
    fn the_checklist_carries_the_reason_the_fix_and_the_fabrics_verdict() {
        let now = 1_800_000_000;
        let fabric = FabricView {
            state: Match::Matched,
            trust_level: Some("community".into()),
            baseline_ok: Some(false),
            failing: vec!["disk_encrypted".into(), "screen_lock".into()],
            ..FabricView::default()
        };
        let items = checklist(&report(now), Some(&fabric));
        assert_eq!(items.len(), Item::ALL.len());
        let disk = &items[0];
        assert_eq!(
            (disk.key, disk.state, disk.fabric),
            ("disk_encrypted", "fail", Some("fail"))
        );
        assert!(disk.steps[0].contains("BitLocker"), "{:?}", disk.steps);
        assert_eq!(disk.other_os.len(), 2);
        let fw = items.iter().find(|i| i.key == "firewall_on").unwrap();
        assert_eq!((fw.state, fw.fabric), ("pass", Some("pass")));
        assert!(fw.steps.is_empty());
        let ssh = items.iter().find(|i| i.key == "ssh_password_auth").unwrap();
        assert_eq!(ssh.state, "not_applicable");
        // Without the fabric's view there is no fabric column, not a guess.
        assert!(checklist(&report(now), None)
            .iter()
            .all(|i| i.fabric.is_none()));
    }

    #[test]
    fn standing_names_what_is_missing_on_both_sides() {
        let now = 1_800_000_000;
        let fabric = FabricView {
            state: Match::Matched,
            trust_level: Some("community".into()),
            baseline_ok: Some(false),
            failing: vec!["disk_encrypted".into(), "signature".into()],
            ..FabricView::default()
        };
        let s = standing(Some(&report(now)), Some(&fabric), now);
        assert_eq!(s.level, "community");
        assert!(
            !s.missing.iter().any(|m| m.contains("still fails")),
            "failing on both sides is listed once: {:?}",
            s.missing
        );
        assert!(
            s.missing.iter().any(|m| m.starts_with("Disk encryption")),
            "{:?}",
            s.missing
        );
        assert!(s.missing.iter().any(|m| m.starts_with("Screen lock")));
        assert!(s.missing.iter().any(|m| m.contains("not signed")));

        let mut ok = report(now);
        ok.disk_encrypted = Some(true);
        ok.screen_lock = Some(true);
        let passing = FabricView {
            baseline_ok: Some(true),
            failing: vec![],
            ..fabric.clone()
        };
        let older = FabricView {
            failing: vec!["screen_lock".into()],
            ..fabric.clone()
        };
        let s = standing(Some(&ok), Some(&older), now);
        assert!(
            s.missing
                .iter()
                .any(|m| m.contains("Screen lock still fails in the check it holds")),
            "{:?}",
            s.missing
        );
        let s = standing(Some(&ok), Some(&passing), now);
        assert!(s.headline.contains("account side"), "{}", s.headline);
        assert!(s.missing.iter().any(|m| m.contains("trust grant")));

        let trusted = FabricView {
            trust_level: Some("trusted".into()),
            ..passing
        };
        assert!(standing(Some(&ok), Some(&trusted), now).missing.is_empty());

        let s = standing(None, None, now);
        assert_eq!(s.level, "unknown");
        assert!(s.missing[0].contains("still running"));
        for text in [&s.headline, &fabric_line(&FabricView::default())] {
            assert!(!text.contains('\u{2014}'), "no em dash in operator text");
        }
    }

    #[test]
    fn logs_lose_their_secrets_on_the_way_to_a_screen() {
        assert_eq!(
            redact("auth Bearer abc.def ok"),
            "auth Bearer [redacted] ok"
        );
        assert_eq!(redact("url ?token=1234 x"), "url ?token=[redacted] x");
        assert_eq!(
            redact("http://127.0.0.1:5/?t=abcd"),
            "http://127.0.0.1:5/?t=[redacted]"
        );
        let sig = "ab".repeat(32);
        assert_eq!(redact(&format!("sig {sig}.")), "sig [redacted].");
        // Ordinary lines and short ids pass unchanged.
        let line = "node identity 8f2c1a5b90de4a17… joined https://fabric.example";
        assert_eq!(redact(line), line);
    }
}
