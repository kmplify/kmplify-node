//! Optional: what a rewards companion reports, shown next to the work.
//!
//! Lending a GPU and being paid for it are two different systems on purpose.
//! This node meters nothing it could be paid on, holds no wallet, signs no
//! transaction and knows no token. A separate program — today the Chaingence
//! payment plugin — binds this node's public id to an account and settles
//! against the fabric's own signed receipts.
//!
//! This module is the seam between the two, and it is deliberately thin:
//!
//! * the node **publishes** its public identity ([`crate::identity`]) and what
//!   it has delivered ([`crate::status::Delivered`]);
//! * if — and only if — the operator switched rewards on AND installed a
//!   companion, the node **asks** that companion for a status line and shows
//!   it in `kmplify-node rewards` and on the dashboard.
//!
//! # Why it is off until asked for, twice
//!
//! Running another program is not something a node should do because a binary
//! happened to be on `PATH`. So it takes two deliberate acts: installing the
//! companion, and `kmplify-node set rewards=on`. Until both, nothing here
//! executes, nothing is shown, and the node behaves exactly as it does today.
//!
//! # What this will never do
//!
//! Handle a key, an address or a balance as money; take an instruction from
//! the gateway about payouts; make scheduling, admission or pricing depend on
//! anything a companion says. The chain is a payout rail, never a control
//! plane — and a node that stops serving because a payment plugin is unhappy
//! would be exactly that. Full contract: `docs/REWARDS.md`.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The companion this node knows how to ask. An operator may point somewhere
/// else with `CHAINGENCE_PLUGIN`.
pub const DEFAULT_COMPANION: &str = "chaingence-plugin";

/// Ceiling on the companion's answer. It is a local process reading local
/// state; anything slower than this is a companion in trouble, and the node
/// must not wait on it.
const ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// What a companion says about this node's rewards.
///
/// Its shape is the companion's business, not the node's, so everything is
/// optional and unknown fields are ignored: a newer plugin reporting more
/// must not break an older node, and the node never has to be updated in
/// step with a payment system it does not own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// Is this node bound to an account at all.
    pub linked: bool,
    /// The account or plugin id, as the companion wants it shown.
    pub account: String,
    /// Payout destination, already redacted by the companion (it owns the
    /// address; the node neither stores nor shortens it).
    pub destination: String,
    /// Which rail and network, e.g. "evm:base-sepolia" or "sepa".
    pub rail: String,
    /// True while that rail is a test network. Shown loudly, because
    /// "earnings" that cannot be spent must never look like money.
    pub testnet: bool,
    /// Accrued but unpaid, as the companion formatted it ("12.40 EURC").
    /// A string on purpose: rounding money is not the node's job.
    pub pending: String,
    /// Paid out so far, same rules.
    pub paid: String,
    /// Anything the companion wants an operator to read — "destination not
    /// verified yet", "testnet only", a rate limit.
    pub note: String,
}

/// Where the companion is, or why there is none.
#[derive(Clone, Debug, PartialEq)]
pub enum Companion {
    /// The operator has not switched rewards on. Nothing is executed.
    Off,
    /// Switched on, but no companion binary was found.
    Missing(String),
    Found(PathBuf),
}

impl Companion {
    /// Resolve the companion for `enabled`, without running anything.
    pub fn resolve(enabled: bool) -> Self {
        if !enabled {
            return Companion::Off;
        }
        // An explicit path wins and is not searched for: an operator who says
        // exactly which binary to run gets exactly that one.
        if let Ok(p) = std::env::var("CHAINGENCE_PLUGIN") {
            let p = PathBuf::from(p.trim());
            return if p.is_file() {
                Companion::Found(p)
            } else {
                Companion::Missing(format!("CHAINGENCE_PLUGIN={} is not a file", p.display()))
            };
        }
        match crate::proc::find(DEFAULT_COMPANION) {
            Some(p) => Companion::Found(p),
            None => Companion::Missing(format!(
                "rewards are on but `{DEFAULT_COMPANION}` was not found — install it, \
                 or set CHAINGENCE_PLUGIN to its path"
            )),
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Companion::Found(p) => Some(p),
            _ => None,
        }
    }
}

