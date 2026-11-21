//! KMPLIFY Credits: what the gateway says this node has earned.
//!
//! A node that hosts rented GPU sessions earns credits for the seconds the
//! renter paid for, minus the platform fee. The gateway meters, the ledger
//! books, and this module only **shows** the result: it asks the gateway the
//! node already talks to, with the node's own token, and prints the answer.
//!
//! # The node decides nothing here
//!
//! No rate, no fee and no amount is written in this file. Every number on
//! the operator's screen came from the gateway in this answer. The node holds
//! no balance, books nothing and cannot spend: credits live in the KMPLIFY
//! account the node is linked to. They are a prepaid balance for KMPLIFY
//! services, not a currency, and there is no cash payout.
//!
//! # Off means exactly as before
//!
//! The feature is switched on at the gateway. A gateway that answers
//! `enabled: false`, or one that does not know the endpoint at all, leaves
//! this node byte for byte where it was: no line in `status`, no key in
//! `status.json`, no section in `rewards`. Nothing about serving reads any
//! of this.
//!
//! # Not the rewards companion
//!
//! [`crate::rewards`] asks an optional local program about payouts on a
//! rail that program owns. This is a different thing: platform credits,
//! reported by the fabric itself, with no companion and no setting. See
//! `docs/CREDITS.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The gateway endpoint, relative to the gateway URL. Authenticated with the
/// node token as a bearer, like `/fabric/invitations`.
pub const ENDPOINT: &str = "/fabric/credits/node";

/// How often a running node asks. Earnings are booked per metering tick on
/// the server; an operator's screen does not need them faster than this.
pub const POLL: Duration = Duration::from_secs(300);

/// How often it asks a gateway that said "off" or does not know the endpoint.
pub const POLL_WHEN_OFF: Duration = Duration::from_secs(900);

/// The first question waits for the hello, so the gateway has classed this
/// node's GPU before it is asked for the class.
const FIRST_ASK_AFTER: Duration = Duration::from_secs(15);

const ASK_TIMEOUT: Duration = Duration::from_secs(8);

/// Earnings kept in `status.json`. The command `rewards` asks live and shows
/// what the gateway sent.
const KEPT_HISTORY: usize = 10;

/// One booked earning: a metered stretch of a session a renter paid for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Earning {
    /// When it was booked, as the gateway wrote it (RFC 3339).
    pub at: String,
    /// Credits this node's account received, after the fee.
    pub credits: u64,
    pub session_id: String,
    pub gpu_class: String,
    /// Paid seconds this earning covers.
    pub seconds: u64,
    /// The platform fee taken from the same seconds, in credits.
    pub fee_credits: u64,
}

/// The gateway's answer for this node.
///
/// Every field is optional and unknown fields are ignored, so a newer
/// gateway never breaks an older node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// The credit system is switched on at the gateway. `false` is the same
    /// as no answer.
    pub enabled: bool,
    /// This node is linked to a KMPLIFY account. Earnings wait until it is.
    pub linked: bool,
    /// The class the gateway put this node's GPU in (`s`, `m`, `l`, `xl`),
    /// or `None` when it is in none.
    pub gpu_class: Option<String>,
    /// The VRAM the gateway classed it on, in GB.
    pub vram_gb: Option<u64>,
    /// What a renter is charged per minute on this class.
    pub credits_per_minute: Option<u64>,
    /// The share KMPLIFY keeps, in percent.
    pub platform_fee_percent: Option<f64>,
    /// Credits earned in the current calendar month, after the fee.
    pub earned_this_month: u64,
    /// Credits earned since the node first earned anything, after the fee.
    pub earned_total: u64,
    /// Credits held for this node because it is not linked yet. They move to
    /// the account when it is.
    pub held: u64,
    /// The latest earnings, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Earning>,
}

/// What asking produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The credit system is on and this is the node's state.
    Report(Report),
    /// The gateway said `enabled: false`, or has no such endpoint. Show
    /// nothing.
    Off,
    /// No usable answer this time (network, a server error, an unreadable
    /// body). Keep what was shown before.
    Unknown(String),
}

