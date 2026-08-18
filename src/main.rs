//! kmplify-node — the headless KMPLIFY provider.
//!
//! The same fabric worker the desktop app runs (fabric_worker.rs, with all
//! its hardening: container-death reporting, reconnect-safe state, live
//! inventory, operator ceilings), with everything else stripped: no webview,
//! no chat, no RAG stack. For GUI-less Linux hosts, and anywhere else a
//! machine should lend GPU time without being someone's desktop.
//!
//! Configuration is environment variables, deliberately the SAME names the
//! desktop writes into its stack .env, so knowledge transfers 1:1:
//!
//!   PROVIDER_GATEWAY_URL   gateway to join      (default: the public fabric)
//!   PROVIDER_WORKLOADS     template ids to host (default: EMPTY = sessions
//!                          off; running other people's containers is opt-in
//!                          here exactly as it is in the app)
//!   PROVIDER_MAX_CPUS      ceiling on CPUs lent to sessions
//!   PROVIDER_MAX_VRAM_MB   ceiling on advertised VRAM
//!   PROVIDER_MAX_DISK_GB   ceiling on disk sessions may fill
//!   PROVIDER_COUNTRY       ISO alpha-2 for EU-only consumers, "" = undeclared
//!   OLLAMA_BASE            host Ollama for model serving (default localhost)
//!   KMPLIFY_NODE_DIR       identity/credentials dir
//!                          (default: $XDG_CONFIG_HOME|~/.config /kmplify-node)
//!   KMPLIFY_GPU_BACKEND    force the accelerator: cuda|rocm|oneapi|metal|cpu
//!   KMPLIFY_CUDA           older CUDA-only override: 1/0
//!
//! `kmplify-node check` prints the resolved configuration, the detected
//! accelerators and the host probes (docker, vendor SMI tools, the model
//! server) without connecting — run it first.

use std::path::PathBuf;

use kmplify_node::fabric_worker::{self, WorkerConfig};
use kmplify_node::PUBLIC_FABRIC_URL;

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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

