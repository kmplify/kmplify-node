//! The start-up trace and the crash log.
//!
//! A node that ends without a word is the hardest report there is: the
//! operator sees the prompt come back and nothing else. Two files in the
//! node directory say where it ended:
//!
//! - `startup.log`: one timestamped line per start step (version, which
//!   probe began and ended, the dashboard bind). Rewritten on every start,
//!   the previous one kept as `startup.prev.log`, at most [`MAX_LINES`]
//!   lines. When the process dies, the last line is the step it died in,
//!   whatever killed it: a panic, a native crash or another program.
//! - `startup-crash.log`: written by the panic hook, with the message, the
//!   location and a backtrace. In the OS temp directory when the node
//!   directory is not known yet or cannot be written.
//!
//! Neither file ever holds an environment value, a credential or a token:
//! the trace takes fixed step names and the arguments pass [`redact_args`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const TRACE_FILE: &str = "startup.log";
pub const PREVIOUS_TRACE_FILE: &str = "startup.prev.log";
pub const CRASH_FILE: &str = "startup-crash.log";
/// The crash file's name in the OS temp directory, where it is not alone.
pub const TEMP_CRASH_FILE: &str = "kmplify-node-startup-crash.log";
/// Most lines one start writes, the closing note included.
pub const MAX_LINES: usize = 200;

struct Trace {
    started: Instant,
    /// Where the trace goes, once known.
    dir: Option<PathBuf>,
    file: Option<std::fs::File>,
    /// Lines from before the directory was known.
    pending: Vec<String>,
    lines: usize,
}

fn cell() -> &'static Mutex<Trace> {
    static TRACE: OnceLock<Mutex<Trace>> = OnceLock::new();
    TRACE.get_or_init(|| {
        Mutex::new(Trace {
            started: Instant::now(),
            dir: None,
            file: None,
            pending: Vec::new(),
            lines: 0,
        })
    })
}

/// `2026-11-23T10:04:05.123Z`, from the system clock, without a date crate.
pub fn utc_stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

impl Trace {
    fn line(&self, msg: &str) -> String {
        format!(
            "{} +{:.3}s {}",
            utc_stamp(SystemTime::now()),
            self.started.elapsed().as_secs_f64(),
            msg.replace(['\r', '\n'], " ")
        )
    }

    /// Admit one more line under the cap; the last admitted one says so.
    fn admit(&mut self, line: String) -> Option<String> {
        if self.lines >= MAX_LINES {
            return None;
        }
        self.lines += 1;
        if self.lines == MAX_LINES {
            return Some(format!(
                "[trace stops here: {MAX_LINES} lines; later steps are not recorded]"
            ));
        }
        Some(line)
    }

