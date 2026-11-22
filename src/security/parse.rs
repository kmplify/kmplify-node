//! Pure parsers for the baseline checks: text in, facts out.
//!
//! Nothing here starts a process or reads a file. The probes in `probe.rs`
//! collect what the system prints and hand it here, so every rule that
//! decides "encrypted" or "patched" is tested against recorded output from
//! each OS (the fixtures beside this file) rather than against whatever
//! machine happens to run `cargo test`.
//!
//! Every parser answers `None` when the text does not say. The gateway fails
//! closed on an unknown, so guessing "probably fine" here would be the one
//! way to make an unsafe machine look safe.

use serde::Deserialize;

/// Seconds a screen may stay unlocked while idle: 15 minutes, the usual
/// baseline figure (CIS benchmarks for Windows, macOS and GNOME use it).
pub const SCREEN_LOCK_MAX_S: u64 = 900;

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). No date crate: this is the only date math the node does.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// Unix seconds of `YYYY-MM-DD[T| ]+HH:MM[:SS][Z|+HHMM|+HH:MM|-…]`.
///
/// A missing offset is read as UTC. The local offset of an apt or dnf log is
/// at most a few hours, which cannot move a 30 day threshold by a whole day
/// in a way that matters, and guessing the zone would be worse.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_at(s.find(['T', ' '])?);
    let mut dp = date.split('-');
    let (y, m, d) = (dp.next()?, dp.next()?, dp.next()?);
    if dp.next().is_some() || y.len() != 4 {
        return None;
    }
    let (y, m, d) = (i64::from(num(y)?), num(m)?, num(d)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let rest = rest.trim_start_matches(['T', ' ']);
    // Split the clock from an offset.
    let (clock, offset_s) = match rest.find(['Z', '+', '-']) {
        Some(i) => {
            let (c, o) = rest.split_at(i);
            (c, parse_offset(o)?)
        }
        None => (rest, 0),
    };
    let mut cp = clock.trim().split(':');
    let hh = num(cp.next()?)?;
    let mm = num(cp.next()?)?;
    let ss = match cp.next() {
        // Fractions of a second are dropped.
        Some(v) => num(v.split('.').next()?)?,
        None => 0,
    };
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    Some(days * 86_400 + i64::from(hh * 3600 + mm * 60 + ss) - offset_s)
}

fn parse_offset(o: &str) -> Option<i64> {
    if o == "Z" {
        return Some(0);
    }
    let sign = match o.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = o[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 {
        return None;
    }
    let h = i64::from(num(&digits[..2])?);
    let m = i64::from(num(&digits[2..])?);
    Some(sign * (h * 3600 + m * 60))
}

/// Whole days from `then` to `now`; 0 when the clock says `then` is ahead.
pub fn age_days(then: i64, now: i64) -> u64 {
    u64::try_from((now - then).max(0) / 86_400).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

/// `fdesetup status`. "On" only when FileVault says it is on and finished:
/// a volume that is still encrypting is not yet protected.
pub fn filevault(stdout: &str) -> Option<bool> {
    let t = stdout.to_ascii_lowercase();
    if t.contains("encryption in progress") || t.contains("decryption in progress") {
        return Some(false);
    }
    if t.contains("filevault is on") {
        return Some(true);
    }
    if t.contains("filevault is off") {
        return Some(false);
    }
    None
}

/// `socketfilterfw --getglobalstate`: "(State = 1)" is on, "(State = 2)" is
/// "block all incoming", "(State = 0)" is off.
pub fn app_firewall(stdout: &str) -> Option<bool> {
    let t = stdout.to_ascii_lowercase();
    if let Some(i) = t.find("state = ") {
        return match t[i + 8..].chars().next()? {
            '1' | '2' => Some(true),
            '0' => Some(false),
            _ => None,
        };
    }
    if t.contains("firewall is enabled") || t.contains("block all") {
        return Some(true);
    }
    if t.contains("firewall is disabled") {
        return Some(false);
    }
    None
}

#[derive(Deserialize)]
struct InstallHistory {
    #[serde(rename = "SPInstallHistoryDataType")]
    items: Vec<InstallItem>,
}

#[derive(Deserialize)]
struct InstallItem {
    #[serde(rename = "_name", default)]
    name: String,
    #[serde(default)]
    install_date: String,
    #[serde(default)]
    package_source: String,
}

/// Is this install-history entry a security update of macOS itself?
///
/// Counted: macOS releases ("macOS 26.7.3"), Rapid Security Responses and
/// Background Security Improvements. Not counted: XProtect, MRT and the
/// other malware-definition feeds, which update every few days and would
/// make a machine whose OS has not been patched in months look current, and
/// apps (Safari, GarageBand) and anything not from Apple.
pub fn is_macos_security_update(name: &str, package_source: &str) -> bool {
    if package_source != "package_source_apple" {
        return false;
    }
    let n = name.to_ascii_lowercase();
    let definitions = ["xprotect", "mrt", "gatekeeper", "configdata", "rosetta"];
    if definitions.iter().any(|d| n.contains(d)) {
        return false;
    }
    n.starts_with("macos ") || n.contains("security response") || n.contains("security improvement")
}

/// `system_profiler SPInstallHistoryDataType -json`: the time of the newest
/// macOS security update, as unix seconds.
pub fn macos_last_security_update(json: &str) -> Option<i64> {
    // system_profiler may print warnings around the document.
    let start = json.find('{')?;
    let end = json.rfind('}')?;
    let h: InstallHistory = serde_json::from_str(json.get(start..=end)?).ok()?;
    h.items
        .iter()
        .filter(|i| is_macos_security_update(&i.name, &i.package_source))
        .filter_map(|i| parse_datetime(&i.install_date))
        .max()
}

/// What `sysadminctl -screenLock status` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockDelay {
    /// No password is asked when the screen saver starts or the display sleeps.
    Off,
    /// A password is asked this many seconds after that.
    After(u64),
}

/// `sysadminctl -screenLock status` (it writes to stderr): "screenLock delay
/// is immediate", "screenLock delay is 300 seconds", "screenLock is off".
pub fn sysadminctl_screen_lock(text: &str) -> Option<LockDelay> {
    let t = text.to_ascii_lowercase();
    let i = t.find("screenlock")?;
    let rest = &t[i..];
    if rest.starts_with("screenlock is off") {
        return Some(LockDelay::Off);
    }
    let after = rest.strip_prefix("screenlock delay is ")?;
    if after.starts_with("immediate") {
        return Some(LockDelay::After(0));
    }
    let n: String = after.chars().take_while(char::is_ascii_digit).collect();
    n.parse().ok().map(LockDelay::After)
}

/// `defaults -currentHost read com.apple.screensaver idleTime`: seconds of
/// idle time before the screen saver starts; 0 is never.
pub fn defaults_integer(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `pmset -g`: the `displaysleep` line, in minutes; 0 is never.
pub fn pmset_display_sleep(text: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next()? == "displaysleep").then_some(())?;
        w.next()?.parse().ok()
    })
}

