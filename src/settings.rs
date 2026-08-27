//! What this machine lends, as the operator last chose it.
//!
//! The desktop app has a "Provide this machine's Resources" panel: switches
//! for inference, container sessions and CPU/RAM, ceilings for cores, VRAM,
//! RAM and disk, a country, a colibri upstream, and manual admission. On a
//! headless node all of that was environment variables, which means a change
//! required an editor, a service file and a restart — and the machine's owner
//! was the one person who could not adjust their own machine from where they
//! were standing.
//!
//! This module is that panel's durable half: a small JSON file in the node
//! directory holding ONLY the fields the operator changed at runtime.
//!
//! # Precedence, and why this file wins
//!
//! ```text
//! defaults  <-  environment (unit file, container -e, flags)  <-  settings.json
//! ```
//!
//! The stored choice wins, which is the same contract the desktop app has
//! always had (it re-asserts its `provider_sharing.json` into the stack `.env`
//! on every boot). The alternative — environment wins — would mean a slider
//! silently springs back on the next restart, which is worse than the
//! surprise this way round. The surprise is handled rather than accepted:
//! [`Settings::conflicts`] lists every value that is overriding the
//! environment, `kmplify-node check` prints those lines, and both the
//! dashboard and `kmplify-node set --clear` can drop an override so the
//! environment governs again.
//!
//! # None preserves
//!
//! Every field is an `Option`, and `None` means "the operator never touched
//! this", not "off". Writing a partial update therefore cannot clear
//! settings that are not part of it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fabric_worker::WorkerConfig;

/// File name inside the node directory.
pub const SETTINGS_FILE: &str = "settings.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
// Absent fields are absent from the FILE too, not written as nulls: this
// is a file operators read and sometimes hand-edit, and a wall of nulls
// hides the two lines that are actually set.
#[serde(default)]
pub struct Settings {
    /// Serve chat and embedding jobs (`PROVIDER_SHARE_INFERENCE`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_inference: Option<bool>,
    /// Lend spare CPU threads and system RAM (`PROVIDER_SHARE_CPU`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_cpu: Option<bool>,
    /// Container session templates this node accepts (`PROVIDER_WORKLOADS`).
    /// An empty list is a real choice: sessions off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workloads: Option<Vec<String>>,
    /// `auto` or `manual` (`PROVIDER_APPROVAL_MODE`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    /// ISO-3166-1 alpha-2, or empty to declare nothing (`PROVIDER_COUNTRY`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// The local inference engine's base URL (`OLLAMA_BASE` — the variable
    /// keeps its historic name; the engine behind it is anything that speaks
    /// the OpenAI API: Ollama, llama.cpp, vLLM, LM Studio, LiteLLM, Jan).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Colibri gateway, empty to switch it off (`COLIBRI_BASE`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colibri_base: Option<String>,
    /// Bearer for that gateway (`COLIBRI_API_KEY`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colibri_api_key: Option<String>,
    /// Ceiling on logical CPUs lent to sessions (`PROVIDER_MAX_CPUS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cpus: Option<f64>,
    /// Ceiling on advertised VRAM in MB (`PROVIDER_MAX_VRAM_MB`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_vram_mb: Option<u64>,
    /// Ceiling on advertised system RAM in MB (`PROVIDER_MAX_RAM_MB`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_ram_mb: Option<u64>,
    /// Ceiling on disk sessions may fill, in GB (`PROVIDER_MAX_DISK_GB`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_disk_gb: Option<u64>,
    /// Host signed Wasm functions (`PROVIDER_FUNCTIONS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<bool>,
    /// The catalog key those functions must be signed with
    /// (`PROVIDER_FUNCTIONS_PUBKEY`). Without it the node trusts nothing and
    /// refuses every function, which is why it is settable next to the switch
    /// rather than only in a unit file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions_pubkey: Option<String>,
    /// Lend storage for replicated vector collections (`PROVIDER_SHARE_VECTORS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_vectors: Option<bool>,
    /// Ceiling on those collections in MB (`PROVIDER_MAX_VECTOR_MB`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_vector_mb: Option<u64>,
    /// Show what an installed rewards companion reports (`PROVIDER_REWARDS`).
    ///
    /// Deliberately NOT applied to `WorkerConfig`: the worker has no idea
    /// rewards exist, and that is the point. Serving must never depend on a
    /// payment system being happy — see [`crate::rewards`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewards: Option<bool>,
}

