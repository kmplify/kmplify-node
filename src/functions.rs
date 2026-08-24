//! Signed Wasm functions: the serverless-function lane (protocol v3.0).
//!
//! The gateway asks this node to run a short-lived WebAssembly module on
//! behalf of a consumer. What makes that acceptable on a stranger's machine
//! is decided HERE, not on the gateway:
//!
//! - Only a module whose manifest (id + sha256 + limits) is signed by the
//!   function key this node was configured to trust may run. No key, no
//!   functions: the lane is fail-closed.
//! - The bytes are downloaded from the node's own gateway and re-hashed;
//!   the signature covers the hash, so a swapped module is refused before
//!   it is compiled.
//! - The sandbox is WASI preview 1 with stdin/stdout/stderr only. No
//!   filesystem, no network, no environment, no clock beyond what WASI
//!   grants. Memory, fuel and wall-clock are bounded by the manifest AND
//!   clamped to the operator's ceilings.
//!
//! The runtime itself (wasmtime) is an optional feature, `wasm`, because
//! it is a large dependency most providers never need. Without it the node
//! still understands the protocol and answers every function job with a
//! clear "no runtime compiled in", so a gateway learns immediately rather
//! than timing out.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
#[cfg(feature = "wasm")]
use std::time::{Duration, Instant};

/// Ceilings the operator cannot raise past; the manifest asks, these cap.
pub const HARD_MAX_MEMORY_MB: u64 = 1024;
pub const HARD_MAX_TIMEOUT_MS: u64 = 300_000;
/// Largest module this node will download, decoded.
pub const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;
/// Largest stdout/stderr captured; more is truncated, never unbounded.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const STDERR_TAIL_CHARS: usize = 900;

/// What this node offers, as the hello frame's `functions` block.
#[derive(Clone, Debug, Default)]
pub struct FunctionsConfig {
    /// Host functions at all. Off by default; running code for strangers is
    /// a separate grant from answering chat requests.
    pub enabled: bool,
    /// Hex Ed25519 public key of the catalog this node trusts. Empty means
    /// trust nothing, which means refuse everything.
    pub trusted_pubkey: String,
    /// Operator ceiling on per-function memory, MB.
    pub max_memory_mb: u64,
    /// Operator ceiling on per-function wall-clock, ms.
    pub max_ms: u64,
}

impl FunctionsConfig {
    pub fn capability(&self) -> Value {
        if !self.enabled || self.trusted_pubkey.is_empty() {
            return Value::Null;
        }
        json!({
            "enabled": runtime_available(),
            "runtime": if runtime_available() { "wasmtime" } else { "none" },
            "max_memory_mb": self.max_memory_mb.clamp(1, HARD_MAX_MEMORY_MB),
            "max_ms": self.max_ms.clamp(100, HARD_MAX_TIMEOUT_MS),
            "pubkey": self.trusted_pubkey.to_ascii_lowercase(),
        })
    }
}

/// Is a Wasm runtime compiled into this build?
pub fn runtime_available() -> bool {
    cfg!(feature = "wasm")
}

/// The manifest a job carries, as this node reads it.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub sha256: String,
    pub signature: String,
    pub memory_mb: u64,
    pub timeout_ms: u64,
    pub module_url: String,
}

impl Manifest {
    pub fn from_frame(v: &Value) -> Result<Self, String> {
        let f = v
            .get("function")
            .ok_or("job carries no function manifest")?;
        let s = |k: &str| {
            f.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let m = Manifest {
            id: s("id"),
            sha256: s("sha256").to_ascii_lowercase(),
            signature: s("signature").to_ascii_lowercase(),
            memory_mb: f.get("memory_mb").and_then(Value::as_u64).unwrap_or(0),
            timeout_ms: f.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0),
            module_url: s("module_url"),
        };
        if m.id.is_empty()
            || m.sha256.len() != 64
            || m.signature.is_empty()
            || m.module_url.is_empty()
        {
            return Err("malformed function manifest".into());
        }
        if m.memory_mb == 0 || m.timeout_ms == 0 {
            return Err("function manifest declares no limits".into());
        }
        Ok(m)
    }

