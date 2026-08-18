//! Subprocess spawning that stays invisible on Windows.
//!
//! The launcher shells out constantly: the boot status poll runs
//! `docker compose ps` every 2 seconds for as long as the window is open,
//! and the fabric worker probes `nvidia-smi` and `docker` per session.
//!
//! A release build sets `windows_subsystem = "windows"`, so the process has
//! no console of its own to lend a child. Windows therefore allocates a NEW
//! console per spawn — the user sees a black `docker.exe` box flash every
//! couple of seconds, forever, and `ollama serve` (spawned detached for the
//! whole session) leaves one parked on screen. `cargo run` from a terminal
//! hides all of this: there the child inherits the terminal's console, which
//! is why it only reproduces on an installed .exe.
//!
//! CREATE_NO_WINDOW runs the child without allocating a console at all.
//! Every subprocess in this app must be built here rather than with
//! `Command::new`, or the flicker comes straight back.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Directories a GUI-launched app does not get, but every developer shell has.
///
/// An app opened from Finder or a .dmg inherits launchd's PATH —
/// `/usr/bin:/bin:/usr/sbin:/sbin` — not the user's shell PATH. Docker
/// Desktop symlinks its CLI into /usr/local/bin and Homebrew installs ollama
/// into /opt/homebrew/bin, so `Command::new("docker")` resolves from a
/// terminal and fails from the bundle. The launcher then reported "No
/// container runtime found" on a machine with Docker running — which is
/// exactly what `make dev` could never reproduce, because cargo run inherits
/// the shell.
#[cfg(target_os = "macos")]
const EXTRA_BIN_DIRS: &[&str] = &[
    "/usr/local/bin",    // Docker Desktop symlinks, Intel Homebrew
    "/opt/homebrew/bin", // Apple-silicon Homebrew (ollama)
    "/Applications/Docker.app/Contents/Resources/bin", // Docker Desktop itself
];

#[cfg(all(unix, not(target_os = "macos")))]
const EXTRA_BIN_DIRS: &[&str] = &["/usr/local/bin", "/snap/bin"];

#[cfg(windows)]
const EXTRA_BIN_DIRS: &[&str] = &[];

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Where a helper binary actually is, or `None`.
///
/// Same search as `resolve`, but it admits failure. Detecting whether a
/// vendor's driver tooling is installed is exactly that question, and
/// answering it by spawning the tool is both slower and wrong: a present but
/// non-functional `nvidia-smi` still means a CUDA stack is installed.
pub fn find(program: impl AsRef<OsStr>) -> Option<PathBuf> {
    let program = program.as_ref();
    if Path::new(program).components().count() > 1 {
        let p = PathBuf::from(program);
        return is_executable(&p).then_some(p);
    }
    // On Windows the executable extension is not part of the name callers use.
    let names: Vec<OsString> = if cfg!(windows) {
        let mut n = program.to_os_string();
        n.push(".exe");
        vec![n, program.to_os_string()]
    } else {
        vec![program.to_os_string()]
    };

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(paths) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&paths));
    }
    dirs.extend(EXTRA_BIN_DIRS.iter().map(PathBuf::from));
    // Docker Desktop's per-user install, which has no fixed prefix.
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".docker/bin"));
    }

    for dir in dirs {
        for name in &names {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Where a GUI-launched app should look for a helper binary.
///
/// PATH first — a user who put their own docker ahead of Docker Desktop's
/// means it — then the well-known locations launchd omits. Falls back to the
/// bare name so the OS produces its own "not found" rather than us inventing
/// one.
fn resolve(program: &OsStr) -> OsString {
    find(program)
        .map(PathBuf::into_os_string)
        .unwrap_or_else(|| program.to_os_string())
}

/// <https://learn.microsoft.com/windows/win32/procthread/process-creation-flags>
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `Command::new`, minus the console window on Windows.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    // `mut` is load-bearing on Windows, where creation_flags() below mutates
    // it, and genuinely unused everywhere else — so every macOS and Linux
    // build warned. Removing the `mut` would fix those two targets and break
    // the one that matters; this silences the warning on the platforms where
    // it is noise without touching Windows.
    #[allow(unused_mut)]
    let mut cmd = Command::new(resolve(program.as_ref()));
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(test)]
mod path_resolution_tests {
    use super::*;

    /// The bug: an app opened from the .dmg reported "No container runtime
    /// found" while Docker was running, because launchd's PATH omits the
    /// directory Docker Desktop symlinks into. `make dev` could never
    /// reproduce it — cargo run inherits the shell's PATH.
    #[test]
    fn finds_a_binary_the_gui_path_would_miss() {
        let dir = std::env::temp_dir().join(format!("kmplify-proc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("faux-docker");
        std::fs::write(&tool, "#!/bin/sh\ntrue\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // On PATH: found there, and returned as an absolute path.
        std::env::set_var("PATH", &dir);
        assert_eq!(resolve(OsStr::new("faux-docker")), tool.as_os_str());

        // Stripped PATH, as a Finder launch gives: unknown names stay bare so
        // the OS reports its own error rather than us guessing a location.
        std::env::set_var("PATH", "/usr/bin:/bin");
        assert_eq!(
            resolve(OsStr::new("faux-docker")),
            OsStr::new("faux-docker")
        );

        // A real one still resolves from the well-known dirs on macOS.
        #[cfg(target_os = "macos")]
        if std::path::Path::new("/usr/local/bin/docker").exists() {
            assert_eq!(
                resolve(OsStr::new("docker")),
                OsStr::new("/usr/local/bin/docker")
            );
        }

        // An explicit path is never second-guessed.
        assert_eq!(resolve(OsStr::new("/bin/sh")), OsStr::new("/bin/sh"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