/// Ask the companion how this node is doing.
///
/// Read-only by construction: one subcommand, `status --json`, with no
/// arguments derived from anything the gateway said, and no credential passed
/// to it. The companion finds the node's public identity in the node
/// directory itself — that is what [`crate::identity`] is for.
pub async fn ask(companion: &Companion, node_dir: &std::path::Path) -> Result<Report, String> {
    let path = match companion {
        Companion::Off => return Err("rewards are off (kmplify-node set rewards=on)".into()),
        Companion::Missing(why) => return Err(why.clone()),
        Companion::Found(p) => p.clone(),
    };
    let child = crate::proc::command(&path)
        .arg("status")
        .arg("--json")
        .arg("--node-dir")
        .arg(node_dir)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(ASK_TIMEOUT, child)
        .await
        .map_err(|_| format!("{} did not answer within {:?}", path.display(), ASK_TIMEOUT))?
        .map_err(|e| format!("could not run {}: {e}", path.display()))?;
    if !out.status.success() {
        // The companion's own words, not ours: it knows why it is unhappy
        // ("not logged in", "no destination yet") and an operator needs that
        // sentence rather than an exit code.
        let said = String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no reason given")
            .trim()
            .to_string();
        return Err(said);
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("{} answered with something unreadable: {e}", path.display()))
}

/// The shortest true thing: what is owed, and whether it is real money.
///
/// For the dashboard, where this shares a panel with five other lines. The
/// testnet marker survives every truncation on purpose — a balance that
/// cannot be spent must never be shown as one that can.
pub fn summary_short(report: &Report) -> String {
    if !report.linked {
        return "not linked yet".into();
    }
    let mut line = String::new();
    if report.testnet {
        line.push_str("TESTNET  ");
    }
    if report.pending.is_empty() {
        line.push_str("linked");
    } else {
        line.push_str(&format!("{} pending", report.pending));
    }
    line
}

/// One line for `kmplify-node rewards`, where there is room for all of it.
pub fn summary(report: &Report) -> String {
    if !report.linked {
        return "not linked to an account yet".into();
    }
    let mut line = String::new();
    if report.testnet {
        // First, always. An operator reading a balance must know before the
        // number whether it is real.
        line.push_str("TESTNET  ");
    }
    if !report.pending.is_empty() {
        line.push_str(&format!("{} pending", report.pending));
    }
    if !report.paid.is_empty() {
        if !report.pending.is_empty() {
            line.push_str("  ·  ");
        }
        line.push_str(&format!("{} paid", report.paid));
    }
    if line.trim().is_empty() {
        line.push_str("linked");
    }
    if !report.rail.is_empty() {
        line.push_str(&format!("  ·  {}", report.rail));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_runs_until_the_operator_says_so() {
        assert_eq!(Companion::resolve(false), Companion::Off);
        assert!(Companion::resolve(false).path().is_none());
    }

    #[tokio::test]
    async fn asking_while_off_is_an_answer_not_an_execution() {
        let err = ask(&Companion::Off, std::path::Path::new("/tmp"))
            .await
            .unwrap_err();
        assert!(err.contains("rewards are off"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_companion_says_how_to_fix_it() {
        let err = ask(
            &Companion::Missing("not found".into()),
            std::path::Path::new("/tmp"),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "not found");
    }

    #[test]
    fn a_companion_that_reports_more_than_we_know_still_parses() {
        // The plugin owns its own shape; a node must never be the reason an
        // operator cannot upgrade it.
        let r: Report = serde_json::from_str(
            r#"{"linked":true,"pending":"12.40 EURC","rail":"evm:base-sepolia",
                "testnet":true,"something_new":{"nested":1}}"#,
        )
        .unwrap();
        assert!(r.linked);
        assert_eq!(r.pending, "12.40 EURC");
        assert!(r.testnet);
    }

    #[test]
    fn a_test_network_says_so_before_it_says_a_number() {
        let line = summary(&Report {
            linked: true,
            testnet: true,
            pending: "12.40 EURC".into(),
            rail: "evm:base-sepolia".into(),
            ..Default::default()
        });
        assert!(line.starts_with("TESTNET"), "{line}");
        assert!(line.contains("12.40 EURC pending"));
    }

    #[test]
    fn the_short_form_keeps_the_warning_and_drops_the_rest() {
        let r = Report {
            linked: true,
            testnet: true,
            pending: "12.40 tEURC".into(),
            paid: "0.00 tEURC".into(),
            rail: "evm:base-sepolia".into(),
            ..Default::default()
        };
        let short = summary_short(&r);
        assert_eq!(short, "TESTNET  12.40 tEURC pending");
        assert!(short.len() < summary(&r).len());
        assert_eq!(summary_short(&Report::default()), "not linked yet");
    }

    #[test]
    fn an_unlinked_node_is_told_plainly() {
        assert_eq!(summary(&Report::default()), "not linked to an account yet");
    }

    #[test]
    fn a_linked_node_with_no_numbers_yet_still_reads() {
        let line = summary(&Report {
            linked: true,
            rail: "sepa".into(),
            ..Default::default()
        });
        assert!(line.contains("linked"));
        assert!(line.contains("sepa"));
    }
}