/// The settings a caller may address by name, in `kmplify-node set` and in
/// the dashboard's own bookkeeping.
///
/// One table, so the CLI, the usage text and the clear-an-override path can
/// never disagree about what a key is called or which variable it overrides.
pub const KEYS: &[(&str, &str)] = &[
    ("share-inference", "PROVIDER_SHARE_INFERENCE"),
    ("share-cpu", "PROVIDER_SHARE_CPU"),
    ("workloads", "PROVIDER_WORKLOADS"),
    ("approval-mode", "PROVIDER_APPROVAL_MODE"),
    ("country", "PROVIDER_COUNTRY"),
    ("engine", "OLLAMA_BASE"),
    ("colibri", "COLIBRI_BASE"),
    ("colibri-key", "COLIBRI_API_KEY"),
    ("max-cpus", "PROVIDER_MAX_CPUS"),
    ("max-vram-mb", "PROVIDER_MAX_VRAM_MB"),
    ("max-ram-mb", "PROVIDER_MAX_RAM_MB"),
    ("max-disk-gb", "PROVIDER_MAX_DISK_GB"),
    ("functions", "PROVIDER_FUNCTIONS"),
    ("functions-pubkey", "PROVIDER_FUNCTIONS_PUBKEY"),
    ("share-vectors", "PROVIDER_SHARE_VECTORS"),
    ("max-vector-mb", "PROVIDER_MAX_VECTOR_MB"),
    ("rewards", "PROVIDER_REWARDS"),
];

pub fn path(node_dir: &Path) -> PathBuf {
    node_dir.join(SETTINGS_FILE)
}