/// Does this Mac lock within `SCREEN_LOCK_MAX_S` of going idle?
///
/// The screen locks when the screen saver starts or the display sleeps,
/// whichever comes first, plus the password delay. A trigger that is unknown
/// (the key was never written) is not assumed to be either short or long.
pub fn macos_screen_lock(
    delay: Option<LockDelay>,
    screensaver_idle_s: Option<u64>,
    display_sleep_min: Option<u64>,
) -> Option<bool> {
    let delay = match delay? {
        LockDelay::Off => return Some(false),
        LockDelay::After(d) => d,
    };
    let triggers = [screensaver_idle_s, display_sleep_min.map(|m| m * 60)];
    let first = triggers.iter().flatten().filter(|t| **t > 0).min().copied();
    if let Some(start) = first {
        if start + delay <= SCREEN_LOCK_MAX_S {
            return Some(true);
        }
    }
    if triggers.iter().any(Option::is_none) {
        return None;
    }
    Some(false)
}

// ---------------------------------------------------------------------------
// Windows: one PowerShell document (see probe.rs for the script)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WindowsFacts {
    pub system_drive: Option<String>,
    pub volumes: Vec<WinVolume>,
    pub firewall: Vec<WinFirewallProfile>,
    pub hotfixes: Vec<WinHotfix>,
    pub wu_history: Vec<WinUpdate>,
    pub inactivity_timeout: Option<serde_json::Value>,
    pub screensaver: WinScreensaver,
    /// The sshd service's status ("Running", "Stopped") or null when absent.
    pub sshd: Option<String>,
    pub is_admin: bool,
    pub is_system: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WinVolume {
    pub drive: String,
    pub status: Option<i64>,
    /// "wmi" (Win32_EncryptableVolume.ProtectionStatus, needs administrator
    /// rights), "shell" (the System.Volume.BitLockerProtection property, any
    /// user), or "none".
    pub source: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WinFirewallProfile {
    pub name: String,
    pub enabled: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WinHotfix {
    pub id: String,
    pub description: String,
    pub installed: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WinUpdate {
    pub result: i64,
    pub operation: i64,
    pub date: Option<i64>,
    /// Category ids (GUIDs): language independent, unlike the titles.
    pub categories: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WinScreensaver {
    pub active: Option<String>,
    pub secure: Option<String>,
    pub timeout: Option<String>,
    pub policy_active: Option<String>,
    pub policy_secure: Option<String>,
    pub policy_timeout: Option<String>,
}

/// The Windows Update classification "Security Updates". Monthly cumulative
/// updates carry it; Defender definitions ("Definition Updates") do not.
pub const WU_SECURITY_UPDATES: &str = "0fa1201d-4330-4fa8-8ae9-b877473b6441";

impl WindowsFacts {
    pub fn parse(json: &str) -> Option<Self> {
        let start = json.find('{')?;
        let end = json.rfind('}')?;
        serde_json::from_str(json.get(start..=end)?).ok()
    }

    /// BitLocker on every fixed volume. `Some(true)` only when each one reads
    /// as protected; one unprotected volume is `Some(false)`; a volume whose
    /// state could not be read makes the whole answer unknown.
    ///
    /// Win32_EncryptableVolume.ProtectionStatus: 0 off, 1 on, 2 unknown.
    /// System.Volume.BitLockerProtection: 1 on, 2 off, 3 encrypting,
    /// 4 decrypting, 5 suspended, 6 on (locked), 8 waiting for activation,
    /// 0 not encryptable. Encrypting, suspended and waiting are not protected.
    pub fn bitlocker(&self) -> (Option<bool>, Vec<String>) {
        if self.volumes.is_empty() {
            return (None, Vec::new());
        }
        let mut plain = Vec::new();
        let mut unknown = false;
        for v in &self.volumes {
            let on = match (v.source.as_str(), v.status) {
                ("wmi", Some(1)) => Some(true),
                ("wmi", Some(0)) => Some(false),
                ("shell", Some(1 | 6)) => Some(true),
                ("shell", Some(0 | 2 | 3 | 4 | 5 | 8)) => Some(false),
                _ => None,
            };
            match on {
                Some(true) => {}
                Some(false) => plain.push(v.drive.clone()),
                None => unknown = true,
            }
        }
        if !plain.is_empty() {
            (Some(false), plain)
        } else if unknown {
            (None, plain)
        } else {
            (Some(true), plain)
        }
    }

    /// Newest installed security update: Windows Update history entries in
    /// the "Security Updates" classification that installed successfully,
    /// and hotfixes Windows itself labels "Security Update".
    pub fn last_security_update(&self) -> Option<i64> {
        let from_history = self
            .wu_history
            .iter()
            // ResultCode 2 succeeded, 3 succeeded with errors; Operation 1 install.
            .filter(|u| matches!(u.result, 2 | 3) && u.operation == 1)
            .filter(|u| {
                u.categories
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(WU_SECURITY_UPDATES))
            })
            .filter_map(|u| u.date);
        let from_hotfix = self
            .hotfixes
            .iter()
            .filter(|h| h.description.eq_ignore_ascii_case("Security Update"))
            .filter_map(|h| h.installed);
        from_history.chain(from_hotfix).max()
    }

    /// Defender Firewall on in the domain, private and public profiles
    /// (effective policy, so a group policy that turns one off counts).
    pub fn firewall_on(&self) -> (Option<bool>, Vec<String>) {
        if self.firewall.is_empty() {
            return (None, Vec::new());
        }
        let off: Vec<String> = self
            .firewall
            .iter()
            .filter(|p| !p.enabled.eq_ignore_ascii_case("true"))
            .map(|p| p.name.clone())
            .collect();
        (Some(off.is_empty()), off)
    }

    /// Locks within `SCREEN_LOCK_MAX_S`: the machine inactivity limit
    /// (policy InactivityTimeoutSecs), or a password-protected screen saver,
    /// policy values first. Under the SYSTEM account the per-user values are
    /// the service account's, not the person's, so only the machine policy
    /// can answer there.
    pub fn screen_lock(&self) -> Option<bool> {
        let inactivity = self
            .inactivity_timeout
            .as_ref()
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()));
        if inactivity.is_some_and(|s| (1..=SCREEN_LOCK_MAX_S).contains(&s)) {
            return Some(true);
        }
        let s = &self.screensaver;
        let pick = |policy: &Option<String>, user: &Option<String>| {
            let v = policy.as_deref().filter(|v| !v.trim().is_empty());
            let v = if self.is_system {
                v
            } else {
                v.or(user.as_deref())
            };
            v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        let active = pick(&s.policy_active, &s.active);
        let secure = pick(&s.policy_secure, &s.secure);
        let timeout = pick(&s.policy_timeout, &s.timeout);
        if active.is_none() && secure.is_none() && timeout.is_none() {
            // Nothing readable. For a person's own account that means "never
            // configured", which on Windows is no screen saver: off.
            return if self.is_system { None } else { Some(false) };
        }
        let timeout: Option<u64> = timeout.and_then(|t| t.parse().ok());
        Some(
            active.as_deref() == Some("1")
                && secure.as_deref() == Some("1")
                && timeout.is_some_and(|t| (1..=SCREEN_LOCK_MAX_S).contains(&t)),
        )
    }

    pub fn sshd_running(&self) -> bool {
        self.sshd
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("running"))
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Lsblk {
    blockdevices: Vec<BlockDev>,
}

#[derive(Debug, Deserialize)]
struct BlockDev {
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    children: Vec<BlockDev>,
}

/// What lsblk says about encryption at rest.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinuxDisks {
    /// Mounts that sit on a dm-crypt (LUKS) device.
    pub encrypted: Vec<String>,
    /// Mounts that do not.
    pub plain: Vec<String>,
    /// Whether `/` was among the block-device mounts at all.
    pub root_seen: bool,
}

impl LinuxDisks {
    /// `Some(true)` when `/` and every other system or data mount is on
    /// dm-crypt, `Some(false)` when one is not, `None` when `/` is not on a
    /// block device lsblk knows (a container, ZFS, a network root).
    pub fn verdict(&self) -> Option<bool> {
        if !self.plain.is_empty() {
            return Some(false);
        }
        self.root_seen.then_some(true)
    }
}

/// Mounts that carry no user data and are unencrypted by design: the boot
/// and EFI partitions the firmware has to read, and read-only images.
fn exempt_mount(mount: &str, fstype: Option<&str>) -> bool {
    matches!(mount, "/boot" | "/boot/efi" | "/efi" | "/boot/firmware")
        || mount.starts_with("/snap/")
        || matches!(fstype, Some("squashfs" | "iso9660" | "udf" | "erofs"))
}

/// `lsblk -J -o NAME,TYPE,MOUNTPOINT,FSTYPE` (or with MOUNTPOINTS). A mount
/// counts as encrypted when a device of type `crypt` is between it and the
/// disk: LUKS directly, LVM on LUKS, or LUKS on RAID. Swap counts as a data
/// volume (it holds memory pages); zram swap is RAM and is skipped.
pub fn lsblk_disks(json: &str) -> Option<LinuxDisks> {
    let l: Lsblk = serde_json::from_str(json).ok()?;
    let mut out = LinuxDisks::default();
    fn walk(d: &BlockDev, under_crypt: bool, out: &mut LinuxDisks) {
        let crypt = under_crypt || d.kind.as_deref() == Some("crypt");
        let mut mounts: Vec<&str> = d.mountpoints.iter().flatten().map(String::as_str).collect();
        if let Some(m) = d.mountpoint.as_deref() {
            if !mounts.contains(&m) {
                mounts.push(m);
            }
        }
        for m in mounts {
            if m.is_empty() || exempt_mount(m, d.fstype.as_deref()) {
                continue;
            }
            if m == "[SWAP]" && d.name.starts_with("zram") {
                continue;
            }
            if m == "/" {
                out.root_seen = true;
            }
            let label = m.to_string();
            let list = if crypt {
                &mut out.encrypted
            } else {
                &mut out.plain
            };
            if !list.contains(&label) {
                list.push(label);
            }
        }
        for c in &d.children {
            walk(c, crypt, out);
        }
    }
    for d in &l.blockdevices {
        walk(d, false, &mut out);
    }
    // A mount seen through two paths (RAID members) counts once, and
    // encrypted wins only if it is encrypted on every path.
    out.encrypted.retain(|m| !out.plain.contains(m));
    Some(out)
}

/// `/var/log/apt/history.log`: the start of the newest transaction that
/// upgraded packages. apt does not label security updates, so this is the
/// last upgrade of anything; `apt upgrade` and unattended-upgrades both
/// install every pending security update in that transaction.
pub fn apt_last_upgrade(history: &str) -> Option<i64> {
    let mut newest = None;
    let mut start: Option<i64> = None;
    let mut upgraded = false;
    let mut flush = |start: &mut Option<i64>, upgraded: &mut bool| {
        if *upgraded {
            if let Some(t) = *start {
                newest = Some(newest.map_or(t, |n: i64| n.max(t)));
            }
        }
        *start = None;
        *upgraded = false;
    };
    for line in history.lines() {
        if let Some(d) = line.strip_prefix("Start-Date:") {
            flush(&mut start, &mut upgraded);
            start = parse_datetime(&d.split_whitespace().collect::<Vec<_>>().join(" "));
        } else if line.starts_with("Upgrade:") {
            upgraded = true;
        }
    }
    flush(&mut start, &mut upgraded);
    newest
}

/// `dnf history list` (dnf 4 with `|` columns, dnf 5 with spaces): the
/// newest transaction whose actions include an upgrade. The action column
/// decides where it has a value; the command line only where it is empty.
pub fn dnf_last_upgrade(text: &str) -> Option<i64> {
    text.lines()
        .filter_map(|line| {
            let bytes = line.as_bytes();
            // Find "YYYY-MM-DD HH:MM" anywhere in the line.
            let at = (0..bytes.len().saturating_sub(15)).find(|&i| {
                let w = &bytes[i..i + 16];
                w[..4].iter().all(u8::is_ascii_digit)
                    && w[4] == b'-'
                    && w[7] == b'-'
                    && w[10] == b' '
                    && w[13] == b':'
            })?;
            let tail = &line[at..];
            let mut parts = tail.split_whitespace();
            let date = parts.next()?;
            let clock = parts.next()?;
            let ts = parse_datetime(&format!("{date} {clock}"))?;
            let actions = parts.collect::<Vec<_>>().join(" ");
            let upgraded = actions
                .split(|c: char| c == ',' || c == '|' || c.is_whitespace())
                .any(|a| matches!(a, "U" | "Upgrade" | "Upgraded" | "Update"));
            // dnf 5 leaves Action(s) empty in `history list`; there the
            // command line is all there is, and only these verbs upgrade.
            let command_upgrades = !actions.chars().any(char::is_alphabetic)
                && line[..at]
                    .split(|c: char| c == '|' || c.is_whitespace())
                    .any(|w| matches!(w, "upgrade" | "update" | "distro-sync" | "upgrade-minimal"));
            (upgraded || command_upgrades).then_some(ts)
        })
        .max()
}

/// `/var/log/pacman.log`: the newest `[ALPM] upgraded` line.
pub fn pacman_last_upgrade(log: &str) -> Option<i64> {
    log.lines()
        .filter(|l| l.contains("[ALPM] upgraded "))
        .filter_map(|l| parse_datetime(l.strip_prefix('[')?.split(']').next()?))
        .max()
}

/// `/etc/ufw/ufw.conf`: `ENABLED=yes`.
pub fn ufw_enabled(conf: &str) -> Option<bool> {
    conf.lines().find_map(|l| {
        let v = l.trim().strip_prefix("ENABLED=")?;
        Some(
            v.trim()
                .trim_matches(['"', '\''])
                .eq_ignore_ascii_case("yes"),
        )
    })
}

/// Does an nftables ruleset (`nft list ruleset`, or an `nftables.conf`) drop
/// unsolicited incoming traffic? Yes when a chain hooked on `input` has
/// `policy drop`, or ends with a bare `drop` / `reject`. A ruleset that
/// merely exists is not a firewall: Debian's stock nftables.conf hooks input
/// with policy accept.
pub fn nft_filters_input(ruleset: &str) -> bool {
    let mut chains: Vec<Vec<String>> = Vec::new();
    let mut current: Option<(Vec<String>, i32)> = None;
    for raw in ruleset.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if let Some((body, depth)) = current.as_mut() {
            *depth += line.matches('{').count() as i32;
            *depth -= line.matches('}').count() as i32;
            if *depth <= 0 {
                let done = std::mem::take(body);
                chains.push(done);
                current = None;
            } else if !line.is_empty() {
                body.push(line.to_string());
            }
            continue;
        }
        if line.starts_with("chain ") && line.ends_with('{') {
            current = Some((Vec::new(), 1));
        }
    }
    chains.iter().any(|body| {
        let hooked = body
            .iter()
            .any(|l| l.contains("type filter") && l.contains("hook input"));
        if !hooked {
            return false;
        }
        if body.iter().any(|l| {
            l.replace(';', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|w| w == ["policy", "drop"])
        }) {
            return true;
        }
        let last_rule = body
            .iter()
            .rev()
            .find(|l| !l.starts_with("type ") && !l.starts_with("policy "));
        last_rule.is_some_and(|r| {
            let words: Vec<&str> = r
                .split_whitespace()
                .filter(|w| {
                    !matches!(*w, "counter" | "packets" | "bytes") && w.parse::<u64>().is_err()
                })
                .collect();
            matches!(words.first(), Some(&"drop") | Some(&"reject"))
        })
    })
}

/// GNOME: `gsettings get org.gnome.desktop.screensaver lock-enabled`
/// ("true"), `… org.gnome.desktop.session idle-delay` ("uint32 300", 0 is
/// never) and `… org.gnome.desktop.screensaver lock-delay` ("uint32 0").
pub fn gnome_screen_lock(lock_enabled: &str, idle_delay: &str, lock_delay: &str) -> Option<bool> {
    let enabled = match lock_enabled.trim() {
        "true" => true,
        "false" => return Some(false),
        _ => return None,
    };
    let last_num = |s: &str| -> Option<u64> { s.split_whitespace().last()?.parse().ok() };
    let idle = last_num(idle_delay)?;
    let delay = last_num(lock_delay).unwrap_or(0);
    Some(enabled && idle > 0 && idle + delay <= SCREEN_LOCK_MAX_S)
}

/// KDE Plasma: `kreadconfig6 --file kscreenlockerrc --group Daemon --key
/// Autolock` / `Timeout` (minutes) / `LockGrace` (seconds). An empty answer is
/// KDE's default: Autolock on, 5 minutes, 5 seconds grace.
pub fn kde_screen_lock(autolock: &str, timeout_min: &str, grace_s: &str) -> Option<bool> {
    let autolock = match autolock.trim() {
        "" | "true" => true,
        "false" => false,
        _ => return None,
    };
    let timeout: u64 = match timeout_min.trim() {
        "" => 5,
        t => t.parse().ok()?,
    };
    let grace: u64 = match grace_s.trim() {
        "" => 5,
        g => g.parse().ok()?,
    };
    Some(autolock && timeout > 0 && timeout * 60 + grace <= SCREEN_LOCK_MAX_S)
}

// ---------------------------------------------------------------------------
// sshd (every OS)
// ---------------------------------------------------------------------------

/// The sshd settings the baseline reads, with OpenSSH's own defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sshd {
    pub password_authentication: bool,
    pub kbd_interactive: bool,
    pub use_pam: bool,
    pub permit_root_login: String,
    /// A `Match` block turns password logins on for someone.
    pub match_password: bool,
    /// A `Match` block allows root logins for someone.
    pub match_root: bool,
}

