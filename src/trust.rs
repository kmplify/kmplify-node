//! Where this node stands on the way to Trusted, in plain words.
//!
//! Two views meet here:
//!
//! * **this machine's**: the baseline the node checks itself
//!   ([`crate::security`]), with the exact reason per item and how to fix it;
//! * **the fabric's**: the trust level, site and baseline as the gateway
//!   evaluated them, read from the gateway's PUBLIC `/fabric/nodes` list,
//!   which names each node by its id or a prefix of it (8, 16 or 24
//!   characters before 2026-11-23, the whole 32 character id since).
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
/// How soon to read it again while this node is not found in it.
pub const FABRIC_RETRY_INTERVAL: Duration = Duration::from_secs(30);

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

/// The shortest public id the gateway has ever published (8 hex characters
/// before 2026-11-13, 16 until 2026-11-15, 24 until 2026-11-23, the whole
/// 32 character id since). Anything shorter cannot name a node.
pub const MIN_PUBLIC_ID: usize = 8;

/// A node id as the gateway compares it: lower case, without dashes.
fn norm_id(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The entries of a `/fabric/nodes` answer: a bare list (today), or a list
/// under `nodes` (the shape other fabric listings use).
fn node_list(body: &Value) -> Option<&Vec<Value>> {
    body.as_array()
        .or_else(|| body.get("nodes").and_then(Value::as_array))
}

/// Find `node_id` in a `/fabric/nodes` answer. Pure, so every branch is
/// tested without a gateway.
///
/// The gateway publishes the node id, or a prefix of it, in `node`, and has
/// grown that over time (8, then 16, then 24 hex characters, and since
/// 2026-11-23 the whole 32 character id). So an entry is this node when its
/// `node` is a prefix of the full id (the whole id included), whatever its
/// length; the longest such prefix wins, and two entries that match equally
/// well are ambiguous.
pub fn match_entry(body: &Value, node_id: &str, now_ms: u64) -> FabricView {
    let mut view = FabricView {
        read_at_ms: now_ms,
        ..FabricView::default()
    };
    let me = norm_id(node_id);
    if me.len() < MIN_PUBLIC_ID {
        view.state = Match::NoIdentity;
        return view;
    }
    let Some(list) = node_list(body) else {
        view.state = Match::Unreachable;
        view.detail = "the gateway answered, but not with a node list".into();
        return view;
    };
    let hits: Vec<(usize, &Value)> = list
        .iter()
        .filter_map(|e| {
            let public = norm_id(e["node"].as_str()?);
            (public.len() >= MIN_PUBLIC_ID && me.starts_with(&public)).then_some((public.len(), e))
        })
        .collect();
    let best = hits.iter().map(|(n, _)| *n).max().unwrap_or(0);
    let top: Vec<&Value> = hits
        .iter()
        .filter(|(n, _)| *n == best)
        .map(|(_, e)| *e)
        .collect();
    let entry = match top.as_slice() {
        [] => {
            view.state = Match::NotListed;
            view.detail = format!("{} nodes listed", list.len());
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
    if norm_id(node_id).len() < MIN_PUBLIC_ID {
        return match_entry(&Value::Null, node_id, now);
    }
    let url = format!("{}/fabric/nodes", gateway.trim_end_matches('/'));
    let answer = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .map_err(|e| format!("the answer is not JSON: {e}")),
        Ok(r) => Err(format!("{url} answered HTTP {}", r.status().as_u16())),
        Err(e) => Err(error_chain(&e)),
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

/// An error with its causes ("error sending request: ... : certificate
/// ..."), so a TLS or DNS problem on the operator's machine is named.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let t = s.to_string();
        if !out.contains(&t) {
            out.push_str(": ");
            out.push_str(&t);
        }
        src = s.source();
    }
    out
}

/// How long to wait before reading the list again: soon while this node is
/// not found there yet (it is listed from its first hello on, and the read
/// may have raced the connect), the normal interval once it is.
pub fn next_poll(view: &FabricView) -> Duration {
    match view.state {
        Match::Matched | Match::NoTrustFields => FABRIC_POLL_INTERVAL,
        _ => FABRIC_RETRY_INTERVAL,
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
        let node_id = crate::status::snapshot().node_id;
        let view = read_once(&client, &gateway, &node_id).await;
        delay = next_poll(&view);
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
        Match::NotListed => "the fabric answered, but this node is not in its public list \
                             yet (a node is listed while it is connected; read again within \
                             30 s)"
            .into(),
        Match::Ambiguous => "cannot be matched: more than one node in the gateway's list \
                             starts with this node's id"
            .into(),
        Match::NoTrustFields => "the gateway does not publish trust levels (older gateway)".into(),
        Match::Unreachable => format!("the fabric could not be reached ({})", v.detail),
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

/// One baseline item on the readiness meter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReadyItem {
    pub key: &'static str,
    pub label: &'static str,
    /// `pass`, `fail`, `unknown`, `not_applicable` from the last finished
    /// check; `checking` or `checked` (read, verdict not out yet) while the
    /// first one runs.
    pub state: &'static str,
}

/// One of the three conditions for Trusted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReadyStep {
    pub key: &'static str,
    pub label: &'static str,
    /// `done`, `missing`, `checking`, or `open`: an account-side step this
    /// node cannot see, so it is shown as not confirmed rather than guessed.
    pub state: &'static str,
    pub text: String,
}

/// How close this machine is to Trusted, as a meter: N of 6 baseline items,
/// the two account-side conditions as separate steps, and one sentence.
/// Shared by the dashboard, the startup summary, `status` and the TUI, so
/// they say the same words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Readiness {
    /// Baseline items that pass (not applicable counts as passing, like at
    /// the gateway).
    pub passed: usize,
    pub total: usize,
    /// `green` (all pass), `amber` (4 or 5), `red` (3 or fewer), `none`
    /// (no finished check yet). Never the only signal: `verdict` says it.
    pub band: &'static str,
    /// A check is running now (the first one, or a re-check).
    pub checking: bool,
    /// Items read so far by the running check.
    pub checked: usize,
    pub items: Vec<ReadyItem>,
    pub steps: Vec<ReadyStep>,
    /// Labels of the failing or unreadable items, lower case.
    pub missing: Vec<String>,
    pub verdict: String,
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The readiness meter. `progress` is where a running check is.
pub fn readiness(
    r: Option<&Report>,
    f: Option<&FabricView>,
    progress: &crate::security::Progress,
    now_s: u64,
) -> Readiness {
    let total = Item::ALL.len();
    let level = f
        .filter(|f| f.state == Match::Matched)
        .and_then(|f| f.trust_level.as_deref())
        .unwrap_or("unknown");
    let granted = matches!(level, "trusted" | "cloud");
    let checking = progress.running;
    let items: Vec<ReadyItem> = Item::ALL
        .iter()
        .map(|&i| ReadyItem {
            key: i.key(),
            label: i.label(),
            state: match r {
                Some(r) => verdict_word(r.verdict(i)),
                None if progress.has(i) => "checked",
                None => "checking",
            },
        })
        .collect();
    let checked = Item::ALL.iter().filter(|i| progress.has(**i)).count();
    let passed = items
        .iter()
        .filter(|i| matches!(i.state, "pass" | "not_applicable"))
        .count();
    let missing: Vec<String> = items
        .iter()
        .filter(|i| matches!(i.state, "fail" | "unknown"))
        .map(|i| lower_first(i.label))
        .collect();
    let stale =
        r.is_some_and(|r| now_s.saturating_sub(r.checked_at) > crate::security::MAX_CHECK_AGE_S);
    let band = match r {
        None => "none",
        Some(_) if passed == total && !stale => "green",
        Some(_) if passed + 2 >= total => "amber",
        Some(_) => "red",
    };
    let baseline = ReadyStep {
        key: "baseline",
        label: "Security baseline (this machine)",
        state: match r {
            None => "checking",
            Some(_) if passed == total && !stale => "done",
            Some(_) => "missing",
        },
        text: match r {
            None => format!("checking: {checked} of {total} read"),
            Some(_) if stale => {
                format!("{passed} of {total} passed, but the check is older than 6 hours")
            }
            Some(_) => format!("{passed} of {total} passed"),
        },
    };
    let (account, grant) = if granted {
        (
            (
                "done",
                "confirmed: the fabric lists this node as Trusted".to_string(),
            ),
            ("done", "granted".to_string()),
        )
    } else {
        (
            (
                "open",
                "your KMPLIFY provider account passes the identity check (trader verified, \
                 2FA on); this node cannot see it"
                    .to_string(),
            ),
            (
                "open",
                "KMPLIFY grants Trusted for this node once the account check passes".to_string(),
            ),
        )
    };
    let steps = vec![
        baseline,
        ReadyStep {
            key: "identity",
            label: "Identity check (provider account)",
            state: account.0,
            text: account.1,
        },
        ReadyStep {
            key: "grant",
            label: "Trust grant (KMPLIFY)",
            state: grant.0,
            text: grant.1,
        },
    ];
    let mut verdict = match (r, level) {
        (_, "cloud") => "Cloud node: run by KMPLIFY in its data centre.".to_string(),
        (Some(_), "trusted") => format!("Trusted: {passed} of {total} checks passed."),
        (None, _) => format!("Checking: {checked} of {total} checks read..."),
        (Some(_), _) if missing.is_empty() => format!(
            "Ready for Trusted: {passed} of {total} checks passed; still needed: identity \
             check and trust grant."
        ),
        (Some(_), _) => format!(
            "Ready for Trusted: {passed} of {total} checks passed; missing: {}.",
            missing.join(", ")
        ),
    };
    if stale && r.is_some() && level != "cloud" {
        verdict.push_str(" The last check is older than 6 hours.");
    }
    if checking && r.is_some() {
        verdict.push_str(&format!(" Re-checking now: {checked} of {total} read."));
    }
    Readiness {
        passed,
        total,
        band,
        checking,
        checked,
        items,
        steps,
        missing,
        verdict,
    }
}

/// The meter as text for a terminal: `[####--] 4/6`, one cell per item, so
/// it reads without colour.
pub fn readiness_bar(r: &Readiness) -> String {
    let cells: String = r
        .items
        .iter()
        .map(|i| match i.state {
            "pass" | "not_applicable" => '#',
            "checked" => '+',
            "checking" => '.',
            _ => '-',
        })
        .collect();
    if r.band == "none" {
        format!("[{cells}] {}/{} read", r.checked, r.total)
    } else {
        format!("[{cells}] {}/{}", r.passed, r.total)
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

    /// The live gateway's answer shape (2026-11): a bare list, `node` = the
    /// first 24 hex characters of the id. Field names as served, values fake.
    const LIVE_SHAPE: &str = include_str!("security/fixtures/fabric_nodes_2026_11.json");
    /// The full id whose 24-character prefix is the fixture's first entry.
    const LIVE_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_node_is_found_by_the_24_character_public_id_of_todays_gateway() {
        let body: Value = serde_json::from_str(LIVE_SHAPE).unwrap();
        let v = match_entry(&body, LIVE_ID, 7);
        assert_eq!(v.state, Match::Matched, "{v:?}");
        assert_eq!(v.trust_level.as_deref(), Some("community"));
        assert_eq!(v.site.as_deref(), Some("home"));
        assert_eq!(v.baseline_ok, Some(false));
        assert_eq!(v.failing, ["disk_encrypted", "screen_lock"]);
        // The other entry is someone else's node.
        let other = match_entry(&body, "fedcba9876543210fedcba9876543210", 7);
        assert_eq!(other.trust_level.as_deref(), Some("trusted"));
        // Upper case and a dashed (uuid) spelling of the same id match too.
        let dashed = "01234567-89AB-CDEF-0123-456789ABCDEF";
        assert_eq!(match_entry(&body, dashed, 7).state, Match::Matched);
        // The same list under `nodes`, as other fabric listings are shaped.
        let wrapped = json!({ "nodes": body });
        assert_eq!(match_entry(&wrapped, LIVE_ID, 7).state, Match::Matched);
        // An id this list does not carry: not listed, and the line says the
        // fabric answered (it is not a connection problem).
        let v = match_entry(&body, "99999999aaaaaaaabbbbbbbbcccccccc", 7);
        assert_eq!(v.state, Match::NotListed);
        assert!(fabric_line(&v).contains("the fabric answered"));
    }

    #[test]
    fn the_node_is_found_by_the_whole_32_character_id_of_the_newer_gateway() {
        // The same live shape, as a gateway that publishes the whole id
        // (2026-11-23) serves it: every `node` is the full 32 characters.
        let mut body: Value = serde_json::from_str(LIVE_SHAPE).unwrap();
        for e in body.as_array_mut().unwrap() {
            let full = match e["node"].as_str().unwrap() {
                "0123456789abcdef01234567" => LIVE_ID,
                _ => "fedcba9876543210fedcba9876543210",
            };
            e["node"] = json!(full);
        }
        let v = match_entry(&body, LIVE_ID, 7);
        assert_eq!(v.state, Match::Matched, "{v:?}");
        assert_eq!(v.trust_level.as_deref(), Some("community"));
        let other = match_entry(&body, "fedcba9876543210fedcba9876543210", 7);
        assert_eq!(other.trust_level.as_deref(), Some("trusted"));
        // A gateway still on 24 characters and one on 32 name the same node:
        // the 24-character entry keeps matching, and next to a 32-character
        // entry of the same node the longer one wins.
        let entry = |node: &str, level: &str| json!({"node": node, "trust_level": level});
        let mixed = json!([
            entry(&LIVE_ID[..24], "community"),
            entry(LIVE_ID, "trusted")
        ]);
        let v = match_entry(&mixed, LIVE_ID, 7);
        assert_eq!(
            (v.state, v.trust_level.as_deref()),
            (Match::Matched, Some("trusted"))
        );
        let only_24 = json!([entry(&LIVE_ID[..24], "community")]);
        assert_eq!(match_entry(&only_24, LIVE_ID, 7).state, Match::Matched);
        // A 32-character id that differs only in its last 8 characters is
        // another node, not this one.
        let lookalike = format!("{}ffffffff", &LIVE_ID[..24]);
        assert_eq!(
            match_entry(&json!([entry(&lookalike, "trusted")]), LIVE_ID, 7).state,
            Match::NotListed
        );
    }

    #[test]
    fn every_public_id_length_matches_and_the_longest_prefix_wins() {
        let entry = |node: &str, level: &str| json!({"node": node, "trust_level": level});
        for len in [8, 16, 24, 32] {
            let v = match_entry(&json!([entry(&ID[..len], "community")]), ID, 1);
            assert_eq!(v.state, Match::Matched, "prefix of {len}");
        }
        // Too short to name a node, or not a prefix of this id.
        assert_eq!(
            match_entry(&json!([entry(&ID[..7], "community")]), ID, 1).state,
            Match::NotListed
        );
        assert_eq!(
            match_entry(&json!([entry("8f2c1a5b00000000", "community")]), ID, 1).state,
            Match::NotListed
        );
        // An older 8-character entry of another node with the same first 8
        // characters does not hide this node's 24-character entry.
        let v = match_entry(
            &json!([entry(&ID[..8], "trusted"), entry(&ID[..24], "community")]),
            ID,
            1,
        );
        assert_eq!(
            (v.state, v.trust_level.as_deref()),
            (Match::Matched, Some("community"))
        );
    }

    #[test]
    fn unreachable_and_not_listed_read_differently() {
        let down = FabricView {
            state: Match::Unreachable,
            detail: "error sending request: dns error".into(),
            ..FabricView::default()
        };
        let line = fabric_line(&down);
        assert!(line.contains("could not be reached"), "{line}");
        assert!(line.contains("dns error"), "{line}");
        let absent = FabricView {
            state: Match::NotListed,
            ..FabricView::default()
        };
        assert!(!fabric_line(&absent).contains("could not be reached"));
        // Not found yet: read again soon. Found: the normal interval.
        assert_eq!(next_poll(&absent), FABRIC_RETRY_INTERVAL);
        assert_eq!(next_poll(&down), FABRIC_RETRY_INTERVAL);
        let found = FabricView {
            state: Match::Matched,
            ..FabricView::default()
        };
        assert_eq!(next_poll(&found), FABRIC_POLL_INTERVAL);
    }

    #[test]
    fn readiness_counts_the_six_items_and_names_what_is_missing() {
        use crate::security::Progress;
        let now = 1_800_000_000;
        let idle = Progress::default();
        let community = FabricView {
            state: Match::Matched,
            trust_level: Some("community".into()),
            ..FabricView::default()
        };
        // The owner's Windows PC: disk encryption and screen lock off.
        let r = readiness(Some(&report(now)), Some(&community), &idle, now);
        assert_eq!((r.passed, r.total, r.band), (4, 6, "amber"));
        assert_eq!(
            r.verdict,
            "Ready for Trusted: 4 of 6 checks passed; missing: disk encryption, screen lock."
        );
        assert_eq!(readiness_bar(&r), "[-##-##] 4/6");
        let steps: Vec<(&str, &str)> = r.steps.iter().map(|s| (s.key, s.state)).collect();
        assert_eq!(
            steps,
            [
                ("baseline", "missing"),
                ("identity", "open"),
                ("grant", "open")
            ]
        );
        // Unknown counts as failing, like at the gateway: red at 3 of 6.
        let mut worse = report(now);
        worse.firewall_on = None;
        let r = readiness(Some(&worse), None, &idle, now);
        assert_eq!((r.passed, r.band), (3, "red"));
        assert!(r
            .verdict
            .contains("missing: disk encryption, firewall, screen lock"));
        // All six: green, and the account side is what is left.
        let mut ok = report(now);
        ok.disk_encrypted = Some(true);
        ok.screen_lock = Some(true);
        let r = readiness(Some(&ok), Some(&community), &idle, now);
        assert_eq!((r.passed, r.band, r.steps[0].state), (6, "green", "done"));
        assert!(r
            .verdict
            .contains("still needed: identity check and trust grant"));
        // Old check: not green even when everything passed.
        let r = readiness(Some(&ok), None, &idle, now + 7 * 3600);
        assert_eq!(r.band, "amber");
        assert!(r.verdict.contains("older than 6 hours"));
        // Trusted at the fabric: every step done.
        let trusted = FabricView {
            trust_level: Some("trusted".into()),
            ..community
        };
        let r = readiness(Some(&ok), Some(&trusted), &idle, now);
        assert!(r.steps.iter().all(|s| s.state == "done"), "{:?}", r.steps);
        assert_eq!(r.verdict, "Trusted: 6 of 6 checks passed.");
        assert!(
            !r.verdict.contains('\u{2014}'),
            "no em dash in operator text"
        );
    }

    #[test]
    fn readiness_shows_a_running_check_item_by_item() {
        use crate::security::Progress;
        let running = Progress {
            running: true,
            started_at: 1,
            done: vec!["disk_encrypted".into(), "firewall_on".into()],
        };
        let r = readiness(None, None, &running, 10);
        assert!(r.checking);
        assert_eq!((r.checked, r.band), (2, "none"));
        assert_eq!(r.items[0].state, "checked");
        assert_eq!(r.items[1].state, "checking");
        assert_eq!(r.steps[0].state, "checking");
        assert_eq!(r.verdict, "Checking: 2 of 6 checks read...");
        assert_eq!(readiness_bar(&r), "[+.+...] 2/6 read");
        // A re-check over a finished result keeps the result and says so.
        let r = readiness(Some(&report(10)), None, &running, 10);
        assert_eq!(r.passed, 4);
        assert!(
            r.verdict.ends_with("Re-checking now: 2 of 6 read."),
            "{}",
            r.verdict
        );
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
