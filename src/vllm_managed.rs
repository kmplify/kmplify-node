//! vLLM started and stopped by this node (`set vllm-model=<hf id>`).
//!
//! The twin of kmplify-desktop's native/vllm.rs: same pinned images, same
//! tool-call flags, loopback port only. When a model is configured, `run`
//! starts the container before joining the fabric and lends it as the
//! node's engine; when the node stops, the container goes with it ("a
//! stopped node must leave nothing running on the owner's GPU"). If vLLM
//! does not come up, the node keeps its previous engine (Ollama or whatever
//! `engine` points at), so it still lends something.
//!
//! No API key, unlike the desktop: the port is loopback-only on a machine
//! that lends its GPU, the same exposure as the Ollama this node already
//! lends without one. The weights live in a volume OUTSIDE the
//! `kmplify-fabric-` namespace, so no consumer session can ever mount it.

use std::time::{Duration, Instant};

use crate::gpu::Backend;

pub const CONTAINER: &str = "kmplify-node-vllm";
pub const PORT: u16 = 18300;
pub const HF_VOLUME: &str = "kmplify-node-vllm-hf";
pub const DEFAULT_MAX_MODEL_LEN: u32 = 65_536;
/// First start: image pull plus weights download plus load.
pub const START_TIMEOUT: Duration = Duration::from_secs(1800);
pub const IMAGE_CUDA: &str =
    "vllm/vllm-openai:v0.32.0@sha256:befccf0e7e4089aa23926bb6f887a85c0d46c2a99e6de5c5ffdf5cb7957c5b34";
pub const IMAGE_ROCM: &str = "rocm/vllm:rocm10.1.0_ubuntu24.04_py3.14-pytorch_2.13.0_vllm-0.29.0@sha256:e0ea9a10b729573ea8598bdc073bda0ea382fe1037eec7c0761b6fa7f46baaf4";

/// A Hugging Face repo id; refuses anything that could be read as an option
/// or a path, because it ends up as a `docker run` argument.
pub fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 200
        && !model.starts_with('-')
        && !model.starts_with('/')
        && !model.contains("..")
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
        && model.matches('/').count() <= 1
}

