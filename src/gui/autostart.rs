//! "Start the window when I sign in": the per-user autostart entry each
//! desktop has, written and removed by the window's settings screen.
//!
//! Per user, never system-wide, because the window runs as the person who
//! owns the node directory. Nothing here needs elevation, and removing the
//! entry is exactly the inverse of adding it: a Run value under HKCU on
//! Windows, a LaunchAgent plist in `~/Library/LaunchAgents` on macOS, an
//! XDG autostart `.desktop` file on Linux.

#[cfg(not(windows))]
use std::path::PathBuf;

const NAME: &str = "KMPLIFY Node";

/// Is an entry present for this binary (or any `kmplify-node gui`)?
pub fn enabled() -> bool {
    platform::enabled()
}

/// Add or remove the entry. The message says what was written where.
pub fn set(on: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    if on {
        platform::enable(&exe)
    } else {
        platform::disable()
    }
}

#[cfg(not(windows))]
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "no home directory in the environment".to_string())
}

#[cfg(windows)]
mod platform {
    use super::NAME;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("reg.exe: {e}"))
    }

    pub fn enabled() -> bool {
        reg(&["query", KEY, "/v", NAME])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn enable(exe: &Path) -> Result<String, String> {
        let value = format!("\"{}\" gui", exe.display());
        let out = reg(&["add", KEY, "/v", NAME, "/t", "REG_SZ", "/d", &value, "/f"])?;
        if out.status.success() {
            Ok(format!("{NAME} starts when you sign in (HKCU Run entry)"))
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    pub fn disable() -> Result<String, String> {
        let out = reg(&["delete", KEY, "/v", NAME, "/f"])?;
        if out.status.success() || !enabled() {
            Ok("removed the sign-in entry".into())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};

    const LABEL: &str = "io.kmplify.node";

    fn plist() -> Result<PathBuf, String> {
        Ok(super::home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    pub fn enabled() -> bool {
        plist().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn enable(exe: &Path) -> Result<String, String> {
        let path = plist()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>gui</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
            exe = exe.display()
        );
        std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(format!(
            "{} starts at login ({})",
            super::NAME,
            path.display()
        ))
    }

    pub fn disable() -> Result<String, String> {
        let path = plist()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok("removed the login item".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok("no login item to remove".into())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use std::path::{Path, PathBuf};

    fn desktop_file() -> Result<PathBuf, String> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(super::home()?.join(".config"));
        Ok(config.join("autostart/kmplify-node.desktop"))
    }

    pub fn enabled() -> bool {
        desktop_file().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn enable(exe: &Path) -> Result<String, String> {
        let path = desktop_file()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let body = format!(
            "[Desktop Entry]\nType=Application\nName={}\nComment=Personal inference router and fabric node\nExec=\"{}\" gui\nIcon=kmplify-node\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            super::NAME,
            exe.display()
        );
        std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(format!(
            "{} starts with your session ({})",
            super::NAME,
            path.display()
        ))
    }

    pub fn disable() -> Result<String, String> {
        let path = desktop_file()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok("removed the autostart entry".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok("no autostart entry to remove".into())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}
