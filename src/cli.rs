//! Argument parsing for the `kmplify-node` binary.
//!
//! Hand-rolled, and deliberately: this binary's whole appeal is that it drops
//! onto a headless box with no runtime and no surprises, and a derive-based
//! argument crate would be the largest dependency in the tree for the sake of
//! six subcommands. What is here is small enough to read in one sitting and
//! tested below.
//!
//! # Flags are a front-end for the environment
//!
//! Configuration is environment variables, matching the desktop app's stack
//! `.env` name for name (see the module docs in `main.rs`). Every value flag
//! therefore *sets* its environment variable and nothing else, so there is one
//! config path rather than two that can disagree: `check` reports exactly what
//! `run` would use, whether the value came from a unit file or the command
//! line. Flags win over the environment, because the person typing is more
//! recent than the unit file.
//!
//! Before this existed the binary took one positional word — `check` — and
//! ignored everything else, which meant `kmplify-node --help` silently joined
//! the public fabric and started lending the machine's GPU. Unknown arguments
//! are now a usage error.

use std::time::Duration;

/// What the process was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Join the fabric and serve, logging to stdout. The default.
    Run,
    /// Full-screen dashboard: watch and control the node from a terminal.
    Tui,
    /// The desktop window: this node and the other kmplify-nodes on the
    /// local network, with live meters, jobs and settings.
    Gui,
    /// Resolve the configuration, probe the host, report whether this machine
    /// can actually serve. Never connects.
    Check,
    /// One-shot report of a running node, for scripts and `watch`.
    Status,
    /// Print this install's node id, the handle consumers pin and invite.
    Id,
    /// Change what this machine lends, durably — the dashboard's sharing
    /// screen for scripts and shells with no terminal.
    Set,
    /// Who may use this machine: list them, and decide — the dashboard's
    /// peers screen for scripts and shells with no terminal.
    Peers,
    /// First-run wizard: find the engine, ask the sharing questions, save,
    /// preflight, offer to start.
    Init,
    /// Scan localhost for inference engines and say which one is active.
    Engines,
    /// What this node delivered, and what an optional rewards companion
    /// makes of it.
    Rewards,
    Version,
    Help,
}

#[derive(Clone, Debug)]
pub struct Cli {
    pub cmd: Cmd,
    /// Machine-readable output, for `check` and `status`.
    pub json: bool,
    /// Per-probe ceiling in `check`, so a wedged docker socket cannot hang a
    /// provisioning run.
    pub probe_timeout: Duration,
    /// `tui`: attach to an already-running node, never start one.
    pub attach: bool,
    /// `tui`: run the node in this process, even if another is running.
    pub standalone: bool,
    /// `run`/`tui`: also run the LAN router (discovery, node-info, the
    /// routing proxies). The window always has the router: it attaches to
    /// one already running here, or runs its own.
    pub router: bool,
    /// Environment overrides collected from flags, applied before anything
    /// reads the environment.
    pub env: Vec<(&'static str, String)>,
    /// `set`: `key=value` assignments, in the order given.
    pub assignments: Vec<(String, String)>,
    /// `set --clear KEY`: overrides to drop, so the environment governs again.
    pub clear: Vec<String>,
    /// `set --clear-all`: drop every override.
    pub clear_all: bool,
    /// `set --list`: print the stored overrides and change nothing.
    pub list: bool,
    /// `peers`: the verb and its argument, e.g. `["approve", "node-9"]`.
    pub args: Vec<String>,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            cmd: Cmd::Run,
            json: false,
            probe_timeout: Duration::from_secs(5),
            attach: false,
            standalone: false,
            router: false,
            env: Vec::new(),
            assignments: Vec::new(),
            clear: Vec::new(),
            clear_all: false,
            list: false,
            args: Vec::new(),
        }
    }
}

impl Cli {
    /// Apply the flag overrides to this process's environment.
    ///
    /// Called once, before any configuration is read and before any thread
    /// that might read the environment is spawned.
    pub fn apply_env(&self) {
        for (key, value) in &self.env {
            std::env::set_var(key, value);
        }
    }
}

