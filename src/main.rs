//! kmplify-node — the headless KMPLIFY provider.
//!
//! The same fabric worker the desktop app runs (fabric_worker.rs, with all
//! its hardening: container-death reporting, reconnect-safe state, live
//! inventory, operator ceilings), with everything else stripped: no webview,
//! no chat, no RAG stack. For GUI-less Linux hosts, and anywhere else a
//! machine should lend GPU time without being someone's desktop.
//!
//! Six commands, `kmplify-node help` for the full text:
//!
//!   run       join the fabric and serve (the default)
//!   tui       full-screen dashboard: watch AND control the node
//!   check     preflight this host without connecting — run it first
//!   status    one-shot report of the node running here
//!   id        print this install's node id
//!   version   version and build stamp
//!
//! Configuration is environment variables, deliberately the SAME names the
//! desktop writes into its stack .env, so knowledge transfers 1:1. Every one
//! also has a flag (see `cli.rs`), which sets the variable rather than living
//! beside it, so there is only ever one configuration surface:
//!
//!   PROVIDER_GATEWAY_URL   gateway to join      (default: the public fabric)
//!   PROVIDER_WORKLOADS     template ids to host (default: EMPTY = sessions
//!                          off; running other people's containers is opt-in
//!                          here exactly as it is in the app)
//!   PROVIDER_MAX_CPUS      ceiling on CPUs lent to sessions
//!   PROVIDER_MAX_VRAM_MB   ceiling on advertised VRAM
//!   PROVIDER_MAX_RAM_MB    ceiling on advertised RAM
//!   PROVIDER_MAX_DISK_GB   ceiling on disk sessions may fill
//!   PROVIDER_COUNTRY       ISO alpha-2 for EU-only consumers, "" = undeclared
//!   PROVIDER_SHARE_INFERENCE  serve chat/embedding jobs (default true)
//!   PROVIDER_SHARE_CPU     lend spare CPU threads and RAM (default false)
//!   PROVIDER_APPROVAL_MODE auto | manual consumer admission
//!   OLLAMA_BASE            host Ollama for model serving (default localhost)
//!   COLIBRI_BASE           optional colibri gateway for frontier models
//!   KMPLIFY_NODE_DIR       identity/credentials dir
//!                          (default: $XDG_CONFIG_HOME|~/.config /kmplify-node)
//!   KMPLIFY_GPU_BACKEND    force the accelerator: cuda|rocm|oneapi|metal|cpu
//!   KMPLIFY_CUDA           older CUDA-only override: 1/0
//!   PROVIDER_FUNCTIONS     host signed Wasm functions (true/false, default off)
//!   PROVIDER_FUNCTIONS_PUBKEY  hex Ed25519 key of the catalog to trust
//!   PROVIDER_MAX_FUNCTION_MB / _MS   per-call memory and wall-clock ceilings
//!   PROVIDER_SHARE_VECTORS lend vector-collection storage (default off)
//!   PROVIDER_MAX_VECTOR_MB ceiling on stored collections (default 1024)
//!
//! A malformed value is a startup error rather than a silent fallback. It was
//! the other way round, and `PROVIDER_MAX_CPUS=eigth` then meant "no ceiling"
//! on a machine whose owner believed they had set one.

mod cli;
mod onboard;
#[cfg(feature = "tui")]
mod tui;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use kmplify_node::fabric_worker::{self, WorkerConfig};
use kmplify_node::gpu::{self, Backend};
use kmplify_node::settings::Settings;
use kmplify_node::{control, status, PUBLIC_FABRIC_URL};

/// Ready, or a clean stop.
const EXIT_OK: i32 = 0;
/// This host cannot serve as configured. Provisioning should stop here.
const EXIT_UNUSABLE: i32 = 1;
/// The command line or the configuration is wrong.
const EXIT_USAGE: i32 = 2;

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A boolean from the environment, spelled the way people actually spell them.
///
/// The old parsing was two different rules — `!= "false"` for one variable and
/// `== "true"` for the rest — so `PROVIDER_SHARE_INFERENCE=0` meant ON and
/// `PROVIDER_FUNCTIONS=1` meant OFF. Both are now what they look like, and a
/// value that is neither is an error rather than a guess.
fn env_bool(key: &str, default: bool, errs: &mut Vec<String>) -> bool {
    match env_opt(key) {
        None => default,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                errs.push(format!(
                    "{key}={v} is not a boolean (true/false, 1/0, yes/no, on/off)"
                ));
                default
            }
        },
    }
}

fn env_num<T: std::str::FromStr>(key: &str, errs: &mut Vec<String>) -> Option<T> {
    let raw = env_opt(key)?;
    match raw.parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            errs.push(format!("{key}={raw} is not a number"));
            None
        }
    }
}

fn node_dir() -> PathBuf {
    if let Some(d) = env_opt("KMPLIFY_NODE_DIR") {
        return PathBuf::from(d);
    }
    // XDG first, then ~/.config, then the cwd as a last resort — a headless
    // box without HOME set (some service managers) still gets a stable path
    // relative to WorkingDirectory= rather than a panic.
    if let Some(x) = env_opt("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("kmplify-node");
    }
    if let Some(h) = env_opt("HOME").or_else(|| env_opt("USERPROFILE")) {
        return PathBuf::from(h).join(".config").join("kmplify-node");
    }
    PathBuf::from(".kmplify-node")
}

/// Run `cmd`, giving up after `timeout`.
///
/// The ceiling is the point: `docker version` against a wedged socket blocks
/// until the daemon answers, which on a broken host is never, and `check` is
/// exactly the command someone runs on a broken host. `kill_on_drop` so the
/// abandoned probe does not outlive us as an orphan.
async fn probe(timeout: Duration, cmd: &str, args: &[&str]) -> Option<String> {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, child).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
    )
}

