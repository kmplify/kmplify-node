//! Which accelerator this machine has, and what that implies for sessions.
//!
//! The node used to know exactly one thing: "is there a working nvidia-smi".
//! That answered the hello frame, gated container sessions, chose the docker
//! flags and read live VRAM, which meant an AMD or Intel card was a CPU node
//! with a nice name and an Apple GPU was advertised but never usable. This
//! module makes the vendor a value rather than a boolean, so adding one is a
//! new [`Backend`] arm instead of a new `if` in five places.
//!
//! # Two detectors, on purpose
//!
//! There are two questions here that look like one, and answering them with a
//! single probe is a bug in whichever direction you pick:
//!
//! | | [`detect`] | [`detect_installed`] |
//! |---|---|---|
//! | asks | is a GPU **usable**, and how big | is a driver stack **installed** |
//! | how | runs the vendor tool, parses VRAM | filesystem lookups only |
//! | cost | subprocesses, async | cheap, sync, safe on a UI thread |
//! | used by | the fabric, advertising capacity | a launcher or installer |
//!
//! They can legitimately disagree, and the disagreement is informative rather
//! than a fault. A host with `nvidia-smi` present but a broken driver is
//! `Cuda` to one and `Cpu` to the other. Both are right: there IS a CUDA
//! stack installed, and it CANNOT serve a session right now.
//!
//! Collapse them and you get one of two failures. Use the installed answer
//! for the fabric and the node advertises cards it cannot serve, so sessions
//! are scheduled onto it and fail after placement. Use the usable answer for
//! the launcher and one non-zero exit from a vendor tool pushes someone onto
//! remote GPUs while their own card sits idle.
//!
//! Everything here fails soft. A probe that is absent, unreadable, or from a
//! version whose output we cannot parse returns `None` and the next backend
//! is tried; the machine ends up CPU-only rather than crashing or, worse,
//! advertising capacity it cannot serve.
//!
//! Verified against real hardware for CUDA and Metal. The ROCm and oneAPI
//! paths are written against the documented output of `rocm-smi`, `amd-smi`
//! and `xpu-smi` and unit-tested on captured samples, but no AMD or Intel GPU
//! has run this code. Treat a first report from such a host as evidence, not
//! noise: `kmplify-node check` prints exactly what was detected.

use serde::Serialize;