/// Value flags, each a front-end for one environment variable.
const VALUE_FLAGS: &[(&str, &str, &str)] = &[
    (
        "--gateway",
        "PROVIDER_GATEWAY_URL",
        "URL of the fabric to join",
    ),
    (
        "--ollama",
        "OLLAMA_BASE",
        "local model server (Ollama, vLLM, LiteLLM, TGI)",
    ),
    (
        "--colibri",
        "COLIBRI_BASE",
        "optional colibri gateway for frontier models",
    ),
    (
        "--node-dir",
        "KMPLIFY_NODE_DIR",
        "identity, status and control directory",
    ),
    (
        "--country",
        "PROVIDER_COUNTRY",
        "ISO alpha-2 declared to the fabric",
    ),
    (
        "--workloads",
        "PROVIDER_WORKLOADS",
        "template ids to host as container sessions",
    ),
    (
        "--max-cpus",
        "PROVIDER_MAX_CPUS",
        "ceiling on CPUs lent to sessions",
    ),
    (
        "--max-vram-mb",
        "PROVIDER_MAX_VRAM_MB",
        "ceiling on advertised VRAM",
    ),
    (
        "--max-ram-mb",
        "PROVIDER_MAX_RAM_MB",
        "ceiling on advertised RAM",
    ),
    (
        "--max-disk-gb",
        "PROVIDER_MAX_DISK_GB",
        "ceiling on disk sessions may fill",
    ),
    (
        "--approval-mode",
        "PROVIDER_APPROVAL_MODE",
        "auto | manual consumer admission",
    ),
    (
        "--gpu-backend",
        "KMPLIFY_GPU_BACKEND",
        "force cuda | rocm | oneapi | metal | cpu",
    ),
];

/// Switches, each with a `--no-` twin, over the same environment variables.
const BOOL_FLAGS: &[(&str, &str, &str)] = &[
    (
        "--share-inference",
        "PROVIDER_SHARE_INFERENCE",
        "serve chat and embedding jobs",
    ),
    (
        "--share-cpu",
        "PROVIDER_SHARE_CPU",
        "lend spare CPU threads and RAM",
    ),
    (
        "--functions",
        "PROVIDER_FUNCTIONS",
        "host signed Wasm functions",
    ),
    (
        "--share-vectors",
        "PROVIDER_SHARE_VECTORS",
        "host vector collections",
    ),
];

