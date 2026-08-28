//! Which inference engine is serving on this machine, found rather than typed.
//!
//! KMPLIFY ships no model runner of its own; the node lends whatever engine
//! the operator already runs — Ollama, llama.cpp, vLLM, LM Studio, LiteLLM,
//! Jan, colibri — through one OpenAI-compatible base URL. That design was
//! always in the code, but the interface for it was an environment variable
//! named after one engine (`OLLAMA_BASE`), which is how a machine running
//! llama.cpp ends up advertising nothing and its owner ends up reading docs.
//!
//! This module turns the URL into a choice: it knows where each well-known
//! engine listens by default, asks each candidate what it is, and reports
//! what answered along with the models it serves. `kmplify-node engines`
//! prints that; `kmplify-node init` builds the first-run choice on it; and
//! `kmplify-node set engine=…` makes the choice durable.
//!
//! Detection is evidence-based, not port-based. A port number is a hint for
//! the scan list; what an endpoint IS comes from how it answers:
//! `/api/tags` answering with a model list is Ollama, and an OpenAI
//! `/v1/models` list is classified by the `owned_by` field its entries
//! carry, which the major engines fill with their own name. Anything else is
//! reported as the honest "OpenAI-compatible", which is all the node needs
//! to serve it.

use std::time::Duration;

use serde::Serialize;

/// One engine this module knows how to find and name.
pub struct Known {
    /// Stable id, accepted by `kmplify-node set engine=<id>`.
    pub id: &'static str,
    /// Human name, as the website and the app spell it.
    pub name: &'static str,
    /// Where it listens when installed with defaults.
    pub default_base: &'static str,
    /// One line for the wizard when nothing is running: how to get it.
    pub hint: &'static str,
}

/// The engines worth scanning for, in the order the wizard offers them.
///
/// The order is deliberate: Ollama first because it is the app's own default
/// and the most common install, then the engines in rough order of how often
/// a machine that has one runs it as a server.
pub const KNOWN: &[Known] = &[
    Known {
        id: "ollama",
        name: "Ollama",
        default_base: "http://127.0.0.1:11434",
        hint: "https://ollama.com — `ollama serve` runs on :11434 by default",
    },
    Known {
        id: "llamacpp",
        name: "llama.cpp",
        default_base: "http://127.0.0.1:8080",
        hint: "`llama-server -m model.gguf` serves OpenAI-compatible on :8080",
    },
    Known {
        id: "mlx",
        name: "MLX",
        default_base: "http://127.0.0.1:8080",
        hint: "`mlx_lm.server` serves OpenAI-compatible on :8080 (Apple Silicon)",
    },
    Known {
        id: "vllm",
        name: "vLLM",
        default_base: "http://127.0.0.1:8000",
        hint: "`vllm serve <model>` listens on :8000 (CUDA/ROCm hosts)",
    },
    Known {
        id: "lmstudio",
        name: "LM Studio",
        default_base: "http://127.0.0.1:1234",
        hint: "LM Studio → Developer → Start server (:1234)",
    },
    Known {
        id: "litellm",
        name: "LiteLLM",
        default_base: "http://127.0.0.1:4000",
        hint: "`litellm --model …` proxies many engines on :4000",
    },
    Known {
        id: "jan",
        name: "Jan",
        default_base: "http://127.0.0.1:1337",
        hint: "Jan → Local API Server (:1337)",
    },
    Known {
        id: "colibri",
        name: "colibri",
        default_base: "http://127.0.0.1:5000",
        hint: "`coli serve` streams frontier MoE models from NVMe (:5000)",
    },
];

/// Look up a known engine by the id `set engine=<id>` accepts.
pub fn known(id: &str) -> Option<&'static Known> {
    let id = id.trim().to_ascii_lowercase();
    // The spellings people actually type, not just the canonical ids.
    let id = match id.as_str() {
        "llama.cpp" | "llama-cpp" | "llama_cpp" | "llamaserver" | "llama-server" => "llamacpp",
        "lm-studio" | "lm_studio" | "lm studio" => "lmstudio",
        "mlx-lm" | "mlx_lm" => "mlx",
        other => other,
    }
    .to_string();
    KNOWN.iter().find(|k| k.id == id)
}