/// vLLM refuses `tool_choice: auto` (every agent run) without a parser for
/// the model family's tool-call format.
pub fn tool_call_parser(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.contains("mistral") || m.contains("ministral") || m.contains("devstral") {
        "mistral"
    } else if m.contains("llama-3") || m.contains("llama3") {
        "llama3_json"
    } else if m.contains("granite") {
        "granite"
    } else {
        "hermes"
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// `docker run` arguments (detached; the node stops it on exit).
pub fn run_args(backend: Backend, model: &str, max_model_len: u32) -> Result<Vec<String>, String> {
    let (gpu, image): (Vec<String>, &str) = match backend {
        Backend::Cuda => (s(&["--gpus", "all"]), IMAGE_CUDA),
        Backend::Rocm => (
            s(&[
                "--device",
                "/dev/kfd",
                "--device",
                "/dev/dri",
                "--group-add",
                "video",
            ]),
            IMAGE_ROCM,
        ),
        other => {
            return Err(format!(
                "vLLM needs an NVIDIA (CUDA) or AMD (ROCm) GPU; this node has {other:?}"
            ))
        }
    };
    if !valid_model_id(model) {
        return Err(format!("{model:?} is not a Hugging Face model id"));
    }
    let mut a = s(&[
        "run",
        "-d",
        "--name",
        CONTAINER,
        "--label",
        "kmplify.node.vllm=1",
        "--ipc",
        "host",
    ]);
    a.extend(gpu);
    a.extend(s(&["-p"]));
    a.push(format!("127.0.0.1:{PORT}:8000"));
    a.extend(s(&["-v"]));
    a.push(format!("{HF_VOLUME}:/root/.cache/huggingface"));
    a.extend(s(&["-e", "VLLM_NO_USAGE_STATS=1", "-e", "DO_NOT_TRACK=1"]));
    a.push(image.to_string());
    a.extend(s(&[
        "--model",
        model,
        "--served-model-name",
        model,
        "--max-model-len",
    ]));
    a.push(max_model_len.to_string());
    a.extend(s(&[
        "--gpu-memory-utilization",
        "0.90",
        "--enable-auto-tool-choice",
        "--tool-call-parser",
    ]));
    a.push(tool_call_parser(model).to_string());
    Ok(a)
}

pub fn base_url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

pub async fn remove() {
    let _ = crate::proc::command("docker")
        .args(["rm", "-f", CONTAINER])
        .output()
        .await;
}

/// Start vLLM for `model` and wait until it answers. Returns the base URL
/// to lend (the node's `ollama_base` for this run).
pub async fn start(backend: Backend, model: &str, max_model_len: u32) -> Result<String, String> {
    let args = run_args(backend, model, max_model_len)?;
    // Never two: a container left by a killed node holds the GPU and port.
    remove().await;
    let out = crate::proc::command("docker")
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("docker is not usable: {e}"))?;
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr)
            .lines()
            .last()
            .unwrap_or_default()
            .to_string();
        return Err(format!("docker run failed: {tail}"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let health = format!("{}/health", base_url());
    let began = Instant::now();
    loop {
        if let Ok(r) = client.get(&health).send().await {
            if r.status().is_success() {
                return Ok(base_url());
            }
        }
        // Died (bad model id, out of memory): say so now, not in 30 minutes.
        let running = crate::proc::command("docker")
            .args(["inspect", "-f", "{{.State.Running}}", CONTAINER])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false);
        if !running {
            let logs = crate::proc::command("docker")
                .args(["logs", "--tail", "3", CONTAINER])
                .output()
                .await;
            let tail = logs
                .map(|o| {
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .last()
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_default();
            remove().await;
            return Err(format!("vLLM stopped while starting: {tail}"));
        }
        if began.elapsed() > START_TIMEOUT {
            remove().await;
            return Err("vLLM did not answer within 30 minutes".into());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// {model id: max_model_len} from an OpenAI `/v1/models` body, for the
/// servers that publish it (vLLM). The fabric uses it to budget prompts.
pub fn contexts_from_models(body: &serde_json::Value) -> std::collections::HashMap<String, u64> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?;
                    let len = m.get("max_model_len")?.as_u64()?;
                    (!id.is_empty() && len > 0).then(|| (id.to_string(), len))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_and_rocm_get_their_devices_and_pinned_images() {
        let a = run_args(Backend::Cuda, "Qwen/Qwen3-8B", 65536).unwrap();
        assert!(a.windows(2).any(|w| w[0] == "--gpus" && w[1] == "all"));
        assert!(a.contains(&IMAGE_CUDA.to_string()) && IMAGE_CUDA.contains("@sha256:"));
        let r = run_args(Backend::Rocm, "Qwen/Qwen3-8B", 65536).unwrap();
        assert!(r
            .windows(2)
            .any(|w| w[0] == "--device" && w[1] == "/dev/kfd"));
        assert!(r.contains(&IMAGE_ROCM.to_string()));
        assert!(run_args(Backend::Metal, "Qwen/Qwen3-8B", 65536).is_err());
        assert!(run_args(Backend::Cpu, "Qwen/Qwen3-8B", 65536).is_err());
    }

    #[test]
    fn loopback_only_tool_calls_on_and_weights_outside_the_fabric_namespace() {
        let a = run_args(Backend::Cuda, "ibm-granite/granite-3.3-2b-instruct", 65536).unwrap();
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-p" && w[1] == format!("127.0.0.1:{PORT}:8000")));
        assert!(a.contains(&"--enable-auto-tool-choice".to_string()));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--tool-call-parser" && w[1] == "granite"));
        assert!(
            !HF_VOLUME.starts_with("kmplify-fabric-"),
            "consumer sessions may mount kmplify-fabric-* volumes"
        );
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-e" && w[1] == "VLLM_NO_USAGE_STATS=1"));
    }

    #[test]
    fn bad_model_ids_are_refused() {
        for bad in [
            "",
            "--trust-remote-code",
            "/etc/x",
            "a/../b",
            "a/b/c",
            "a b",
        ] {
            assert!(run_args(Backend::Cuda, bad, 4096).is_err(), "{bad}");
        }
    }

    #[test]
    fn contexts_are_read_from_a_vllm_model_list() {
        let body = serde_json::json!({"data": [
            {"id": "q", "owned_by": "vllm", "max_model_len": 65536},
            {"id": "x", "owned_by": "library"}
        ]});
        let c = contexts_from_models(&body);
        assert_eq!(c.get("q"), Some(&65536));
        assert!(!c.contains_key("x"));
    }
}