pub fn usage() -> String {
    let mut out = String::new();
    out.push_str(
        "kmplify-node — lend this machine's GPU, CPU and local models to a KMPLIFY fabric.\n\
         \n\
         USAGE\n\
         \x20 kmplify-node [run] [options]     join the fabric and serve (logs to stdout)\n\
         \x20 kmplify-node tui [options]       live dashboard: watch and control the node\n\
         \x20 kmplify-node gui [options]       desktop window: this node and the LAN's other nodes\n\
         \x20 kmplify-node check [options]     preflight this host, then exit\n\
         \x20 kmplify-node status [--json]     one-shot report of the running node\n\
         \x20 kmplify-node id                  print this install's node id\n\
         \x20 kmplify-node set KEY=VALUE …     change what this machine lends, durably\n\
         \x20 kmplify-node init                first-run wizard: engine, sharing, preflight\n\
         \x20 kmplify-node engines             scan localhost for inference engines\n\
         \x20 kmplify-node peers [VERB …]      who may use it; approve, block, invite\n\
         \x20 kmplify-node rewards             delivered work, and an optional payout companion\n\
         \x20 kmplify-node version | help\n\
         \n\
         DASHBOARD\n\
         \x20 `tui` attaches to a node already running here (systemd, Docker) and\n\
         \x20 drives it: pause and resume sharing, evict a peer's session, force a\n\
         \x20 reconnect, stop the node. With no node running it starts one itself, so\n\
         \x20 a GUI-less machine is operated entirely from the terminal.\n\
         \n\
         ENGINES\n\
         \x20 The node lends whatever OpenAI-compatible engine you already run:\n\
         \x20 Ollama, llama.cpp, vLLM, LM Studio, LiteLLM, Jan, colibri. Find and\n\
         \x20 pick one:\n\
         \n\
         \x20 kmplify-node engines                  # what is running, what it serves\n\
         \x20 kmplify-node set engine=llamacpp      # a name resolves to its usual port\n\
         \x20 kmplify-node set engine=http://10.0.0.7:8000   # or say exactly where\n\
         \n\
         SHARING\n\
         \x20 What this machine lends is the dashboard's sharing screen (key 5) and,\n\
         \x20 for scripts, `set`. Both write the same file in the node directory and\n\
         \x20 take effect on the reconnect they trigger — no restart, no unit-file\n\
         \x20 edit. A stored choice overrides the environment until it is cleared.\n\
         \n\
         \x20 kmplify-node set max-cpus=6 share-cpu=true\n\
         \x20 kmplify-node set workloads=vllm-openai,comfyui\n\
         \x20 kmplify-node set --clear max-cpus     # back to the unit file\n\
         \x20 kmplify-node set --list\n\
         \n\
         OPTIONS\n\
         \x20 --json                 machine-readable output (check, status, peers)\n\
         \x20 --timeout SECS         per-probe ceiling in check (default 5)\n\
         \x20 --attach               tui/gui: require a running node, never start one\n\
         \x20 --standalone           tui/gui: run the node in this process\n\
         \x20 --router               run/tui: also run the LAN router (gui and tui attach to it)\n\
         \x20 -h, --help             this text\n\
         \x20 -V, --version          version and build stamp\n\
         \n\
         CONFIGURATION\n\
         \x20 Each flag below sets the environment variable beside it, which is the\n\
         \x20 only configuration surface: what a unit file sets, a flag overrides.\n",
    );
    for (flag, key, help) in VALUE_FLAGS {
        out.push_str(&format!("\x20 {:<22} {:<26} {help}\n", *flag, *key));
    }
    for (flag, key, help) in BOOL_FLAGS {
        // `--[no-]x` rather than `--x/--no-x`: both spellings, one column,
        // and the table stays readable in an 80-column terminal.
        out.push_str(&format!(
            "\x20 {:<22} {:<26} {help}\n",
            format!("--[no-]{}", flag.trim_start_matches("--")),
            *key
        ));
    }
    out.push_str(
        "\n\
         ADMISSION\n\
         \x20 `auto` lets any consumer use this machine; `manual` parks unknown ones\n\
         \x20 until you decide. Invitations always connect, in either mode.\n\
         \n\
         \x20 kmplify-node set approval-mode=manual\n\
         \x20 kmplify-node peers                    # who is waiting, who is using it\n\
         \x20 kmplify-node peers approve node-9abc\n\
         \x20 kmplify-node peers block anon-1a2b3c4d\n\
         \n\
         \x20 verbs:\n",
    );
    for (verb, _, help) in PEER_VERBS {
        out.push_str(&format!("\x20   {:<20} {help}\n", *verb));
    }
    out.push_str("\n SETTINGS (for `set`), each overriding the variable beside it\n");
    for (key, var) in kmplify_node::settings::KEYS {
        out.push_str(&format!("\x20 {:<22} {}\n", *key, *var));
    }
    out.push_str(
        "\n\
         EXIT CODES\n\
         \x20 0  ready (check), or a clean stop\n\
         \x20 1  this host cannot serve as configured, or no node is running (status)\n\
         \x20 2  the command line or the configuration is wrong\n",
    );
    out
}

