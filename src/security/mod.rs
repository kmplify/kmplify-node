//! Trusted node baseline: the node checks its own security settings.
//!
//! A peer may run personal or confidential work only when it is Trusted, and
//! Trusted means two things: the provider's account passed KMPLIFY's identity
//! check, and the machine meets a baseline the node verifies itself, on every
//! OS the same way:
//!
//! | fact | what passes |
//! |---|---|
//! | `disk_encrypted` | BitLocker / FileVault / LUKS on the system and data volumes |
//! | `patch_age_days` | the last security update is at most 30 days old |
//! | `firewall_on` | Defender Firewall / Application Firewall / ufw, firewalld or an nftables input filter |
//! | `screen_lock` | not `false`: desktops lock within 15 minutes; `null` on a headless server |
//! | `ssh_password_auth` | not `true`: no password logins over SSH; `null` without sshd |
//! | `ssh_root_login` | not `true`: no `PermitRootLogin yes`; `null` without sshd |
//!
//! The check runs at start and every 3 hours and travels in the hello and
//! every pong as `security`, signed by the node's identity key (KIP-1 purpose
//! `node-security`, see PROTOCOL.md). The gateway evaluates it; this module
//! only reports. What it could not read is `null`, never a guess, and the
//! gateway fails closed on a required `null`.
//!
//! Nothing here asks for more rights than the node already has. A check that
//! needs administrator rights the node lacks answers `null` and adds a line to
//! `hints` saying so; it never prompts, never uses `sudo`, never elevates.

pub mod parse;
mod probe;

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

/// Bumped whenever a rule here changes what passes, so the gateway can tell
/// two verdicts made by different rules apart.
pub const CHECKER_VERSION: &str = "1";
/// KIP-1 purpose of the signature over the baseline facts.
pub const PURPOSE_NODE_SECURITY: &str = "node-security";
/// How old the last security update may be.
pub const MAX_PATCH_AGE_DAYS: u64 = 30;
/// How old a check may be before it no longer counts (the gateway's rule).
pub const MAX_CHECK_AGE_S: u64 = 6 * 3600;
/// How often the node checks itself.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(3 * 3600);

/// One run of the self-check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub os: String,
    pub disk_encrypted: Option<bool>,
    pub patch_age_days: Option<u64>,
    pub firewall_on: Option<bool>,
    pub screen_lock: Option<bool>,
    pub ssh_password_auth: Option<bool>,
    pub ssh_root_login: Option<bool>,
    pub checked_at: u64,
    pub checker_version: String,
    /// Why a value is null, in plain words (for example that a check needs
    /// administrator rights). Advisory text for the operator; not signed and
    /// never used for a decision.
    pub hints: Vec<String>,
    /// What each check saw, for `kmplify-node security check` only (for
    /// example which mount is not encrypted). Never sent: it can name
    /// local paths.
    #[serde(skip)]
    pub details: Vec<(Item, String)>,
}

/// The baseline items, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    DiskEncrypted,
    PatchAge,
    Firewall,
    ScreenLock,
    SshPasswordAuth,
    SshRootLogin,
}

impl Item {
    pub const ALL: [Item; 6] = [
        Item::DiskEncrypted,
        Item::PatchAge,
        Item::Firewall,
        Item::ScreenLock,
        Item::SshPasswordAuth,
        Item::SshRootLogin,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Item::DiskEncrypted => "Disk encryption",
            Item::PatchAge => "Security updates",
            Item::Firewall => "Firewall",
            Item::ScreenLock => "Screen lock",
            Item::SshPasswordAuth => "SSH password logins",
            Item::SshRootLogin => "SSH root login",
        }
    }

    /// The fact's key in the `security` object.
    pub fn key(self) -> &'static str {
        match self {
            Item::DiskEncrypted => "disk_encrypted",
            Item::PatchAge => "patch_age_days",
            Item::Firewall => "firewall_on",
            Item::ScreenLock => "screen_lock",
            Item::SshPasswordAuth => "ssh_password_auth",
            Item::SshRootLogin => "ssh_root_login",
        }
    }
}

/// How one item reads against the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// Required and not readable: fails, like the gateway.
    Unknown,
    /// Optional and absent (no desktop session, no sshd): passes.
    NotApplicable,
}

