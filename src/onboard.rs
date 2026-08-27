//! `kmplify-node init` — the first ten minutes of a provider, in one sitting.
//!
//! Becoming a peer compute provider used to mean reading HEADLESS-NODE.md,
//! learning eleven environment variables, knowing which port your inference
//! engine listens on, and finding out from a warning that your country was
//! never declared. Every one of those is a place to give up.
//!
//! This wizard is that walk, done together: look at the machine, find the
//! engine that is already running, ask the sharing questions in plain words,
//! write the answers where the node reads them (`settings.json`, the same
//! file `kmplify-node set` and the dashboard write), preflight the result
//! and offer to start. Six steps, nothing shared until the summary is
//! confirmed, Ctrl-C abandons everything.
//!
//! It is a conversation, not a form: every question has a safe default, every
//! answer is validated where it is typed (the same validators `set` uses, so
//! the wizard cannot produce a configuration the node would refuse), and
//! anything the wizard can find out on its own — the accelerator, the running
//! engines, the fabric's function key — is found, not asked.

use std::io::{BufRead, IsTerminal, Write};

use kmplify_node::fabric_worker::WorkerConfig;
use kmplify_node::settings::Settings;
use kmplify_node::{engines, gpu, hostcpu};

/// Answers come from stdin so the wizard is scriptable and testable; a piped
/// run is a first-class citizen, not a degraded one.
struct Io {
    color: bool,
}

/// The operator walked away or the pipe ran dry. Not an error to report
/// loudly; the message says what was (not) changed.
const ABORTED: &str = "aborted — nothing was saved";