/// The accelerator a node offers, in the order [`detect`] prefers them.
///
/// Preference is "most capable for LLM inference that we can actually drive
/// in a container", not popularity: a box with both an NVIDIA and an AMD card
/// reports CUDA because that is the one the template catalog can schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cuda,
    Rocm,
    /// Intel GPUs (Arc, Data Center Flex/Max) via oneAPI/Level Zero.
    OneApi,
    /// Apple Silicon unified memory. Serves inference; cannot host container
    /// sessions, see [`Backend::hosts_container_sessions`].
    Metal,
    Cpu,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Cuda => "cuda",
            Backend::Rocm => "rocm",
            Backend::OneApi => "oneapi",
            Backend::Metal => "metal",
            Backend::Cpu => "cpu",
        }
    }

    /// Parse a backend name from a config value or a `workload_start` frame.
    /// Unknown names are `None` so an unrecognised requirement fails closed
    /// rather than being treated as "no requirement".
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cuda" | "nvidia" => Some(Backend::Cuda),
            "rocm" | "hip" | "amd" => Some(Backend::Rocm),
            "oneapi" | "sycl" | "intel" | "levelzero" | "level_zero" => Some(Backend::OneApi),
            "metal" | "apple" => Some(Backend::Metal),
            "cpu" | "none" => Some(Backend::Cpu),
            _ => None,
        }
    }

    /// Human-facing name, for a launcher telling someone what it found.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Cuda => "CUDA (NVIDIA)",
            Backend::Rocm => "ROCm (AMD)",
            Backend::OneApi => "oneAPI (Intel Arc)",
            Backend::Metal => "Metal (Apple Silicon)",
            Backend::Cpu => "CPU only",
        }
    }

    /// Can a consumer's container get at this accelerator on this host?
    ///
    /// Metal is the interesting no: macOS has no GPU passthrough into Docker
    /// (the daemon runs in a VM), so a Mac is a first-class INFERENCE
    /// provider through its local model server and never a session host. A
    /// node that advertised otherwise would win sessions it then had to run
    /// on CPU at minutes per answer, which reads to the consumer as a hang.
    pub fn hosts_container_sessions(self) -> bool {
        matches!(self, Backend::Cuda | Backend::Rocm | Backend::OneApi)
    }

    /// The `docker run` flags that expose this accelerator to a session.
    ///
    /// Deliberately the narrowest form that works per vendor. Note what is
    /// NOT here: no `--privileged`, and no `--security-opt seccomp=unconfined`
    /// for ROCm even though some vendor images ask for it, because widening
    /// the sandbox for every session to satisfy one image is the wrong trade
    /// on someone else's machine. An image that genuinely needs more should
    /// be a catalog decision, visible in the template.
    pub fn docker_args(self) -> Vec<String> {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        match self {
            Backend::Cuda => v(&["--gpus", "all"]),
            // /dev/kfd is the compute device, /dev/dri the render nodes; the
            // video group is what makes them readable without root.
            Backend::Rocm => v(&[
                "--device",
                "/dev/kfd",
                "--device",
                "/dev/dri",
                "--group-add",
                "video",
            ]),
            // Level Zero reaches Intel GPUs through the render nodes alone.
            Backend::OneApi => v(&["--device", "/dev/dri"]),
            Backend::Metal | Backend::Cpu => Vec::new(),
        }
    }

    /// A command run INSIDE a started container to prove the accelerator is
    /// really visible there.
    ///
    /// `--gpus all` succeeding proves nothing: on a half-installed WSL2
    /// toolkit it succeeds while the driver stays invisible inside, and the
    /// consumer rents "GPU" time to get CPU inference. `None` means there is
    /// no cheap in-container probe for this backend and the session is taken
    /// at face value.
    pub fn container_probe(self) -> Option<&'static [&'static str]> {
        match self {
            Backend::Cuda => Some(&["nvidia-smi", "-L"]),
            Backend::Rocm => Some(&["rocm-smi", "--showid"]),
            // No universal Intel probe: xpu-smi is not in most images, so
            // presence of a render node is the honest check.
            Backend::OneApi => Some(&["test", "-e", "/dev/dri/renderD128"]),
            Backend::Metal | Backend::Cpu => None,
        }
    }
}

/// What a probe found. `total_mb` is the card's own memory, before the
/// operator's ceiling is applied by the caller.
#[derive(Debug, Clone)]
pub struct Gpu {
    pub backend: Backend,
    pub name: String,
    pub total_mb: u64,
}

// ---------------------------------------------------------------------------
// Pure parsers. Split out from the probes so the formats are testable without
// the hardware or the vendor tool -- which is the only way this file could be
// written honestly for vendors we cannot run here.
// ---------------------------------------------------------------------------

/// First field of the first data line of an `nvidia-smi --format=csv,noheader`
/// reply.
pub(crate) fn parse_nvidia_first(out: &str) -> Option<String> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let field = line.split(',').next()?.trim();
    (!field.is_empty()).then(|| field.to_string())
}

/// Megabytes from a `nounits` nvidia-smi field, which is already MB.
pub(crate) fn parse_nvidia_mb(out: &str) -> Option<u64> {
    parse_nvidia_first(out)?
        .parse::<f64>()
        .ok()
        .map(|v| v as u64)
}

/// `(total_mb, used_mb)` from `rocm-smi --showmeminfo vram --csv`.
///
/// The header names shift between ROCm releases ("VRAM Total Memory (B)" vs
/// "VRAM Total Memory"), so columns are found by substring rather than by an
/// exact name or a fixed index. Values are BYTES.
pub(crate) fn parse_rocm_meminfo_csv(out: &str) -> Option<(u64, u64)> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next()?.to_ascii_lowercase();
    let cols: Vec<&str> = header.split(',').map(|c| c.trim()).collect();
    let total_i = cols
        .iter()
        .position(|c| c.contains("total") && c.contains("vram"))?;
    let used_i = cols
        .iter()
        .position(|c| c.contains("used") && c.contains("vram"));
    let row: Vec<&str> = lines.next()?.split(',').map(|c| c.trim()).collect();
    let bytes = |i: usize| -> Option<u64> { row.get(i)?.parse::<f64>().ok().map(|v| v as u64) };
    let total = bytes(total_i)? / (1024 * 1024);
    let used = used_i
        .and_then(bytes)
        .map(|b| b / (1024 * 1024))
        .unwrap_or(0);
    Some((total, used))
}