    fn write(&mut self, line: &str) {
        if let Some(f) = self.file.as_mut() {
            // Unbuffered: each line reaches the OS before the next step
            // starts, so a process killed from outside keeps it.
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// Record one start step. Also flushes stdout and stderr, so whatever the
/// step printed is out before anything can end the process.
pub fn step(msg: impl AsRef<str>) {
    if let Ok(mut t) = cell().lock() {
        let line = t.line(msg.as_ref());
        if let Some(line) = t.admit(line) {
            if t.dir.is_some() {
                t.write(&line);
            } else {
                t.pending.push(line);
            }
        }
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

/// Where the trace goes from now on. The previous start's trace is kept as
/// `startup.prev.log`; the lines recorded so far are written first. A
/// directory that cannot be written leaves the trace in memory only (the
/// node still starts).
pub fn set_dir(dir: &Path) {
    let Ok(mut t) = cell().lock() else { return };
    if t.dir.is_some() {
        return;
    }
    let path = dir.join(TRACE_FILE);
    if path.exists() {
        let _ = std::fs::rename(&path, dir.join(PREVIOUS_TRACE_FILE));
    }
    t.file = std::fs::File::create(&path).ok();
    t.dir = Some(dir.to_path_buf());
    for line in std::mem::take(&mut t.pending) {
        t.write(&line);
    }
}

/// The node directory the trace was given, if any.
pub fn dir() -> Option<PathBuf> {
    cell().lock().ok().and_then(|t| t.dir.clone())
}

/// Add one line to the END of `dir`'s trace, without taking the trace over.
///
/// For a start that is refused because another node already serves from
/// `dir`: [`set_dir`] would rotate that node's trace away, and its trace is
/// what an operator needs. The refusal is appended instead, so the file says
/// both what the running node did and that a second one was turned away.
pub fn append_note(dir: &Path, msg: &str) {
    let line = format!(
        "{} {}",
        utc_stamp(SystemTime::now()),
        msg.replace(['\r', '\n'], " ")
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join(TRACE_FILE))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The command line as it may be written down: flags and short words stay,
/// everything else (URLs, paths, numbers, a `key=value`'s value) becomes
/// `…`. A value never reaches the file, whatever flag it belongs to.
pub fn redact_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if let Some((flag, _)) = a.split_once('=') {
                if flag.starts_with('-') || is_word(flag) {
                    return format!("{flag}=…");
                }
                return "…".to_string();
            }
            if (a.starts_with('-') && is_word(a.trim_start_matches('-'))) || is_word(a) {
                a.clone()
            } else {
                "…".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_word(s: &str) -> bool {
    !s.is_empty() && s.len() <= 24 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

/// What the panic hook writes: message, location, thread and a backtrace.
pub fn crash_report(info: &std::panic::PanicHookInfo<'_>, backtrace: &str) -> String {
    let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "(a panic without a message)".to_string()
    };
    let at = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "an unknown location".into());
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();
    format!(
        "kmplify-node {} crashed at {}\nthread '{thread}' panicked at {at}:\n{msg}\n\nbacktrace:\n{backtrace}\n",
        crate::version_string(),
        utc_stamp(SystemTime::now()),
    )
}

/// Write `text` to the crash file in `dir`, or in the OS temp directory when
/// there is no `dir` or it cannot be written. The path written, if any.
pub fn write_crash(dir: Option<&Path>, text: &str) -> Option<PathBuf> {
    dir.map(|d| d.join(CRASH_FILE))
        .into_iter()
        .chain(std::iter::once(std::env::temp_dir().join(TEMP_CRASH_FILE)))
        .find(|path| std::fs::write(path, text).is_ok())
}

/// Install the hook. First thing in `main()`, before anything can panic.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let report = crash_report(info, &backtrace);
        let _ = writeln!(std::io::stderr(), "\n[kmplify-node] {report}");
        // try_lock: a panic while the trace lock is held must not deadlock.
        let dir = cell().try_lock().ok().and_then(|t| t.dir.clone());
        match write_crash(dir.as_deref(), &report) {
            Some(p) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "[kmplify-node] the crash report is in {}",
                    p.display()
                );
            }
            None => {
                let _ = writeln!(
                    std::io::stderr(),
                    "[kmplify-node] could not write the crash report to a file"
                );
            }
        }
        if let Ok(mut t) = cell().try_lock() {
            let line = t.line("PANIC: see startup-crash.log");
            t.write(&line);
        }
        let _ = std::io::stderr().flush();
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kmplify-startup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_stamp_is_utc_iso() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_795_000_000_123);
        assert_eq!(utc_stamp(t), "2026-11-18T11:06:40.123Z");
        assert_eq!(utc_stamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // A leap day.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400);
        assert!(utc_stamp(t).starts_with("2000-02-29T"));
    }

    #[test]
    fn values_never_reach_the_trace() {
        let args: Vec<String> = [
            "run",
            "--ui",
            "gui",
            "--gateway",
            "https://user:secret@example.org/x",
            "--node-dir",
            "C:\\Users\\someone\\node",
            "--ui-port=8080",
            "set",
            "max-cpus=4",
            "TOKEN=abc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let r = redact_args(&args);
        assert_eq!(
            r,
            "run --ui gui --gateway … --node-dir … --ui-port=… set max-cpus=… …"
        );
        assert!(!r.contains("secret") && !r.contains("someone") && !r.contains("abc"));
    }

    /// The trace lives in one process-wide cell, so the cap is tested on a
    /// Trace of its own.
    #[test]
    fn the_trace_is_capped_and_says_so() {
        let mut t = Trace {
            started: Instant::now(),
            dir: None,
            file: None,
            pending: Vec::new(),
            lines: 0,
        };
        let admitted: Vec<String> = (0..MAX_LINES + 50)
            .filter_map(|i| t.admit(format!("step {i}")))
            .collect();
        assert_eq!(admitted.len(), MAX_LINES);
        assert_eq!(admitted[MAX_LINES - 2], format!("step {}", MAX_LINES - 2));
        assert!(admitted[MAX_LINES - 1].contains("trace stops here"));
    }

    /// One start: earlier lines are kept, the previous trace is rotated to
    /// startup.prev.log, and later steps are appended.
    #[test]
    fn the_trace_is_rotated_and_written() {
        let dir = tmp("trace");
        std::fs::write(dir.join(TRACE_FILE), "the previous start\n").unwrap();
        step("before the dir");
        set_dir(&dir);
        step("after the dir");
        let now = std::fs::read_to_string(dir.join(TRACE_FILE)).unwrap();
        assert!(
            now.contains("before the dir") && now.contains("after the dir"),
            "{now}"
        );
        assert!(!now.contains("the previous start"));
        assert_eq!(
            std::fs::read_to_string(dir.join(PREVIOUS_TRACE_FILE)).unwrap(),
            "the previous start\n"
        );
        assert_eq!(super::dir().as_deref(), Some(dir.as_path()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_crash_report_falls_back_to_the_temp_dir() {
        let missing = std::env::temp_dir().join("kmplify-no-such-dir/deeper");
        let p = write_crash(Some(&missing), "report").unwrap();
        assert_eq!(p, std::env::temp_dir().join(TEMP_CRASH_FILE));
        let dir = tmp("crash");
        assert_eq!(
            write_crash(Some(&dir), "report"),
            Some(dir.join(CRASH_FILE))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real hook, in a child process: this test binary runs the ignored
    /// test below on its own, which installs the hook and panics.
    #[tokio::test]
    async fn the_panic_hook_writes_the_crash_file() {
        let dir = tmp("hook");
        let out = crate::proc::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "startup::tests::panicking_child",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("KMPLIFY_STARTUP_TEST_DIR", &dir)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "the child was meant to panic");
        let report = std::fs::read_to_string(dir.join(CRASH_FILE)).expect("no crash file");
        assert!(report.contains("deliberate test panic"), "{report}");
        assert!(report.contains("src/startup.rs") || report.contains("src\\startup.rs"));
        assert!(report.contains("backtrace:"), "{report}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("deliberate test panic"), "{stderr}");
        let trace = std::fs::read_to_string(dir.join(TRACE_FILE)).unwrap();
        assert!(trace.contains("child step"), "{trace}");
        assert!(
            trace.trim_end().ends_with("PANIC: see startup-crash.log"),
            "{trace}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "run by the_panic_hook_writes_the_crash_file in a child process"]
    fn panicking_child() {
        let Some(dir) = std::env::var_os("KMPLIFY_STARTUP_TEST_DIR") else {
            return;
        };
        install_panic_hook();
        set_dir(Path::new(&dir));
        step("child step");
        panic!("deliberate test panic");
    }
}