impl Io {
    fn new() -> Self {
        Self {
            color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn accent(&self, t: &str) -> String {
        self.paint("36", t) // cyan
    }
    fn strong(&self, t: &str) -> String {
        self.paint("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
    fn good(&self, t: &str) -> String {
        self.paint("32", t)
    }
    fn warn(&self, t: &str) -> String {
        self.paint("33", t)
    }

    fn step(&self, n: u8, total: u8, title: &str) {
        println!();
        println!(
            " {} {}",
            self.accent(&format!("[{n}/{total}]")),
            self.strong(title)
        );
    }

    /// Ask, with a default an empty answer accepts. `Err` means stdin ended.
    fn ask(&self, prompt: &str, default: &str) -> Result<String, String> {
        let shown = if default.is_empty() {
            format!("   {prompt}: ")
        } else {
            format!("   {prompt} {}: ", self.dim(&format!("[{default}]")))
        };
        print!("{shown}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => Err(ABORTED.into()),
            Ok(_) => {
                let t = line.trim();
                Ok(if t.is_empty() {
                    default.to_string()
                } else {
                    t.to_string()
                })
            }
        }
    }

    fn ask_yn(&self, prompt: &str, default: bool) -> Result<bool, String> {
        let d = if default { "Y/n" } else { "y/N" };
        loop {
            match self.ask(prompt, d)?.to_ascii_lowercase().as_str() {
                "y" | "yes" | "ja" => return Ok(true),
                "n" | "no" | "nein" => return Ok(false),
                s if s == d.to_ascii_lowercase() => return Ok(default),
                _ => println!("   {}", self.dim("y or n")),
            }
        }
    }
}

/// What the wizard decided, applied to `settings` only at the very end.
struct Choices {
    engine: Option<String>,
    share_inference: bool,
    share_cpu: bool,
    approval_manual: bool,
    country: String,
    functions: Option<String>, // Some(pubkey) = on, trusting this key
    share_vectors: bool,
}

pub async fn run(cfg: &WorkerConfig, dir: &std::path::Path) -> i32 {
    let io = Io::new();
    match walk(&io, cfg, dir).await {
        Ok(code) => code,
        Err(e) => {
            println!("\n {}", io.warn(&e));
            1
        }
    }
}

async fn walk(io: &Io, cfg: &WorkerConfig, dir: &std::path::Path) -> Result<i32, String> {
    println!();
    println!(
        " {} {}",
        io.accent("◆"),
        io.strong("kmplify-node setup — lend this machine to the KMPLIFY Compute Fabric")
    );
    println!(
        "   {}",
        io.dim("Six steps. Nothing is shared until you confirm the summary; Ctrl-C abandons all of it.")
    );

    // ------------------------------------------------------- 1: the machine
    io.step(1, 6, "this machine");
    hostcpu::start();
    let gpus = gpu::detect_all().await;
    let (accel, primary) = gpu::resolve_backend(&gpus);
    let cpu = hostcpu::snapshot();
    match &primary {
        Some(g) => println!(
            "   accelerator : {} · {} · {} MB",
            io.good(accel.as_str()),
            g.name,
            g.total_mb
        ),
        None => println!(
            "   accelerator : none {}",
            io.dim("(a CPU node still serves small models, functions and vectors)")
        ),
    }
    println!(
        "   cpu         : {} · {} cores · {} GB RAM",
        cpu.model,
        cpu.logical_cores,
        hostcpu::read_ram_total_mb_now() / 1024
    );
    println!(
        "   {}",
        io.dim("This is what the fabric would see. Ceilings on all of it can be set later with `kmplify-node set`.")
    );

    // ------------------------------------------------------- 2: the engine
    io.step(2, 6, "inference engine");
    println!("   {}", io.dim("scanning localhost for running engines…"));
    let mut found = engines::scan().await;
    // A fabric gateway is not a local engine; offering it would relay peers
    // to peers. Say it was seen, then take it off the menu.
    found.retain(|f| {
        if f.id == "fabric" {
            println!(
                "   {} {} answered at {} — that is a fabric, not a local engine; skipping it",
                io.warn("note:"),
                f.name,
                f.base
            );
            false
        } else {
            true
        }
    });

    let engine: Option<engines::Found> = loop {
        if found.is_empty() {
            println!("   nothing is listening on the usual ports. The engines this node can lend:");
            for k in engines::KNOWN {
                println!("     {:<10} {}", io.strong(k.id), io.dim(k.hint));
            }
        } else {
            for (i, f) in found.iter().enumerate() {
                let models = match f.models.len() {
                    0 => io.warn("0 models — online but would refuse every job"),
                    n => io.good(&format!("{n} model(s)")),
                };
                println!("   {}) {:<10} {:<28} {}", i + 1, f.name, f.base, models);
            }
        }
        let extra_url = found.len() + 1;
        let extra_rescan = found.len() + 2;
        let extra_none = found.len() + 3;
        println!("   {}) somewhere else (enter a URL)", extra_url);
        println!("   {}) rescan", extra_rescan);
        println!(
            "   {}) no engine — lend CPU, functions or vector storage only",
            extra_none
        );
        let default = if found.is_empty() {
            extra_rescan.to_string()
        } else {
            "1".to_string()
        };
        let answer = io.ask("engine", &default)?;
        // A pasted URL is an answer to the question, not a menu mistake.
        if answer.starts_with("http://") || answer.starts_with("https://") {
            match probe_custom(io, &answer).await {
                Some(f) => break Some(f),
                None => continue,
            }
        }
        match answer.parse::<usize>() {
            Ok(n) if n >= 1 && n <= found.len() => break Some(found[n - 1].clone()),
            Ok(n) if n == extra_url => {
                let url = io.ask("engine URL", "")?;
                match probe_custom(io, &url).await {
                    Some(f) => break Some(f),
                    None => continue,
                }
            }
            Ok(n) if n == extra_rescan => {
                found = engines::scan().await;
                found.retain(|f| f.id != "fabric");
                continue;
            }
            Ok(n) if n == extra_none => break None,
            _ => println!("   {}", io.dim("pick a number from the list")),
        }
    };

    // ------------------------------------------------------ 3: what you lend
    io.step(3, 6, "what you lend");
    let share_inference = match &engine {
        Some(f) => {
            let q = format!(
                "share inference from {} ({} models)?",
                f.name,
                f.models.len()
            );
            io.ask_yn(&q, true)?
        }
        None => {
            println!(
                "   {}",
            io.dim("no engine chosen, so inference stays off; the switches below still make this node useful")
            );
            false
        }
    };
    let share_cpu = io.ask_yn("lend spare CPU threads and RAM to peers?", false)?;

    // ------------------------------------------------------ 4: who may use it
    io.step(4, 6, "who may use it");
    println!("   1) auto — any consumer on the fabric may use this node");
    println!("   2) manual — unknown consumers wait until you approve them (kmplify-node peers)");
    let approval_manual = loop {
        match io.ask("admission", "1")?.as_str() {
            "1" | "auto" => break false,
            "2" | "manual" => break true,
            _ => println!("   {}", io.dim("1 or 2")),
        }
    };
    let country = loop {
        let c = io.ask(
            "country, ISO alpha-2 — lets EU consumers find you (Enter for none)",
            "",
        )?;
        let up = c.to_ascii_uppercase();
        if up.is_empty() || (up.len() == 2 && up.chars().all(|ch| ch.is_ascii_alphabetic())) {
            if up.is_empty() {
                println!(
                    "   {}",
                    io.dim("undeclared: the gateway records XX and EU-only consumers will not see this node")
                );
            }
            break up;
        }
        println!("   {}", io.dim("two letters (DE, FR, US …) or Enter"));
    };

    // ------------------------------------------------------ 5: extra lanes
    io.step(5, 6, "extra lanes (both off by default)");
    let functions = if io.ask_yn(
        "host signed Wasm functions? (small sandboxed jobs: HTML to text, CSV to JSON, …)",
        false,
    )? {
        match fetch_function_key(&cfg.gateway_url).await {
            Ok(key) => {
                println!(
                    "   this fabric signs its catalog with {} — only modules under this key will run",
                    io.strong(&format!("{}…", &key[..16.min(key.len())]))
                );
                io.ask_yn("trust it?", true)?.then_some(key)
            }
            Err(e) => {
                println!(
                    "   {} could not fetch the catalog key from {} ({e})",
                    io.warn("!"),
                    cfg.gateway_url
                );
                println!(
                    "   {}",
                    io.dim("switch it on later: kmplify-node set functions=true functions-pubkey=<key>")
                );
                None
            }
        }
    } else {
        None
    };
    let share_vectors = io.ask_yn(
        "hold peers' vector collections? (replicated RAG indexes, payloads opaque to you)",
        false,
    )?;

    // ------------------------------------------------------ 6: save + preflight
    io.step(6, 6, "summary");
    let choices = Choices {
        engine: engine.as_ref().map(|f| f.base.clone()),
        share_inference,
        share_cpu,
        approval_manual,
        country,
        functions,
        share_vectors,
    };
    let on = |b: bool| {
        if b {
            io.good("on")
        } else {
            io.dim("off")
        }
    };
    match &engine {
        Some(f) => println!("   engine    : {} at {}", f.name, f.base),
        None => println!("   engine    : {}", io.dim("none")),
    }
    println!("   inference : {}", on(choices.share_inference));
    println!("   cpu + ram : {}", on(choices.share_cpu));
    println!(
        "   admission : {}",
        if choices.approval_manual {
            io.warn("manual — decide with `kmplify-node peers`")
        } else {
            "auto".to_string()
        }
    );
    println!(
        "   country   : {}",
        if choices.country.is_empty() {
            io.dim("undeclared (XX)")
        } else {
            choices.country.clone()
        }
    );
    println!("   functions : {}", on(choices.functions.is_some()));
    println!("   vectors   : {}", on(choices.share_vectors));

    if !io.ask_yn("save these choices?", true)? {
        return Err(ABORTED.into());
    }

    // Loaded, not fresh: a re-run of init must not erase settings it never
    // asked about (rewards, ceilings, the colibri key).
    let mut stored = Settings::load(dir);
    let set = |stored: &mut Settings, key: &str, value: &str| -> Result<(), String> {
        stored
            .set(key, value)
            .map_err(|e| format!("could not store {key}: {e}"))
    };
    if let Some(base) = &choices.engine {
        set(&mut stored, "engine", base)?;
    }
    set(
        &mut stored,
        "share-inference",
        if choices.share_inference {
            "true"
        } else {
            "false"
        },
    )?;
    set(
        &mut stored,
        "share-cpu",
        if choices.share_cpu { "true" } else { "false" },
    )?;
    set(
        &mut stored,
        "approval-mode",
        if choices.approval_manual {
            "manual"
        } else {
            "auto"
        },
    )?;
    set(&mut stored, "country", &choices.country)?;
    match &choices.functions {
        Some(key) => {
            set(&mut stored, "functions", "true")?;
            set(&mut stored, "functions-pubkey", key)?;
        }
        None => set(&mut stored, "functions", "false")?,
    }
    set(
        &mut stored,
        "share-vectors",
        if choices.share_vectors {
            "true"
        } else {
            "false"
        },
    )?;
    stored.save(dir).map_err(|e| {
        format!(
            "could not write {}: {e}",
            kmplify_node::settings::path(dir).display()
        )
    })?;
    println!(
        "   {} {}",
        io.good("saved"),
        io.dim(&format!(
            "to {} — change any of it later with `kmplify-node set` or the dashboard's sharing screen",
            kmplify_node::settings::path(dir).display()
        ))
    );

    // The wizard's own preflight: the same checks `check` runs, so what it
    // calls ready IS ready.
    println!();
    println!("   {}", io.dim("preflight…"));
    let mut effective = cfg.clone();
    stored.apply(&mut effective);
    let mut pf = super::gather(&effective, std::time::Duration::from_secs(5)).await;
    pf.gpus = gpus;
    super::judge(&effective, &mut pf);
    for w in &pf.warnings {
        println!("   {} {w}", io.warn("warning:"));
    }
    if pf.errors.is_empty() {
        println!("   {}", io.good("ready."));
    } else {
        for e in &pf.errors {
            println!("   {} {e}", io.warn("problem:"));
        }
        println!(
            "   {}",
            io.dim("saved anyway — fix the above, then `kmplify-node check`")
        );
        return Ok(1);
    }

    if io.ask_yn("start lending now?", true)? {
        println!(
            "   {}",
            io.dim("starting — Ctrl-C stops it and tears down anything peers were running")
        );
        return Ok(super::serve(effective, dir.to_path_buf()).await);
    }
    println!();
    println!("   when you are ready:");
    println!(
        "     {}   {}",
        io.strong("kmplify-node"),
        io.dim("run in this terminal")
    );
    println!(
        "     {}   {}",
        io.strong("kmplify-node tui"),
        io.dim("run with the dashboard")
    );
    println!(
        "     {}",
        io.dim("as a service: docs/HEADLESS-NODE.md (systemd unit ships in packaging/)")
    );
    Ok(0)
}

/// Probe a URL the operator typed, and refuse the one thing that must not be
/// an engine: this fabric's own gateway.
async fn probe_custom(io: &Io, url: &str) -> Option<engines::Found> {
    let url = url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        println!("   {}", io.dim("a URL starts with http:// or https://"));
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .unwrap_or_default();
    match engines::probe(&client, url).await {
        Some(f) if f.id == "fabric" => {
            println!(
                "   {} {} is a fabric gateway — pointing a node's engine at a fabric would relay peers to peers",
                io.warn("!"),
                url
            );
            None
        }
        Some(f) => {
            println!(
                "   found {} serving {} model(s)",
                io.good(&f.name),
                f.models.len()
            );
            Some(f)
        }
        None => {
            println!(
                "   {} nothing OpenAI-compatible answered at {url}",
                io.warn("!")
            );
            None
        }
    }
}

/// The catalog key this fabric signs with, from the endpoint that publishes it.
async fn fetch_function_key(gateway: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = client
        .get(format!("{gateway}/v1/functions"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let key = v
        .get("pubkey")
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    if key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(key)
    } else {
        Err("the gateway published no usable key".into())
    }
}