/// Card name from `rocm-smi --showproductname --csv`, preferring the human
/// series ("Radeon RX 7900 XTX") over the hex model id.
pub(crate) fn parse_rocm_product_csv(out: &str) -> Option<String> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next()?.to_ascii_lowercase();
    let cols: Vec<&str> = header.split(',').map(|c| c.trim()).collect();
    let row: Vec<&str> = lines.next()?.split(',').map(|c| c.trim()).collect();
    for key in ["card series", "card model", "market name", "device name"] {
        if let Some(i) = cols.iter().position(|c| c.contains(key)) {
            if let Some(v) = row.get(i) {
                let v = v.trim();
                // Skip the raw ids ROCm reports when it has no series string.
                if !v.is_empty() && !v.starts_with("0x") && !v.eq_ignore_ascii_case("n/a") {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// `(name, total_mb)` from `xpu-smi discovery -j` / `amd-smi ... --json`.
///
/// Hand-scanned rather than deserialized into a schema: these tools reshape
/// their JSON between releases, and a node that stops detecting a GPU after a
/// driver update is a worse outcome than one that reads a couple of keys
/// loosely. Memory keys are treated as MB unless they are large enough to
/// only make sense as bytes.
pub(crate) fn parse_gpu_json(out: &str) -> Option<(Option<String>, Option<u64>)> {
    let v: serde_json::Value = serde_json::from_str(out).ok()?;
    let mut name = None;
    let mut mem_mb = None;
    fn walk(v: &serde_json::Value, name: &mut Option<String>, mem: &mut Option<u64>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let key = k.to_ascii_lowercase();
                    if name.is_none()
                        && (key.contains("device_name")
                            || key.contains("market_name")
                            || key == "name")
                    {
                        if let Some(s) = val.as_str() {
                            if !s.trim().is_empty() {
                                *name = Some(s.trim().to_string());
                            }
                        }
                    }
                    if mem.is_none()
                        && key.contains("memory")
                        && (key.contains("total")
                            || key.contains("physical")
                            || key.contains("size"))
                    {
                        let raw = val.as_u64().or_else(|| {
                            val.as_str().and_then(|s| {
                                s.trim()
                                    .trim_end_matches(|c: char| {
                                        c.is_alphabetic() || c.is_whitespace()
                                    })
                                    .parse::<u64>()
                                    .ok()
                            })
                        });
                        if let Some(n) = raw {
                            // > 1 TiB expressed in MB is not a GPU; it is bytes.
                            *mem = Some(if n > 1024 * 1024 {
                                n / (1024 * 1024)
                            } else {
                                n
                            });
                        }
                    }
                    walk(val, name, mem);
                }
            }
            serde_json::Value::Array(items) => {
                for i in items {
                    walk(i, name, mem);
                }
            }
            _ => {}
        }
    }
    walk(&v, &mut name, &mut mem_mb);
    (name.is_some() || mem_mb.is_some()).then_some((name, mem_mb))
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

async fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = crate::proc::command(bin).args(args).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn probe_cuda() -> Option<Gpu> {
    let name = run("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"])
        .await
        .and_then(|o| parse_nvidia_first(&o))
        .unwrap_or_else(|| "NVIDIA GPU".to_string());
    let total = run(
        "nvidia-smi",
        &["--query-gpu=memory.total", "--format=csv,noheader,nounits"],
    )
    .await
    .and_then(|o| parse_nvidia_mb(&o))?;
    Some(Gpu {
        backend: Backend::Cuda,
        name,
        total_mb: total,
    })
}

async fn probe_rocm() -> Option<Gpu> {
    // rocm-smi first (ubiquitous), amd-smi second (its replacement).
    if let Some(out) = run("rocm-smi", &["--showmeminfo", "vram", "--csv"]).await {
        if let Some((total, _)) = parse_rocm_meminfo_csv(&out) {
            let name = run("rocm-smi", &["--showproductname", "--csv"])
                .await
                .and_then(|o| parse_rocm_product_csv(&o))
                .unwrap_or_else(|| "AMD GPU".to_string());
            return Some(Gpu {
                backend: Backend::Rocm,
                name,
                total_mb: total,
            });
        }
    }
    let out = run("amd-smi", &["static", "--json"]).await?;
    let (name, mem) = parse_gpu_json(&out)?;
    Some(Gpu {
        backend: Backend::Rocm,
        name: name.unwrap_or_else(|| "AMD GPU".to_string()),
        total_mb: mem?,
    })
}

async fn probe_oneapi() -> Option<Gpu> {
    let out = run("xpu-smi", &["discovery", "-j"]).await?;
    let (name, mem) = parse_gpu_json(&out)?;
    Some(Gpu {
        backend: Backend::OneApi,
        name: name.unwrap_or_else(|| "Intel GPU".to_string()),
        total_mb: mem.unwrap_or(0),
    })
}

async fn probe_metal() -> Option<Gpu> {
    if !(cfg!(target_os = "macos") && cfg!(target_arch = "aarch64")) {
        return None;
    }
    let name = run("sysctl", &["-n", "machdep.cpu.brand_string"])
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Apple Silicon".to_string());
    // What the GPU can actually address, not total RAM: unified memory is
    // shared and the GPU is capped well below the machine's total.
    Some(Gpu {
        backend: Backend::Metal,
        name,
        total_mb: super::fabric_worker::gpu_addressable_mb(),
    })
}

/// Which vendor's driver stack is INSTALLED here, without running anything.
///
/// A different question from [`detect`], and deliberately so. `detect` asks
/// whether a GPU can be *used and sized* right now: it executes the vendor
/// tool and parses VRAM, because advertising capacity to a fabric means
/// promising work can actually land. This asks only whether the tooling
/// exists, which is what an installer or launcher wants when deciding
/// "is local inference worth attempting on this machine".
///
/// The two can legitimately disagree. A host with `nvidia-smi` present but a
/// broken driver is `Cuda` here and `Cpu` there, and both answers are right
/// for their caller. Collapsing them into one probe was tempting and would
/// have been a bug: the fabric would start advertising cards it cannot serve,
/// or the launcher would push someone onto remote GPUs because a tool exited
/// non-zero once.
///
/// Synchronous and cheap: filesystem lookups only, no subprocess except the
/// single Rosetta check below, so it is safe on a UI thread.
// Each cfg block below is a whole alternative implementation, and exactly one
// survives compilation -- which makes its `return` the tail expression and so
// technically needless. Kept anyway: without them the three blocks read as
// fallthrough rather than as alternatives, which is precisely the misreading
// that would let someone "simplify" macOS into the Linux path.
#[allow(clippy::needless_return)]
pub fn detect_installed() -> Backend {
    // An aarch64 build IS Apple Silicon. No subprocess can fail here, which
    // matters: an earlier version shelled out unconditionally and treated any
    // spawn error as "no GPU", demoting an M-series Mac to CPU-only under
    // parallel load.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Backend::Metal;
    }
    // An x86_64 build on macOS still has to ask: under Rosetta the host may be
    // Apple Silicon even though this binary is not, and `hw.optional.arm64`
    // reports 1 there. A real Intel Mac answers 0 and stays CPU-only on
    // purpose, since Metal needs an Apple GPU and no discrete card changes
    // that.
    #[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
    {
        let rosetta = std::process::Command::new("sysctl")
            .args(["-n", "hw.optional.arm64"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        return if rosetta {
            Backend::Metal
        } else {
            Backend::Cpu
        };
    }
    // Windows and Linux: whichever vendor stack is present. Ordered by how
    // well local inference is supported, so a machine with two picks the
    // better one.
    #[cfg(not(target_os = "macos"))]
    {
        if nvidia_installed() {
            return Backend::Cuda;
        }
        if rocm_installed() {
            return Backend::Rocm;
        }
        if oneapi_installed() {
            return Backend::OneApi;
        }
        Backend::Cpu
    }
}

/// NVIDIA: `nvidia-smi` on PATH, or in System32 on Windows.
///
/// The Windows special case is load-bearing rather than defensive. The driver
/// installs the tool into System32, and a GUI launch can carry a trimmed PATH
/// where the ordinary lookup misses it -- which reported a 4090 as "no GPU".
pub fn nvidia_installed() -> bool {
    #[cfg(windows)]
    {
        if let Ok(root) = std::env::var("SystemRoot") {
            if std::path::Path::new(&root)
                .join("System32")
                .join("nvidia-smi.exe")
                .exists()
            {
                return true;
            }
        }
    }
    crate::proc::find("nvidia-smi").is_some()
}

/// AMD: the ROCm stack on Linux, or the HIP SDK on Windows.
///
/// The Windows driver drops ROCm into Program Files without necessarily
/// putting it on PATH, the same trimmed-PATH problem as above.
pub fn rocm_installed() -> bool {
    #[cfg(windows)]
    {
        for var in ["ProgramFiles", "ProgramW6432"] {
            if let Ok(root) = std::env::var(var) {
                if std::path::Path::new(&root)
                    .join("AMD")
                    .join("ROCm")
                    .exists()
                {
                    return true;
                }
            }
        }
    }
    ["rocm-smi", "rocminfo", "hipInfo", "amd-smi"]
        .iter()
        .any(|t| crate::proc::find(t).is_some())
}

/// Intel: oneAPI tooling on PATH, or the toolkit's install directory on
/// Windows.
///
/// Reports what the HOST has, not what a given runtime can use: official
/// Ollama reaches Arc through Vulkan while the ipex-llm fork uses oneAPI
/// directly. Either way the machine has a GPU worth trying.
pub fn oneapi_installed() -> bool {
    #[cfg(windows)]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(root) = std::env::var(var) {
                if std::path::Path::new(&root)
                    .join("Intel")
                    .join("oneAPI")
                    .exists()
                {
                    return true;
                }
            }
        }
    }
    ["xpu-smi", "sycl-ls"]
        .iter()
        .any(|t| crate::proc::find(t).is_some())
}