impl Report {
    pub fn verdict(&self, item: Item) -> Verdict {
        let required = |v: Option<bool>| match v {
            Some(true) => Verdict::Pass,
            Some(false) => Verdict::Fail,
            None => Verdict::Unknown,
        };
        let must_not_be_true = |v: Option<bool>| match v {
            Some(false) => Verdict::Pass,
            Some(true) => Verdict::Fail,
            None => Verdict::NotApplicable,
        };
        match item {
            Item::DiskEncrypted => required(self.disk_encrypted),
            Item::PatchAge => match self.patch_age_days {
                Some(d) if d <= MAX_PATCH_AGE_DAYS => Verdict::Pass,
                Some(_) => Verdict::Fail,
                None => Verdict::Unknown,
            },
            Item::Firewall => required(self.firewall_on),
            Item::ScreenLock => match self.screen_lock {
                Some(true) => Verdict::Pass,
                Some(false) => Verdict::Fail,
                None => Verdict::NotApplicable,
            },
            Item::SshPasswordAuth => must_not_be_true(self.ssh_password_auth),
            Item::SshRootLogin => must_not_be_true(self.ssh_root_login),
        }
    }

    /// The gateway's rule (implementation spec, section 4), so the operator
    /// sees the verdict the fabric will reach.
    pub fn baseline_ok(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) <= MAX_CHECK_AGE_S
            && Item::ALL
                .iter()
                .all(|i| matches!(self.verdict(*i), Verdict::Pass | Verdict::NotApplicable))
    }

    pub fn detail(&self, item: Item) -> Option<&str> {
        self.details
            .iter()
            .find(|(i, _)| *i == item)
            .map(|(_, d)| d.as_str())
    }

    /// Exactly the bytes the identity key signs: the baseline facts plus the
    /// node id and its public key, sorted keys, no whitespace (the gateway's
    /// `canonical_json`). `hints` is not part of it.
    pub fn canonical(&self, node_id: &str, pubkey_hex: &str) -> Vec<u8> {
        let b = |v: Option<bool>| v.map_or("null".to_string(), |v| v.to_string());
        format!(
            "{{\"checked_at\":{},\"checker_version\":{},\"disk_encrypted\":{},\"firewall_on\":{},\
             \"node_id\":{},\"os\":{},\"patch_age_days\":{},\"pubkey\":{},\"screen_lock\":{},\
             \"ssh_password_auth\":{},\"ssh_root_login\":{}}}",
            self.checked_at,
            json!(self.checker_version),
            b(self.disk_encrypted),
            b(self.firewall_on),
            json!(node_id),
            json!(self.os),
            self.patch_age_days
                .map_or("null".to_string(), |d| d.to_string()),
            json!(pubkey_hex),
            b(self.screen_lock),
            b(self.ssh_password_auth),
            b(self.ssh_root_login),
        )
        .into_bytes()
    }

    /// The `security` object of a hello or pong: the facts, the hints, and
    /// `sig` (null when this node has no identity key, which the gateway
    /// reads as unsigned and so as failing).
    pub fn fact(&self, key: Option<&crate::identity::NodeKey>, node_id: &str) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        v["sig"] = match key {
            Some(k) => json!(k.sign(
                PURPOSE_NODE_SECURITY,
                &self.canonical(node_id, &k.public_hex())
            )),
            None => Value::Null,
        };
        v
    }
}

/// Run every check now. Bounded: each probe has its own timeout, so a wedged
/// tool costs that one fact, not the report.
pub async fn check() -> Report {
    probe::collect(crate::identity::now_s()).await
}

fn cell() -> &'static RwLock<Option<Report>> {
    static LATEST: OnceLock<RwLock<Option<Report>>> = OnceLock::new();
    LATEST.get_or_init(|| RwLock::new(None))
}

/// The latest completed check, if one has finished yet.
pub fn latest() -> Option<Report> {
    cell().read().ok()?.clone()
}

fn store(r: Report) {
    if let Ok(mut c) = cell().write() {
        *c = Some(r);
    }
}

/// Start checking: now, then every `RECHECK_INTERVAL`. Idempotent. The
/// first hello does not wait for it; a hello sent before the first check
/// finished carries no `security`, and the next pong does.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async {
        loop {
            let r = check().await;
            store(r);
            tokio::time::sleep(RECHECK_INTERVAL).await;
        }
    });
}