/// Ask the gateway what this node earned.
///
/// The token goes to the gateway it was issued by and nowhere else, and never
/// over plaintext to a public address: the same check every other call with
/// the token passes.
pub async fn fetch(gateway_url: &str, token: &str) -> Outcome {
    if let Err(e) = crate::fabric_worker::check_gateway_url(gateway_url) {
        return Outcome::Unknown(e);
    }
    let client = match reqwest::Client::builder().timeout(ASK_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return Outcome::Unknown(e.to_string()),
    };
    let url = format!("{}{ENDPOINT}", gateway_url.trim().trim_end_matches('/'));
    let resp = match client.get(url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => return Outcome::Unknown(e.to_string()),
    };
    let status = resp.status().as_u16();
    match status {
        200 => {}
        // A gateway from before the credit system, or one with the routes
        // not mounted.
        404 | 405 | 410 | 501 => return Outcome::Off,
        other => return Outcome::Unknown(format!("the gateway answered {other}")),
    }
    match resp.json::<Report>().await {
        Ok(r) if r.enabled => Outcome::Report(r),
        Ok(_) => Outcome::Off,
        Err(e) => Outcome::Unknown(format!("unreadable answer: {e}")),
    }
}

/// Ask with the credential stored at `creds_path`. A node that never
/// registered has nothing to ask with, which is not an error.
pub async fn fetch_for(gateway_url: &str, creds_path: &Path) -> Outcome {
    let creds = match tokio::fs::read(creds_path).await {
        Ok(bytes) => match serde_json::from_slice::<crate::fabric_worker::Credentials>(&bytes) {
            Ok(c) => c,
            Err(_) => return Outcome::Unknown("the node credential is unreadable".into()),
        },
        Err(_) => return Outcome::Unknown("this node has not registered yet".into()),
    };
    fetch(gateway_url, &creds.token).await
}

/// Keep `status.json` current while the node runs.
///
/// Publishes the answer into the status snapshot, where `status`,
/// `status --json` and the dashboard read it. An "off" answer removes the
/// block; an answer that could not be read leaves the last one in place.
pub async fn poll_loop(
    gateway_url: String,
    creds_path: PathBuf,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut wait = FIRST_ASK_AFTER;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
        }
        if *stop.borrow() {
            return;
        }
        wait = publish(fetch_for(&gateway_url, &creds_path).await);
    }
}

/// Put one answer into the status snapshot, and say when to ask again.
fn publish(outcome: Outcome) -> Duration {
    match outcome {
        Outcome::Report(mut r) => {
            r.history.truncate(KEPT_HISTORY);
            crate::status::update(move |s| s.credits = Some(r));
            POLL
        }
        Outcome::Off => {
            crate::status::update(|s| s.credits = None);
            POLL_WHEN_OFF
        }
        // A bad minute at the gateway: what was shown stays shown.
        Outcome::Unknown(_) => POLL,
    }
}

