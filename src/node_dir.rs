//! One serving node per node directory, and file writes that never collide.
//!
//! A node directory holds ONE node: one credential, one identity, one
//! gateway link. Two processes serving from it share the identity, so the
//! gateway hands the link to whichever connected last (close 4003) and the
//! other reconnects, forever. They also share every file the node writes, and
//! a fixed temporary name such as `router.json.tmp` is then renamed away by
//! one process while the other is still writing or restricting it.
//!
//! Two things here stop that:
//!
//! * [`ServeLock`]: an exclusive lock on `node.lock` in the node directory,
//!   held for as long as the process serves. Only the commands that SERVE
//!   take it (`run`, a `tui` that starts its own node). Everything that only
//!   reads or attaches (`status`, `set`, `window`, `security`, `link-code`,
//!   `router key`, `router join`, ...) never touches it and keeps working
//!   next to a running node. The lock is an OS lock (`flock` on Unix,
//!   `LockFileEx` on Windows): it goes away with the process, however the
//!   process ends, so a crash never leaves a node directory locked.
//! * [`unique_tmp`]: the temporary file of an atomic write is named after the
//!   writing process and a random suffix, so two writers can never pick the
//!   same one.
//!
//! The library only offers the lock; the binary decides when to take it. An
//! application embedding the provider (the KMPLIFY desktop app) serves from
//! its own directory and needs nothing from here, unless it is ever pointed
//! at a directory a `kmplify-node` binary serves from too. Then it takes the
//! same [`ServeLock`] before it starts the worker, and the two refuse each
//! other the same way two binaries do.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The lock file in the node directory. Empty: on Windows a locked file
/// cannot even be read by another process, so nothing is kept in it. Who
/// holds it is told by `status.json` instead.
pub const LOCK_FILE: &str = "node.lock";

/// The exclusive serving lock on a node directory. Released when dropped,
/// and by the OS when the process ends.
#[derive(Debug)]
pub struct ServeLock {
    _file: File,
    path: PathBuf,
}

/// Why a [`ServeLock`] was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process serves from this directory. `pid` and `started_at_ms`
    /// come from its `status.json` when that is fresh.
    Held {
        pid: Option<u32>,
        started_at_ms: Option<u64>,
    },
    /// The lock file could not be opened or locked at all.
    Io(std::io::Error),
}

impl ServeLock {
    /// Take the lock on `dir` (which must exist), or say who holds it.
    ///
    /// Also refuses when the lock is free but `status.json` was published a
    /// moment ago by another process that is still alive: that is a node of a
    /// release from before this lock existed, which never takes it. Checked
    /// by process id, so a node that crashed a second ago does not keep its
    /// own restart out for the fifteen seconds its status stays fresh.
    pub fn acquire(dir: &Path) -> Result<Self, LockError> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let (pid, started_at_ms) = published_owner(dir);
                return Err(LockError::Held { pid, started_at_ms });
            }
            Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }
        if let Some((pid, started_at_ms)) = other_live_publisher(dir) {
            return Err(LockError::Held {
                pid: Some(pid),
                started_at_ms: Some(started_at_ms),
            });
        }
        sweep_stale_tmp(dir, STALE_TMP_AGE);
        Ok(Self { _file: file, path })
    }

    /// The lock file this lock is held on.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What an operator is told when a second serving process is refused.
pub fn already_running_message(dir: &Path, pid: Option<u32>, started_at_ms: Option<u64>) -> String {
    let who = match (pid, started_at_ms) {
        (Some(pid), Some(at)) => format!(
            "pid {pid}, started {}",
            crate::startup::utc_stamp(std::time::UNIX_EPOCH + Duration::from_millis(at))
        ),
        (Some(pid), None) => format!("pid {pid}"),
        _ => "pid unknown: its status.json is not fresh".to_string(),
    };
    format!(
        "another kmplify-node is already running in {} ({who}); stop it or give this one \
         its own KMPLIFY_NODE_DIR",
        dir.display()
    )
}

/// Pid and start of the node that publishes here, when its status is fresh.
fn published_owner(dir: &Path) -> (Option<u32>, Option<u64>) {
    match crate::status::read_published(dir).filter(crate::status::Snapshot::is_fresh) {
        Some(s) if s.pid != 0 => (Some(s.pid), Some(s.started_at_ms)),
        _ => (None, None),
    }
}