/// Probe one specific backend.
pub async fn probe(backend: Backend) -> Option<Gpu> {
    match backend {
        Backend::Cuda => probe_cuda().await,
        Backend::Rocm => probe_rocm().await,
        Backend::OneApi => probe_oneapi().await,
        Backend::Metal => probe_metal().await,
        Backend::Cpu => None,
    }
}

/// Everything this host offers, best first. Empty means CPU-only.
///
/// All vendors are probed rather than stopping at the first hit, so a
/// mixed-vendor box can say so in `check` instead of silently hiding a card.
pub async fn detect_all() -> Vec<Gpu> {
    let (cuda, rocm, oneapi, metal) =
        tokio::join!(probe_cuda(), probe_rocm(), probe_oneapi(), probe_metal());
    [cuda, rocm, oneapi, metal].into_iter().flatten().collect()
}

/// The backend this node advertises, honouring the operator's override.
///
/// `KMPLIFY_GPU_BACKEND` names one explicitly; the older `KMPLIFY_CUDA=1/0`
/// still forces CUDA on or off, because deployments already set it.
pub async fn detect() -> (Backend, Option<Gpu>) {
    if let Ok(forced) = std::env::var("KMPLIFY_GPU_BACKEND") {
        if let Some(b) = Backend::parse(&forced) {
            let found = detect_all().await.into_iter().find(|g| g.backend == b);
            return (b, found);
        }
    }
    match std::env::var("KMPLIFY_CUDA").ok().as_deref() {
        Some("0") => return (Backend::Cpu, None),
        Some("1") => {
            let found = probe_cuda().await;
            return (Backend::Cuda, found);
        }
        _ => {}
    }
    match detect_all().await.into_iter().next() {
        Some(g) => (g.backend, Some(g)),
        None => (Backend::Cpu, None),
    }
}

