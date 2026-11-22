//! What each OS is asked, and how. Every answer goes through `parse.rs`.
//!
//! Rules for every probe here:
//! - built with `crate::proc::command` (no console window on Windows, no
//!   PATH search on macOS), stdin closed, a timeout, killed when dropped;
//! - nothing that can prompt: no `sudo`, no elevation, `-NonInteractive`;
//! - a failure is `None` plus, where rights are the reason, a hint.

use std::process::Stdio;
use std::time::Duration;

#[allow(unused_imports)]
use super::parse;
use super::{Item, Report, CHECKER_VERSION};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[allow(dead_code)]
pub(super) struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run a program, never interactively. `None` if it could not start or did
/// not finish within the timeout.
#[allow(dead_code)]
pub(super) async fn run(program: &str, args: &[&str]) -> Option<Out> {
    let mut cmd = crate::proc::command(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Parsers read English, fixed-format output.
    #[cfg(unix)]
    cmd.env("LC_ALL", "C").env("LANG", "C");
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    Some(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Is something listening on the local SSH port?
#[allow(dead_code)]
async fn port_22_open() -> bool {
    for addr in ["127.0.0.1:22", "[::1]:22"] {
        let attempt = tokio::time::timeout(
            Duration::from_millis(800),
            tokio::net::TcpStream::connect(addr),
        )
        .await;
        if matches!(attempt, Ok(Ok(_))) {
            return true;
        }
    }
    false
}

/// Read an sshd_config with its `Include`s. Relative include paths are
/// relative to `etc_ssh`, as sshd reads them; only a `*` in the file name is
/// expanded, which is what every distribution's drop-in directory uses.
#[allow(dead_code)]
fn read_sshd_config(path: &std::path::Path, etc_ssh: &std::path::Path) -> Option<parse::Sshd> {
    let main = std::fs::read_to_string(path).ok()?;
    let etc = etc_ssh.to_path_buf();
    let include = move |pattern: &str| -> Vec<String> {
        let p = std::path::Path::new(pattern);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            etc.join(p)
        };
        let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) else {
            return Vec::new();
        };
        if !name.contains('*') {
            return std::fs::read_to_string(&p).into_iter().collect();
        }
        let (pre, post) = name.split_once('*').unwrap_or((name, ""));
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|f| {
                f.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with(pre) && n.ends_with(post) && n.len() >= pre.len() + post.len()
                })
            })
            .collect();
        // sshd expands a glob in lexical order.
        files.sort();
        files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .collect()
    };
    Some(parse::sshd_config(&main, &include))
}

/// What the probes of one OS found.
#[derive(Default)]
#[allow(dead_code)]
struct Found {
    disk_encrypted: Option<bool>,
    last_update: Option<i64>,
    firewall_on: Option<bool>,
    screen_lock: Option<bool>,
    ssh: Option<(bool, bool)>,
    hints: Vec<String>,
    details: Vec<(Item, String)>,
}

