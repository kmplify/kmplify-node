//! Native host CPU telemetry.
//!
//! Read here, in the shell, rather than from the rag-backend — that runs in a
//! container and describes the Docker VM, not the machine. On this 13900K it
//! reported "16 physical cores" for a 24-core part, and on a Mac it would
//! describe a Linux VM instead of the M2 Max entirely. `sysinfo` reads the
//! real host on Windows, Linux and macOS (including Apple Silicon), which is
//! the same approach gpgpu-utilizer's sysmon takes.
//!
//! A percentage needs two samples: the first refresh after start has no
//! previous point to diff against and always reads 0. So a background thread
//! samples on a fixed interval and the command returns the latest value,
//! instead of every caller paying a blocking wait — and never showing 0%
//! merely because it asked early.

use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

/// sysinfo's own guidance: shorter than this and consecutive refreshes have
/// too little delta to be meaningful.
const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostCpu {
    /// Total load across all cores, 0-100.
    pub percent: f32,
    /// Logical CPUs — what `docker run --cpus` counts in.
    pub logical_cores: usize,
    /// Physical cores, when the platform can tell them apart. Falls back to
    /// logical rather than reporting 0, which would read as "no CPU".
    pub physical_cores: usize,
    /// e.g. "13th Gen Intel(R) Core(TM) i9-13900K" or "Apple M2 Max".
    pub model: String,
    /// True once at least two samples have been taken, so the UI can tell
    /// "genuinely idle" apart from "no reading yet".
    pub sampled: bool,
    /// Per-logical-CPU load, 0-100, in the platform's own core order.
    ///
    /// The total says how busy the machine is; this says HOW it is busy, which
    /// is the difference between "one thread pinned" and "everything warm" —
    /// and on a machine that lends cores to peers, that is the interesting
    /// question.
    pub per_core: Vec<f32>,
    /// HOST memory in MB — the machine's real RAM (unified memory on Apple
    /// Silicon), not the Docker VM's slice the containerized backend sees.
    /// A 64 GB M2 Max reported "7.7 GB total" through that path.
    pub ram_total_mb: u64,
    pub ram_used_mb: u64,
}

static STATE: OnceLock<Arc<Mutex<HostCpu>>> = OnceLock::new();

fn cell() -> &'static Arc<Mutex<HostCpu>> {
    STATE.get_or_init(|| Arc::new(Mutex::new(HostCpu::default())))
}

/// Start the sampler. Idempotent — safe to call on every reconcile.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );

    // Static facts, read once — and read HERE, before the sampler thread
    // exists. The wizard's "this machine" mirror and the worker's first
    // hello snapshot immediately after start() returns, and a thread that
    // has not run yet left them the default: no model name and 0 cores for
    // exactly the caller that asked first. A CPU does not grow cores at
    // runtime, so reading synchronously is also simply correct.
    let model = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();
    let logical = sys.cpus().len();
    let physical = sys.physical_core_count().unwrap_or(logical).max(1);
    {
        let mut w = cell().lock().unwrap();
        w.model = model;
        w.logical_cores = logical.max(1);
        w.physical_cores = physical;
    }

    std::thread::spawn(move || {
        let mut warmed = false;
        loop {
            std::thread::sleep(SAMPLE_INTERVAL);
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            let pct = sys.global_cpu_usage();
            let per_core: Vec<f32> = sys
                .cpus()
                .iter()
                .map(|c| c.cpu_usage().clamp(0.0, 100.0))
                .collect();
            let ram_total_mb = sys.total_memory() / (1024 * 1024);
            let ram_used_mb = sys.used_memory() / (1024 * 1024);
            let mut w = cell().lock().unwrap();
            w.ram_total_mb = ram_total_mb;
            w.ram_used_mb = ram_used_mb;
            // Skip the very first refresh: with no previous sample it is
            // always 0.0, and publishing that makes a busy machine look idle
            // for one interval.
            if warmed {
                w.percent = pct.clamp(0.0, 100.0);
                w.per_core = per_core;
                w.sampled = true;
            }
            warmed = true;
        }
    });
}

pub fn snapshot() -> HostCpu {
    // Poison-tolerant on purpose. This is read from the fabric worker's
    // hello and pong paths and, since the RAM probe moved here, from
    // host_ram_mb() as well — so an unwrap would turn one panic in the
    // sampler thread into a panic in the connection loop, taking the node
    // off the fabric. Stale-but-present readings beat losing the peer.
    match cell().lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Total system RAM in MB, read synchronously right now.
///
/// The sampler only fills `ram_total_mb` after its first timed refresh a
/// second in, and the fabric hello goes out before that — so callers need a
/// value that is correct at this instant. sysinfo reads it from the OS
/// directly on every platform (sysctl-equivalent on macOS, /proc/meminfo on
/// Linux, GlobalMemoryStatusEx on Windows), which is the whole point of
/// using it here rather than shelling out per platform: `wmic` — the old
/// Windows path — was deprecated in Windows 10 and is REMOVED from Windows
/// 11 24H2 and Server 2025, so that probe returned 0 on current Windows and
/// every RAM figure downstream of it read "0 GB".
pub fn read_ram_total_mb_now() -> u64 {
    use sysinfo::{MemoryRefreshKind, RefreshKind, System};
    let sys =
        System::new_with_specifics(RefreshKind::new().with_memory(MemoryRefreshKind::everything()));
    sys.total_memory() / (1024 * 1024)
}
