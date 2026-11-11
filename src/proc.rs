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
//!
//! The same rule has a second reason on macOS: a bare program name is
//! searched for by launching it in every PATH directory, one dead process
//! per miss (see NOWHERE). Programs are resolved to a path here, by looking
//! and not by launching; what is only a kernel setting is read with
//! `sysctl_u64` / `sysctl_string` and starts nothing at all. A test at the
//! end of this file fails when a launch elsewhere bypasses this module.

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

/// The directories macOS itself searches when PATH says nothing else. They
/// are looked at last, so that a program the system ships is always found by
/// its path here and never left to the system's own search (see NOWHERE).
#[cfg(target_os = "macos")]
const SYSTEM_BIN_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Where a program that was found in no directory is sent on macOS: a
/// directory that is empty by definition, so the launch fails once, with the
/// system's own "not found".
///
/// A bare program name looks harmless and is not. macOS searches PATH by
/// TRYING to launch the program in each directory in turn, and every failed
/// try still creates and destroys a process. Measured on an M2 Max with the
/// 39 PATH entries of a developer shell (2026-11-11):
///
/// ```text
/// a program that is not installed, by bare name      39 processes
/// the same, by an absolute path                       1
/// `sysctl` by bare name (/usr/sbin was entry 23)     23
/// `/usr/sbin/sysctl`                                  1
/// the sysctlbyname system call                        0
/// ```
///
/// The desktop app, which shares this code, asked for the GPU memory limit
/// with a bare `sysctl` every 5 seconds: 31 dead processes per poll in a
/// developer shell, about 370 a minute. The machine ran out of vnodes that
/// evening and restarted itself. Whether this churn was what used them up
/// was never proven; it bought nothing either way.
///
/// So on macOS no name ever reaches the system unresolved: `find` looks for
/// the file itself (no process), and what it cannot find goes here and costs
/// one failed launch instead of one per PATH entry.
#[cfg(target_os = "macos")]
const NOWHERE: &str = "/var/empty";

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
        // An empty PATH entry means the current directory. Said outright, so
        // the result is a path and not the bare name again.
        dirs.extend(std::env::split_paths(&paths).map(|d| {
            if d.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                d
            }
        }));
    }
    dirs.extend(EXTRA_BIN_DIRS.iter().map(PathBuf::from));
    // Docker Desktop's per-user install, which has no fixed prefix.
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".docker/bin"));
    }
    #[cfg(target_os = "macos")]
    dirs.extend(SYSTEM_BIN_DIRS.iter().map(PathBuf::from));

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
/// means it — then the well-known locations launchd omits. A name found
/// nowhere stays bare on Windows and Linux, so the OS produces its own "not
/// found" rather than us inventing one; on macOS it becomes a path that
/// cannot exist, which produces the same error without the search (NOWHERE).
fn resolve(program: &OsStr) -> OsString {
    if Path::new(program).components().count() > 1 {
        return program.to_os_string(); // already a path; respect it
    }
    if let Some(found) = find(program) {
        return found.into_os_string();
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(NOWHERE).join(program).into_os_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        program.to_os_string()
    }
}

/// A numeric kernel setting, as `sysctl -n <name>` prints it, read with one
/// system call and no process. None for a name this system does not have, a
/// negative value, or anything that is not a 4- or 8-byte integer.
///
/// Anything that is asked again and again belongs here and not in a
/// `sysctl` child: see NOWHERE for what the child cost.
#[cfg(target_os = "macos")]
pub fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut buf = [0u8; 8];
    let mut len = buf.len();
    // SAFETY: `name` is NUL-terminated, the buffer is 8 writable bytes and
    // `len` says so; the kernel writes at most `len` bytes and stores the
    // size it wrote back into `len`. Nothing is set (no new value).
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match len {
        4 => u64::try_from(i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]])).ok(),
        8 => u64::try_from(i64::from_ne_bytes(buf)).ok(),
        _ => None,
    }
}

/// A text kernel setting (`sysctl -n machdep.cpu.brand_string`), read the
/// same way. None when the name is unknown or the value is empty.
#[cfg(target_os = "macos")]
pub fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut len = 0usize;
    // SAFETY: a null buffer asks only for the size, which lands in `len`.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: the buffer holds `len` writable bytes; the kernel writes at
    // most that many and reports the count in `len`.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let text = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
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
        // On macOS they go to the empty directory instead: a bare name would
        // be launched once per PATH entry there before the OS gave up.
        std::env::set_var("PATH", "/usr/bin:/bin");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            resolve(OsStr::new("faux-docker")),
            OsStr::new("faux-docker")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            resolve(OsStr::new("faux-docker")),
            OsStr::new("/var/empty/faux-docker")
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