/// The engine this hardware is best served by, and the reason a human reads
/// next to the suggestion.
///
/// Advice, never a decision: callers highlight this row and may move the
/// Enter-accept default onto it, and the operator overrides it with one
/// keystroke. The mapping follows what the desktop app ships per platform —
/// MLX on Apple Silicon, llama.cpp with GPU offloading everywhere else a
/// card is usable, llama.cpp on plain CPUs (GGUF quantization is what makes
/// a CPU node worth lending). A running engine of ANY kind still beats a
/// suggested one that is not up yet; that ranking lives with the caller,
/// because only it knows what is running.
pub fn recommend(accel: crate::gpu::Backend) -> (&'static str, &'static str) {
    use crate::gpu::Backend;
    match accel {
        Backend::Metal => ("mlx", "Apple Silicon"),
        Backend::Cuda => ("llamacpp", "NVIDIA GPU, CUDA offloading"),
        Backend::Rocm => ("llamacpp", "AMD GPU, ROCm offloading"),
        Backend::OneApi => ("llamacpp", "Intel GPU, SYCL offloading"),
        Backend::Cpu => ("llamacpp", "CPU host, quantized GGUF models"),
    }
}

/// What a probe found at one base URL.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    /// The base URL that answered.
    pub base: String,
    /// Engine id when the evidence names one, "openai-compatible" otherwise.
    pub id: String,
    /// Human name for the same.
    pub name: String,
    /// The models it serves right now. Empty is a real answer: an engine
    /// with nothing loaded is online and would refuse every job.
    pub models: Vec<String>,
}

/// Classify an endpoint from the evidence its answers carry.
///
/// `tags_answered` is whether `/api/tags` returned a model list (that API is
/// Ollama-native; nothing else serves it). `owned_by` is the set of values
/// from a `/v1/models` listing. Port hints are used ONLY when the evidence
/// says nothing, and the result then stays honest about being a guess.
pub fn classify(base: &str, tags_answered: bool, owned_by: &[String]) -> (String, String) {
    if tags_answered {
        return ("ollama".into(), "Ollama".into());
    }
    let owners: Vec<String> = owned_by.iter().map(|o| o.to_ascii_lowercase()).collect();
    let has = |needle: &str| owners.iter().any(|o| o.contains(needle));
    if has("vllm") {
        return ("vllm".into(), "vLLM".into());
    }
    if has("llamacpp") || has("llama.cpp") || has("llama-cpp") {
        return ("llamacpp".into(), "llama.cpp".into());
    }
    if has("organization_owner") || has("lmstudio") || has("lm studio") {
        return ("lmstudio".into(), "LM Studio".into());
    }
    if has("mlx") {
        return ("mlx".into(), "MLX".into());
    }
    if has("kmplify-fabric") {
        // Pointing a node at a fabric gateway would relay peers to peers;
        // name it so the wizard can refuse it as a local engine.
        return ("fabric".into(), "a KMPLIFY fabric gateway".into());
    }
    // No evidence: fall back to the port's usual tenants, saying it is a
    // guess by keeping the generic id. Plural on purpose — llama.cpp and MLX
    // share :8080, and naming only one would be a coin toss dressed up as
    // detection.
    let tenants: Vec<&str> = KNOWN
        .iter()
        .filter(|k| base.trim_end_matches('/') == k.default_base)
        .map(|k| k.name)
        .collect();
    if !tenants.is_empty() {
        return (
            "openai-compatible".into(),
            format!("OpenAI-compatible ({}?)", tenants.join(" or ")),
        );
    }
    ("openai-compatible".into(), "OpenAI-compatible".into())
}

/// Ask one base URL what it is and what it serves.
pub async fn probe(client: &reqwest::Client, base: &str) -> Option<Found> {
    let base = base.trim_end_matches('/').to_string();
    // Ollama first: /api/tags is unambiguous, and Ollama's /v1/models
    // answers too, so the order decides how well we can name it.
    let mut tags_answered = false;
    let mut models: Vec<String> = Vec::new();
    if let Ok(resp) = client.get(format!("{base}/api/tags")).send().await {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                if let Some(list) = v.get("models").and_then(|m| m.as_array()) {
                    tags_answered = true;
                    models = list
                        .iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                        .map(String::from)
                        .collect();
                }
            }
        }
    }
    let mut owned_by: Vec<String> = Vec::new();
    if !tags_answered {
        let resp = client.get(format!("{base}/v1/models")).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v = resp.json::<serde_json::Value>().await.ok()?;
        let list = v.get("data").and_then(|d| d.as_array())?;
        for m in list {
            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            }
            if let Some(o) = m.get("owned_by").and_then(|o| o.as_str()) {
                owned_by.push(o.to_string());
            }
        }
    }
    let (id, name) = classify(&base, tags_answered, &owned_by);
    Some(Found {
        base,
        id,
        name,
        models,
    })
}