/// Parse `argv` (without the program name).
pub fn parse(argv: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut cmd_seen = false;
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].clone();
        i += 1;

        if !arg.starts_with('-') {
            if cmd_seen {
                // `set` is the one command that takes positional arguments:
                // the assignments themselves.
                if cli.cmd == Cmd::Set {
                    let Some((key, value)) = arg.split_once('=') else {
                        return Err(format!("`set` takes key=value pairs; `{arg}` has no `=`"));
                    };
                    cli.assignments
                        .push((key.trim().to_string(), value.to_string()));
                    continue;
                }
                if cli.cmd == Cmd::Peers {
                    cli.args.push(arg);
                    continue;
                }
                return Err(format!("unexpected argument `{arg}`"));
            }
            cmd_seen = true;
            cli.cmd = match arg.as_str() {
                "run" | "start" | "serve" => Cmd::Run,
                "tui" | "dashboard" | "top" => Cmd::Tui,
                "gui" | "window" | "app" => Cmd::Gui,
                "check" | "doctor" | "preflight" => Cmd::Check,
                "status" => Cmd::Status,
                "id" | "node-id" => Cmd::Id,
                "set" | "config" => Cmd::Set,
                "peers" | "consumers" => Cmd::Peers,
                "init" | "onboard" | "setup" => Cmd::Init,
                "engines" | "engine" => Cmd::Engines,
                "rewards" | "earnings" => Cmd::Rewards,
                "version" => Cmd::Version,
                "help" => Cmd::Help,
                other => return Err(format!("unknown command `{other}`")),
            };
            continue;
        }

        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        match name.as_str() {
            "-h" | "--help" => {
                return Ok(Cli {
                    cmd: Cmd::Help,
                    ..cli
                })
            }
            "-V" | "--version" => {
                return Ok(Cli {
                    cmd: Cmd::Version,
                    ..cli
                })
            }
            "--json" => cli.json = true,
            "--clear" => {
                let key = take(&name, inline, argv, &mut i)?;
                cli.clear.push(key);
            }
            "--clear-all" => cli.clear_all = true,
            "--list" => cli.list = true,
            "--attach" => cli.attach = true,
            "--standalone" => cli.standalone = true,
            "--router" => cli.router = true,
            "--timeout" => {
                let raw = take(&name, inline, argv, &mut i)?;
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| format!("--timeout wants whole seconds, not `{raw}`"))?;
                if secs == 0 {
                    return Err("--timeout must be at least 1 second".into());
                }
                cli.probe_timeout = Duration::from_secs(secs);
            }
            _ => {
                if let Some((_, key, _)) = VALUE_FLAGS.iter().find(|(f, _, _)| *f == name) {
                    let value = take(&name, inline, argv, &mut i)?;
                    cli.env.push((key, value));
                } else if let Some((_, key, _)) = BOOL_FLAGS.iter().find(|(f, _, _)| *f == name) {
                    let value = inline.unwrap_or_else(|| "true".into());
                    cli.env.push((key, value));
                } else if let Some(stripped) = name.strip_prefix("--no-") {
                    let long = format!("--{stripped}");
                    let Some((_, key, _)) = BOOL_FLAGS.iter().find(|(f, _, _)| *f == long) else {
                        return Err(format!("unknown flag `{name}`"));
                    };
                    if inline.is_some() {
                        return Err(format!("`{name}` takes no value"));
                    }
                    cli.env.push((key, "false".into()));
                } else {
                    return Err(format!("unknown flag `{name}`"));
                }
            }
        }
    }

    if cli.attach && cli.standalone {
        return Err("--attach and --standalone ask for opposite things".into());
    }
    if cli.json
        && !matches!(
            cli.cmd,
            Cmd::Check | Cmd::Status | Cmd::Peers | Cmd::Engines
        )
    {
        return Err("--json applies to `check`, `status`, `peers` and `engines`".into());
    }
    if (cli.attach || cli.standalone) && !matches!(cli.cmd, Cmd::Tui | Cmd::Gui) {
        return Err("--attach and --standalone apply to `tui` and `gui`".into());
    }
    if cli.router && !matches!(cli.cmd, Cmd::Run | Cmd::Tui | Cmd::Gui) {
        return Err(
            "--router applies to `run` and `tui` (the window always runs the router)".into(),
        );
    }
    let touches_settings = !cli.assignments.is_empty() || !cli.clear.is_empty() || cli.clear_all;
    if (touches_settings || cli.list) && cli.cmd != Cmd::Set {
        return Err("--clear, --clear-all, --list and key=value belong to `set`".into());
    }
    if cli.cmd == Cmd::Set && !touches_settings && !cli.list {
        return Err(
            "`set` needs something to do: key=value, --clear KEY, --clear-all, or --list".into(),
        );
    }
    if cli.list && touches_settings {
        return Err("--list only reports; drop it to change a setting".into());
    }
    if !cli.args.is_empty() && cli.cmd != Cmd::Peers {
        return Err(format!("unexpected argument `{}`", cli.args[0]));
    }
    if cli.cmd == Cmd::Peers {
        validate_peers(&cli.args)?;
    }
    Ok(cli)
}