impl Settings {
    /// Load the operator's choices, or an empty set when none were made.
    ///
    /// An unreadable or corrupt file yields the empty set rather than an
    /// error: the node must still start and serve on whatever the
    /// environment says, and the dashboard will show the environment's
    /// values with no override marked.
    pub fn load(node_dir: &Path) -> Self {
        match std::fs::read(path(node_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write the choices, replacing the file.
    ///
    /// Owner-only and atomic: a node reads this on every reconnect, and half
    /// a file parses as no overrides at all, which would silently restore
    /// whatever the environment said.
    pub fn save(&self, node_dir: &Path) -> std::io::Result<()> {
        let target = path(node_dir);
        let tmp = target.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        write_owner_only(&tmp, &bytes)?;
        std::fs::rename(&tmp, &target)
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Overlay these choices onto a configuration built from the environment.
    pub fn apply(&self, cfg: &mut WorkerConfig) {
        if let Some(v) = self.share_inference {
            cfg.share_inference = v;
        }
        if let Some(v) = self.share_cpu {
            cfg.share_cpu = v;
        }
        if let Some(v) = &self.workloads {
            cfg.workload_templates = v.clone();
        }
        if let Some(v) = &self.approval_mode {
            cfg.approval_mode = v.clone();
        }
        if let Some(v) = &self.country {
            cfg.country = v.clone();
        }
        if let Some(v) = &self.engine {
            cfg.ollama_base = v.clone();
        }
        if let Some(v) = &self.colibri_base {
            cfg.colibri_base = v.clone();
        }
        if let Some(v) = &self.colibri_api_key {
            cfg.colibri_api_key = v.clone();
        }
        // A ceiling of zero is how "lend none of this" is spelled on the way
        // in; the worker's contract for that is None (no explicit choice) for
        // everything except a real number, so keep zero out of the config.
        if let Some(v) = self.max_cpus {
            cfg.max_shared_cpus = (v > 0.0).then_some(v);
        }
        if let Some(v) = self.max_vram_mb {
            cfg.max_shared_vram_mb = (v > 0).then_some(v);
        }
        if let Some(v) = self.max_ram_mb {
            cfg.max_shared_ram_mb = (v > 0).then_some(v);
        }
        if let Some(v) = self.max_disk_gb {
            cfg.max_shared_disk_gb = (v > 0).then_some(v);
        }
        if let Some(v) = self.functions {
            cfg.functions.enabled = v;
        }
        if let Some(v) = &self.functions_pubkey {
            cfg.functions.trusted_pubkey = v.clone();
        }
        if let Some(v) = self.share_vectors {
            cfg.vectors.enabled = v;
        }
        // Zero would mean "lend no storage", which is what the switch above
        // is for; a ceiling of nothing is not a ceiling.
        if let Some(v) = self.max_vector_mb {
            if v > 0 {
                cfg.vectors.max_mb = v;
            }
        }
    }

    /// Human lines naming every override that contradicts the environment.
    ///
    /// Printed by `check` and shown in the dashboard, because a stored choice
    /// silently beating a unit file is exactly the kind of thing an operator
    /// should be told once rather than discover during an incident.
    pub fn conflicts(&self, from_env: &WorkerConfig) -> Vec<String> {
        let mut out = Vec::new();
        let mut note = |key: &str, env: String, stored: String| {
            if env != stored {
                let shown = |v: String| {
                    if v.is_empty() {
                        "(none)".to_string()
                    } else {
                        v
                    }
                };
                out.push(format!(
                    "{key}: {} (environment) -> {} (set here)",
                    shown(env),
                    shown(stored)
                ));
            }
        };
        if let Some(v) = self.share_inference {
            note(
                "PROVIDER_SHARE_INFERENCE",
                from_env.share_inference.to_string(),
                v.to_string(),
            );
        }
        if let Some(v) = self.share_cpu {
            note(
                "PROVIDER_SHARE_CPU",
                from_env.share_cpu.to_string(),
                v.to_string(),
            );
        }
        if let Some(v) = &self.workloads {
            note(
                "PROVIDER_WORKLOADS",
                from_env.workload_templates.join(","),
                v.join(","),
            );
        }
        if let Some(v) = &self.approval_mode {
            note(
                "PROVIDER_APPROVAL_MODE",
                from_env.approval_mode.clone(),
                v.clone(),
            );
        }
        if let Some(v) = &self.country {
            note("PROVIDER_COUNTRY", from_env.country.clone(), v.clone());
        }
        if let Some(v) = &self.engine {
            note("OLLAMA_BASE", from_env.ollama_base.clone(), v.clone());
        }
        if let Some(v) = &self.colibri_base {
            note("COLIBRI_BASE", from_env.colibri_base.clone(), v.clone());
        }
        if let Some(v) = self.functions {
            note(
                "PROVIDER_FUNCTIONS",
                from_env.functions.enabled.to_string(),
                v.to_string(),
            );
        }
        if let Some(v) = self.share_vectors {
            note(
                "PROVIDER_SHARE_VECTORS",
                from_env.vectors.enabled.to_string(),
                v.to_string(),
            );
        }
        // Deliberately no line for the colibri API key: reporting a secret to
        // say it differs is still reporting it.
        if let Some(v) = self.max_cpus {
            note(
                "PROVIDER_MAX_CPUS",
                opt_str(from_env.max_shared_cpus),
                v.to_string(),
            );
        }
        if let Some(v) = self.max_vram_mb {
            note(
                "PROVIDER_MAX_VRAM_MB",
                opt_str(from_env.max_shared_vram_mb),
                v.to_string(),
            );
        }
        if let Some(v) = self.max_ram_mb {
            note(
                "PROVIDER_MAX_RAM_MB",
                opt_str(from_env.max_shared_ram_mb),
                v.to_string(),
            );
        }
        if let Some(v) = self.max_disk_gb {
            note(
                "PROVIDER_MAX_DISK_GB",
                opt_str(from_env.max_shared_disk_gb),
                v.to_string(),
            );
        }
        out
    }

    /// Set one addressable key from its text form, validating as the node
    /// would validate the matching environment variable.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        match key {
            "share-inference" => self.share_inference = Some(parse_bool(key, value)?),
            "share-cpu" => self.share_cpu = Some(parse_bool(key, value)?),
            "workloads" => {
                self.workloads = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                        .collect(),
                )
            }
            "approval-mode" => match value.to_ascii_lowercase().as_str() {
                m @ ("auto" | "manual") => self.approval_mode = Some(m.to_string()),
                _ => return Err(format!("{key} must be auto or manual, not {value:?}")),
            },
            "country" => {
                let up = value.to_ascii_uppercase();
                let alpha2 = up.len() == 2 && up.chars().all(|c| c.is_ascii_alphabetic());
                if !up.is_empty() && !alpha2 {
                    return Err(format!(
                        "{key} must be an ISO-3166-1 alpha-2 code (DE, FR, US …) or empty"
                    ));
                }
                self.country = Some(up);
            }
            "engine" => {
                // A URL, or the name of a known engine, which resolves to
                // where that engine listens by default. `set engine=llamacpp`
                // beats asking someone to know its port.
                let resolved = match crate::engines::known(value) {
                    Some(k) => k.default_base.to_string(),
                    None => value.trim_end_matches('/').to_string(),
                };
                if !resolved.starts_with("http://") && !resolved.starts_with("https://") {
                    let names: Vec<&str> = crate::engines::KNOWN.iter().map(|k| k.id).collect();
                    return Err(format!(
                        "{key} must be a URL or one of: {}",
                        names.join(", ")
                    ));
                }
                self.engine = Some(resolved);
            }
            "colibri" => {
                if !value.is_empty()
                    && !value.starts_with("http://")
                    && !value.starts_with("https://")
                {
                    return Err(format!("{key} must start with http:// or https://"));
                }
                self.colibri_base = Some(value.trim_end_matches('/').to_string());
            }
            "colibri-key" => self.colibri_api_key = Some(value.to_string()),
            "max-cpus" => self.max_cpus = Some(parse_num::<f64>(key, value)?),
            "max-vram-mb" => self.max_vram_mb = Some(parse_capacity(key, value, Unit::Mb)?),
            "max-ram-mb" => self.max_ram_mb = Some(parse_capacity(key, value, Unit::Mb)?),
            "max-disk-gb" => self.max_disk_gb = Some(parse_capacity(key, value, Unit::Gb)?),
            "functions" => self.functions = Some(parse_bool(key, value)?),
            "functions-pubkey" => {
                let v = value.to_ascii_lowercase();
                // 32 bytes of Ed25519, hex. Checked here because the failure
                // it prevents is silent: a mistyped key refuses every job
                // with a signature error and looks like the gateway's fault.
                if !v.is_empty() && (v.len() != 64 || !v.chars().all(|c| c.is_ascii_hexdigit())) {
                    return Err(format!(
                        "{key} must be 64 hex characters (the \"pubkey\" from GET /v1/functions)"
                    ));
                }
                self.functions_pubkey = Some(v);
            }
            "share-vectors" => self.share_vectors = Some(parse_bool(key, value)?),
            "max-vector-mb" => self.max_vector_mb = Some(parse_num::<u64>(key, value)?),
            "rewards" => self.rewards = Some(parse_bool(key, value)?),
            _ => {
                return Err(format!(
                    "unknown setting {key:?} (one of: {})",
                    KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
                ))
            }
        }
        Ok(())
    }

    /// Drop one override, so the environment governs that field again.
    pub fn clear(&mut self, key: &str) -> Result<(), String> {
        match key {
            "share-inference" => self.share_inference = None,
            "share-cpu" => self.share_cpu = None,
            "workloads" => self.workloads = None,
            "approval-mode" => self.approval_mode = None,
            "country" => self.country = None,
            "engine" => self.engine = None,
            "colibri" => self.colibri_base = None,
            "colibri-key" => self.colibri_api_key = None,
            "max-cpus" => self.max_cpus = None,
            "max-vram-mb" => self.max_vram_mb = None,
            "max-ram-mb" => self.max_ram_mb = None,
            "max-disk-gb" => self.max_disk_gb = None,
            "functions" => self.functions = None,
            "functions-pubkey" => self.functions_pubkey = None,
            "share-vectors" => self.share_vectors = None,
            "max-vector-mb" => self.max_vector_mb = None,
            "rewards" => self.rewards = None,
            _ => {
                return Err(format!(
                    "unknown setting {key:?} (one of: {})",
                    KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
                ))
            }
        }
        Ok(())
    }

    /// `key = value` lines for everything the operator has set, for
    /// `kmplify-node set --list`. The colibri key is never printed.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                out.push(format!("{k} = {v}"));
            }
        };
        push(
            "share-inference",
            self.share_inference.map(|v| v.to_string()),
        );
        push("share-cpu", self.share_cpu.map(|v| v.to_string()));
        push("workloads", self.workloads.as_ref().map(|v| v.join(",")));
        push("approval-mode", self.approval_mode.clone());
        push("country", self.country.clone());
        push("engine", self.engine.clone());
        push("colibri", self.colibri_base.clone());
        push(
            "colibri-key",
            self.colibri_api_key
                .as_ref()
                .map(|k| if k.is_empty() { "(empty)" } else { "(set)" }.to_string()),
        );
        push("max-cpus", self.max_cpus.map(|v| v.to_string()));
        push("max-vram-mb", self.max_vram_mb.map(|v| v.to_string()));
        push("max-ram-mb", self.max_ram_mb.map(|v| v.to_string()));
        push("max-disk-gb", self.max_disk_gb.map(|v| v.to_string()));
        push("functions", self.functions.map(|v| v.to_string()));
        push(
            "functions-pubkey",
            self.functions_pubkey.as_ref().map(|k| {
                if k.is_empty() {
                    "(cleared)".into()
                } else {
                    format!("{}…", &k[..8.min(k.len())])
                }
            }),
        );
        push("share-vectors", self.share_vectors.map(|v| v.to_string()));
        push("max-vector-mb", self.max_vector_mb.map(|v| v.to_string()));
        push("rewards", self.rewards.map(|v| v.to_string()));
        out
    }
}