/// How to fix a failing or unknown item, per OS. The same wording as the
/// trusted-nodes page on kmplify.io.
pub fn how_to(item: Item, os: &str) -> &'static str {
    match (item, os) {
        (Item::DiskEncrypted, "windows") => {
            "Turn on BitLocker for the system drive and every data drive: Control Panel > \
             BitLocker Drive Encryption. On Windows Home use Settings > Privacy & security > \
             Device encryption."
        }
        (Item::DiskEncrypted, "macos") => {
            "Turn on FileVault: System Settings > Privacy & Security > FileVault > Turn On. Wait \
             until encryption has finished."
        }
        (Item::DiskEncrypted, _) => {
            "Use LUKS full disk encryption for the root, swap and data partitions. On an existing \
             install this means reinstalling with disk encryption chosen in the installer, or \
             moving the data to a new LUKS volume (cryptsetup luksFormat)."
        }
        (Item::PatchAge, "windows") => {
            "Install the latest updates: Settings > Windows Update > Check for updates. Keep \
             automatic updates on."
        }
        (Item::PatchAge, "macos") => {
            "Install the latest macOS update: System Settings > General > Software Update. Turn \
             on automatic updates, including security responses and system files."
        }
        (Item::PatchAge, _) => {
            "Install the security updates: sudo apt update && sudo apt upgrade (Debian, Ubuntu) \
             or sudo dnf upgrade --security (Fedora, RHEL). Turn on automatic security updates \
             with unattended-upgrades or dnf-automatic."
        }
        (Item::Firewall, "windows") => {
            "Turn on Microsoft Defender Firewall for the domain, private and public networks: \
             Windows Security > Firewall & network protection."
        }
        (Item::Firewall, "macos") => "Turn on the firewall: System Settings > Network > Firewall.",
        (Item::Firewall, _) => {
            "Turn on a firewall that blocks incoming connections you did not allow: sudo ufw \
             enable (Debian, Ubuntu) or sudo systemctl enable --now firewalld (Fedora, RHEL)."
        }
        (Item::ScreenLock, "windows") => {
            "Lock the screen after 15 minutes at most: Settings > Personalization > Lock screen > \
             Screen saver, choose a wait of 15 minutes or less and tick \"On resume, display \
             logon screen\"."
        }
        (Item::ScreenLock, "macos") => {
            "Lock the screen after 15 minutes at most: System Settings > Lock Screen, set \"Turn \
             display off when inactive\" to 15 minutes or less and \"Require password after \
             screen saver begins or display is turned off\" to immediately."
        }
        (Item::ScreenLock, _) => {
            "Lock the screen after 15 minutes at most: in GNOME, Settings > Privacy > Screen Lock \
             (Automatic Screen Lock on, Blank Screen Delay 15 minutes or less); in KDE, System \
             Settings > Screen Locking."
        }
        (Item::SshPasswordAuth, "windows") => {
            "Allow only key logins over SSH: set PasswordAuthentication no in \
             C:\\ProgramData\\ssh\\sshd_config, then run Restart-Service sshd. If you do not need \
             SSH, stop and disable the OpenSSH Server service."
        }
        (Item::SshPasswordAuth, "macos") => {
            "Allow only key logins over SSH: set PasswordAuthentication no and \
             KbdInteractiveAuthentication no in /etc/ssh/sshd_config.d/ (a new .conf file). If \
             you do not need SSH, turn off Remote Login: System Settings > General > Sharing."
        }
        (Item::SshPasswordAuth, _) => {
            "Allow only key logins over SSH: set PasswordAuthentication no and \
             KbdInteractiveAuthentication no in /etc/ssh/sshd_config.d/ (a new .conf file), then \
             sudo systemctl restart ssh. If you do not need SSH, turn it off."
        }
        (Item::SshRootLogin, "windows") => "Windows has no root login over SSH.",
        (Item::SshRootLogin, _) => {
            "Do not allow root to log in with a password over SSH: set PermitRootLogin no (or \
             prohibit-password for keys only) in /etc/ssh/sshd_config.d/ (a new .conf file), then \
             restart sshd."
        }
    }
}