/// Text from the gateway, made safe for a terminal: no control characters
/// (an escape sequence in a session id must not move the operator's cursor),
/// and bounded.
fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// A number without a trailing `.0`, and with at most two decimals.
fn plain(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn class_label(r: &Report) -> Option<String> {
    let class = clean(r.gpu_class.as_deref()?.trim(), 8).to_ascii_uppercase();
    if class.is_empty() {
        return None;
    }
    Some(match r.vram_gb {
        Some(gb) => format!("class {class} ({gb} GB)"),
        None => format!("class {class}"),
    })
}

/// What reaches the node per minute: the renter's rate minus the fee. Both
/// numbers are the gateway's; the node only subtracts.
fn node_share_per_minute(r: &Report) -> Option<f64> {
    let rate = r.credits_per_minute? as f64;
    let fee = r.platform_fee_percent?;
    if !(0.0..=100.0).contains(&fee) {
        return None;
    }
    Some(rate * (100.0 - fee) / 100.0)
}

/// "class M (24 GB) · 6 credits per minute · platform fee 15 % · about 5.1
/// per minute to this node", with whatever the gateway left out left out.
fn rate_line(r: &Report) -> String {
    let Some(class) = class_label(r) else {
        return "the gateway put this node in no GPU class, so it earns no session credits".into();
    };
    let mut parts = vec![class];
    if let Some(rate) = r.credits_per_minute {
        parts.push(format!("{rate} credits per minute"));
    }
    if let Some(fee) = r.platform_fee_percent {
        parts.push(format!("platform fee {} %", plain(fee)));
    }
    if let Some(share) = node_share_per_minute(r) {
        parts.push(format!("about {} per minute to this node", plain(share)));
    }
    parts.join(" · ")
}

/// The lines `kmplify-node status` adds under its own. Empty when there is
/// no report, which is what keeps the output unchanged with the system off.
pub fn status_lines(report: Option<&Report>) -> Vec<String> {
    let Some(r) = report.filter(|r| r.enabled) else {
        return Vec::new();
    };
    let mut out = vec![
        format!(
            "  credits  : {} earned this month · {} in total",
            r.earned_this_month, r.earned_total
        ),
        format!("  rate     : {}", rate_line(r)),
    ];
    out.push(if r.linked {
        "  account  : linked".to_string()
    } else if r.held > 0 {
        format!(
            "  account  : NOT LINKED: {} credits wait until it is (`kmplify-node rewards` says how)",
            r.held
        )
    } else {
        "  account  : NOT LINKED: earnings wait until it is (`kmplify-node rewards` says how)"
            .to_string()
    });
    out
}

/// One line for the dashboard's home panel.
pub fn summary_short(r: &Report) -> String {
    let mut line = format!(
        "{} this month · {} in total",
        r.earned_this_month, r.earned_total
    );
    if !r.linked {
        line.push_str(" · not linked, earnings wait");
    }
    line
}

/// The section `kmplify-node rewards` prints. `address` is this node's
/// public address (`kmpn1…`), the thing an operator pastes to link it.
pub fn detail_lines(r: &Report, address: Option<&str>) -> Vec<String> {
    let mut out = vec![
        "KMPLIFY Credits (as the gateway reports them)".to_string(),
        format!(
            "  earned   : {} this month · {} in total",
            r.earned_this_month, r.earned_total
        ),
        format!("  rate     : {}", rate_line(r)),
    ];
    if r.linked {
        out.push("  account  : linked to a KMPLIFY account".to_string());
    } else {
        out.push(
            "  account  : NOT LINKED. Earnings wait until this node is linked to a KMPLIFY account."
                .to_string(),
        );
        if r.held > 0 {
            out.push(format!(
                "  waiting  : {} credits are held for this node and move to the account once it is linked",
                r.held
            ));
        }
        out.push(match address {
            Some(a) => format!("  to link  : 1. copy this node's address: {a}"),
            None => {
                "  to link  : 1. run `kmplify-node id` and copy the address (kmpn1…)".to_string()
            }
        });
        out.push(
            "             2. in the KMPLIFY app open Settings, \"Your KMPLIFY identity\", \"My nodes\""
                .to_string(),
        );
        out.push("             3. paste the address and choose \"Claim node\"".to_string());
    }
    for e in r.history.iter().take(5) {
        out.push(format!(
            "  {:<9}: {}  session {}…  {}  {} s  +{} (fee {})",
            "earning",
            clean(&e.at, 25),
            clean(&e.session_id, 8),
            clean(&e.gpu_class, 8).to_ascii_uppercase(),
            e.seconds,
            e.credits,
            e.fee_credits
        ));
    }
    out.push(
        "  Credits are a prepaid balance for KMPLIFY services, not a currency. There is no cash payout."
            .to_string(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A small local fake of the gateway endpoint: answers every request
    /// with `status` and `body`, and hands back the request heads it saw.
    async fn fake_gateway(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read_exact(&mut byte).await.is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                let _ = tx.send(String::from_utf8_lossy(&head).to_string());
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    const LINKED: &str = r#"{
        "enabled": true, "linked": true, "gpu_class": "m", "vram_gb": 24,
        "credits_per_minute": 6, "platform_fee_percent": 15,
        "earned_this_month": 1240, "earned_total": 5678, "held": 0,
        "history": [{"at": "2026-11-21T20:14:03Z", "kind": "earning", "credits": 153,
                     "session_id": "7f3a9c1e22aa", "gpu_class": "m", "seconds": 1800,
                     "fee_credits": 27}],
        "something_new": {"nested": true}
    }"#;

    const UNLINKED: &str = r#"{
        "enabled": true, "linked": false, "gpu_class": "l", "vram_gb": 48,
        "credits_per_minute": 10, "platform_fee_percent": 15,
        "earned_this_month": 0, "earned_total": 0, "held": 420
    }"#;

    fn report(body: &str) -> Report {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn the_node_asks_its_gateway_with_its_own_token() {
        let (url, mut seen) = fake_gateway(200, LINKED).await;
        let out = fetch(&url, "tok-123").await;
        let Outcome::Report(r) = out else {
            panic!("{out:?}");
        };
        assert!(r.linked);
        assert_eq!(r.earned_this_month, 1240);
        assert_eq!(r.earned_total, 5678);
        assert_eq!(r.history.len(), 1);
        let head = seen.recv().await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /fabric/credits/node "), "{head}");
        assert!(head.contains("authorization: bearer tok-123"), "{head}");
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_gateway_url_asks_the_same_path() {
        let (url, mut seen) = fake_gateway(200, LINKED).await;
        assert!(matches!(
            fetch(&format!("{url}/"), "t").await,
            Outcome::Report(_)
        ));
        let head = seen.recv().await.unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /fabric/credits/node "), "{head}");
    }

    #[tokio::test]
    async fn a_switched_off_gateway_is_off() {
        let (url, _seen) = fake_gateway(200, r#"{"enabled": false}"#).await;
        assert_eq!(fetch(&url, "t").await, Outcome::Off);
    }

    #[tokio::test]
    async fn a_gateway_without_the_endpoint_is_off() {
        for status in [404, 405, 501] {
            let (url, _seen) = fake_gateway(status, r#"{"detail":"Not Found"}"#).await;
            assert_eq!(fetch(&url, "t").await, Outcome::Off, "{status}");
        }
    }

    #[tokio::test]
    async fn a_failing_gateway_is_unknown_not_off() {
        // A bad minute must not wipe what the operator was shown.
        let (url, _seen) = fake_gateway(503, "{}").await;
        assert!(matches!(fetch(&url, "t").await, Outcome::Unknown(_)));
        let (url, _seen) = fake_gateway(200, "<html>proxy</html>").await;
        assert!(matches!(fetch(&url, "t").await, Outcome::Unknown(_)));
        // Nothing listens here.
        assert!(matches!(
            fetch("http://127.0.0.1:1", "t").await,
            Outcome::Unknown(_)
        ));
    }

    #[tokio::test]
    async fn the_token_never_goes_to_a_public_plaintext_gateway() {
        let out = fetch("http://fabric.example.com", "t").await;
        let Outcome::Unknown(why) = out else {
            panic!("{out:?}");
        };
        assert!(why.contains("plaintext"), "{why}");
    }

    #[tokio::test]
    async fn a_node_that_never_registered_asks_nothing() {
        let (url, mut seen) = fake_gateway(200, LINKED).await;
        let missing = std::env::temp_dir().join(format!(
            "kmplify-credits-none-{}/fabric_node.json",
            std::process::id()
        ));
        assert!(matches!(
            fetch_for(&url, &missing).await,
            Outcome::Unknown(_)
        ));
        assert!(
            seen.try_recv().is_err(),
            "a request was sent without a token"
        );
    }

    #[tokio::test]
    async fn the_stored_credential_is_what_it_asks_with() {
        let (url, mut seen) = fake_gateway(200, UNLINKED).await;
        let dir =
            std::env::temp_dir().join(format!("kmplify-credits-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fabric_node.json");
        std::fs::write(&path, r#"{"node_id":"n1","token":"stored-token"}"#).unwrap();
        let out = fetch_for(&url, &path).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(out, Outcome::Report(ref r) if !r.linked && r.held == 420),
            "{out:?}"
        );
        let head = seen.recv().await.unwrap().to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer stored-token"),
            "{head}"
        );
    }

    #[test]
    fn status_shows_earnings_rate_fee_and_the_link() {
        let lines = status_lines(Some(&report(LINKED)));
        assert_eq!(
            lines,
            vec![
                "  credits  : 1240 earned this month · 5678 in total",
                "  rate     : class M (24 GB) · 6 credits per minute · platform fee 15 % · about 5.1 per minute to this node",
                "  account  : linked",
            ]
        );
    }

    #[test]
    fn status_adds_nothing_without_a_report() {
        assert!(status_lines(None).is_empty());
        // A stored block that says "off" is as good as none.
        assert!(status_lines(Some(&Report::default())).is_empty());
    }

    #[test]
    fn an_unlinked_node_is_told_that_earnings_wait_and_how_to_link() {
        let r = report(UNLINKED);
        let status = status_lines(Some(&r)).join("\n");
        assert!(status.contains("NOT LINKED"), "{status}");
        assert!(status.contains("420 credits wait"), "{status}");
        assert!(status.contains("kmplify-node rewards"), "{status}");

        let detail = detail_lines(&r, Some("kmpn1qexample")).join("\n");
        assert!(detail.contains("NOT LINKED. Earnings wait"), "{detail}");
        assert!(detail.contains("420 credits are held"), "{detail}");
        assert!(detail.contains("kmpn1qexample"), "{detail}");
        assert!(detail.contains("Claim node"), "{detail}");

        // Without a key to derive the address from, it says where to get one.
        let detail = detail_lines(&r, None).join("\n");
        assert!(detail.contains("kmplify-node id"), "{detail}");

        assert!(summary_short(&r).contains("not linked"));
    }

    #[test]
    fn a_linked_node_gets_no_linking_instructions() {
        let detail = detail_lines(&report(LINKED), Some("kmpn1qexample")).join("\n");
        assert!(detail.contains("linked to a KMPLIFY account"), "{detail}");
        assert!(!detail.contains("to link"), "{detail}");
        assert!(!detail.contains("NOT LINKED"), "{detail}");
        assert!(
            detail.contains("2026-11-21T20:14:03Z  session 7f3a9c1e…  M  1800 s  +153 (fee 27)"),
            "{detail}"
        );
        assert!(!summary_short(&report(LINKED)).contains("not linked"));
    }

    #[test]
    fn the_wording_is_a_prepaid_balance_and_never_money() {
        for body in [LINKED, UNLINKED] {
            let r = report(body);
            let mut text = detail_lines(&r, None).join("\n");
            text.push_str(&status_lines(Some(&r)).join("\n"));
            text.push_str(&summary_short(&r));
            assert!(text.contains("prepaid balance"), "{text}");
            assert!(text.contains("not a currency"), "{text}");
            assert!(text.contains("no cash payout"), "{text}");
            let lower = text.to_ascii_lowercase();
            for word in ["token", "coin", "eur", "€", "$", "wallet", "payout address"] {
                assert!(!lower.contains(word), "`{word}` in: {text}");
            }
        }
    }

    #[test]
    fn every_number_shown_came_from_the_gateway() {
        // Other rates and another fee: the node repeats them, and its own
        // share follows from them. Nothing is assumed.
        let r = Report {
            enabled: true,
            linked: true,
            gpu_class: Some("xl".into()),
            credits_per_minute: Some(40),
            platform_fee_percent: Some(12.5),
            ..Default::default()
        };
        let line = &status_lines(Some(&r))[1];
        assert_eq!(
            line,
            "  rate     : class XL · 40 credits per minute · platform fee 12.5 % · about 35 per minute to this node"
        );
        // A gateway that sends no rate and no fee gets none invented.
        let bare = Report {
            enabled: true,
            gpu_class: Some("s".into()),
            ..Default::default()
        };
        assert_eq!(status_lines(Some(&bare))[1], "  rate     : class S");
    }

    #[test]
    fn this_file_holds_no_rate_and_no_fee() {
        // The price table lives in one configuration file on the server. A
        // rate or fee written here would be a second, drifting copy.
        let src = include_str!("credits.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for line in code.lines() {
            let l = line.trim();
            if l.starts_with("//") {
                continue;
            }
            for needle in [
                "credits_per_minute:",
                "platform_fee_percent:",
                "credits_per_eur",
            ] {
                if let Some(rest) = l.split(needle).nth(1) {
                    assert!(
                        !rest.trim_start().starts_with(|c: char| c.is_ascii_digit()),
                        "a literal after `{needle}` in: {l}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_node_without_a_gpu_class_is_told_so() {
        let r = Report {
            enabled: true,
            linked: true,
            ..Default::default()
        };
        assert!(status_lines(Some(&r))[1].contains("no GPU class"));
    }

    #[test]
    fn text_from_the_gateway_cannot_drive_the_terminal() {
        let r = Report {
            enabled: true,
            linked: true,
            gpu_class: Some("m\u{1b}[2J".into()),
            history: vec![Earning {
                at: "2026\u{1b}[31m".into(),
                session_id: "\u{7}abc".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = detail_lines(&r, None).join("\n") + &status_lines(Some(&r)).join("\n");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{7}'), "{text:?}");
    }

    #[test]
    fn status_json_is_unchanged_while_there_is_no_report() {
        let snap = crate::status::Snapshot::default();
        let json = serde_json::to_value(&snap).unwrap();
        assert!(json.get("credits").is_none(), "{json}");
        // And a file written before this existed still reads.
        let old: crate::status::Snapshot = serde_json::from_str(r#"{"pid": 7}"#).unwrap();
        assert!(old.credits.is_none());
    }

    #[test]
    fn status_json_carries_the_report_when_there_is_one() {
        let snap = crate::status::Snapshot {
            credits: Some(report(LINKED)),
            ..Default::default()
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["credits"]["earned_this_month"], 1240);
        assert_eq!(json["credits"]["earned_total"], 5678);
        assert_eq!(json["credits"]["linked"], true);
        assert_eq!(json["credits"]["gpu_class"], "m");
        assert_eq!(json["credits"]["credits_per_minute"], 6);
        assert_eq!(json["credits"]["platform_fee_percent"], 15.0);
        let back: crate::status::Snapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back.credits, Some(report(LINKED)));
    }

    #[test]
    fn a_report_is_published_kept_through_a_bad_minute_and_removed_when_off() {
        let mut long = report(LINKED);
        long.history = vec![long.history[0].clone(); KEPT_HISTORY + 5];
        assert_eq!(publish(Outcome::Report(long)), POLL);
        let shown = crate::status::snapshot().credits.expect("published");
        assert_eq!(shown.earned_total, 5678);
        assert_eq!(shown.history.len(), KEPT_HISTORY);

        assert_eq!(publish(Outcome::Unknown("503".into())), POLL);
        assert!(crate::status::snapshot().credits.is_some());

        assert_eq!(publish(Outcome::Off), POLL_WHEN_OFF);
        assert!(crate::status::snapshot().credits.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_poller_stops_when_the_node_stops() {
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(poll_loop(
            "http://127.0.0.1:1".into(),
            PathBuf::from("/nonexistent/fabric_node.json"),
            rx,
        ));
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the poller ended")
            .unwrap();
    }
}