/// Scan every known default port, concurrently.
///
/// A short per-probe timeout, because the common case is "nothing there" and
/// seven refused connections should feel instant, not like seven timeouts.
pub async fn scan() -> Vec<Found> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    // Each BASE once: llama.cpp and MLX share a default port, and probing
    // the same endpoint twice would list one server as two engines.
    let mut bases: Vec<&str> = KNOWN.iter().map(|k| k.default_base).collect();
    bases.dedup();
    let probes = bases.into_iter().map(|b| probe(&client, b));
    let results = futures_util::future::join_all(probes).await;
    let mut found: Vec<Found> = results.into_iter().flatten().collect();
    // Two entries answering with the same identity on different ports is one
    // engine mounted twice in our scan list (llama.cpp and vLLM both like
    // :8000/:8080 territory); keep the first sighting of each base only.
    found.dedup_by(|a, b| a.base == b.base);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recommendation_is_a_real_roster_engine() {
        use crate::gpu::Backend;
        for accel in [
            Backend::Cuda,
            Backend::Rocm,
            Backend::OneApi,
            Backend::Metal,
            Backend::Cpu,
        ] {
            let (id, why) = recommend(accel);
            assert!(
                known(id).is_some(),
                "recommend({accel:?}) names '{id}', which the roster does not know"
            );
            assert!(!why.is_empty(), "recommend({accel:?}) has no reason text");
        }
    }

    #[test]
    fn apple_silicon_gets_mlx_and_nvidia_gets_llamacpp() {
        // The two pairings the website promises by name; the rest are
        // covered by the roster check above.
        assert_eq!(recommend(crate::gpu::Backend::Metal).0, "mlx");
        assert_eq!(recommend(crate::gpu::Backend::Cuda).0, "llamacpp");
    }

    #[test]
    fn the_native_api_is_the_strongest_evidence() {
        let (id, name) = classify("http://127.0.0.1:9999", true, &[]);
        assert_eq!(id, "ollama");
        assert_eq!(name, "Ollama");
    }

    #[test]
    fn owned_by_names_the_engine() {
        for (owner, want) in [
            ("vllm", "vllm"),
            ("llamacpp", "llamacpp"),
            ("organization_owner", "lmstudio"),
        ] {
            let (id, _) = classify("http://127.0.0.1:9999", false, &[owner.to_string()]);
            assert_eq!(id, want, "owner {owner}");
        }
    }

    #[test]
    fn a_fabric_gateway_is_named_so_it_can_be_refused() {
        // Pointing OLLAMA_BASE at a fabric would relay peers to peers.
        let (id, name) = classify(
            "http://127.0.0.1:8100",
            false,
            &["kmplify-fabric".to_string()],
        );
        assert_eq!(id, "fabric");
        assert!(name.contains("fabric"));
    }

    #[test]
    fn no_evidence_stays_honest_about_guessing() {
        // On a known port the name is offered with a question mark…
        let (id, name) = classify("http://127.0.0.1:8000", false, &[]);
        assert_eq!(id, "openai-compatible");
        assert!(name.contains("vLLM?"), "{name}");
        // …a SHARED port names every usual tenant rather than a coin toss…
        let (_, name8080) = classify("http://127.0.0.1:8080", false, &[]);
        assert!(
            name8080.contains("llama.cpp") && name8080.contains("MLX"),
            "{name8080}"
        );
        let (id, _) = classify("http://127.0.0.1:8080", false, &["mlx".to_string()]);
        assert_eq!(id, "mlx", "evidence beats the port guess");
        // …and on an unknown one nothing is invented.
        let (id, name) = classify("http://10.0.0.7:9090", false, &[]);
        assert_eq!(id, "openai-compatible");
        assert_eq!(name, "OpenAI-compatible");
    }

    #[test]
    fn the_spellings_people_type_resolve() {
        for s in [
            "ollama",
            "llama.cpp",
            "llama-cpp",
            "llamacpp",
            "LM Studio",
            "lmstudio",
        ] {
            assert!(known(s).is_some(), "{s} should resolve");
        }
        assert!(known("gpt4").is_none());
        assert_eq!(known("llama-server").unwrap().id, "llamacpp");
    }

    #[test]
    fn every_known_engine_has_a_hint_and_a_localhost_base() {
        for k in KNOWN {
            assert!(k.default_base.starts_with("http://127.0.0.1:"), "{}", k.id);
            assert!(!k.hint.is_empty(), "{}", k.id);
        }
    }
}