// The trailing `..Default::default()` below is redundant TODAY, which is
// precisely why clippy objects and precisely why it stays: it is what makes
// the next field added to WorkerConfig a non-event here instead of a build
// break, and this binary is the example embedders copy.
#[allow(clippy::needless_update)]
fn resolve_config(errs: &mut Vec<String>) -> WorkerConfig {
    let dir = node_dir();

    // Trailing slashes are trimmed rather than tolerated: every URL here is
    // used with `format!("{base}/path")`, and `https://gw/` then produces a
    // double slash that some gateways route and others 404.
    let gateway_url = env_opt("PROVIDER_GATEWAY_URL")
        .unwrap_or_else(|| PUBLIC_FABRIC_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    if !gateway_url.starts_with("http://") && !gateway_url.starts_with("https://") {
        errs.push(format!(
            "PROVIDER_GATEWAY_URL={gateway_url} must start with http:// or https:// \
             (the node derives its ws:// URL from it)"
        ));
    }

    // An alpha-2 code or nothing. The gateway turns anything else into "XX",
    // which quietly hides the node from every EU-only consumer — a typo that
    // costs work for weeks and shows up nowhere.
    let country = match env_opt("PROVIDER_COUNTRY") {
        None => String::new(),
        Some(c) => {
            let up = c.to_ascii_uppercase();
            if up.len() == 2 && up.chars().all(|ch| ch.is_ascii_alphabetic()) {
                up
            } else {
                errs.push(format!(
                    "PROVIDER_COUNTRY={c} is not an ISO-3166-1 alpha-2 code (DE, FR, US …)"
                ));
                String::new()
            }
        }
    };

    let approval_mode = match env_opt("PROVIDER_APPROVAL_MODE") {
        None => "auto".to_string(),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            // Not fail-open on a typo: "Manual" used to resolve to "auto",
            // silently admitting every consumer on a node whose owner had
            // asked to vet them.
            m @ ("auto" | "manual") => m.to_string(),
            _ => {
                errs.push(format!("PROVIDER_APPROVAL_MODE={v} is not auto or manual"));
                "manual".to_string()
            }
        },
    };

    if let Some(forced) = env_opt("KMPLIFY_GPU_BACKEND") {
        if Backend::parse(&forced).is_none() {
            errs.push(format!(
                "KMPLIFY_GPU_BACKEND={forced} is not cuda, rocm, oneapi, metal or cpu"
            ));
        }
    }

    WorkerConfig {
        gateway_url,
        ollama_base: env_opt("OLLAMA_BASE")
            .unwrap_or_else(|| "http://127.0.0.1:11434".to_string())
            .trim_end_matches('/')
            .to_string(),
        // Both spellings on purpose: COLIBRI_BASE matches the reference
        // Python worker's env, PROVIDER_COLIBRI_BASE matches the desktop
        // stack's .env family. Empty = no colibri upstream.
        colibri_base: env_opt("COLIBRI_BASE")
            .or_else(|| env_opt("PROVIDER_COLIBRI_BASE"))
            .map(|v| v.trim_end_matches('/').to_string())
            .unwrap_or_default(),
        colibri_api_key: env_opt("COLIBRI_API_KEY")
            .or_else(|| env_opt("PROVIDER_COLIBRI_API_KEY"))
            .unwrap_or_default(),
        creds_path: fabric_worker::default_creds_path(&dir),
        country,
        workload_templates: env_opt("PROVIDER_WORKLOADS")
            .map(|w| {
                w.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        max_shared_cpus: env_num("PROVIDER_MAX_CPUS", errs),
        max_shared_vram_mb: env_num("PROVIDER_MAX_VRAM_MB", errs),
        max_shared_ram_mb: env_num("PROVIDER_MAX_RAM_MB", errs),
        max_shared_disk_gb: env_num("PROVIDER_MAX_DISK_GB", errs),
        // Headless default: a node someone runs on purpose shares inference
        // unless it says otherwise — matching pre-switch behaviour.
        share_inference: env_bool("PROVIDER_SHARE_INFERENCE", true, errs),
        share_cpu: if std::env::var("PROVIDER_SHARE_CPU").is_ok() {
            env_bool("PROVIDER_SHARE_CPU", false, errs)
        } else {
            env_bool("KMPLIFY_SHARE_CPU", false, errs)
        },
        approval_mode,
        // Filled in by the caller once the accelerator has been probed; doing
        // it here would make every command that merely reads configuration
        // pay for four subprocesses.
        cuda: false,
        accelerator: Backend::Cpu,
        // Protocol v3.0 lanes, both opt-in. Functions additionally need the
        // gateway's function key, or the node trusts nothing and refuses all.
        functions: kmplify_node::functions::FunctionsConfig {
            enabled: env_bool("PROVIDER_FUNCTIONS", false, errs),
            trusted_pubkey: env_opt("PROVIDER_FUNCTIONS_PUBKEY").unwrap_or_default(),
            max_memory_mb: env_num("PROVIDER_MAX_FUNCTION_MB", errs).unwrap_or(256),
            max_ms: env_num("PROVIDER_MAX_FUNCTION_MS", errs).unwrap_or(30_000),
        },
        vectors: kmplify_node::vectors::VectorsConfig {
            enabled: env_bool("PROVIDER_SHARE_VECTORS", false, errs),
            max_mb: env_num("PROVIDER_MAX_VECTOR_MB", errs).unwrap_or(1024),
        },
        // This binary IS the crate, so its own version is the right answer.
        client_version: None,
        events: None, // headless: the log IS the surface
        // Absorbs any field added later; see WorkerConfig's Default impl.
        ..Default::default()
    }
}

/// Everything `check` learned about this host, gathered once.
struct Preflight {
    docker: Option<String>,
    nvidia: Option<String>,
    rocm: bool,
    xpu: bool,
    gpus: Vec<gpu::Gpu>,
    installed: Backend,
    models: Vec<String>,
    engines: HashMap<String, String>,
    gateway: Result<u16, String>,
    /// Stored sharing choices that are overriding the environment.
    overrides: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

/// Can this host reach its gateway at all?
///
/// A GET at the registration endpoint: it answers (405, most likely) without
/// registering anything, so the preflight stays free of side effects while
/// still proving that DNS, routing, TLS and the far end all work. That is the
/// single most common reason a node "starts fine" and never appears.
async fn probe_gateway(client: &reqwest::Client, gateway: &str) -> Result<u16, String> {
    match client
        .get(format!("{gateway}/fabric/register"))
        .send()
        .await
    {
        Ok(r) => Ok(r.status().as_u16()),
        Err(e) => Err(e.to_string()),
    }
}

async fn gather(cfg: &WorkerConfig, timeout: Duration) -> Preflight {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default();
    // One round of probes, all at once. Sequentially this was the slowest
    // command in the binary on exactly the hosts where it matters: four
    // vendor tools, a docker socket and two HTTP round trips, one after the
    // other.
    let (docker, nvidia, rocm, xpu, gpus, (models, engines), gateway) = tokio::join!(
        probe(
            timeout,
            "docker",
            &["version", "--format", "{{.Server.Version}}"]
        ),
        probe(timeout, "nvidia-smi", &["-L"]),
        probe(timeout, "rocm-smi", &["--showid"]),
        probe(timeout, "xpu-smi", &["discovery", "-j"]),
        gpu::detect_all(),
        // The SAME question the worker asks, rather than a sign of life:
        // `/api/version` answering proved nothing (a vLLM endpoint 404s it,
        // and a 404 is still a response), while an Ollama with nothing pulled
        // also looked healthy and then refused every job. What decides
        // whether this node is useful is the MODEL LIST.
        fabric_worker::discover(&client, cfg),
        probe_gateway(&client, &cfg.gateway_url),
    );
    Preflight {
        docker,
        nvidia,
        rocm: rocm.is_some(),
        xpu: xpu.is_some(),
        gpus,
        installed: gpu::detect_installed(),
        models,
        engines,
        gateway,
        overrides: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
    }
}

/// Turn the gathered facts into the verdicts, so the text and JSON renderings
/// can never disagree about whether this host is ready.
fn judge(cfg: &WorkerConfig, pf: &mut Preflight) {
    let advertised = cfg.accel();

    // Sessions without docker cannot work; a provisioning script must fail
    // loudly here instead of deploying a node that refuses every session.
    if !cfg.workload_templates.is_empty() && pf.docker.is_none() {
        pf.errors
            .push("PROVIDER_WORKLOADS is set but docker is unreachable".into());
    }
    // Same reasoning one layer up: a node that shares inference but lists no
    // models joins the fabric, counts toward online_nodes, and has every job
    // refused — silently, because connecting SUCCEEDED.
    if cfg.share_inference && pf.models.is_empty() {
        pf.errors.push(format!(
            "no models at {} — nothing to serve. Check the endpoint lists them:\n\
             \x20   curl {}/v1/models      # vLLM, LiteLLM, TGI\n\
             \x20   curl {}/api/tags       # Ollama",
            cfg.ollama_base, cfg.ollama_base, cfg.ollama_base
        ));
    }
    // Every lane counts as sharing, the v3.0 ones included: a functions-only
    // node is a real provider, not a misconfiguration.
    if !cfg.share_inference
        && cfg.workload_templates.is_empty()
        && !cfg.share_cpu
        && !cfg.functions.enabled
        && !cfg.vectors.enabled
    {
        pf.errors.push(
            "nothing is shared: inference off, no session templates, no CPU sharing, \
             functions off, vectors off — this node would connect and offer the fabric \
             nothing"
                .into(),
        );
    }

    if let Err(e) = &pf.gateway {
        // A warning, not an error: the worker retries with backoff, and a
        // gateway having a bad minute must not fail an install.
        pf.warnings
            .push(format!("gateway {} unreachable: {e}", cfg.gateway_url));
    }
    if advertised != Backend::Cpu && !pf.gpus.iter().any(|g| g.backend == advertised) {
        pf.warnings.push(format!(
            "{} is forced but not detected here, so the hello frame will report cpu",
            advertised.as_str()
        ));
    }
    if cfg.country.is_empty() {
        pf.warnings.push(
            "PROVIDER_COUNTRY is undeclared, so the gateway records XX and consumers \
             filtering for EU residency will not see this node"
                .into(),
        );
    }
    if cfg.functions.enabled && cfg.functions.trusted_pubkey.is_empty() {
        // Naming the endpoint, because the key is now the ONLY thing between
        // an operator and a working functions lane: the runtime ships.
        pf.errors.push(format!(
            "PROVIDER_FUNCTIONS is on but PROVIDER_FUNCTIONS_PUBKEY is empty — \
             every function job would be refused. The key this fabric signs with:\n\
             \x20   curl {}/v1/functions      # the \"pubkey\" field",
            cfg.gateway_url
        ));
    }
    if cfg.functions.enabled && !kmplify_node::functions::runtime_available() {
        pf.errors.push(
            "PROVIDER_FUNCTIONS is on but this build has no Wasm runtime — it was \
             built with --no-default-features. The released binaries include it; \
             rebuild with --features wasm."
                .into(),
        );
    }

    // Template-by-template rather than by substring: the catalog knows what
    // each one needs, so an unknown id is a typo and a mismatched one is a
    // session the gateway will never schedule here.
    for t in &cfg.workload_templates {
        match fabric_worker::template_accelerator(t) {
            None => pf.warnings.push(format!(
                "template {t:?} is unknown to this build; it will be refused unless \
                 KMPLIFY_FABRIC_EXTRA_IMAGE_PINS names it"
            )),
            Some(need) if need != Backend::Cpu && need != advertised => pf.warnings.push(format!(
                "template {t:?} needs {}, but this node advertises {} — it will not be scheduled here",
                need.as_str(),
                advertised.as_str()
            )),
            Some(_) => {}
        }
    }
    if !cfg.workload_templates.is_empty()
        && !advertised.hosts_container_sessions()
        && advertised != Backend::Cpu
    {
        pf.warnings.push(format!(
            "{} serves inference but cannot pass a GPU into a container, \
             so this node cannot host sessions",
            advertised.as_str()
        ));
    }
}

fn render_check_text(cfg: &WorkerConfig, pf: &Preflight) {
    let advertised = cfg.accel();
    println!("kmplify-node {}", kmplify_node::version_string());
    println!();
    println!("configuration");
    println!("  gateway    : {}", cfg.gateway_url);
    println!("  creds      : {}", cfg.creds_path.display());
    println!("  ollama     : {}", cfg.ollama_base);
    if !cfg.colibri_base.is_empty() {
        println!("  colibri    : {}", cfg.colibri_base);
    }
    println!(
        "  country    : {}",
        if cfg.country.is_empty() {
            "(undeclared -> XX)"
        } else {
            &cfg.country
        }
    );
    println!(
        "  inference  : {}",
        if cfg.share_inference {
            "ON"
        } else {
            "off (PROVIDER_SHARE_INFERENCE=false)"
        }
    );
    println!(
        "  cpu share  : {}",
        if cfg.share_cpu { "ON" } else { "off" }
    );
    println!("  approval   : {}", cfg.approval_mode);
    println!(
        "  sessions   : {}",
        if cfg.workload_templates.is_empty() {
            "OFF (set PROVIDER_WORKLOADS to opt in)".to_string()
        } else {
            cfg.workload_templates.join(",")
        }
    );
    println!(
        "  ceilings   : cpus={:?} vram_mb={:?} ram_mb={:?} disk_gb={:?}",
        cfg.max_shared_cpus, cfg.max_shared_vram_mb, cfg.max_shared_ram_mb, cfg.max_shared_disk_gb
    );
    println!(
        "  functions  : {}",
        if !cfg.functions.enabled {
            "OFF (set PROVIDER_FUNCTIONS=true to opt in)".to_string()
        } else {
            format!(
                "key {}…, {} MB / {} ms per call",
                &cfg.functions.trusted_pubkey[..8.min(cfg.functions.trusted_pubkey.len())],
                cfg.functions.max_memory_mb,
                cfg.functions.max_ms
            )
        }
    );
    println!(
        "  vectors    : {}",
        if cfg.vectors.enabled {
            format!("ON, up to {} MB of collections", cfg.vectors.max_mb)
        } else {
            "OFF (set PROVIDER_SHARE_VECTORS=true to opt in)".to_string()
        }
    );

    println!();
    println!("accelerator");
    if pf.gpus.is_empty() {
        println!("  detected   : none (CPU-only node)");
    } else {
        for g in &pf.gpus {
            let primary = if g.backend == advertised {
                " <- advertised"
            } else {
                ""
            };
            println!(
                "  {:<10} : {} ({} MB){}",
                g.backend.as_str(),
                g.name,
                g.total_mb,
                primary
            );
        }
    }
    // What actually goes on the wire, said plainly. Forcing a backend the
    // host does not have is otherwise invisible: the card list looks healthy
    // while the node advertises CPU.
    println!("  advertised : {}", advertised.as_str());
    // The two detectors can legitimately disagree, and this is where an
    // operator meets it: "I installed ROCm, why does it say cpu?".
    if pf.installed != advertised {
        println!(
            "  installed  : {} driver tooling is present",
            pf.installed.label()
        );
        if pf.gpus.iter().all(|g| g.backend != pf.installed) {
            println!("               but its tool did not answer, so nothing is advertised");
            println!("               for it. Local inference may still work; the fabric");
            println!("               only advertises what it can size and serve.");
        }
    }

    println!();
    println!("probes");
    println!(
        "  gateway    : {}",
        match &pf.gateway {
            Ok(code) => format!("reachable (http {code})"),
            Err(e) => format!("UNREACHABLE — {e}"),
        }
    );
    println!(
        "  docker     : {}",
        pf.docker.clone().unwrap_or_else(|| "NOT REACHABLE".into())
    );
    println!(
        "  nvidia-smi : {}",
        pf.nvidia.clone().unwrap_or_else(|| "not found".into())
    );
    if pf.rocm {
        println!("  rocm-smi   : present");
    }
    if pf.xpu {
        println!("  xpu-smi    : present");
    }
    if pf.models.is_empty() {
        println!("  models     : NONE");
    } else {
        let colibri = pf.engines.values().filter(|e| *e == "colibri").count();
        let via = if colibri > 0 {
            format!(" ({colibri} via colibri)")
        } else {
            String::new()
        };
        println!(
            "  models     : {}{via} ({})",
            pf.models.len(),
            pf.models.join(", ")
        );
    }

    if !pf.overrides.is_empty() {
        println!();
        println!("stored sharing settings (kmplify-node set --list) override the environment:");
        for line in &pf.overrides {
            println!("  {line}");
        }
    }
    if !pf.warnings.is_empty() {
        println!();
        for w in &pf.warnings {
            println!("WARNING: {w}");
        }
    }
    if !pf.errors.is_empty() {
        eprintln!();
        for e in &pf.errors {
            eprintln!("ERROR: {e}");
        }
    } else {
        println!();
        println!("ready.");
    }
}

fn render_check_json(cfg: &WorkerConfig, pf: &Preflight) {
    let body = serde_json::json!({
        "version": kmplify_node::version_string(),
        "ok": pf.errors.is_empty(),
        "config": {
            "gateway": cfg.gateway_url,
            "creds": cfg.creds_path.display().to_string(),
            "ollama": cfg.ollama_base,
            "colibri": cfg.colibri_base,
            "country": cfg.country,
            "share_inference": cfg.share_inference,
            "share_cpu": cfg.share_cpu,
            "approval_mode": cfg.approval_mode,
            "workloads": cfg.workload_templates,
            "max_cpus": cfg.max_shared_cpus,
            "max_vram_mb": cfg.max_shared_vram_mb,
            "max_ram_mb": cfg.max_shared_ram_mb,
            "max_disk_gb": cfg.max_shared_disk_gb,
            "functions": cfg.functions.enabled,
            "vectors": cfg.vectors.enabled,
        },
        "accelerator": {
            "advertised": cfg.accel().as_str(),
            "installed": pf.installed.as_str(),
            "detected": pf.gpus.iter().map(|g| serde_json::json!({
                "backend": g.backend.as_str(), "name": g.name, "total_mb": g.total_mb,
            })).collect::<Vec<_>>(),
        },
        "probes": {
            "gateway_status": pf.gateway.as_ref().ok(),
            "gateway_error": pf.gateway.as_ref().err(),
            "docker": pf.docker,
            "nvidia_smi": pf.nvidia,
            "rocm_smi": pf.rocm,
            "xpu_smi": pf.xpu,
        },
        "models": pf.models,
        "engines": pf.engines,
        "overrides": pf.overrides,
        "warnings": pf.warnings,
        "errors": pf.errors,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
}

/// `check`: resolve, probe, judge, report. Never connects to the fabric.
///
/// `gpus` is passed in rather than probed here because the caller already
/// needed the accelerator to build the config, and asking the vendor tools
/// twice doubled the wall-clock of the one command an operator runs when
/// something is already wrong.
async fn run_check(
    cfg: &WorkerConfig,
    stored: &Settings,
    from_env: &WorkerConfig,
    gpus: Vec<gpu::Gpu>,
    json: bool,
    timeout: Duration,
) -> i32 {
    let mut pf = gather(cfg, timeout).await;
    pf.gpus = gpus;
    pf.overrides = stored.conflicts(from_env);
    judge(cfg, &mut pf);
    if json {
        render_check_json(cfg, &pf);
    } else {
        render_check_text(cfg, &pf);
    }
    if pf.errors.is_empty() {
        EXIT_OK
    } else {
        EXIT_UNUSABLE
    }
}

/// Compute time, in the unit it actually happened in.
///
/// Seconds hide the answer when the answer is milliseconds: a node that
/// served two functions in 40 ms should not read "0s of compute" and look
/// like it did nothing.
fn human_compute(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        human_duration(Duration::from_millis(ms))
    }
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `status`: what the node running here is doing, for scripts and `watch`.
fn run_status(dir: &std::path::Path, json: bool) -> i32 {
    let Some(snap) = status::read_published(dir) else {
        if json {
            println!("{}", serde_json::json!({"running": false}));
        } else {
            eprintln!(
                "no node has run in {} — start one with `kmplify-node` or `kmplify-node tui`",
                dir.display()
            );
        }
        return EXIT_UNUSABLE;
    };
    if json {
        let mut body = serde_json::to_value(&snap).unwrap_or_default();
        body["running"] = serde_json::json!(snap.is_fresh());
        body["age_ms"] = serde_json::json!(snap.age().as_millis() as u64);
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return if snap.is_fresh() {
            EXIT_OK
        } else {
            EXIT_UNUSABLE
        };
    }
    if !snap.is_fresh() {
        eprintln!(
            "node not running — last seen {} ago (pid {}, {})",
            human_duration(snap.age()),
            snap.pid,
            snap.link.label()
        );
        return EXIT_UNUSABLE;
    }
    println!(
        "{}{} pid {} up {}",
        snap.link.label(),
        if snap.paused { " (paused)" } else { "" },
        snap.pid,
        human_duration(snap.uptime())
    );
    println!("  node     : {}", snap.node_id);
    println!("  gateway  : {}", snap.gateway);
    println!(
        "  models   : {}",
        if snap.models.is_empty() {
            "none advertised".to_string()
        } else {
            snap.models.join(", ")
        }
    );
    println!(
        "  load     : cpu {:.0}%{}{}",
        snap.cpu_percent,
        match snap.gpu_percent {
            Some(p) => format!("  gpu {p}%"),
            None => String::new(),
        },
        if snap.ram_total_mb > 0 {
            format!(
                "  ram {}/{} GB",
                snap.ram_used_mb / 1024,
                snap.ram_total_mb / 1024
            )
        } else {
            String::new()
        }
    );
    println!(
        "  jobs     : {} active, {} finished, {} errors, avg {} ms",
        snap.jobs.active, snap.jobs.done, snap.jobs.failed, snap.jobs.avg_ms
    );
    println!("  sessions : {}", snap.sessions.len());
    let d = &snap.delivered;
    println!(
        "  delivered: {} calls ({} inference · {} functions · {} vector) in {} of compute",
        d.calls(),
        d.jobs,
        d.functions,
        d.vector_ops,
        human_compute(d.compute_ms())
    );
    if d.sessions > 0 || d.session_seconds > 0 {
        println!(
            "  hosted   : {} sessions, {} of machine time",
            d.sessions,
            human_duration(Duration::from_secs(d.session_seconds))
        );
    }
    EXIT_OK
}

/// `set`: change what this machine lends, durably.
///
/// The dashboard's sharing screen for a shell — same file, same effect, and
/// the same nudge to the running node, so `kmplify-node set max-cpus=6` from
/// a provisioning script lands exactly like dragging the slider.
fn run_set(dir: &std::path::Path, cli: &cli::Cli, from_env: &WorkerConfig) -> i32 {
    let mut stored = Settings::load(dir);
    if cli.list {
        let lines = stored.lines();
        if lines.is_empty() {
            println!("no settings stored here — this node runs on its environment alone");
        } else {
            println!("stored in {}", kmplify_node::settings::path(dir).display());
            for line in lines {
                println!("  {line}");
            }
            for line in stored.conflicts(from_env) {
                println!("  overriding {line}");
            }
        }
        return EXIT_OK;
    }

    if cli.clear_all {
        stored = Settings::default();
    }
    for key in &cli.clear {
        if let Err(e) = stored.clear(key) {
            eprintln!("kmplify-node: {e}");
            return EXIT_USAGE;
        }
    }
    for (key, value) in &cli.assignments {
        if let Err(e) = stored.set(key, value) {
            eprintln!("kmplify-node: {e}");
            return EXIT_USAGE;
        }
    }
    if let Err(e) = stored.save(dir) {
        eprintln!(
            "kmplify-node: cannot write {}: {e}",
            kmplify_node::settings::path(dir).display()
        );
        return EXIT_UNUSABLE;
    }

    // Show the outcome rather than echoing the input: what matters is what
    // this machine now lends, including the fields the change did not touch.
    let mut effective = from_env.clone();
    stored.apply(&mut effective);
    println!(
        "sharing: inference {} · cpu/ram {} · sessions {} · admission {}",
        onoff(effective.share_inference),
        onoff(effective.share_cpu),
        if effective.workload_templates.is_empty() {
            "off".to_string()
        } else {
            effective.workload_templates.join(",")
        },
        effective.approval_mode
    );
    println!(
        "ceilings: cpus {} · vram {} · ram {} · disk {}",
        ceiling(effective.max_shared_cpus, ""),
        ceiling(effective.max_shared_vram_mb, " MB"),
        ceiling(effective.max_shared_ram_mb, " MB"),
        ceiling(effective.max_shared_disk_gb, " GB"),
    );

    // Nudge the running node, if there is one. Without this the change would
    // be correct and invisible until the next restart.
    match status::read_published(dir).filter(kmplify_node::status::Snapshot::is_fresh) {
        Some(_) => match control::request(dir, &control::Command::Reload) {
            Ok(()) => println!("the node running here will re-advertise within a second"),
            Err(e) => eprintln!("saved, but the running node could not be told: {e}"),
        },
        None => println!("no node is running here; this takes effect when one starts"),
    }
    EXIT_OK
}

fn onoff(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

fn ceiling<T: std::fmt::Display>(v: Option<T>, unit: &str) -> String {
    v.map(|v| format!("{v}{unit}"))
        .unwrap_or_else(|| "unset".into())
}

/// `rewards`: what this node has delivered, and what a companion makes of it.
///
/// The node's own half is always here: its public identity, and what it has
/// actually served. The other half — accounts, wallets, tokens, payouts —
/// belongs to a separate program that an operator installs on purpose. If
/// none is installed, this command says so and the node carries on exactly
/// as before; nothing about serving depends on it.
async fn run_rewards(dir: &std::path::Path, cfg: &WorkerConfig, stored: &Settings) -> i32 {
    use kmplify_node::rewards::{self, Companion};

    let identity = kmplify_node::identity::Identity::read(dir);
    println!("this node");
    match &identity {
        Some(id) => {
            println!("  node id  : {}", id.node_id);
            println!("  gateway  : {}", id.gateway);
            println!(
                "  published: {}",
                kmplify_node::identity::path(dir).display()
            );
        }
        None => {
            println!("  node id  : none yet — start the node once so it registers");
            println!(
                "  gateway  : {}",
                match status::read_published(dir) {
                    Some(s) if !s.gateway.is_empty() => s.gateway,
                    _ => cfg.gateway_url.clone(),
                }
            );
        }
    }

    // What this machine has actually served. Not an accounting record, and
    // said so out loud: only the fabric's signed receipts settle anything.
    if let Some(snap) = status::read_published(dir) {
        let d = &snap.delivered;
        println!(
            "\ndelivered since this node started ({})",
            human_duration(snap.uptime())
        );
        println!(
            "  calls    : {} in {:.1} s of compute",
            d.calls(),
            d.compute_ms() as f64 / 1000.0
        );
        println!(
            "    inference {} ({:.1} s) · functions {} ({:.1} s) · vector {} ({:.1} s)",
            d.jobs,
            d.job_ms as f64 / 1000.0,
            d.functions,
            d.function_ms as f64 / 1000.0,
            d.vector_ops,
            d.vector_ms as f64 / 1000.0
        );
        println!(
            "  sessions : {} hosted, {} of machine time",
            d.sessions,
            human_duration(Duration::from_secs(d.session_seconds))
        );
        println!("  (the node's own count — the fabric's signed receipts are what settle)");
    }

    let enabled = stored.rewards_enabled();
    let companion = Companion::resolve(enabled);
    println!("\nrewards companion");
    match &companion {
        Companion::Off => {
            println!("  off. Rewards are optional and this node needs nothing to serve.");
            println!("  To use one: install a companion, then `kmplify-node set rewards=on`.");
            return EXIT_OK;
        }
        Companion::Missing(why) => {
            println!("  {why}");
            return EXIT_UNUSABLE;
        }
        Companion::Found(p) => println!("  {}", p.display()),
    }

    match rewards::ask(&companion, dir).await {
        Ok(report) => {
            println!("  {}", rewards::summary(&report));
            if !report.account.is_empty() {
                println!("  account  : {}", report.account);
            }
            if !report.destination.is_empty() {
                println!("  paid to  : {}", report.destination);
            }
            // Said once. The companion usually says it too, and two
            // sentences about the same thing read as boilerplate rather than
            // as a warning.
            if report.testnet && !report.note.to_ascii_lowercase().contains("test") {
                println!("  note     : this rail is a TEST network — the balance is not money");
            }
            if !report.note.is_empty() {
                println!("  note     : {}", report.note);
            }
            EXIT_OK
        }
        Err(e) => {
            println!("  {e}");
            EXIT_UNUSABLE
        }
    }
}

/// `engines`: what is serving on this machine, and which one the node uses.
///
/// The read half of engine selection. The write half is one of:
/// `kmplify-node set engine=<name|url>` or the init wizard.
async fn run_engines(cfg: &WorkerConfig, json: bool) -> i32 {
    let found = kmplify_node::engines::scan().await;
    let active = cfg.ollama_base.trim_end_matches('/');
    if json {
        let body = serde_json::json!({
            "active": active,
            "found": found,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return EXIT_OK;
    }
    if found.is_empty() {
        println!("no inference engine is answering on the usual localhost ports.");
        println!("the node can lend any of these once one runs:");
        for k in kmplify_node::engines::KNOWN {
            println!("  {:<10} {}", k.id, k.hint);
        }
        println!("\nactive setting: {active} (nothing answered there)");
        return EXIT_UNUSABLE;
    }
    let mut active_seen = false;
    for f in &found {
        let mark = if f.base == active {
            active_seen = true;
            " <- active"
        } else {
            ""
        };
        let models = if f.models.is_empty() {
            "0 models (online, but would refuse every job)".to_string()
        } else if f.models.len() <= 4 {
            format!("{} ({})", f.models.len(), f.models.join(", "))
        } else {
            format!("{} ({}, …)", f.models.len(), f.models[..3].join(", "))
        };
        println!("  {:<12} {:<28} {models}{mark}", f.name, f.base);
    }
    if !active_seen {
        println!(
            "\nactive setting: {active} — which is NOT one of the engines that answered.\n\
             pick one:  kmplify-node set engine=<name or URL from the list above>"
        );
    }
    EXIT_OK
}

/// `peers`: who may use this machine, and the verbs that decide.
///
/// The dashboard's peers screen for a shell — same gateway calls, same
/// wording — because the machines that most need manual admission are the
/// ones nobody is sitting in front of. Works whether or not a node is
/// running here: the decision lives on the gateway, and this speaks to it
/// with the node's stored credential.
async fn run_peers(dir: &std::path::Path, cfg: &WorkerConfig, cli: &cli::Cli) -> i32 {
    // The gateway the RUNNING node uses wins over this shell's environment:
    // asking the wrong gateway about "my consumers" answers about a node
    // that is not this one.
    let gateway = match status::read_published(dir) {
        Some(s) if !s.gateway.is_empty() => s.gateway,
        _ => cfg.gateway_url.clone(),
    };
    let Some(creds) = kmplify_node::peers::credential(&cfg.creds_path) else {
        eprintln!(
            "no node identity at {} — nothing has run here yet, so there is nobody to admit",
            cfg.creds_path.display()
        );
        return EXIT_UNUSABLE;
    };
    let timeout = Duration::from_secs(10);

    match cli.args.first().map(String::as_str) {
        Some(verb @ ("approve" | "deny" | "block" | "clear")) => {
            let consumer = &cli.args[1];
            let decision = match verb {
                "approve" => Some("approved"),
                "deny" => Some("denied"),
                "block" => Some("blocked"),
                _ => None,
            };
            match kmplify_node::peers::decide(&gateway, &creds.token, consumer, decision, timeout)
                .await
            {
                Ok(()) => {
                    println!(
                        "{consumer}: {}",
                        match decision {
                            Some("approved") => "approved — admitted from now on",
                            Some("denied") =>
                                "denied — refused quietly while manual admission is on",
                            Some("blocked") => "blocked — refused in every mode",
                            _ => "standing rule cleared — the admission mode decides again",
                        }
                    );
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("the gateway refused: {e}");
                    EXIT_UNUSABLE
                }
            }
        }
        Some("invite") => {
            let label = cli.args.get(1).cloned().unwrap_or_default();
            match kmplify_node::peers::invite(&gateway, &creds.token, &label, timeout).await {
                Ok(inv) => {
                    // The id is the whole point of the command, so it goes on
                    // stdout alone and unadorned: `INV=$(kmplify-node peers
                    // invite laptop)` has to work.
                    println!("{}", inv.invitation_id);
                    if !inv.invite_url.is_empty() {
                        eprintln!("share: {}", inv.invite_url);
                    }
                    eprintln!("an invitation always connects, manual admission or not");
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("could not mint an invitation: {e}");
                    EXIT_UNUSABLE
                }
            }
        }
        Some("revoke") => {
            match kmplify_node::peers::revoke(&gateway, &creds.token, &cli.args[1], timeout).await {
                Ok(()) => {
                    println!("{}: revoked", cli.args[1]);
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("could not revoke: {e}");
                    EXIT_UNUSABLE
                }
            }
        }
        // Bare `peers` lists.
        _ => match kmplify_node::peers::fetch(&gateway, &creds.token, timeout).await {
            Ok(p) => {
                if cli.json {
                    print_peers_json(&p, cfg);
                } else {
                    print_peers_text(&p, cfg);
                }
                EXIT_OK
            }
            Err(e) => {
                eprintln!("cannot ask {gateway}: {e}");
                EXIT_UNUSABLE
            }
        },
    }
}

fn print_peers_text(p: &kmplify_node::peers::Peers, cfg: &WorkerConfig) {
    let mode = p.approval_mode.clone().unwrap_or_else(|| {
        // Offline: the gateway has no live hello to report, so say what this
        // node WOULD advertise rather than nothing.
        format!(
            "{} (node offline; this is the configured mode)",
            cfg.approval_mode
        )
    });
    println!("admission : {mode}");

    // What the GATEWAY believes, falling back to the configured value only
    // while the node is offline: the two differ exactly when a change has
    // not been re-advertised yet, and the gateway's answer is the one that
    // decides who gets in.
    let manual = p.approval_mode.as_deref().unwrap_or(&cfg.approval_mode) == "manual";
    println!("\nwaiting for a decision ({})", p.pending.len());
    if p.pending.is_empty() && !manual {
        println!("  none — admission is automatic, so nobody has to wait");
    } else if p.pending.is_empty() {
        println!("  none right now; unknown consumers appear here until you decide");
    }
    for x in &p.pending {
        println!(
            "  {:<24} waiting {:<8} asked for {}",
            x.consumer,
            human_duration(Duration::from_secs(x.first_seen_seconds.max(0) as u64)),
            if x.model.is_empty() { "—" } else { &x.model }
        );
        println!("      kmplify-node peers approve {}", x.consumer);
    }

    println!("\nconsumers seen recently ({})", p.consumers.len());
    for c in &p.consumers {
        println!(
            "  {:<24} {:<7} via {:<18} last seen {:<8} {}",
            c.consumer,
            if c.active { "active" } else { "idle" },
            if c.via.is_empty() { "—" } else { &c.via },
            human_duration(Duration::from_secs(c.last_seen_seconds.max(0) as u64)),
            c.rule.clone().unwrap_or_default()
        );
    }

    let live: Vec<_> = p.invitations.iter().filter(|i| !i.revoked).collect();
    println!("\ninvitations ({})", live.len());
    for i in live {
        println!(
            "  {}  {:<20} {}",
            i.invitation_id,
            if i.label.is_empty() {
                "(no label)"
            } else {
                &i.label
            },
            if i.paused {
                "held".to_string()
            } else if i.consumer_active {
                "in use".to_string()
            } else {
                "idle".to_string()
            }
        );
    }
}

fn print_peers_json(p: &kmplify_node::peers::Peers, cfg: &WorkerConfig) {
    let body = serde_json::json!({
        "approval_mode": p.approval_mode.clone().unwrap_or_else(|| cfg.approval_mode.clone()),
        "online": p.approval_mode.is_some(),
        "pending": p.pending.iter().map(|x| serde_json::json!({
            "consumer": x.consumer,
            "waiting_seconds": x.first_seen_seconds,
            "last_seen_seconds": x.last_seen_seconds,
            "model": x.model,
        })).collect::<Vec<_>>(),
        "consumers": p.consumers.iter().map(|c| serde_json::json!({
            "consumer": c.consumer,
            "active": c.active,
            "via": c.via,
            "connected_for_seconds": c.connected_for_seconds,
            "last_seen_seconds": c.last_seen_seconds,
            "rule": c.rule,
        })).collect::<Vec<_>>(),
        "invitations": p.invitations.iter().filter(|i| !i.revoked).map(|i| serde_json::json!({
            "invitation_id": i.invitation_id,
            "label": i.label,
            "paused": i.paused,
            "consumer_active": i.consumer_active,
        })).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
}

/// `id`: the handle consumers pin and invite. Registers if this install has
/// never had one, because a node id that does not exist yet cannot be pinned.
async fn run_id(cfg: &WorkerConfig) -> i32 {
    match fabric_worker::ensure_identity(&cfg.gateway_url, &cfg.creds_path).await {
        Ok(c) => {
            println!("{}", c.node_id);
            EXIT_OK
        }
        Err(e) => {
            eprintln!("no identity yet: {e}");
            EXIT_UNUSABLE
        }
    }
}

/// A running node: the worker plus the two side tasks that make it
/// observable and controllable from outside this process.
///
/// Shared by `run` and by the dashboard's standalone mode, so a node started
/// from a terminal is the same node in every respect as one started by
/// systemd — same publishing, same control path, same teardown.
pub(crate) struct Node {
    stop: tokio::sync::watch::Sender<bool>,
    worker: tokio::task::JoinHandle<()>,
    publisher: tokio::task::JoinHandle<()>,
    commands: tokio::task::JoinHandle<()>,
    dir: PathBuf,
}

pub(crate) async fn start_node(cfg: WorkerConfig, dir: PathBuf) -> Node {
    match fabric_worker::ensure_identity(&cfg.gateway_url, &cfg.creds_path).await {
        Ok(c) => {
            let id = c.node_id.clone();
            status::update(move |s| s.node_id = id);
            // The PUBLIC half, in its own file, so a companion never has to
            // open the credential to learn the node id — see identity.rs.
            kmplify_node::identity::publish_for(&dir, &c.node_id, &cfg.gateway_url);
            status::push_log(format!(
                "node identity {}…",
                &c.node_id[..8.min(c.node_id.len())]
            ));
        }
        Err(e) => {
            // Not fatal: run() retries with backoff and self-heals a rejected
            // identity; a gateway that is briefly down should not kill a
            // service-managed node at boot.
            status::push_log(format!(
                "identity not established yet ({e}) — the worker will keep trying"
            ));
        }
    }
    let accel = cfg.accel();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(fabric_worker::run(cfg, stop_rx.clone()));
    // Publish for `status` and for a dashboard attaching from another
    // process, and accept the commands such a dashboard sends back.
    let publisher = tokio::spawn(status::publish_loop(dir.clone(), accel, stop_rx.clone()));
    let commands = tokio::spawn(control::watch(dir.clone(), stop_rx));
    Node {
        stop,
        worker,
        publisher,
        commands,
        dir,
    }
}

impl Node {
    /// Stop serving and leave nothing of other people's running on this
    /// machine.
    async fn shutdown(self) {
        status::set_link(status::Link::Stopping, "stopping");
        let _ = self.stop.send(true);
        // run() tears down every hosted session container before it returns.
        let _ = self.worker.await;
        let _ = self.publisher.await;
        self.commands.abort();
        status::clear_published(&self.dir);
    }
}

/// Join the fabric and serve until told to stop. Returns the exit code.
async fn serve(cfg: WorkerConfig, dir: PathBuf) -> i32 {
    println!("[kmplify-node] joining {}", cfg.gateway_url);
    let node = start_node(cfg, dir).await;
    let snap = status::snapshot();
    if !snap.node_id.is_empty() {
        println!(
            "[kmplify-node] node identity {}…",
            &snap.node_id[..8.min(snap.node_id.len())]
        );
    }
    let mut control_rx = fabric_worker::subscribe_control();

    // SIGTERM is what systemd sends on stop; Ctrl-C covers interactive use.
    // Either one flips the stop signal, and run() tears down every hosted
    // session container before returning — a stopped node must leave nothing
    // running on the owner's GPU.
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    #[cfg(unix)]
    let terminate = async move {
        sigterm.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let Node {
        stop,
        mut worker,
        publisher,
        commands,
        dir,
    } = node;
    let mut code = EXIT_OK;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
        // The worker returns only when asked to stop, so reaching this arm
        // means it panicked. Exiting non-zero is what gets a service-managed
        // node restarted; sitting here would leave a healthy-looking process
        // that lends nothing.
        joined = &mut worker => {
            if let Err(e) = joined {
                eprintln!("[kmplify-node] worker stopped unexpectedly: {e}");
                code = EXIT_UNUSABLE;
            }
            let _ = stop.send(true);
            let _ = publisher.await;
            commands.abort();
            status::clear_published(&dir);
            return code;
        }
        // A dashboard, here or attached, asking this node to stop.
        _ = wait_for_shutdown_command(&mut control_rx) => {}
    }

    println!("[kmplify-node] stopping — tearing down hosted sessions…");
    Node {
        stop,
        worker,
        publisher,
        commands,
        dir,
    }
    .shutdown()
    .await;
    println!("[kmplify-node] stopped cleanly");
    code
}

/// Resolves when someone asks this node to stop; never otherwise.
async fn wait_for_shutdown_command(rx: &mut tokio::sync::broadcast::Receiver<serde_json::Value>) {
    loop {
        match rx.recv().await {
            Ok(f) if f["type"] == "node_shutdown" => return,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            // The channel is process-global and never closed, but a future
            // refactor must not turn this into a busy loop.
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = match cli::parse(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kmplify-node: {e}\n");
            eprint!("{}", cli::usage());
            std::process::exit(EXIT_USAGE);
        }
    };
    // Before anything reads the environment, so `check` reports what `run`
    // would use.
    cli.apply_env();

    match cli.cmd {
        cli::Cmd::Help => {
            print!("{}", cli::usage());
            return;
        }
        cli::Cmd::Version => {
            println!("kmplify-node {}", kmplify_node::version_string());
            return;
        }
        _ => {}
    }

    let dir = node_dir();
    // `status` only reads; creating the directory for it would invent a node
    // that never ran.
    if cli.cmd != cli::Cmd::Status {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            eprintln!("cannot create node dir {}: {e}", dir.display());
            std::process::exit(EXIT_UNUSABLE);
        }
    }

    if cli.cmd == cli::Cmd::Status {
        std::process::exit(run_status(&dir, cli.json));
    }

    let mut errs = Vec::new();
    let mut cfg = resolve_config(&mut errs);
    // The environment alone, kept for two jobs: reporting which stored
    // choices are overriding it, and giving `set --clear` something to fall
    // back to.
    let from_env = cfg.clone();
    let stored = Settings::load(&dir);

    // `set` runs BEFORE the configuration gate on purpose: a unit file with a
    // bad value is exactly when an operator needs to change a setting, and
    // refusing to let them would be a trap.
    if cli.cmd == cli::Cmd::Set {
        if !errs.is_empty() {
            eprintln!("note: the environment has problems the node would refuse to start on:");
            for e in &errs {
                eprintln!("  {e}");
            }
        }
        std::process::exit(run_set(&dir, &cli, &from_env));
    }

    if !errs.is_empty() {
        for e in &errs {
            eprintln!("kmplify-node: {e}");
        }
        eprintln!("\nnothing was started. Fix the configuration and try again.");
        std::process::exit(EXIT_USAGE);
    }
    // What the operator chose in the dashboard or with `set` wins over the
    // environment; see the settings module for why round that way.
    stored.apply(&mut cfg);

    // One detection round, shared by the config and by `check`'s card list —
    // and skipped entirely for the commands that never look at the hardware,
    // because probing four vendor tools to print a node id is pure latency in
    // whatever script called it.
    let gpus = if matches!(cli.cmd, cli::Cmd::Check | cli::Cmd::Run | cli::Cmd::Tui) {
        gpu::detect_all().await
    } else {
        Vec::new()
    };
    let (accel, primary) = gpu::resolve_backend(&gpus);
    cfg.accelerator = accel;
    cfg.cuda = accel == Backend::Cuda;
    {
        let (name, total) = primary
            .map(|g| (g.name, g.total_mb))
            .unwrap_or_else(|| (String::new(), 0));
        status::update(move |s| {
            s.accelerator = accel.as_str().to_string();
            s.gpu_name = name;
            s.vram_total_mb = total;
        });
    }

    let code = match cli.cmd {
        cli::Cmd::Check => {
            run_check(&cfg, &stored, &from_env, gpus, cli.json, cli.probe_timeout).await
        }
        cli::Cmd::Id => run_id(&cfg).await,
        cli::Cmd::Peers => run_peers(&dir, &cfg, &cli).await,
        cli::Cmd::Init => onboard::run(&cfg, &dir).await,
        cli::Cmd::Engines => run_engines(&cfg, cli.json).await,
        cli::Cmd::Rewards => run_rewards(&dir, &cfg, &stored).await,
        cli::Cmd::Set => unreachable!("handled above"),
        cli::Cmd::Run => serve(cfg, dir).await,
        cli::Cmd::Tui => run_tui(cfg, dir, &cli).await,
        cli::Cmd::Status | cli::Cmd::Help | cli::Cmd::Version => unreachable!("handled above"),
    };
    std::process::exit(code);
}

#[cfg(feature = "tui")]
async fn run_tui(cfg: WorkerConfig, dir: PathBuf, cli: &cli::Cli) -> i32 {
    tui::main(cfg, dir, cli.attach, cli.standalone).await
}

#[cfg(not(feature = "tui"))]
async fn run_tui(_cfg: WorkerConfig, _dir: PathBuf, _cli: &cli::Cli) -> i32 {
    eprintln!(
        "this build has no dashboard (built without the `tui` feature).\n\
         Rebuild with `cargo build --release --features tui`, or use \
         `kmplify-node status` for a one-shot report."
    );
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment is process-global, so these run under one lock rather
    /// than racing each other through `cargo test`'s thread pool.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        match LOCK.get_or_init(|| std::sync::Mutex::new(())).lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn clear() {
        for k in [
            "PROVIDER_GATEWAY_URL",
            "PROVIDER_COUNTRY",
            "PROVIDER_MAX_CPUS",
            "PROVIDER_SHARE_INFERENCE",
            "PROVIDER_SHARE_CPU",
            "KMPLIFY_SHARE_CPU",
            "PROVIDER_APPROVAL_MODE",
            "PROVIDER_WORKLOADS",
            "OLLAMA_BASE",
            "KMPLIFY_GPU_BACKEND",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn defaults_join_the_public_fabric_and_share_inference() {
        let _g = env_lock();
        clear();
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.gateway_url, PUBLIC_FABRIC_URL);
        assert!(cfg.share_inference);
        assert!(!cfg.share_cpu);
        assert_eq!(cfg.approval_mode, "auto");
        assert!(cfg.workload_templates.is_empty());
    }

    #[test]
    fn a_mistyped_ceiling_is_an_error_not_an_unlimited_node() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_MAX_CPUS", "eigth");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        assert_eq!(cfg.max_shared_cpus, None);
        assert!(errs.iter().any(|e| e.contains("PROVIDER_MAX_CPUS")));
        clear();
    }

    #[test]
    fn booleans_mean_what_they_look_like() {
        let _g = env_lock();
        clear();
        // "0" used to mean ON here, because the rule was `!= "false"`.
        std::env::set_var("PROVIDER_SHARE_INFERENCE", "0");
        std::env::set_var("PROVIDER_SHARE_CPU", "yes");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.share_inference);
        assert!(cfg.share_cpu);

        std::env::set_var("PROVIDER_SHARE_CPU", "maybe");
        let mut errs = Vec::new();
        resolve_config(&mut errs);
        assert!(errs.iter().any(|e| e.contains("PROVIDER_SHARE_CPU")));
        clear();
    }

    #[test]
    fn the_older_cpu_switch_still_works_but_the_new_one_wins() {
        let _g = env_lock();
        clear();
        std::env::set_var("KMPLIFY_SHARE_CPU", "true");
        let mut errs = Vec::new();
        assert!(resolve_config(&mut errs).share_cpu);
        std::env::set_var("PROVIDER_SHARE_CPU", "false");
        assert!(!resolve_config(&mut errs).share_cpu);
        assert!(errs.is_empty(), "{errs:?}");
        clear();
    }

    #[test]
    fn a_gateway_url_without_a_scheme_is_refused() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_GATEWAY_URL", "fabric.kmplify.io");
        let mut errs = Vec::new();
        resolve_config(&mut errs);
        assert!(errs.iter().any(|e| e.contains("http")));
        clear();
    }

    #[test]
    fn trailing_slashes_are_trimmed_from_every_base_url() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_GATEWAY_URL", "https://gw.example/");
        std::env::set_var("OLLAMA_BASE", "http://127.0.0.1:11434/");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        assert_eq!(cfg.gateway_url, "https://gw.example");
        assert_eq!(cfg.ollama_base, "http://127.0.0.1:11434");
        clear();
    }

    #[test]
    fn a_country_is_alpha_2_or_nothing() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_COUNTRY", "de");
        let mut errs = Vec::new();
        assert_eq!(resolve_config(&mut errs).country, "DE");
        assert!(errs.is_empty());

        std::env::set_var("PROVIDER_COUNTRY", "DEU");
        let mut errs = Vec::new();
        // Left to the gateway this silently became "XX" and the node vanished
        // from every EU-only search.
        assert_eq!(resolve_config(&mut errs).country, "");
        assert!(errs.iter().any(|e| e.contains("alpha-2")));
        clear();
    }

    #[test]
    fn a_mistyped_approval_mode_fails_closed() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_APPROVAL_MODE", "Manual");
        let mut errs = Vec::new();
        assert_eq!(resolve_config(&mut errs).approval_mode, "manual");
        assert!(errs.is_empty());

        std::env::set_var("PROVIDER_APPROVAL_MODE", "vet-everyone");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        assert!(!errs.is_empty());
        // Whatever else happens, an unreadable admission policy must not
        // resolve to "admit everyone".
        assert_eq!(cfg.approval_mode, "manual");
        clear();
    }

    #[test]
    fn a_node_that_shares_nothing_is_a_configuration_error() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_SHARE_INFERENCE", "false");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        let mut pf = empty_preflight();
        judge(&cfg, &mut pf);
        assert!(pf.errors.iter().any(|e| e.contains("nothing is shared")));
        // …but any single lane, the v3.0 ones included, is a real provider.
        let mut cfg2 = cfg.clone();
        cfg2.functions.enabled = true;
        cfg2.functions.trusted_pubkey = "ab".repeat(32);
        let mut pf2 = empty_preflight();
        judge(&cfg2, &mut pf2);
        assert!(
            !pf2.errors.iter().any(|e| e.contains("nothing is shared")),
            "a functions-only node shares plenty"
        );
        clear();
    }

    #[test]
    fn sessions_without_docker_stop_a_provisioning_run() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_WORKLOADS", "vllm-openai");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        let mut pf = empty_preflight();
        pf.models.push("llama3".into());
        judge(&cfg, &mut pf);
        assert!(pf.errors.iter().any(|e| e.contains("docker")));
        clear();
    }

    #[test]
    fn a_template_this_node_cannot_run_is_a_warning() {
        let _g = env_lock();
        clear();
        std::env::set_var("PROVIDER_WORKLOADS", "vllm-openai,not-a-template");
        let mut errs = Vec::new();
        let cfg = resolve_config(&mut errs);
        let mut pf = empty_preflight();
        pf.docker = Some("27.0".into());
        pf.models.push("llama3".into());
        judge(&cfg, &mut pf);
        // CPU node: the CUDA template cannot be scheduled here, and the typo
        // is unknown to the catalog.
        assert!(pf.warnings.iter().any(|w| w.contains("vllm-openai")));
        assert!(pf.warnings.iter().any(|w| w.contains("not-a-template")));
        clear();
    }

    fn empty_preflight() -> Preflight {
        Preflight {
            docker: None,
            nvidia: None,
            rocm: false,
            xpu: false,
            gpus: Vec::new(),
            installed: Backend::Cpu,
            models: Vec::new(),
            engines: HashMap::new(),
            gateway: Ok(405),
            overrides: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(human_duration(Duration::from_secs(9)), "9s");
        assert_eq!(human_duration(Duration::from_secs(605)), "10m 05s");
        assert_eq!(human_duration(Duration::from_secs(7_800)), "2h 10m");
    }
}