/// 2026-11-11: a Mac restarted itself (kernel panic "initproc exited": the
/// system had run out of vnodes and launchd died with it). For the hour
/// before, a dev build of the desktop app, which shares this code, had been
/// creating about 370 dead processes a minute: a polled handler started
/// `sysctl` by bare name with std's `Command::new`, past this module, and
/// macOS tried the launch in each of the 31 PATH directories in front of
/// /usr/sbin. This binary had the same call.
#[cfg(all(test, target_os = "macos"))]
mod no_process_per_path_entry_tests {
    use super::*;

    fn printed_by_sysctl(name: &str) -> Option<String> {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", name])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !text.is_empty()).then_some(text)
    }

    /// The system call answers what the program prints, for a 4-byte value
    /// (the GPU limit, the Rosetta question), an 8-byte one and a name that
    /// may not exist on this machine.
    #[test]
    fn a_kernel_setting_read_directly_is_what_sysctl_prints() {
        for name in [
            c"iogpu.wired_limit_mb",
            c"hw.optional.arm64",
            c"hw.ncpu",
            c"hw.memsize",
        ] {
            let expected =
                printed_by_sysctl(name.to_str().unwrap()).and_then(|t| t.parse::<u64>().ok());
            assert_eq!(sysctl_u64(name), expected, "{name:?}");
        }
        assert!(
            sysctl_u64(c"hw.memsize").is_some_and(|bytes| bytes > 256 * 1024 * 1024),
            "the 8-byte path is broken"
        );
        assert_eq!(
            sysctl_string(c"machdep.cpu.brand_string"),
            printed_by_sysctl("machdep.cpu.brand_string")
        );
        assert!(sysctl_string(c"machdep.cpu.brand_string").is_some());
    }

    #[test]
    fn an_unknown_or_non_numeric_setting_is_none() {
        assert_eq!(sysctl_u64(c"kmplify.no.such.setting"), None);
        assert_eq!(sysctl_string(c"kmplify.no.such.setting"), None);
        // Text is not a number, whatever its length happens to be.
        assert_eq!(sysctl_u64(c"kern.osrelease"), None);
    }

    /// macOS only searches PATH for a name without a slash. So: nothing
    /// `resolve` returns may be one, found or not.
    #[test]
    fn no_name_reaches_the_system_bare() {
        for program in [
            "sysctl",
            "kill",
            "docker",
            "nvidia-smi",
            "kmplify-no-such-program",
        ] {
            let resolved = resolve(OsStr::new(program));
            assert!(
                Path::new(&resolved).components().count() > 1,
                "{program} would be searched for by launching it in every PATH directory: {resolved:?}"
            );
        }
        // What the system ships is found even when PATH does not name its
        // directory (the existing test leaves PATH at /usr/bin:/bin).
        assert_eq!(
            resolve(OsStr::new("sysctl")),
            OsStr::new("/usr/sbin/sysctl")
        );
        // `find` still admits failure; only the launch is redirected.
        assert_eq!(find("kmplify-no-such-program"), None);
    }

    /// And the launch of something that is not installed still fails the way
    /// callers expect: "not found", from the system.
    #[tokio::test]
    async fn a_missing_program_is_still_not_found() {
        let err = command("kmplify-no-such-program")
            .output()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}

/// The module doc's rule, enforced: no `Command::new` outside this file. A
/// new one fails the suite until it is built with `command()`.
#[cfg(test)]
mod every_launch_is_built_here_tests {
    use std::path::Path;

    fn offenders(dir: &Path, root: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                offenders(&path, root, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if name == "proc.rs" {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (i, line) in text.lines().enumerate() {
                    if !line.trim_start().starts_with("//") && line.contains("Command::new(") {
                        found.push(format!("{name}:{}", i + 1));
                    }
                }
            }
        }
    }

    #[test]
    fn no_program_is_started_past_this_module() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        offenders(&root, &root, &mut found);
        assert!(
            found.is_empty(),
            "`Command::new(` outside proc.rs: {found:?}. Build the launch with crate::proc::command(): a bare \
             program name costs one dead process per PATH directory on macOS, and a console window per launch \
             on Windows. Read a kernel setting with crate::proc::sysctl_u64(), not by starting `sysctl`."
        );
    }
}