pub(super) async fn collect(now: u64) -> Report {
    let os = std::env::consts::OS;
    let f = collect_os().await;
    Report {
        os: match os {
            "macos" | "windows" | "linux" => os.to_string(),
            other => other.to_string(),
        },
        disk_encrypted: f.disk_encrypted,
        patch_age_days: f
            .last_update
            .map(|t| parse::age_days(t, i64::try_from(now).unwrap_or(i64::MAX))),
        firewall_on: f.firewall_on,
        screen_lock: f.screen_lock,
        ssh_password_auth: f.ssh.map(|s| s.0),
        ssh_root_login: f.ssh.map(|s| s.1),
        checked_at: now,
        checker_version: CHECKER_VERSION.to_string(),
        hints: f.hints,
        details: f.details,
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn collect_os() -> Found {
    // FileVault protects the startup disk's data volume, which holds every
    // user's data and the node directory. External volumes are not read.
    let mut f = Found {
        disk_encrypted: match run("/usr/bin/fdesetup", &["status"]).await {
            Some(o) => parse::filevault(&o.stdout),
            None => None,
        },
        ..Found::default()
    };
    if f.disk_encrypted.is_none() {
        f.hints
            .push("FileVault status could not be read (fdesetup status).".into());
    }

    f.last_update = match run(
        "/usr/sbin/system_profiler",
        &["SPInstallHistoryDataType", "-json"],
    )
    .await
    {
        Some(o) if o.ok => parse::macos_last_security_update(&o.stdout),
        _ => None,
    };
    if f.last_update.is_none() {
        f.hints
            .push("No macOS update was found in the install history.".into());
    }

    f.firewall_on = match run(
        "/usr/libexec/ApplicationFirewall/socketfilterfw",
        &["--getglobalstate"],
    )
    .await
    {
        Some(o) => parse::app_firewall(&format!("{}{}", o.stdout, o.stderr)),
        None => None,
    };
    if f.firewall_on == Some(false) {
        f.details.push((
            Item::Firewall,
            "The Application Firewall is off (pf rules are not read).".into(),
        ));
    }

    // Screen lock is a per-user setting: read for the account the node runs
    // as. Under launchd as root there is no person's setting to read.
    let delay = run("/usr/sbin/sysadminctl", &["-screenLock", "status"])
        .await
        .and_then(|o| parse::sysadminctl_screen_lock(&format!("{}{}", o.stderr, o.stdout)));
    let idle = run(
        "/usr/bin/defaults",
        &["-currentHost", "read", "com.apple.screensaver", "idleTime"],
    )
    .await
    .filter(|o| o.ok)
    .and_then(|o| parse::defaults_integer(&o.stdout));
    let display = run("/usr/bin/pmset", &["-g"])
        .await
        .filter(|o| o.ok)
        .and_then(|o| parse::pmset_display_sleep(&o.stdout));
    f.screen_lock = parse::macos_screen_lock(delay, idle, display);
    if f.screen_lock.is_none() {
        f.hints.push(
            "The screen lock settings of this account could not be read; run `kmplify-node \
             security check` as the person who uses this Mac to see them."
                .into(),
        );
    }

    // Remote Login is launchd's com.openssh.sshd: loaded only while it is on.
    let remote_login = run("/bin/launchctl", &["print", "system/com.openssh.sshd"])
        .await
        .is_some_and(|o| o.ok)
        || port_22_open().await;
    if remote_login {
        let etc = std::path::Path::new("/etc/ssh");
        match read_sshd_config(&etc.join("sshd_config"), etc) {
            Some(s) => f.ssh = Some((s.password_login(false), s.root_login())),
            None => f
                .hints
                .push("Remote Login is on but /etc/ssh/sshd_config could not be read.".into()),
        }
    }
    f
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// One PowerShell run collects everything as JSON. Only reads; every part
/// that needs administrator rights falls back or yields null without them.
#[cfg(windows)]
const WINDOWS_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$o = [ordered]@{}
$o.system_drive = $env:SystemDrive
$o.volumes = @(Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | ForEach-Object {
  $d = $_.DeviceID; $st = $null; $src = 'none'
  $ev = Get-CimInstance -Namespace 'root\cimv2\security\microsoftvolumeencryption' -ClassName Win32_EncryptableVolume -Filter "DriveLetter='$d'"
  if ($ev) { $st = [int]$ev.ProtectionStatus; $src = 'wmi' }
  else {
    try { $p = (New-Object -ComObject Shell.Application).NameSpace($d + '\').Self.ExtendedProperty('System.Volume.BitLockerProtection'); if ($p -ne $null) { $st = [int]$p; $src = 'shell' } } catch {}
  }
  [ordered]@{ drive = $d; status = $st; source = $src }
})
$o.firewall = @(Get-NetFirewallProfile -PolicyStore ActiveStore | ForEach-Object { [ordered]@{ name = [string]$_.Name; enabled = [string]$_.Enabled } })
$o.hotfixes = @(Get-HotFix | Where-Object { $_.InstalledOn } | ForEach-Object { [ordered]@{ id = [string]$_.HotFixID; description = [string]$_.Description; installed = ([DateTimeOffset]$_.InstalledOn).ToUnixTimeSeconds() } })
$o.wu_history = @()
try {
  $se = (New-Object -ComObject Microsoft.Update.Session).CreateUpdateSearcher()
  $n = $se.GetTotalHistoryCount()
  if ($n -gt 0) {
    $o.wu_history = @($se.QueryHistory(0, [Math]::Min($n, 300)) | ForEach-Object {
      [ordered]@{ result = [int]$_.ResultCode; operation = [int]$_.Operation; date = ([DateTimeOffset]$_.Date).ToUnixTimeSeconds(); categories = @($_.Categories | ForEach-Object { [string]$_.CategoryID }) }
    })
  }
} catch {}
$o.inactivity_timeout = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name InactivityTimeoutSecs).InactivityTimeoutSecs
$d = Get-ItemProperty 'HKCU:\Control Panel\Desktop'
$pd = Get-ItemProperty 'HKCU:\Software\Policies\Microsoft\Windows\Control Panel\Desktop'
$o.screensaver = [ordered]@{ active = $d.ScreenSaveActive; secure = $d.ScreenSaverIsSecure; timeout = $d.ScreenSaveTimeOut; policy_active = $pd.ScreenSaveActive; policy_secure = $pd.ScreenSaverIsSecure; policy_timeout = $pd.ScreenSaveTimeOut }
$sv = Get-Service -Name sshd
$o.sshd = if ($sv) { [string]$sv.Status } else { $null }
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$o.is_admin = ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$o.is_system = $id.IsSystem
$o | ConvertTo-Json -Depth 5 -Compress
"#;

#[cfg(windows)]
async fn collect_os() -> Found {
    let mut f = Found::default();
    let facts = run(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT],
    )
    .await
    .and_then(|o| parse::WindowsFacts::parse(&o.stdout));
    let Some(w) = facts else {
        f.hints
            .push("Windows PowerShell did not answer; no Windows check could run.".into());
        return f;
    };

    let (disk, plain) = w.bitlocker();
    f.disk_encrypted = disk;
    if !plain.is_empty() {
        f.details.push((
            Item::DiskEncrypted,
            format!("Not protected by BitLocker: {}", plain.join(", ")),
        ));
    }
    if disk.is_none() && !w.is_admin {
        f.hints.push(
            "BitLocker status needs administrator rights for some drives: run the node as \
             administrator, or run `kmplify-node security check` as administrator to see it."
                .into(),
        );
    } else if disk.is_none() {
        f.hints
            .push("BitLocker status could not be read for every fixed drive.".into());
    }

    f.last_update = w.last_security_update();
    if f.last_update.is_none() {
        f.hints
            .push("No installed security update was found in the Windows Update history.".into());
    }

    let (fw, off) = w.firewall_on();
    f.firewall_on = fw;
    if !off.is_empty() {
        f.details.push((
            Item::Firewall,
            format!("Defender Firewall is off for: {}", off.join(", ")),
        ));
    }

    f.screen_lock = w.screen_lock();
    if f.screen_lock.is_none() {
        f.hints.push(
            "The node runs as a service, so only a machine-wide lock policy can be read; \
             run `kmplify-node security check` as the person who uses this PC to see their \
             screen lock."
                .into(),
        );
    }

    if w.sshd_running() || port_22_open().await {
        let base = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"))
            .join("ssh");
        match read_sshd_config(&base.join("sshd_config"), &base) {
            // Windows has no root account; PermitRootLogin does not apply.
            Some(s) => f.ssh = Some((s.password_login(true), false)),
            None => f.hints.push(
                "OpenSSH Server is running but its sshd_config could not be read; \
                 run `kmplify-node security check` as administrator to see it."
                    .into(),
            ),
        }
    }
    f
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn mtime_s(path: &str) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    i64::try_from(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs()).ok()
}

/// `systemctl is-active` for one unit: Some(true) active, Some(false) any
/// other state, None without systemd.
#[cfg(target_os = "linux")]
async fn unit_active(unit: &str) -> Option<bool> {
    crate::proc::find("systemctl")?;
    let o = run("systemctl", &["is-active", unit]).await?;
    Some(o.stdout.trim() == "active")
}

#[cfg(target_os = "linux")]
async fn collect_os() -> Found {
    let mut f = Found::default();

    // Disk encryption: every mounted block filesystem but /boot and the EFI
    // partition must sit on dm-crypt.
    let disks = match run("lsblk", &["-J", "-o", "NAME,TYPE,MOUNTPOINT,FSTYPE"]).await {
        Some(o) if o.ok => parse::lsblk_disks(&o.stdout),
        _ => None,
    };
    match &disks {
        Some(d) => {
            f.disk_encrypted = d.verdict();
            if !d.plain.is_empty() {
                f.details.push((
                    Item::DiskEncrypted,
                    format!("Not encrypted: {}", d.plain.join(", ")),
                ));
            }
            if f.disk_encrypted.is_none() {
                f.hints.push(
                    "The root filesystem is not on a block device lsblk lists (a container, ZFS \
                     or a network root), so disk encryption could not be read."
                        .into(),
                );
            }
        }
        None => f
            .hints
            .push("lsblk is missing or failed; disk encryption could not be read.".into()),
    }

    // Last security update, best effort: apt, dnf, pacman.
    let mut stamps: Vec<i64> = Vec::new();
    for log in ["/var/log/apt/history.log", "/var/log/apt/history.log.1"] {
        if let Ok(text) = std::fs::read_to_string(log) {
            stamps.extend(parse::apt_last_upgrade(&text));
        }
    }
    // Written by apt.systemd.daily only after unattended-upgrade succeeded:
    // every pending security update was installed then, or there was none.
    stamps.extend(mtime_s("/var/lib/apt/periodic/upgrade-stamp"));
    if crate::proc::find("dnf").is_some() {
        if let Some(o) = run("dnf", &["-C", "-q", "history", "list"]).await {
            stamps.extend(parse::dnf_last_upgrade(&o.stdout));
        }
    }
    if let Ok(text) = std::fs::read_to_string("/var/log/pacman.log") {
        stamps.extend(parse::pacman_last_upgrade(&text));
    }
    f.last_update = stamps.into_iter().max();
    if f.last_update.is_none() {
        f.hints.push(
            "The last security update could not be found (apt, dnf and pacman logs are read)."
                .into(),
        );
    }

    // Firewall: ufw or firewalld switched on, or an nftables input filter.
    let ufw_conf = std::fs::read_to_string("/etc/ufw/ufw.conf")
        .ok()
        .and_then(|c| parse::ufw_enabled(&c));
    let ufw_unit = unit_active("ufw").await;
    let firewalld = unit_active("firewalld").await;
    let ruleset = match run("nft", &["list", "ruleset"]).await {
        Some(o) if o.ok => Some(o.stdout),
        _ => None,
    };
    let nft_unit = unit_active("nftables").await;
    let nft_conf = std::fs::read_to_string("/etc/nftables.conf").ok();
    let ufw_on = ufw_conf == Some(true) && ufw_unit != Some(false);
    f.firewall_on = if ufw_on || firewalld == Some(true) {
        Some(true)
    } else if let Some(rules) = &ruleset {
        // Readable rules are the ground truth (iptables-nft rules show here too).
        Some(parse::nft_filters_input(rules))
    } else if nft_unit == Some(true) && nft_conf.as_deref().is_some_and(parse::nft_filters_input) {
        Some(true)
    } else {
        f.hints.push(
            "No ufw or firewalld is on, and the nftables rules need root to read; enable ufw or \
             firewalld, or run the node as root, to have the firewall checked."
                .into(),
        );
        None
    };

    // Screen lock: only where someone sits at a desktop.
    let own_desktop =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let mut graphical = own_desktop;
    if !graphical && crate::proc::find("loginctl").is_some() {
        if let Some(o) = run("loginctl", &["list-sessions", "--no-legend"]).await {
            for id in o.stdout.lines().filter_map(|l| l.split_whitespace().next()) {
                if let Some(t) =
                    run("loginctl", &["show-session", id, "-p", "Type", "--value"]).await
                {
                    if matches!(t.stdout.trim(), "x11" | "wayland" | "mir") {
                        graphical = true;
                        break;
                    }
                }
            }
        }
    }
    if graphical {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_lowercase();
        f.screen_lock = if !own_desktop {
            None
        } else if desktop.contains("kde") {
            let tool = if crate::proc::find("kreadconfig6").is_some() {
                "kreadconfig6"
            } else {
                "kreadconfig5"
            };
            let key = |k: &'static str| async move {
                run(
                    tool,
                    &["--file", "kscreenlockerrc", "--group", "Daemon", "--key", k],
                )
                .await
                .filter(|o| o.ok)
                .map(|o| o.stdout)
            };
            match (
                key("Autolock").await,
                key("Timeout").await,
                key("LockGrace").await,
            ) {
                (Some(a), Some(t), Some(g)) => parse::kde_screen_lock(&a, &t, &g),
                _ => None,
            }
        } else if crate::proc::find("gsettings").is_some() {
            let get = |schema: &'static str, key: &'static str| async move {
                run("gsettings", &["get", schema, key])
                    .await
                    .filter(|o| o.ok)
                    .map(|o| o.stdout)
            };
            match (
                get("org.gnome.desktop.screensaver", "lock-enabled").await,
                get("org.gnome.desktop.session", "idle-delay").await,
                get("org.gnome.desktop.screensaver", "lock-delay").await,
            ) {
                (Some(l), Some(i), Some(d)) => parse::gnome_screen_lock(&l, &i, &d),
                _ => None,
            }
        } else {
            None
        };
        if f.screen_lock.is_none() {
            f.hints.push(
                "A desktop session is running but its screen lock could not be read from here; \
                 run `kmplify-node security check` inside the desktop session (GNOME and KDE \
                 are read)."
                    .into(),
            );
        }
    }

    // sshd: running (a process, a socket-activated unit, or port 22).
    let sshd_process = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "sshd"));
    let mut running = sshd_process;
    if !running {
        for unit in ["ssh.socket", "ssh", "sshd"] {
            if unit_active(unit).await == Some(true) {
                running = true;
                break;
            }
        }
    }
    if !running {
        running = port_22_open().await;
    }
    let etc = std::path::Path::new("/etc/ssh");
    let config_present = etc.join("sshd_config").exists();
    if running && config_present {
        // Effective values when sshd will say (it usually needs root to read
        // the host keys); the files otherwise. Match blocks come from the files.
        let from_files = read_sshd_config(&etc.join("sshd_config"), etc);
        let effective = match run("sshd", &["-T"]).await {
            Some(o) if o.ok => parse::sshd_t(&o.stdout),
            _ => None,
        };
        let merged = match (effective, from_files) {
            (Some(mut e), Some(files)) => {
                e.match_password = files.match_password;
                e.match_root = files.match_root;
                Some(e)
            }
            (Some(e), None) => Some(e),
            (None, files) => files,
        };
        match merged {
            Some(s) => f.ssh = Some((s.password_login(false), s.root_login())),
            None => f
                .hints
                .push("sshd is running but /etc/ssh/sshd_config could not be read.".into()),
        }
    }
    f
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
async fn collect_os() -> Found {
    Found {
        hints: vec!["This operating system has no baseline checks yet.".into()],
        ..Found::default()
    }
}