/// The checklist `kmplify-node security check` prints. `color` adds ANSI
/// green and red for a terminal.
pub fn render(r: &Report, now: u64, color: bool) -> String {
    let paint = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let mut out = format!(
        "Trusted node baseline ({}, checker {})\n\n",
        r.os, r.checker_version
    );
    for item in Item::ALL {
        let v = r.verdict(item);
        let mark = match v {
            Verdict::Pass => paint("32", "[ ok ]"),
            Verdict::Fail => paint("31", "[FAIL]"),
            Verdict::Unknown => paint("31", "[ ?? ]"),
            Verdict::NotApplicable => paint("2", "[ -- ]"),
        };
        let state = match (item, v) {
            (Item::PatchAge, _) => match r.patch_age_days {
                Some(d) => format!(
                    "last security update {d} day{} ago (at most {MAX_PATCH_AGE_DAYS})",
                    if d == 1 { "" } else { "s" }
                ),
                None => "unknown: counts as failing".into(),
            },
            (_, Verdict::Pass) => match item {
                Item::SshPasswordAuth => "off".into(),
                Item::SshRootLogin => "off".into(),
                _ => "on".into(),
            },
            (_, Verdict::Fail) => match item {
                Item::SshPasswordAuth | Item::SshRootLogin => "allowed".into(),
                _ => "off".into(),
            },
            (_, Verdict::Unknown) => "unknown: counts as failing".into(),
            (Item::ScreenLock, Verdict::NotApplicable) => {
                "not checked (no desktop session, or not readable here)".into()
            }
            (_, Verdict::NotApplicable) => "not checked (no SSH server running)".into(),
        };
        out.push_str(&format!("  {mark} {:<20} {state}\n", item.label()));
        if let Some(d) = r.detail(item) {
            out.push_str(&format!("         {d}\n"));
        }
        if matches!(v, Verdict::Fail | Verdict::Unknown) {
            out.push_str(&format!("         How to fix: {}\n", how_to(item, &r.os)));
        }
    }
    if !r.hints.is_empty() {
        out.push_str("\nNotes\n");
        for h in &r.hints {
            out.push_str(&format!("  - {h}\n"));
        }
    }
    out.push('\n');
    out.push_str(&if r.baseline_ok(now) {
        paint("32", "Result: this machine meets the trusted node baseline.")
    } else {
        paint(
            "31",
            "Result: this machine does not meet the trusted node baseline yet. Unknown counts as failing.",
        )
    });
    out.push_str(
        "\nThe node checks again every 3 hours and sends the result, signed, to the fabric.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good(now: u64) -> Report {
        Report {
            os: "linux".into(),
            disk_encrypted: Some(true),
            patch_age_days: Some(12),
            firewall_on: Some(true),
            screen_lock: None,
            ssh_password_auth: Some(false),
            ssh_root_login: Some(false),
            checked_at: now,
            checker_version: CHECKER_VERSION.into(),
            hints: vec![],
            details: vec![],
        }
    }

    #[test]
    fn the_gateways_rule_fails_closed() {
        let now = 1_800_000_000;
        assert!(good(now).baseline_ok(now));
        assert!(!good(now).baseline_ok(now + MAX_CHECK_AGE_S + 1), "stale");
        type Break = fn(&mut Report);
        let cases: [(&str, Break); 7] = [
            ("disk unknown", |r| r.disk_encrypted = None),
            ("disk off", |r| r.disk_encrypted = Some(false)),
            ("patch unknown", |r| r.patch_age_days = None),
            ("patch old", |r| r.patch_age_days = Some(31)),
            ("firewall unknown", |r| r.firewall_on = None),
            ("screen lock off", |r| r.screen_lock = Some(false)),
            ("ssh password", |r| r.ssh_password_auth = Some(true)),
        ];
        for (name, f) in cases {
            let mut r = good(now);
            f(&mut r);
            assert!(!r.baseline_ok(now), "{name}");
        }
        let mut r = good(now);
        r.ssh_root_login = Some(true);
        assert!(!r.baseline_ok(now));
        let mut r = good(now);
        r.patch_age_days = Some(30);
        r.ssh_password_auth = None;
        r.ssh_root_login = None;
        assert!(r.baseline_ok(now), "30 days, no sshd, headless");
    }

    #[test]
    fn the_signature_covers_the_facts_the_node_id_and_the_key() {
        let key = crate::identity::NodeKey::generate();
        let r = good(1_800_000_000);
        let fact = r.fact(Some(&key), "node-1");
        let sig = fact["sig"].as_str().unwrap();
        let canonical = r.canonical("node-1", &key.public_hex());
        assert!(crate::identity::verify(
            &key.public_hex(),
            PURPOSE_NODE_SECURITY,
            &canonical,
            sig
        ));
        // The canonical bytes are what the gateway's canonical_json makes of
        // the same object: sorted keys, no whitespace, null for unknown.
        let expected = format!(
            "{{\"checked_at\":1800000000,\"checker_version\":\"1\",\"disk_encrypted\":true,\
             \"firewall_on\":true,\"node_id\":\"node-1\",\"os\":\"linux\",\"patch_age_days\":12,\
             \"pubkey\":\"{}\",\"screen_lock\":null,\"ssh_password_auth\":false,\
             \"ssh_root_login\":false}}",
            key.public_hex()
        );
        assert_eq!(String::from_utf8(canonical).unwrap(), expected);
        let mut sorted: Value = serde_json::from_str(&expected).unwrap();
        assert_eq!(
            sorted.to_string(),
            expected,
            "serde_json's sorted map agrees"
        );
        sorted["node_id"] = json!("node-2");
        assert!(!crate::identity::verify(
            &key.public_hex(),
            PURPOSE_NODE_SECURITY,
            sorted.to_string().as_bytes(),
            sig
        ));
        // Another purpose's signature never verifies as this one.
        assert!(!crate::identity::verify(
            &key.public_hex(),
            "node-hello",
            &r.canonical("node-1", &key.public_hex()),
            sig
        ));
    }

    /// The gateway's known-answer test (kmplify-compute-fabric,
    /// gateway/tests/test_node_trust_placement.py,
    /// `test_signed_bytes_known_answer`), byte for byte.
    #[test]
    fn signed_bytes_match_the_gateways_known_answer() {
        let r = Report {
            checked_at: 1_763_856_000,
            ..good(0)
        };
        let pubkey = "ab".repeat(32);
        let input =
            crate::identity::signing_input(PURPOSE_NODE_SECURITY, &r.canonical("node-1", &pubkey));
        let expected = format!(
            "KMPLIFY-ID-v1/node-security\n\
             {{\"checked_at\":1763856000,\"checker_version\":\"1\",\"disk_encrypted\":true,\"firewall_on\":true,\
             \"node_id\":\"node-1\",\"os\":\"linux\",\"patch_age_days\":12,\"pubkey\":\"{pubkey}\",\
             \"screen_lock\":null,\"ssh_password_auth\":false,\"ssh_root_login\":false}}"
        );
        assert_eq!(String::from_utf8(input).unwrap(), expected);
    }

    #[test]
    fn the_sent_object_has_the_spec_fields_and_no_details() {
        let mut r = good(1_800_000_000);
        r.details
            .push((Item::DiskEncrypted, "/srv/private-path".into()));
        r.hints.push("a hint".into());
        let v = r.fact(None, "node-1");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "checked_at",
                "checker_version",
                "disk_encrypted",
                "firewall_on",
                "hints",
                "os",
                "patch_age_days",
                "screen_lock",
                "sig",
                "ssh_password_auth",
                "ssh_root_login"
            ]
        );
        assert!(v["sig"].is_null(), "no identity key, no signature");
        assert!(!v.to_string().contains("private-path"));
    }

    #[test]
    fn the_checklist_says_how_to_fix_what_fails() {
        let now = 1_800_000_000;
        let mut r = good(now);
        r.os = "windows".into();
        r.disk_encrypted = None;
        r.hints
            .push("BitLocker status needs administrator rights.".into());
        let text = render(&r, now, false);
        assert!(text.contains("[ ?? ] Disk encryption"), "{text}");
        assert!(text.contains("How to fix: Turn on BitLocker"), "{text}");
        assert!(text.contains("does not meet"), "{text}");
        assert!(text.contains("BitLocker status needs administrator rights."));
        assert!(!text.contains('\u{2014}'), "no em dash in operator text");
        for os in ["windows", "macos", "linux"] {
            for item in Item::ALL {
                assert!(!how_to(item, os).contains('\u{2014}'));
            }
        }
        let text = render(&good(now), now, false);
        assert!(text.contains("meets the trusted node baseline"), "{text}");
        assert!(!text.contains("How to fix"), "{text}");
    }
}