/// What `kmplify-node peers <verb>` accepts, and how many arguments each
/// verb takes. One table, so the parser, the usage text and the runner
/// cannot disagree about what is a verb.
pub const PEER_VERBS: &[(&str, usize, &str)] = &[
    (
        "approve",
        1,
        "admit this consumer (a standing rule, so it holds)",
    ),
    ("deny", 1, "refuse quietly in manual mode"),
    ("block", 1, "refuse in EVERY mode, invitations aside"),
    (
        "clear",
        1,
        "drop the standing rule; the admission mode decides again",
    ),
    ("invite", 0, "mint an invitation, optionally labelled"),
    ("revoke", 1, "revoke an invitation by id"),
];

fn validate_peers(args: &[String]) -> Result<(), String> {
    let Some(verb) = args.first() else {
        return Ok(()); // bare `peers` lists
    };
    let Some((_, takes, _)) = PEER_VERBS.iter().find(|(v, _, _)| v == verb) else {
        return Err(format!(
            "unknown `peers` verb `{verb}` (one of: {})",
            PEER_VERBS
                .iter()
                .map(|(v, _, _)| *v)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let given = args.len() - 1;
    // `invite` takes an OPTIONAL label; everything else needs exactly its
    // argument, because approving nobody in particular is not a thing.
    let ok = if verb == "invite" {
        given <= 1
    } else {
        given == *takes
    };
    if !ok {
        return Err(match verb.as_str() {
            "invite" => "`peers invite` takes at most a label".to_string(),
            "revoke" => "`peers revoke` needs an invitation id".to_string(),
            _ => format!("`peers {verb}` needs a consumer id"),
        });
    }
    Ok(())
}

/// The value for `name`, from `--flag=value` or the next argument.
fn take(
    name: &str,
    inline: Option<String>,
    argv: &[String],
    i: &mut usize,
) -> Result<String, String> {
    if let Some(v) = inline {
        return Ok(v);
    }
    match argv.get(*i) {
        // A flag where a value belongs is a typo, not a value: taking it
        // would silently set the gateway URL to "--json".
        Some(v) if !v.starts_with("--") => {
            *i += 1;
            Ok(v.clone())
        }
        _ => Err(format!("`{name}` needs a value")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_arguments_runs_the_node() {
        assert_eq!(parse(&[]).unwrap().cmd, Cmd::Run);
    }

    #[test]
    fn every_subcommand_resolves() {
        for (word, cmd) in [
            ("run", Cmd::Run),
            ("tui", Cmd::Tui),
            ("gui", Cmd::Gui),
            ("check", Cmd::Check),
            ("status", Cmd::Status),
            ("id", Cmd::Id),
            ("version", Cmd::Version),
            ("help", Cmd::Help),
        ] {
            assert_eq!(parse(&args(&[word])).unwrap().cmd, cmd, "{word}");
        }
    }

    #[test]
    fn help_and_version_never_start_a_node() {
        // The whole reason this module exists: these used to fall through to
        // "run", so asking for help joined the public fabric.
        for flag in ["-h", "--help", "--version", "-V"] {
            let cmd = parse(&args(&[flag])).unwrap().cmd;
            assert!(matches!(cmd, Cmd::Help | Cmd::Version), "{flag}");
        }
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        assert!(parse(&args(&["--frobnicate"])).is_err());
        assert!(parse(&args(&["wibble"])).is_err());
        assert!(parse(&args(&["check", "extra"])).is_err());
    }

    #[test]
    fn value_flags_set_their_environment_variable() {
        let cli = parse(&args(&["run", "--gateway", "https://gw.example"])).unwrap();
        assert_eq!(
            cli.env,
            vec![("PROVIDER_GATEWAY_URL", "https://gw.example".to_string())]
        );
        let inline = parse(&args(&["--gateway=https://gw.example"])).unwrap();
        assert_eq!(inline.env, cli.env);
    }

    #[test]
    fn a_flag_where_a_value_belongs_is_a_typo() {
        assert!(parse(&args(&["run", "--gateway", "--json"])).is_err());
        assert!(parse(&args(&["run", "--gateway"])).is_err());
    }

    #[test]
    fn switches_have_a_negative_twin() {
        assert_eq!(
            parse(&args(&["--no-share-inference"])).unwrap().env,
            vec![("PROVIDER_SHARE_INFERENCE", "false".to_string())]
        );
        assert_eq!(
            parse(&args(&["--share-cpu"])).unwrap().env,
            vec![("PROVIDER_SHARE_CPU", "true".to_string())]
        );
        assert_eq!(
            parse(&args(&["--share-cpu=false"])).unwrap().env,
            vec![("PROVIDER_SHARE_CPU", "false".to_string())]
        );
        assert!(parse(&args(&["--no-share-cpu=true"])).is_err());
        assert!(parse(&args(&["--no-such-switch"])).is_err());
    }

    #[test]
    fn flags_that_belong_to_one_command_are_not_accepted_for_another() {
        assert!(parse(&args(&["run", "--json"])).is_err());
        assert!(parse(&args(&["check", "--attach"])).is_err());
        assert!(parse(&args(&["tui", "--attach", "--standalone"])).is_err());
        assert!(parse(&args(&["status", "--json"])).is_ok());
    }

    #[test]
    fn the_probe_timeout_must_be_a_real_duration() {
        assert_eq!(
            parse(&args(&["check", "--timeout", "9"]))
                .unwrap()
                .probe_timeout,
            Duration::from_secs(9)
        );
        assert!(parse(&args(&["check", "--timeout", "0"])).is_err());
        assert!(parse(&args(&["check", "--timeout", "soon"])).is_err());
    }

    #[test]
    fn peers_lists_by_default_and_decides_when_asked() {
        assert_eq!(parse(&args(&["peers"])).unwrap().cmd, Cmd::Peers);
        let cli = parse(&args(&["peers", "approve", "node-9"])).unwrap();
        assert_eq!(cli.args, vec!["approve".to_string(), "node-9".to_string()]);
        assert!(parse(&args(&["peers", "--json"])).is_ok());
    }

    #[test]
    fn a_decision_needs_someone_to_decide_about() {
        // "approve" on its own would otherwise be a call with an empty
        // consumer id, which the gateway would happily store a rule for.
        for verb in ["approve", "deny", "block", "clear", "revoke"] {
            assert!(parse(&args(&["peers", verb])).is_err(), "{verb}");
            assert!(
                parse(&args(&["peers", verb, "x"])).is_ok(),
                "{verb} with an argument"
            );
            assert!(
                parse(&args(&["peers", verb, "x", "y"])).is_err(),
                "{verb} extra"
            );
        }
    }

    #[test]
    fn an_invitation_label_is_optional_but_at_most_one() {
        assert!(parse(&args(&["peers", "invite"])).is_ok());
        assert!(parse(&args(&["peers", "invite", "Anna's phone"])).is_ok());
        assert!(parse(&args(&["peers", "invite", "a", "b"])).is_err());
    }

    #[test]
    fn an_unknown_peers_verb_is_refused_rather_than_listing() {
        // Silently listing would be worse than an error: a typo'd `blcok`
        // would look like it worked.
        let err = parse(&args(&["peers", "blcok", "node-9"])).unwrap_err();
        assert!(err.contains("unknown"), "{err}");
        assert!(err.contains("block"), "the error names the real verbs");
    }

    #[test]
    fn peers_arguments_belong_to_peers_alone() {
        assert!(parse(&args(&["status", "approve"])).is_err());
        assert!(parse(&args(&["check", "node-9"])).is_err());
    }

    #[test]
    fn the_usage_text_documents_every_peers_verb() {
        let text = usage();
        for (verb, _, _) in PEER_VERBS {
            assert!(text.contains(verb), "{verb} missing from usage");
        }
        assert!(
            text.contains("approval-mode"),
            "how to switch mode is missing"
        );
    }

    #[test]
    fn the_usage_text_documents_every_flag_it_accepts() {
        let text = usage();
        for (flag, key, _) in VALUE_FLAGS {
            assert!(text.contains(flag), "{flag} missing from usage");
            assert!(text.contains(key), "{key} missing from usage");
        }
        for (flag, key, _) in BOOL_FLAGS {
            // Listed as `--[no-]share-cpu`, so match the name rather than the
            // positive spelling.
            let name = flag.trim_start_matches("--");
            assert!(text.contains(name), "{flag} missing from usage");
            assert!(text.contains(key), "{key} missing from usage");
        }
    }
}