/// Accelerator memory currently in use, MB. `None` when the vendor gives us
/// no cheap way to ask.
pub async fn used_mb(backend: Backend) -> Option<u64> {
    match backend {
        Backend::Cuda => run(
            "nvidia-smi",
            &["--query-gpu=memory.used", "--format=csv,noheader,nounits"],
        )
        .await
        .and_then(|o| parse_nvidia_mb(&o)),
        Backend::Rocm => run("rocm-smi", &["--showmeminfo", "vram", "--csv"])
            .await
            .and_then(|o| parse_rocm_meminfo_csv(&o))
            .map(|(_, used)| used),
        // Unified memory: "used VRAM" is not a distinct number, and reporting
        // system RAM here would make the gateway's capacity view wrong.
        Backend::OneApi | Backend::Metal | Backend::Cpu => None,
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn nvidia_csv_shapes() {
        assert_eq!(
            parse_nvidia_first("NVIDIA GeForce RTX 4090\n").as_deref(),
            Some("NVIDIA GeForce RTX 4090")
        );
        assert_eq!(parse_nvidia_mb("24564\n"), Some(24564));
        // Multi-GPU: first card wins, and a trailing blank line is not a card.
        assert_eq!(parse_nvidia_mb("24564\n24564\n\n"), Some(24564));
        assert_eq!(parse_nvidia_first(""), None);
    }

    /// Captured from `rocm-smi --showmeminfo vram --csv`. Bytes in, MB out.
    #[test]
    fn rocm_meminfo_csv() {
        let out = "device,VRAM Total Memory (B),VRAM Total Used Memory (B)\ncard0,25753026560,1073741824\n";
        assert_eq!(parse_rocm_meminfo_csv(out), Some((24560, 1024)));
    }

    /// Header wording moves between ROCm releases; columns are found by
    /// substring so a rename does not silently zero the card.
    #[test]
    fn rocm_meminfo_survives_header_rename() {
        let out = "device,VRAM Total Memory,VRAM Total Used Memory\ncard0,25753026560,0\n";
        assert_eq!(parse_rocm_meminfo_csv(out), Some((24560, 0)));
        // Used column absent entirely: total still reported.
        let no_used = "device,VRAM Total Memory (B)\ncard0,25753026560\n";
        assert_eq!(parse_rocm_meminfo_csv(no_used), Some((24560, 0)));
    }

    #[test]
    fn rocm_product_name_prefers_series_over_hex_id() {
        let out = "device,Card Series,Card Model,Card Vendor\ncard0,Radeon RX 7900 XTX,0x744c,Advanced Micro Devices\n";
        assert_eq!(
            parse_rocm_product_csv(out).as_deref(),
            Some("Radeon RX 7900 XTX")
        );
        // No series: fall through the hex id rather than reporting "0x744c".
        let hex = "device,Card Series,Card Model\ncard0,0x744c,0x744c\n";
        assert_eq!(parse_rocm_product_csv(hex), None);
    }

    #[test]
    fn gpu_json_is_scanned_not_schema_bound() {
        let xpu = r#"{"device_list":[{"device_name":"Intel(R) Arc(TM) A770 Graphics","memory_physical_size_byte":17179869184}]}"#;
        let (name, mem) = parse_gpu_json(xpu).unwrap();
        assert_eq!(name.as_deref(), Some("Intel(R) Arc(TM) A770 Graphics"));
        assert_eq!(mem, Some(16384));
        // A megabyte-valued key is not re-divided into nothing.
        let mb = r#"{"gpu":{"name":"Radeon","memory_total":24560}}"#;
        assert_eq!(parse_gpu_json(mb).unwrap().1, Some(24560));
        assert_eq!(parse_gpu_json("not json"), None);
    }

    #[test]
    fn backend_names_round_trip_and_unknown_fails_closed() {
        for b in [
            Backend::Cuda,
            Backend::Rocm,
            Backend::OneApi,
            Backend::Metal,
            Backend::Cpu,
        ] {
            assert_eq!(Backend::parse(b.as_str()), Some(b));
        }
        assert_eq!(Backend::parse("NVIDIA"), Some(Backend::Cuda));
        assert_eq!(Backend::parse("amd"), Some(Backend::Rocm));
        assert_eq!(Backend::parse("intel"), Some(Backend::OneApi));
        // An unrecognised requirement must not read as "no requirement".
        assert_eq!(Backend::parse("tpu"), None);
        assert_eq!(Backend::parse(""), None);
    }

    /// Metal serves inference but cannot host a consumer's container: macOS
    /// has no GPU passthrough into the Docker VM.
    #[test]
    fn only_passthrough_capable_backends_host_sessions() {
        assert!(Backend::Cuda.hosts_container_sessions());
        assert!(Backend::Rocm.hosts_container_sessions());
        assert!(Backend::OneApi.hosts_container_sessions());
        assert!(!Backend::Metal.hosts_container_sessions());
        assert!(!Backend::Cpu.hosts_container_sessions());
    }

    /// The two detectors answer different questions and may disagree; that
    /// is the design, not a bug. This pins the vocabulary they share.
    #[test]
    fn installed_detection_agrees_with_this_host() {
        let installed = detect_installed();
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(installed, Backend::Metal, "an aarch64 mac is always Metal");
        }
        // Whatever it says, it must be a backend the rest of the module can
        // talk about: a label, and a defined session capability.
        assert!(!installed.label().is_empty());
        let _ = installed.hosts_container_sessions();
    }

    #[test]
    fn every_backend_has_a_human_label() {
        for b in [
            Backend::Cuda,
            Backend::Rocm,
            Backend::OneApi,
            Backend::Metal,
            Backend::Cpu,
        ] {
            assert!(!b.label().is_empty(), "{b:?} has no label");
            // The label names the vendor, not the enum arm, so a launcher can
            // show it verbatim.
            assert!(
                b.label().len() > 3,
                "{b:?} label is too terse: {}",
                b.label()
            );
        }
        assert_eq!(Backend::Cpu.label(), "CPU only");
    }

    /// Presence probes answer about THEIR vendor only. Conflating them is how
    /// a ROCm box ends up with an NVIDIA container reservation it cannot
    /// honour.
    #[test]
    fn vendor_presence_probes_are_independent() {
        let probes = [nvidia_installed(), rocm_installed(), oneapi_installed()];
        // Each is a real, independent answer; a host may legitimately have
        // none, one, or several vendor stacks installed at once.
        assert!(probes.iter().filter(|x| **x).count() <= probes.len());
    }

    /// A vendor mismatch must refuse, not "try anyway": an AMD host given a
    /// CUDA image starts a container that cannot see a GPU and bills the
    /// consumer for CPU inference.
    #[test]
    fn accelerators_do_not_substitute_for_each_other() {
        for (have, need) in [
            (Backend::Cuda, Backend::Rocm),
            (Backend::Rocm, Backend::Cuda),
            (Backend::OneApi, Backend::Cuda),
            (Backend::Cpu, Backend::Cuda),
        ] {
            assert_ne!(have, need, "{have:?} must not satisfy {need:?}");
        }
        // And the flags really are distinct, so a mismatch cannot pass
        // silently by producing the same docker invocation.
        assert_ne!(Backend::Cuda.docker_args(), Backend::Rocm.docker_args());
        assert_ne!(Backend::Rocm.docker_args(), Backend::OneApi.docker_args());
    }

    /// The sandbox is not widened for any vendor: no --privileged anywhere.
    #[test]
    fn docker_args_stay_narrow() {
        assert_eq!(Backend::Cuda.docker_args(), vec!["--gpus", "all"]);
        assert!(Backend::Rocm
            .docker_args()
            .contains(&"/dev/kfd".to_string()));
        assert!(Backend::OneApi
            .docker_args()
            .contains(&"/dev/dri".to_string()));
        assert!(Backend::Metal.docker_args().is_empty());
        for b in [Backend::Cuda, Backend::Rocm, Backend::OneApi] {
            let args = b.docker_args().join(" ");
            assert!(!args.contains("--privileged"), "{b:?} widens the sandbox");
            assert!(!args.contains("seccomp"), "{b:?} relaxes seccomp");
        }
    }
}