/// A fresh `status.json` from another process that still runs, and looks
/// like a KMPLIFY node (a reused pid of something else does not count).
fn other_live_publisher(dir: &Path) -> Option<(u32, u64)> {
    let snap = crate::status::read_published(dir).filter(crate::status::Snapshot::is_fresh)?;
    if snap.pid == 0 || snap.pid == std::process::id() {
        return None;
    }
    let pid = sysinfo::Pid::from_u32(snap.pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let name = sys
        .process(pid)?
        .name()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.contains("kmplify")
        .then_some((snap.pid, snap.started_at_ms))
}

/// How old a leftover temporary file must be before a starting node removes
/// it. Long enough that a `set` writing next to the node is never cut off.
const STALE_TMP_AGE: Duration = Duration::from_secs(60);

/// The temporary file for an atomic write of `target`, in the same
/// directory (a rename across file systems is not atomic):
/// `<name>.<pid>.<random>.tmp`. Two processes, or two tasks of one, never get
/// the same name.
pub fn unique_tmp(target: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    use rand_core::RngCore;
    let rand = rand_core::OsRng.next_u32();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    target.with_file_name(format!(
        "{name}.{}.{seq:x}{rand:08x}.tmp",
        std::process::id()
    ))
}

/// Remove `*.tmp` files older than `age` from the top of the node directory:
/// what a write interrupted by a crash leaves behind. Unique names mean such
/// a file is never overwritten by the next write, so without this they would
/// collect.
fn sweep_stale_tmp(dir: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().ends_with(".tmp") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|el| el >= age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Whether `path` lies inside `base`, compared the way Windows compares
/// paths (case-insensitively, component by component).
pub fn is_within(path: &Path, base: &Path) -> bool {
    let mut p = path.components();
    for b in base.components() {
        match p.next() {
            Some(c) if c.as_os_str().eq_ignore_ascii_case(b.as_os_str()) => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kmplify-node-dir-{tag}-{}-{}",
            std::process::id(),
            crate::status::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_second_serving_lock_on_one_dir_is_refused() {
        let dir = tmp_dir("lock");
        let first = ServeLock::acquire(&dir).expect("the first lock");
        match ServeLock::acquire(&dir) {
            Err(LockError::Held { .. }) => {}
            other => panic!("second lock must be refused, got {other:?}"),
        }
        drop(first);
        let again = ServeLock::acquire(&dir).expect("free again once the first is gone");
        assert!(again.path().ends_with(LOCK_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_dirs_are_not_affected() {
        let (a, b) = (tmp_dir("lock-a"), tmp_dir("lock-b"));
        let _la = ServeLock::acquire(&a).unwrap();
        let _lb = ServeLock::acquire(&b).expect("another node dir is another node");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// What the read and attach commands use, while a node holds the lock:
    /// status, settings, the published identity, the lock file itself.
    #[test]
    fn reads_and_writes_work_while_the_lock_is_held() {
        let dir = tmp_dir("read");
        let _held = ServeLock::acquire(&dir).unwrap();
        let s = crate::settings::Settings {
            router: Some("local".into()),
            ..Default::default()
        };
        s.save(&dir).expect("`set` writes next to a running node");
        assert_eq!(crate::settings::Settings::load(&dir), s);
        assert!(crate::status::read_published_result(&dir).is_ok());
        // Opening (not locking) the lock file is fine too.
        assert!(std::fs::metadata(dir.join(LOCK_FILE)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_message_names_the_dir_the_pid_and_the_way_out() {
        let dir = Path::new("/srv/node");
        let m = already_running_message(dir, Some(4242), Some(1_700_000_000_000));
        assert!(m.contains("already running in /srv/node"), "{m}");
        assert!(m.contains("pid 4242, started 2023-11-14"), "{m}");
        assert!(m.contains("KMPLIFY_NODE_DIR"), "{m}");
        let m = already_running_message(dir, None, None);
        assert!(m.contains("pid unknown"), "{m}");
    }

    #[test]
    fn temp_names_are_unique_and_beside_the_target() {
        let target = Path::new("/n/router.json");
        let names: std::collections::HashSet<PathBuf> =
            (0..1000).map(|_| unique_tmp(target)).collect();
        assert_eq!(names.len(), 1000);
        for n in &names {
            assert_eq!(n.parent(), target.parent());
            let s = n.file_name().unwrap().to_string_lossy().into_owned();
            let pid = std::process::id().to_string();
            assert!(s.starts_with(&format!("router.json.{pid}.")), "{s}");
            assert!(s.ends_with(".tmp"), "{s}");
            assert_ne!(s, "router.json.tmp");
        }
    }

    #[test]
    fn concurrent_atomic_writers_never_collide() {
        let dir = tmp_dir("race");
        let target = dir.join("router.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let target = target.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let tmp = unique_tmp(&target);
                        std::fs::write(&tmp, format!("{{\"w\":{i}}}")).unwrap();
                        std::fs::rename(&tmp, &target).expect("own tmp is never taken");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_old_tmp_files_are_swept() {
        let dir = tmp_dir("sweep");
        std::fs::write(dir.join("router.json.1.ab.tmp"), b"x").unwrap();
        std::fs::write(dir.join("settings.json"), b"{}").unwrap();
        sweep_stale_tmp(&dir, Duration::from_secs(3600));
        assert!(dir.join("router.json.1.ab.tmp").exists(), "fresh: kept");
        sweep_stale_tmp(&dir, Duration::ZERO);
        assert!(!dir.join("router.json.1.ab.tmp").exists(), "old: removed");
        assert!(dir.join("settings.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn within_compares_like_windows() {
        assert!(is_within(
            Path::new("/Users/Ann/.config/x"),
            Path::new("/users/ann")
        ));
        assert!(!is_within(
            Path::new("/Users/Annette/x"),
            Path::new("/Users/Ann")
        ));
        assert!(!is_within(Path::new("/srv"), Path::new("/srv/node")));
    }
}