impl Settings {
    /// Is a rewards companion allowed to be asked at all?
    ///
    /// Stored choice first, then the environment, then off. Off is the only
    /// default there can be: nothing should run another program on an
    /// operator's machine because a file happened to be installed.
    pub fn rewards_enabled(&self) -> bool {
        if let Some(v) = self.rewards {
            return v;
        }
        matches!(
            std::env::var("PROVIDER_REWARDS")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        )
    }
}

fn opt_str<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "unset".into())
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{key} must be true or false, not {value:?}")),
    }
}

/// The unit a capacity key is stored in.
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Mb,
    Gb,
}

/// A capacity, in whatever unit a person reaches for.
///
/// The ceilings are stored in the wire's units (MB for VRAM and RAM, GB for
/// disk), and nobody thinks about their card in megabytes: the desktop's
/// sliders say "48 / 48 GB". So `set max-vram-mb=48g` means what it looks
/// like, `49152` still means what it always did (the bare number IS the
/// key's own unit — the unit is in its name), and `1t` works for the disk
/// on a machine that actually has one.
fn parse_capacity(key: &str, value: &str, native: Unit) -> Result<u64, String> {
    let v = value.trim().to_ascii_lowercase();
    let (digits, suffix) = v.split_at(
        v.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(v.len()),
    );
    let n: f64 = digits
        .parse()
        .map_err(|_| format!("{key} must be a number, optionally with mb/gb/tb, not {value:?}"))?;
    let mb = match suffix.trim() {
        "" => match native {
            Unit::Mb => n,
            Unit::Gb => n * 1024.0,
        },
        "m" | "mb" | "mib" => n,
        "g" | "gb" | "gib" => n * 1024.0,
        "t" | "tb" | "tib" => n * 1024.0 * 1024.0,
        other => return Err(format!("{key}: unknown unit {other:?} (use mb, gb or tb)")),
    };
    Ok(match native {
        Unit::Mb => mb.round() as u64,
        Unit::Gb => (mb / 1024.0).round() as u64,
    })
}