    /// The exact bytes the gateway signed. Field order and spacing are the
    /// canonical form (sorted keys, no whitespace); a difference of one
    /// byte here is a signature that never verifies, so this is pinned by a
    /// known-answer test against the gateway's canonical_manifest().
    pub fn canonical(&self) -> Vec<u8> {
        format!(
            "{{\"id\":{},\"memory_mb\":{},\"sha256\":{},\"timeout_ms\":{}}}",
            json!(self.id),
            self.memory_mb,
            json!(self.sha256),
            self.timeout_ms
        )
        .into_bytes()
    }
}

/// Does `trusted_pubkey_hex` vouch for this manifest?
pub fn verify_manifest(m: &Manifest, trusted_pubkey_hex: &str) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key_bytes = hex_decode(trusted_pubkey_hex).ok_or("trusted function key is not hex")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "trusted function key must be 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| format!("trusted function key invalid: {e}"))?;
    let sig_bytes = hex_decode(&m.signature).ok_or("manifest signature is not hex")?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|e| format!("manifest signature malformed: {e}"))?;
    key.verify(&m.canonical(), &sig).map_err(|_| {
        "manifest signature does not verify under the trusted function key".to_string()
    })
}

/// Is this a URL the node will download a module from? Its own gateway,
/// over https (or http for a loopback/LAN gateway in development), nothing
/// else: the signature makes the bytes trustworthy, the origin rule keeps
/// the node from being pointed at arbitrary hosts by a job frame.
pub fn module_url_ok(url: &str, gateway_url: &str) -> bool {
    let gw = gateway_url.trim_end_matches('/');
    if gw.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
        return false;
    }
    url.starts_with(&format!("{gw}/"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && !url[url.find("//").map(|i| i + 2).unwrap_or(0)..]
            .split('/')
            .next()
            .unwrap_or("")
            .contains('@')
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where verified modules are cached, by hash. A module is immutable by
/// construction (its name IS its hash), so the cache never needs
/// invalidation; a manifest with a new hash is simply a different file.
pub fn cache_path(node_dir: &Path, sha256: &str) -> PathBuf {
    node_dir.join("functions").join(format!("{sha256}.wasm"))
}

/// Fetch the module bytes for `m`, from cache or from the gateway, and
/// prove they are the bytes the signature covers.
pub async fn fetch_module(
    client: &reqwest::Client,
    node_dir: &Path,
    gateway_url: &str,
    m: &Manifest,
) -> Result<Vec<u8>, String> {
    let path = cache_path(node_dir, &m.sha256);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if sha256_hex(&bytes) == m.sha256 {
            return Ok(bytes);
        }
        // A cache entry that does not hash to its own name is corruption;
        // remove it and fetch fresh rather than trusting the file name.
        let _ = tokio::fs::remove_file(&path).await;
    }
    if !module_url_ok(&m.module_url, gateway_url) {
        return Err(format!(
            "refused: module URL {:?} is not on this node's gateway ({gateway_url})",
            m.module_url
        ));
    }
    let resp = client
        .get(&m.module_url)
        .send()
        .await
        .map_err(|e| format!("module download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("module download failed: http {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_MODULE_BYTES {
            return Err(format!(
                "module is {len} bytes, above the {MAX_MODULE_BYTES} byte ceiling"
            ));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("module download failed: {e}"))?;
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(format!(
            "module is {} bytes, above the {MAX_MODULE_BYTES} byte ceiling",
            bytes.len()
        ));
    }
    let got = sha256_hex(&bytes);
    if got != m.sha256 {
        return Err(format!(
            "module hash mismatch: manifest says {}, bytes are {got}",
            m.sha256
        ));
    }
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(&path, &bytes).await;
    Ok(bytes.to_vec())
}

/// The outcome of one run, as the `done` frame's data.
#[derive(Clone, Debug, Default)]
pub struct RunResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
}

impl RunResult {
    pub fn to_value(&self) -> Value {
        let tail: String = String::from_utf8_lossy(&self.stderr)
            .chars()
            .rev()
            .take(STDERR_TAIL_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        json!({
            "exit_code": self.exit_code,
            "stdout_b64": b64(&self.stdout),
            "stderr_tail": tail,
            "duration_ms": self.duration_ms,
        })
    }
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Limits a run actually gets: the manifest's request clamped to the
/// operator's ceilings and the hard caps, the same posture as session
/// memory and CPUs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub memory_mb: u64,
    pub timeout_ms: u64,
}

pub fn effective_limits(m: &Manifest, cfg: &FunctionsConfig) -> Limits {
    Limits {
        memory_mb: m
            .memory_mb
            .min(cfg.max_memory_mb.max(1))
            .clamp(1, HARD_MAX_MEMORY_MB),
        timeout_ms: m
            .timeout_ms
            .min(cfg.max_ms.max(100))
            .clamp(100, HARD_MAX_TIMEOUT_MS),
    }
}

/// Run `module` with `input` on stdin and `args` as argv, sandboxed.
///
/// Blocking on purpose (wasmtime's WASI p1 sync API); callers spawn it on
/// the blocking pool so the node's socket loop never waits on a guest.
#[cfg(feature = "wasm")]
pub fn run_module(
    module: &[u8],
    input: &[u8],
    args: &[String],
    limits: Limits,
) -> Result<RunResult, String> {
    use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
    use wasmtime_wasi::WasiCtxBuilder;

    struct State {
        wasi: WasiP1Ctx,
        limits: StoreLimits,
    }

    let mut config = Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);
    // No virtual-memory reservation ahead of the guest's own linear memory:
    // the guest gets what it grows into and no window onto anything else.
    // (`static_memory_maximum_size` before wasmtime 29.)
    config.memory_reservation(0);
    let engine = Engine::new(&config).map_err(|e| format!("wasm engine: {e}"))?;
    let module =
        Module::new(&engine, module).map_err(|e| format!("module does not compile: {e}"))?;

    let mut linker: Linker<State> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |s: &mut State| &mut s.wasi)
        .map_err(|e| format!("wasi linker: {e}"))?;

    let stdout = MemoryOutputPipe::new(MAX_OUTPUT_BYTES);
    let stderr = MemoryOutputPipe::new(MAX_OUTPUT_BYTES);
    let mut argv: Vec<String> = vec!["function".to_string()];
    argv.extend(args.iter().cloned());
    let wasi = WasiCtxBuilder::new()
        .stdin(MemoryInputPipe::new(input.to_vec()))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .args(&argv)
        .build_p1();
    let state = State {
        wasi,
        limits: StoreLimitsBuilder::new()
            .memory_size((limits.memory_mb as usize) * 1024 * 1024)
            .instances(1)
            .memories(1)
            .tables(4)
            .trap_on_grow_failure(true)
            .build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|s| &mut s.limits);
    // Fuel bounds instructions; the epoch bounds wall-clock. Both, because a
    // guest blocked in a host call burns no fuel, and a guest spinning burns
    // fuel far faster than the clock moves.
    store
        .set_fuel(u64::MAX / 2)
        .map_err(|e| format!("fuel: {e}"))?;
    store.set_epoch_deadline(1);
    let engine_for_timer = engine.clone();
    let timeout = Duration::from_millis(limits.timeout_ms);
    let started = Instant::now();
    let timer = std::thread::spawn(move || {
        std::thread::sleep(timeout);
        engine_for_timer.increment_epoch();
    });

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| format!("instantiate: {e}"))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|_| "module exports no _start (not a WASI command)".to_string())?;

    let outcome = start.call(&mut store, ());
    let duration_ms = started.elapsed().as_millis() as u64;
    drop(store);
    let _ = timer.join();

    let exit_code = match outcome {
        Ok(()) => 0,
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                exit.0
            } else if duration_ms >= limits.timeout_ms {
                return Err(format!(
                    "function exceeded its {} ms limit",
                    limits.timeout_ms
                ));
            } else {
                return Err(format!("function trapped: {e}"));
            }
        }
    };
    Ok(RunResult {
        exit_code,
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
        duration_ms,
    })
}