impl Default for Sshd {
    fn default() -> Self {
        Self {
            password_authentication: true,
            kbd_interactive: true,
            use_pam: false,
            permit_root_login: "prohibit-password".into(),
            match_password: false,
            match_root: false,
        }
    }
}

impl Sshd {
    /// Can someone log in with a password? PasswordAuthentication, or
    /// keyboard-interactive through PAM (which asks for the same password).
    /// Windows' OpenSSH has no PAM.
    pub fn password_login(&self, windows: bool) -> bool {
        self.password_authentication
            || self.match_password
            || (!windows && self.use_pam && self.kbd_interactive)
    }

    /// `PermitRootLogin yes`: root may log in by any method sshd allows.
    /// `prohibit-password` (keys only), `forced-commands-only` and `no` pass.
    pub fn root_login(&self) -> bool {
        self.permit_root_login == "yes" || self.match_root
    }
}

fn yes(v: &str) -> bool {
    v.eq_ignore_ascii_case("yes")
}

/// `sshd -T`: the effective global configuration, one lowercase keyword and
/// value per line. `None` if none of the keywords is there (sshd refused).
pub fn sshd_t(text: &str) -> Option<Sshd> {
    let mut s = Sshd::default();
    let mut seen = false;
    for line in text.lines() {
        let mut w = line.split_whitespace();
        let (Some(k), Some(v)) = (w.next(), w.next()) else {
            continue;
        };
        match k {
            "passwordauthentication" => s.password_authentication = yes(v),
            "kbdinteractiveauthentication" | "challengeresponseauthentication" => {
                s.kbd_interactive = yes(v)
            }
            "usepam" => s.use_pam = yes(v),
            "permitrootlogin" => s.permit_root_login = v.to_ascii_lowercase(),
            _ => continue,
        }
        seen = true;
    }
    seen.then_some(s)
}