fn parse_num<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .parse::<T>()
        .map_err(|_| format!("{key} must be a number, not {value:?}"))
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // 0600 because this file carries the colibri API key. Same reasoning
        // as the credential file: a headless box is exactly where other local
        // accounts exist.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        f.write_all(bytes)?;
        f.flush()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_cfg() -> WorkerConfig {
        WorkerConfig {
            share_inference: true,
            share_cpu: false,
            approval_mode: "auto".into(),
            country: "DE".into(),
            max_shared_cpus: Some(4.0),
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_file_changes_nothing() {
        let mut cfg = env_cfg();
        let before = format!(
            "{:?}",
            (cfg.share_inference, cfg.share_cpu, cfg.max_shared_cpus)
        );
        Settings::default().apply(&mut cfg);
        assert_eq!(
            before,
            format!(
                "{:?}",
                (cfg.share_inference, cfg.share_cpu, cfg.max_shared_cpus)
            )
        );
    }

    #[test]
    fn stored_choices_win_over_the_environment() {
        let mut s = Settings::default();
        s.set("max-cpus", "6").unwrap();
        s.set("share-cpu", "yes").unwrap();
        let mut cfg = env_cfg();
        s.apply(&mut cfg);
        assert_eq!(cfg.max_shared_cpus, Some(6.0));
        assert!(cfg.share_cpu);
        // …and the operator is told, rather than left to discover it.
        let lines = s.conflicts(&env_cfg());
        assert!(lines.iter().any(|l| l.contains("PROVIDER_MAX_CPUS")));
        assert!(lines.iter().any(|l| l.contains("PROVIDER_SHARE_CPU")));
    }

    #[test]
    fn an_override_equal_to_the_environment_is_not_a_conflict() {
        let mut s = Settings::default();
        s.set("country", "de").unwrap();
        assert!(s.conflicts(&env_cfg()).is_empty());
    }

    #[test]
    fn a_zero_ceiling_means_no_explicit_ceiling_not_zero_capacity() {
        // The worker reads None as "no explicit choice"; storing Some(0)
        // would advertise a machine that can hold nothing.
        let mut s = Settings::default();
        s.set("max-vram-mb", "0").unwrap();
        let mut cfg = env_cfg();
        cfg.max_shared_vram_mb = Some(8000);
        s.apply(&mut cfg);
        assert_eq!(cfg.max_shared_vram_mb, None);
    }

    #[test]
    fn clearing_an_override_hands_the_field_back_to_the_environment() {
        let mut s = Settings::default();
        s.set("max-cpus", "6").unwrap();
        s.clear("max-cpus").unwrap();
        let mut cfg = env_cfg();
        s.apply(&mut cfg);
        assert_eq!(cfg.max_shared_cpus, Some(4.0));
        assert!(s.is_empty());
    }

    #[test]
    fn values_are_validated_the_way_the_environment_is() {
        let mut s = Settings::default();
        assert!(s.set("country", "DEU").is_err());
        assert!(s.set("approval-mode", "vet-everyone").is_err());
        assert!(s.set("max-cpus", "eigth").is_err());
        assert!(s.set("share-cpu", "maybe").is_err());
        assert!(s.set("colibri", "127.0.0.1:5000").is_err());
        assert!(s.set("nonsense", "1").is_err());
        // Valid forms, including the ways people actually spell them.
        s.set("country", "de").unwrap();
        assert_eq!(s.country.as_deref(), Some("DE"));
        s.set("colibri", "http://127.0.0.1:5000/").unwrap();
        assert_eq!(s.colibri_base.as_deref(), Some("http://127.0.0.1:5000"));
        s.set("country", "").unwrap();
        assert_eq!(s.country.as_deref(), Some(""));
    }

    #[test]
    fn sessions_can_be_switched_off_by_storing_an_empty_list() {
        let mut s = Settings::default();
        s.set("workloads", "").unwrap();
        let mut cfg = env_cfg();
        cfg.workload_templates = vec!["vllm-openai".into()];
        s.apply(&mut cfg);
        assert!(cfg.workload_templates.is_empty());
    }

    #[test]
    fn the_v3_lanes_are_switchable_without_touching_a_unit_file() {
        // The runtime ships in every released binary now, so the difference
        // between "can host functions" and "cannot" is these two lines.
        let mut s = Settings::default();
        s.set("functions", "true").unwrap();
        s.set("functions-pubkey", &"ab".repeat(32)).unwrap();
        s.set("share-vectors", "yes").unwrap();
        s.set("max-vector-mb", "4096").unwrap();
        let mut cfg = env_cfg();
        s.apply(&mut cfg);
        assert!(cfg.functions.enabled);
        assert_eq!(cfg.functions.trusted_pubkey, "ab".repeat(32));
        assert!(cfg.vectors.enabled);
        assert_eq!(cfg.vectors.max_mb, 4096);
    }

    #[test]
    fn a_mistyped_catalog_key_is_refused_where_it_is_typed() {
        // Otherwise it fails much later, as a signature error on every job,
        // which reads as the gateway's fault rather than a typo.
        let mut s = Settings::default();
        assert!(s.set("functions-pubkey", "not-a-key").is_err());
        assert!(s.set("functions-pubkey", &"a".repeat(63)).is_err());
        assert!(s.set("functions-pubkey", &"zz".repeat(32)).is_err());
        s.set("functions-pubkey", &"AB".repeat(32)).unwrap();
        assert_eq!(
            s.functions_pubkey.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        // Emptying it is how an operator says "trust nothing again".
        s.set("functions-pubkey", "").unwrap();
        assert_eq!(s.functions_pubkey.as_deref(), Some(""));
    }

    #[test]
    fn capacities_speak_the_units_people_think_in() {
        let mut s = Settings::default();
        // The desktop's slider says "48 / 48 GB"; the CLI now accepts the
        // same sentence.
        s.set("max-vram-mb", "48g").unwrap();
        assert_eq!(s.max_vram_mb, Some(49_152));
        s.set("max-ram-mb", "0.5tb").unwrap();
        assert_eq!(s.max_ram_mb, Some(524_288));
        // A bare number stays the key's own unit — the unit is in its name.
        s.set("max-vram-mb", "16000").unwrap();
        assert_eq!(s.max_vram_mb, Some(16_000));
        s.set("max-disk-gb", "1t").unwrap();
        assert_eq!(s.max_disk_gb, Some(1024));
        s.set("max-disk-gb", "512000mb").unwrap();
        assert_eq!(s.max_disk_gb, Some(500));
        assert!(s.set("max-vram-mb", "48 potatoes").is_err());
        assert!(s.set("max-vram-mb", "lots").is_err());
    }

    #[test]
    fn an_engine_can_be_named_instead_of_addressed() {
        let mut s = Settings::default();
        s.set("engine", "llama.cpp").unwrap();
        assert_eq!(s.engine.as_deref(), Some("http://127.0.0.1:8080"));
        s.set("engine", "http://10.0.0.7:8000/").unwrap();
        assert_eq!(s.engine.as_deref(), Some("http://10.0.0.7:8000"));
        // A typo lists the names it could have meant.
        let err = s.set("engine", "gpt4all").unwrap_err();
        assert!(err.contains("ollama") && err.contains("llamacpp"), "{err}");
        // …and the choice actually steers the worker.
        let mut cfg = env_cfg();
        s.apply(&mut cfg);
        assert_eq!(cfg.ollama_base, "http://10.0.0.7:8000");
    }

    #[test]
    fn the_listing_never_prints_the_colibri_key() {
        let mut s = Settings::default();
        s.set("colibri-key", "super-secret").unwrap();
        let text = s.lines().join("\n");
        assert!(text.contains("colibri-key = (set)"));
        assert!(!text.contains("super-secret"));
        assert!(!s.conflicts(&env_cfg()).join("\n").contains("super-secret"));
    }

    #[test]
    fn a_round_trip_through_the_file_keeps_every_field() {
        let dir = std::env::temp_dir().join(format!("kmplify-node-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Settings::default();
        for (k, v) in [
            ("share-inference", "false"),
            ("workloads", "ollama-cpu, echo-test"),
            ("approval-mode", "manual"),
            ("max-disk-gb", "40"),
        ] {
            s.set(k, v).unwrap();
        }
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        assert_eq!(
            s.workloads.as_deref(),
            Some(&["ollama-cpu".to_string(), "echo-test".to_string()][..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_key_maps_to_the_variable_it_overrides() {
        for (key, _) in KEYS {
            let mut s = Settings::default();
            // Every advertised key must be settable and clearable, or the
            // dashboard offers a row nothing can write.
            assert!(s.clear(key).is_ok(), "{key} not clearable");
            let sample = match *key {
                "share-inference" | "share-cpu" | "functions" | "share-vectors" | "rewards" => {
                    "true"
                }
                "functions-pubkey" => &"a".repeat(64),
                "approval-mode" => "auto",
                "country" => "DE",
                "engine" => "ollama",
                "colibri" => "http://127.0.0.1:5000",
                "colibri-key" => "k",
                "workloads" => "ollama-cpu",
                _ => "1",
            };
            assert!(s.set(key, sample).is_ok(), "{key} not settable");
        }
    }
}