#[cfg(not(feature = "wasm"))]
pub fn run_module(
    _module: &[u8],
    _input: &[u8],
    _args: &[String],
    _limits: Limits,
) -> Result<RunResult, String> {
    Err(
        "this kmplify-node build has no Wasm runtime: it was built with \
         --no-default-features. Rebuild with --features wasm."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "wasm")]
    use std::time::{Duration, Instant};

    fn manifest() -> Manifest {
        Manifest {
            id: "echo".into(),
            sha256: "a".repeat(64),
            signature: "00".repeat(64),
            memory_mb: 16,
            timeout_ms: 5000,
            module_url: "https://fabric.example/v1/functions/echo/module".into(),
        }
    }

    /// Known answer, shared with the gateway's canonical_manifest(): sorted
    /// keys, no whitespace. One byte of drift means nothing ever verifies.
    #[test]
    fn canonical_form_matches_the_gateway() {
        let m = manifest();
        assert_eq!(
            String::from_utf8(m.canonical()).unwrap(),
            format!(
                "{{\"id\":\"echo\",\"memory_mb\":16,\"sha256\":\"{}\",\"timeout_ms\":5000}}",
                "a".repeat(64)
            )
        );
    }

    #[test]
    fn a_signature_under_the_trusted_key_verifies_and_any_change_does_not() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pub_hex: String = key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut m = manifest();
        m.signature = key
            .sign(&m.canonical())
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(verify_manifest(&m, &pub_hex).is_ok());
        // Tampered limits: the signature no longer covers them.
        let mut bigger = m.clone();
        bigger.memory_mb = 512;
        assert!(verify_manifest(&bigger, &pub_hex).is_err());
        // Different hash: different code.
        let mut other = m.clone();
        other.sha256 = "b".repeat(64);
        assert!(verify_manifest(&other, &pub_hex).is_err());
        // Wrong key: a gateway this node never agreed to trust.
        let other_key = SigningKey::from_bytes(&[8u8; 32]);
        let other_hex: String = other_key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(verify_manifest(&m, &other_hex).is_err());
        assert!(
            verify_manifest(&m, "").is_err(),
            "no trusted key means refuse"
        );
    }

    #[test]
    fn modules_are_downloaded_only_from_the_nodes_own_gateway() {
        let gw = "https://fabric.kmplify.io";
        assert!(module_url_ok(
            "https://fabric.kmplify.io/v1/functions/echo/module",
            gw
        ));
        assert!(!module_url_ok(
            "https://evil.example/v1/functions/echo/module",
            gw
        ));
        assert!(!module_url_ok(
            "https://fabric.kmplify.io.evil.example/m",
            gw
        ));
        assert!(!module_url_ok(
            "https://fabric.kmplify.io@evil.example/m",
            gw
        ));
        assert!(!module_url_ok("https://fabric.kmplify.io/m\n", gw));
        assert!(module_url_ok(
            "http://127.0.0.1:18100/v1/functions/echo/module",
            "http://127.0.0.1:18100"
        ));
        assert!(!module_url_ok("ftp://fabric.kmplify.io/m", gw));
    }

    #[test]
    fn limits_are_the_manifest_clamped_to_the_operator() {
        let cfg = FunctionsConfig {
            enabled: true,
            trusted_pubkey: "aa".into(),
            max_memory_mb: 64,
            max_ms: 10_000,
        };
        let mut m = manifest();
        assert_eq!(
            effective_limits(&m, &cfg),
            Limits {
                memory_mb: 16,
                timeout_ms: 5000
            }
        );
        m.memory_mb = 4096;
        m.timeout_ms = 999_999;
        assert_eq!(
            effective_limits(&m, &cfg),
            Limits {
                memory_mb: 64,
                timeout_ms: 10_000
            }
        );
        let wide = FunctionsConfig {
            max_memory_mb: 1 << 20,
            max_ms: 1 << 40,
            ..cfg
        };
        assert_eq!(
            effective_limits(&m, &wide),
            Limits {
                memory_mb: HARD_MAX_MEMORY_MB,
                timeout_ms: HARD_MAX_TIMEOUT_MS
            }
        );
    }

    #[test]
    fn the_capability_is_absent_without_a_trusted_key() {
        let off = FunctionsConfig {
            enabled: true,
            trusted_pubkey: String::new(),
            max_memory_mb: 64,
            max_ms: 1000,
        };
        assert!(off.capability().is_null());
        let on = FunctionsConfig {
            trusted_pubkey: "AbCd".into(),
            ..off
        };
        let cap = on.capability();
        assert_eq!(cap["pubkey"], "abcd");
        assert_eq!(cap["enabled"], json!(runtime_available()));
    }

    #[test]
    fn malformed_manifests_are_refused() {
        assert!(Manifest::from_frame(&json!({})).is_err());
        assert!(Manifest::from_frame(&json!({"function": {"id": "x"}})).is_err());
        let ok = json!({"function": {"id": "echo", "sha256": "a".repeat(64), "signature": "00", "memory_mb": 16, "timeout_ms": 1000, "module_url": "https://g/x"}});
        assert!(Manifest::from_frame(&ok).is_ok());
        let mut no_limits = ok.clone();
        no_limits["function"]["memory_mb"] = json!(0);
        assert!(Manifest::from_frame(&no_limits).is_err());
    }

    #[test]
    fn sha256_and_hex_helpers() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex_decode("00ff"), Some(vec![0, 255]));
        assert_eq!(hex_decode("0g"), None);
        assert_eq!(hex_decode("abc"), None);
    }

    /// A real WASI command module, assembled from text so the test owns its
    /// bytes: reads stdin, writes it to stdout, exits 0. Only meaningful with
    /// the runtime compiled in; without it the lane reports that honestly.
    #[cfg(feature = "wasm")]
    #[test]
    fn a_signed_echo_module_runs_in_the_sandbox() {
        let wasm = wat::parse_str(ECHO_WAT).expect("echo module assembles");
        let limits = Limits {
            memory_mb: 16,
            timeout_ms: 5000,
        };
        let out = run_module(&wasm, b"hello fabric", &[], limits).expect("echo runs");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, b"hello fabric");
        assert!(out.duration_ms < 5000);
    }

    #[cfg(feature = "wasm")]
    #[test]
    fn a_spinning_module_is_stopped_by_the_clock() {
        let wasm = wat::parse_str(SPIN_WAT).expect("spin module assembles");
        let limits = Limits {
            memory_mb: 16,
            timeout_ms: 300,
        };
        let started = Instant::now();
        let err = run_module(&wasm, b"", &[], limits).expect_err("must not return");
        assert!(err.contains("limit") || err.contains("trapped"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[cfg(not(feature = "wasm"))]
    #[test]
    fn without_the_runtime_the_lane_says_so() {
        let err = run_module(
            b"",
            b"",
            &[],
            Limits {
                memory_mb: 1,
                timeout_ms: 100,
            },
        )
        .unwrap_err();
        assert!(err.contains("no Wasm runtime"));
        assert!(!runtime_available());
    }

    /// WASI p1 echo: fd_read(0) into a buffer, fd_write(1) what was read,
    /// until fd_read reports zero bytes.
    #[cfg(feature = "wasm")]
    pub const ECHO_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read"  (func $fd_read  (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; iovec at 0: buf=64, len=4096 ; nread at 16 ; nwritten at 20
  (func (export "_start")
    (local $n i32)
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 4096))
    (block $done
      (loop $again
        (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)))
        (local.set $n (i32.load (i32.const 16)))
        (br_if $done (i32.eqz (local.get $n)))
        ;; write exactly $n bytes from buf
        (i32.store (i32.const 32) (i32.const 64))
        (i32.store (i32.const 36) (local.get $n))
        (drop (call $fd_write (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 20)))
        (br $again))))
)"#;

    #[cfg(feature = "wasm")]
    const SPIN_WAT: &str = r#"
(module
  (memory 1)
  (func (export "_start")
    (loop $forever (br $forever)))
)"#;
}