async fn probe(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
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

/// Which accelerator this host offers. Honours KMPLIFY_GPU_BACKEND and the
/// older KMPLIFY_CUDA; see kmplify_node::gpu.
async fn detect_accelerator() -> (kmplify_node::gpu::Backend, Option<kmplify_node::gpu::Gpu>) {
    kmplify_node::gpu::detect().await
}

// The trailing `..Default::default()` below is redundant TODAY, which is
// precisely why clippy objects and precisely why it stays: it is what makes
// the next field added to WorkerConfig a non-event here instead of a build
// break, and this binary is the example embedders copy.
#[allow(clippy::needless_update)]
async fn resolve_config() -> WorkerConfig {
    let dir = node_dir();
    let (accel, _) = detect_accelerator().await;
    WorkerConfig {
        gateway_url: env_opt("PROVIDER_GATEWAY_URL")
            .unwrap_or_else(|| PUBLIC_FABRIC_URL.to_string()),
        ollama_base: env_opt("OLLAMA_BASE").unwrap_or_else(|| "http://127.0.0.1:11434".to_string()),
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
        country: env_opt("PROVIDER_COUNTRY").unwrap_or_default(),
        workload_templates: env_opt("PROVIDER_WORKLOADS")
            .map(|w| {
                w.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        max_shared_cpus: env_opt("PROVIDER_MAX_CPUS").and_then(|v| v.parse().ok()),
        max_shared_vram_mb: env_opt("PROVIDER_MAX_VRAM_MB").and_then(|v| v.parse().ok()),
        max_shared_ram_mb: env_opt("PROVIDER_MAX_RAM_MB").and_then(|v| v.parse().ok()),
        max_shared_disk_gb: env_opt("PROVIDER_MAX_DISK_GB").and_then(|v| v.parse().ok()),
        // Headless default: a node someone runs on purpose shares inference
        // unless it says otherwise — matching pre-switch behaviour.
        share_inference: env_opt("PROVIDER_SHARE_INFERENCE")
            .map(|v| v != "false")
            .unwrap_or(true),
        share_cpu: env_opt("PROVIDER_SHARE_CPU")
            .or_else(|| env_opt("KMPLIFY_SHARE_CPU"))
            .map(|v| v == "true")
            .unwrap_or(false),
        // Anything but an explicit "manual" is auto — matching the
        // gateway's own fail-open parsing of the hello field.
        approval_mode: match env_opt("PROVIDER_APPROVAL_MODE").as_deref() {
            Some("manual") => "manual".to_string(),
            _ => "auto".to_string(),
        },
        cuda: accel == kmplify_node::gpu::Backend::Cuda,
        accelerator: accel,
        // This binary IS the crate, so its own version is the right answer.
        client_version: None,
        events: None, // headless: the log IS the surface
        // Absorbs any field added later; see WorkerConfig's Default impl.
        ..Default::default()
    }
}

async fn run_check(cfg: &WorkerConfig) -> i32 {
    println!("kmplify-node configuration");
    println!("  gateway    : {}", cfg.gateway_url);
    println!("  creds      : {}", cfg.creds_path.display());
    println!("  ollama     : {}", cfg.ollama_base);
    println!(
        "  country    : {}",
        if cfg.country.is_empty() {
            "(undeclared -> XX)"
        } else {
            &cfg.country
        }
    );
    println!(
        "  sessions   : {}",
        if cfg.workload_templates.is_empty() {
            "OFF (set PROVIDER_WORKLOADS to opt in)".to_string()
        } else {
            cfg.workload_templates.join(",")
        }
    );
    println!(
        "  ceilings   : cpus={:?} vram_mb={:?} disk_gb={:?}",
        cfg.max_shared_cpus, cfg.max_shared_vram_mb, cfg.max_shared_disk_gb
    );
    println!();
    println!("accelerator");
    let all = kmplify_node::gpu::detect_all().await;
    if all.is_empty() {
        println!("  detected   : none (CPU-only node)");
    } else {
        for g in &all {
            let primary = if g.backend == cfg.accel() {
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
    // host does not have is otherwise invisible here: the card list looks
    // healthy while the node advertises CPU.
    let advertised = cfg.accel();
    let found = all.iter().any(|g| g.backend == advertised);
    if advertised == kmplify_node::gpu::Backend::Cpu {
        println!("  advertised : cpu (no accelerator offered to the fabric)");
    } else if found {
        println!("  advertised : {}", advertised.as_str());
    } else {
        println!(
            "  advertised : {} -- NOT DETECTED on this host, so the hello frame",
            advertised.as_str()
        );
        println!("               will report cpu. Check the override or the driver.");
    }
    if !advertised.hosts_container_sessions() && advertised != kmplify_node::gpu::Backend::Cpu {
        println!(
            "  note       : {} serves INFERENCE but cannot pass a GPU into a",
            advertised.as_str()
        );
        println!("               container, so this node cannot host sessions.");
    }
    println!();
    println!("probes");
    let docker = probe("docker", &["version", "--format", "{{.Server.Version}}"]).await;
    println!(
        "  docker     : {}",
        docker.clone().unwrap_or_else(|| "NOT REACHABLE".into())
    );
    let smi = probe("nvidia-smi", &["-L"]).await;
    println!(
        "  nvidia-smi : {}",
        smi.clone().unwrap_or_else(|| "not found".into())
    );
    for (bin, args) in [
        ("rocm-smi", &["--showid"][..]),
        ("xpu-smi", &["discovery", "-j"][..]),
    ] {
        if probe(bin, args).await.is_some() {
            println!("  {bin:<10} : present");
        }
    }
    // Ask the SAME question the worker asks, rather than probing for a sign of
    // life. `/api/version` answering proved nothing: it is Ollama-native, so a
    // vLLM endpoint 404s it — and a 404 is still a response, so the old check
    // printed "reachable" either way. Meanwhile an Ollama with nothing pulled
    // also printed "reachable" and then refused every job.
    //
    // What decides whether this node is useful is the MODEL LIST, so print
    // that. Empty here is exactly the "online but advertising nothing" state
    // that shows up nowhere else until a consumer sees the peer-offline dialog.
    let models = kmplify_node::fabric_worker::local_models(
        &reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default(),
        &cfg.ollama_base,
    )
    .await;
    if models.is_empty() {
        println!("  models     : NONE — this node would be online and refuse every job");
    } else {
        println!("  models     : {} ({})", models.len(), models.join(", "));
    }
    // Sessions without docker cannot work; say so with a non-zero exit so a
    // provisioning script fails loudly instead of deploying a broken node.
    if !cfg.workload_templates.is_empty() && docker.is_none() {
        eprintln!("\nERROR: PROVIDER_WORKLOADS is set but docker is unreachable.");
        return 1;
    }
    // Same reasoning, one layer up: a node that shares inference but lists no
    // models joins the fabric, counts toward online_nodes, and has every job
    // refused by the scheduler — silently, because connecting SUCCEEDED. Fail
    // the preflight so provisioning stops here instead of at a consumer.
    if cfg.share_inference && models.is_empty() {
        eprintln!(
            "\nERROR: no models at {} — nothing to serve.\n\
             Check the endpoint is up and lists models:\n\
             \x20 curl {}/v1/models      # vLLM, LiteLLM, TGI\n\
             \x20 curl {}/api/tags       # Ollama",
            cfg.ollama_base, cfg.ollama_base, cfg.ollama_base
        );
        return 1;
    }
    if cfg
        .workload_templates
        .iter()
        .any(|t| t.contains("vllm") || t.contains("comfyui"))
        && smi.is_none()
    {
        eprintln!("\nWARNING: CUDA templates offered but nvidia-smi not found — the gateway will not schedule them here.");
    }
    0
}

#[tokio::main]
async fn main() {
    let check_only = std::env::args().nth(1).as_deref() == Some("check");

    let dir = node_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        eprintln!("cannot create node dir {}: {e}", dir.display());
        std::process::exit(1);
    }
    let cfg = resolve_config().await;

    if check_only {
        std::process::exit(run_check(&cfg).await);
    }

    println!("[kmplify-node] joining {}", cfg.gateway_url);
    match fabric_worker::ensure_identity(&cfg.gateway_url, &cfg.creds_path).await {
        Ok(c) => println!(
            "[kmplify-node] node identity {}…",
            &c.node_id[..8.min(c.node_id.len())]
        ),
        Err(e) => {
            // Not fatal: run() retries with backoff and self-heals a rejected
            // identity; a gateway that is briefly down should not kill a
            // service-managed node at boot.
            eprintln!(
                "[kmplify-node] identity not established yet ({e}) — the worker will keep trying"
            );
        }
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(fabric_worker::run(cfg, stop_rx));

    // SIGTERM is what systemd sends on stop; Ctrl-C covers interactive use.
    // Either one flips the stop signal, and run() tears down every hosted
    // session container before returning — a stopped node must leave nothing
    // running on the owner's GPU.
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }

    println!("[kmplify-node] stopping — tearing down hosted sessions…");
    let _ = stop_tx.send(true);
    let _ = worker.await;
    println!("[kmplify-node] stopped cleanly");
}