/// An `sshd_config`, read the way sshd reads it: the FIRST value of a
/// keyword wins, keywords are case-insensitive, `Include` is expanded in
/// place (through `include`, which returns the contents of every file a
/// pattern names), and what follows a `Match` line applies only to some
/// connections.
///
/// Inside a `Match` block nothing changes the global values; a block that
/// turns password logins or root logins on is remembered, because it does so
/// for someone. Once a `Match` has been seen, later lines are treated as
/// conditional even after the included file that opened it ends: OpenSSH
/// versions disagree there, and the stricter reading can only make the
/// answer "password logins possible", never hide them.
pub fn sshd_config(main: &str, include: &dyn Fn(&str) -> Vec<String>) -> Sshd {
    #[derive(Default)]
    struct Seen {
        password: bool,
        kbd: bool,
        pam: bool,
        root: bool,
    }
    fn read(
        text: &str,
        include: &dyn Fn(&str) -> Vec<String>,
        depth: u32,
        in_match: &mut bool,
        s: &mut Sshd,
        seen: &mut Seen,
    ) {
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = match line.find(|c: char| c.is_whitespace() || c == '=') {
                Some(i) => (
                    &line[..i],
                    line[i..].trim_start_matches(|c: char| c.is_whitespace() || c == '='),
                ),
                None => (line, ""),
            };
            let k = k.to_ascii_lowercase();
            let v = v.split_whitespace().next().unwrap_or("").trim_matches('"');
            match k.as_str() {
                "include" => {
                    if depth < 8 {
                        for pattern in line[k.len()..].split_whitespace() {
                            for body in include(pattern.trim_matches('"')) {
                                read(&body, include, depth + 1, in_match, s, seen);
                            }
                        }
                    }
                }
                "match" => *in_match = true,
                "passwordauthentication" if *in_match => s.match_password |= yes(v),
                "kbdinteractiveauthentication" | "challengeresponseauthentication" if *in_match => {
                    s.match_password |= yes(v) && s.use_pam
                }
                "permitrootlogin" if *in_match => s.match_root |= v.eq_ignore_ascii_case("yes"),
                "passwordauthentication" if !seen.password => {
                    seen.password = true;
                    s.password_authentication = yes(v);
                }
                "kbdinteractiveauthentication" | "challengeresponseauthentication" if !seen.kbd => {
                    seen.kbd = true;
                    s.kbd_interactive = yes(v);
                }
                "usepam" if !seen.pam && !*in_match => {
                    seen.pam = true;
                    s.use_pam = yes(v);
                }
                "permitrootlogin" if !seen.root => {
                    seen.root = true;
                    s.permit_root_login = v.to_ascii_lowercase();
                }
                _ => {}
            }
        }
    }
    let mut s = Sshd::default();
    let mut in_match = false;
    let mut seen = Seen::default();
    read(main, include, 0, &mut in_match, &mut s, &mut seen);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn dates_parse_in_every_form_the_logs_use() {
        assert_eq!(parse_datetime("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_datetime("2026-11-14T14:52:10Z"), Some(1_794_667_930));
        assert_eq!(parse_datetime("2026-11-14 14:52:10"), Some(1_794_667_930));
        assert_eq!(parse_datetime("2026-11-14 14:52"), Some(1_794_667_920));
        assert_eq!(
            parse_datetime("2026-11-14T15:52:10+0100"),
            Some(1_794_667_930)
        );
        assert_eq!(
            parse_datetime("2026-11-14T15:52:10+01:00"),
            Some(1_794_667_930)
        );
        assert_eq!(parse_datetime("2024-02-29 00:00:00"), Some(1_709_164_800));
        assert_eq!(parse_datetime("not a date"), None);
        assert_eq!(parse_datetime("2026-13-01 00:00"), None);
        assert_eq!(age_days(0, 3 * DAY + 5), 3);
        assert_eq!(
            age_days(10 * DAY, 0),
            0,
            "a clock behind the log is not a negative age"
        );
    }

    // --- macOS -----------------------------------------------------------

    #[test]
    fn filevault_on_off_and_in_progress() {
        assert_eq!(filevault("FileVault is On.\n"), Some(true));
        assert_eq!(filevault("FileVault is Off.\n"), Some(false));
        assert_eq!(
            filevault(include_str!("fixtures/macos_fdesetup_encrypting.txt")),
            Some(false)
        );
        assert_eq!(filevault(""), None);
    }

    #[test]
    fn application_firewall_states() {
        assert_eq!(
            app_firewall("Firewall is enabled. (State = 1)\n"),
            Some(true)
        );
        assert_eq!(
            app_firewall(
                "Firewall is blocking all non-essential incoming connections. (State = 2)"
            ),
            Some(true)
        );
        assert_eq!(
            app_firewall("Firewall is disabled. (State = 0)\n"),
            Some(false)
        );
        assert_eq!(app_firewall("socketfilterfw: unknown option"), None);
    }

    #[test]
    fn the_newest_macos_security_update_ignores_definitions_and_apps() {
        let json = include_str!("fixtures/macos_install_history.json");
        // macOS 26.7.3 at 2026-11-14T14:52:10Z, not the XProtect feed of the
        // 21st, Safari or a third-party app.
        assert_eq!(macos_last_security_update(json), Some(1_794_667_930));
        assert!(is_macos_security_update(
            "Background Security Improvement 26.7.3 (a)",
            "package_source_apple"
        ));
        assert!(!is_macos_security_update(
            "XProtectPayloads",
            "package_source_apple"
        ));
        assert!(!is_macos_security_update(
            "macOS Helper",
            "package_source_other"
        ));
        assert_eq!(
            macos_last_security_update("{\"SPInstallHistoryDataType\":[]}"),
            None
        );
        assert_eq!(macos_last_security_update("garbage"), None);
    }

    #[test]
    fn sysadminctl_screen_lock_forms() {
        let stderr =
            "2026-11-23 00:21:09.704 sysadminctl[62610:43733567] screenLock delay is 300 seconds\n";
        assert_eq!(sysadminctl_screen_lock(stderr), Some(LockDelay::After(300)));
        assert_eq!(
            sysadminctl_screen_lock("sysadminctl[1:2] screenLock delay is immediate"),
            Some(LockDelay::After(0))
        );
        assert_eq!(
            sysadminctl_screen_lock("sysadminctl[1:2] screenLock is off"),
            Some(LockDelay::Off)
        );
        assert_eq!(sysadminctl_screen_lock(""), None);
        assert_eq!(
            pmset_display_sleep(include_str!("fixtures/macos_pmset.txt")),
            Some(10)
        );
        assert_eq!(defaults_integer("600\n"), Some(600));
        assert_eq!(
            defaults_integer("The domain/default pair does not exist"),
            None
        );
    }

    #[test]
    fn macos_screen_lock_needs_a_short_trigger_and_a_password() {
        use LockDelay::*;
        // Display sleeps after 10 minutes, password immediately.
        assert_eq!(
            macos_screen_lock(Some(After(0)), Some(9000), Some(10)),
            Some(true)
        );
        // Screen saver after 5 minutes plus a 5 minute delay.
        assert_eq!(
            macos_screen_lock(Some(After(300)), Some(300), Some(180)),
            Some(true)
        );
        // Both triggers long: the screen stays open for hours.
        assert_eq!(
            macos_screen_lock(Some(After(300)), Some(9000), Some(180)),
            Some(false)
        );
        // Never sleeps, never starts the screen saver.
        assert_eq!(
            macos_screen_lock(Some(After(0)), Some(0), Some(0)),
            Some(false)
        );
        // No password at all.
        assert_eq!(macos_screen_lock(Some(Off), Some(60), Some(1)), Some(false));
        // The screen saver key was never written and the display sleep is long.
        assert_eq!(macos_screen_lock(Some(After(0)), None, Some(180)), None);
        assert_eq!(macos_screen_lock(None, Some(60), Some(1)), None);
    }

    // --- Windows ---------------------------------------------------------

    #[test]
    fn windows_admin_document() {
        let f = WindowsFacts::parse(include_str!("fixtures/windows_admin.json")).unwrap();
        assert_eq!(f.bitlocker(), (Some(true), vec![]));
        // KB from the history (security classification) on 2026-11-12; the
        // newer Defender definition update of the 20th does not count.
        assert_eq!(f.last_security_update(), Some(1_794_441_600));
        assert_eq!(f.firewall_on(), (Some(true), vec![]));
        assert_eq!(f.screen_lock(), Some(true), "InactivityTimeoutSecs 600");
        assert!(f.sshd_running());
    }

    #[test]
    fn windows_user_document_without_admin_rights() {
        let f = WindowsFacts::parse(include_str!("fixtures/windows_user.json")).unwrap();
        // C: read through the shell property (on), D: unreadable.
        assert_eq!(f.bitlocker(), (None, vec![]));
        // No history readable; the hotfix labelled "Security Update" counts,
        // the plain "Update" does not.
        assert_eq!(f.last_security_update(), Some(1_791_331_200));
        assert_eq!(f.firewall_on(), (Some(false), vec!["Public".to_string()]));
        // Screen saver on, secure, 20 minutes: too long.
        assert_eq!(f.screen_lock(), Some(false));
        assert!(!f.sshd_running());
    }

    #[test]
    fn windows_bitlocker_states_that_are_not_protection() {
        let doc = |src: &str, st: i64| {
            WindowsFacts::parse(&format!(
                r#"{{"volumes":[{{"drive":"C:","status":{st},"source":"{src}"}}]}}"#
            ))
            .unwrap()
            .bitlocker()
            .0
        };
        assert_eq!(doc("wmi", 1), Some(true));
        assert_eq!(doc("wmi", 0), Some(false));
        assert_eq!(doc("wmi", 2), None, "locked/unknown");
        assert_eq!(doc("shell", 6), Some(true));
        for not_protected in [0, 2, 3, 4, 5, 8] {
            assert_eq!(doc("shell", not_protected), Some(false), "{not_protected}");
        }
        assert_eq!(doc("none", 1), None);
        assert_eq!(WindowsFacts::parse("{}").unwrap().bitlocker().0, None);
    }

    #[test]
    fn windows_screen_lock_under_the_system_account_needs_a_machine_policy() {
        let f = WindowsFacts::parse(
            r#"{"is_system":true,"screensaver":{"active":"1","secure":"1","timeout":"300"}}"#,
        )
        .unwrap();
        assert_eq!(
            f.screen_lock(),
            None,
            "the service account's screen saver says nothing"
        );
        let f = WindowsFacts::parse(
            r#"{"is_system":true,"screensaver":{"policy_active":"1","policy_secure":"1","policy_timeout":"600"}}"#,
        )
        .unwrap();
        assert_eq!(f.screen_lock(), Some(true));
        let f = WindowsFacts::parse(r#"{"screensaver":{}}"#).unwrap();
        assert_eq!(
            f.screen_lock(),
            Some(false),
            "never configured is no screen saver"
        );
    }

    // --- Linux -----------------------------------------------------------

    #[test]
    fn lsblk_lvm_on_luks_is_encrypted_and_boot_is_exempt() {
        let d = lsblk_disks(include_str!("fixtures/linux_lsblk_luks.json")).unwrap();
        assert_eq!(d.verdict(), Some(true), "{d:?}");
        assert!(d.encrypted.contains(&"/".to_string()));
        assert!(d.encrypted.contains(&"/home".to_string()));
        assert!(d.encrypted.contains(&"[SWAP]".to_string()));
        assert!(d.plain.is_empty());
    }

    #[test]
    fn lsblk_a_plain_data_disk_fails_even_with_an_encrypted_root() {
        let d = lsblk_disks(include_str!("fixtures/linux_lsblk_mixed.json")).unwrap();
        assert_eq!(d.verdict(), Some(false));
        assert_eq!(d.plain, vec!["/srv/data".to_string()]);
    }

    #[test]
    fn lsblk_plain_server_and_unknown_root() {
        let d = lsblk_disks(include_str!("fixtures/linux_lsblk_plain.json")).unwrap();
        assert_eq!(d.verdict(), Some(false));
        assert!(d.plain.contains(&"/".to_string()));
        // A container: overlay root is not a block device at all.
        let d = lsblk_disks(r#"{"blockdevices":[{"name":"vda","type":"disk","mountpoint":null,"fstype":null,"children":[{"name":"vda1","type":"part","mountpoints":["/etc/hosts"],"fstype":"ext4"}]}]}"#).unwrap();
        assert_eq!(d.verdict(), Some(false), "a plain mount still fails");
        let d = lsblk_disks(
            r#"{"blockdevices":[{"name":"zram0","type":"disk","mountpoints":["[SWAP]"]}]}"#,
        )
        .unwrap();
        assert_eq!(d.verdict(), None, "no root seen: unknown");
        assert!(lsblk_disks("not json").is_none());
    }

    #[test]
    fn apt_history_counts_upgrades_not_installs() {
        let log = include_str!("fixtures/linux_apt_history.log");
        // The 2026-11-02 Upgrade transaction, not the 2026-11-09 Install.
        assert_eq!(apt_last_upgrade(log), parse_datetime("2026-11-02 06:25:01"));
        assert_eq!(
            apt_last_upgrade("Start-Date: 2026-11-09  10:00:00\nInstall: a (1)\n"),
            None
        );
        assert_eq!(apt_last_upgrade(""), None);
    }

    #[test]
    fn dnf_history_in_both_dnf_versions() {
        let dnf4 = include_str!("fixtures/linux_dnf4_history.txt");
        assert_eq!(dnf_last_upgrade(dnf4), parse_datetime("2026-11-05 07:12"));
        let dnf5 = include_str!("fixtures/linux_dnf5_history.txt");
        assert_eq!(
            dnf_last_upgrade(dnf5),
            parse_datetime("2026-11-06 08:01:44")
        );
        assert_eq!(dnf_last_upgrade("No transactions"), None);
    }

    #[test]
    fn pacman_log() {
        let log = "[2026-11-01T09:00:00+0100] [ALPM] installed a (1-1)\n\
                   [2026-11-03T09:00:00+0100] [ALPM] upgraded openssl (3.3-1 -> 3.3-2)\n\
                   [2026-11-04T09:00:00+0100] [PACMAN] Running 'pacman -S b'\n";
        assert_eq!(
            pacman_last_upgrade(log),
            parse_datetime("2026-11-03T08:00:00Z")
        );
    }

    #[test]
    fn firewall_sources() {
        assert_eq!(
            ufw_enabled("# comment\nENABLED=yes\nLOGLEVEL=low\n"),
            Some(true)
        );
        assert_eq!(ufw_enabled("ENABLED=no\n"), Some(false));
        assert_eq!(ufw_enabled(""), None);
        assert!(nft_filters_input(include_str!(
            "fixtures/linux_nft_drop.txt"
        )));
        assert!(
            !nft_filters_input(include_str!("fixtures/linux_nft_debian_default.conf")),
            "Debian's stock config hooks input with policy accept"
        );
        assert!(
            !nft_filters_input(include_str!("fixtures/linux_nft_docker_only.txt")),
            "Docker's forward-chain drops are not an input firewall"
        );
        let catch_all = "table inet f {\n chain input {\n  type filter hook input priority 0; policy accept;\n  ct state established,related accept\n  tcp dport 22 accept\n  counter packets 3 bytes 180 reject with icmpx type port-unreachable\n }\n}\n";
        assert!(nft_filters_input(catch_all));
        let only_invalid = "table inet f {\n chain input {\n  type filter hook input priority 0; policy accept;\n  ct state invalid drop\n  tcp dport 22 accept\n }\n}\n";
        assert!(!nft_filters_input(only_invalid));
    }

    #[test]
    fn desktop_screen_lock() {
        assert_eq!(
            gnome_screen_lock("true", "uint32 300", "uint32 0"),
            Some(true)
        );
        assert_eq!(
            gnome_screen_lock("true", "uint32 0", "uint32 0"),
            Some(false),
            "never blanks"
        );
        assert_eq!(
            gnome_screen_lock("true", "uint32 900", "uint32 30"),
            Some(false)
        );
        assert_eq!(
            gnome_screen_lock("false", "uint32 300", "uint32 0"),
            Some(false)
        );
        assert_eq!(gnome_screen_lock("", "", ""), None);
        assert_eq!(kde_screen_lock("", "", ""), Some(true), "KDE defaults");
        assert_eq!(kde_screen_lock("false", "5", "5"), Some(false));
        assert_eq!(kde_screen_lock("true", "30", "5"), Some(false));
    }

    // --- sshd ------------------------------------------------------------

    #[test]
    fn sshd_t_effective_values() {
        let s = sshd_t(include_str!("fixtures/sshd_t_hardened.txt")).unwrap();
        assert!(!s.password_login(false));
        assert!(!s.root_login(), "without-password is keys only");
        let s = sshd_t("passwordauthentication yes\npermitrootlogin yes\n").unwrap();
        assert!(s.password_login(false));
        assert!(s.root_login());
        assert_eq!(sshd_t("sshd: no hostkeys available -- exiting."), None);
    }

    #[test]
    fn sshd_config_first_value_wins_through_includes() {
        let main = include_str!("fixtures/sshd_config_ubuntu");
        let include = |p: &str| -> Vec<String> {
            assert_eq!(p, "/etc/ssh/sshd_config.d/*.conf");
            vec![include_str!("fixtures/sshd_config_d_hardening.conf").to_string()]
        };
        let s = sshd_config(main, &include);
        // The drop-in says no before the main file's later "yes".
        assert!(!s.password_authentication);
        assert!(!s.kbd_interactive);
        assert!(s.use_pam);
        assert!(!s.password_login(false));
        assert_eq!(s.permit_root_login, "no");
        assert!(!s.root_login());
    }

    #[test]
    fn sshd_config_defaults_and_match_blocks() {
        let none = |_: &str| Vec::new();
        // macOS: everything commented out except UsePAM yes, so the
        // defaults rule and keyboard-interactive asks for the password.
        let s = sshd_config(include_str!("fixtures/sshd_config_macos"), &none);
        assert!(s.password_login(false));
        assert!(!s.root_login());
        // Windows has no PAM; PasswordAuthentication's default still applies.
        assert!(s.password_login(true));
        let s = sshd_config(
            "PasswordAuthentication no\nKbdInteractiveAuthentication no\nMatch Address 10.0.0.0/8\n  PasswordAuthentication yes\n  PermitRootLogin yes\n",
            &none,
        );
        assert!(!s.password_authentication);
        assert!(
            s.password_login(false),
            "a Match block allows passwords for someone"
        );
        assert!(s.root_login());
        let s = sshd_config(
            "passwordauthentication=no\nPermitRootLogin=prohibit-password\n",
            &none,
        );
        assert!(!s.password_authentication);
        assert!(!s.root_login());
    }
}
