//! Run a catalog module in the node's own sandbox, from a shell.
//!
//!   cargo run --example run_function -- functions/dist/html-to-text.wasm < page.html
//!   cargo run --example run_function -- functions/dist/json-query.wasm '.items[].name' < data.json
//!
//! Same runtime, same WASI surface and the same limits a provider's node
//! applies — not an approximation of them. Anyone writing a function for this
//! catalog should be able to see it fail here before a peer ever sees it.
//!
//! `--memory-mb` and `--timeout-ms` mirror the manifest fields; the defaults
//! are the catalog's own.
use std::io::{Read, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: run_function <module.wasm> [args…] [--memory-mb N] [--timeout-ms N]");
        std::process::exit(2);
    };
    let mut limits = kmplify_node::functions::Limits {
        memory_mb: 64,
        timeout_ms: 15_000,
    };
    let mut passed: Vec<String> = Vec::new();
    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--memory-mb" => {
                limits.memory_mb = rest.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(64);
                i += 2;
            }
            "--timeout-ms" => {
                limits.timeout_ms = rest
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(15_000);
                i += 2;
            }
            other => {
                passed.push(other.to_string());
                i += 1;
            }
        }
    }

    let module = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2);
    });
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);

    match kmplify_node::functions::run_module(&module, &input, &passed, limits) {
        Ok(result) => {
            let _ = std::io::stdout().write_all(&result.stdout);
            if !result.stderr.is_empty() {
                let _ = std::io::stderr().write_all(&result.stderr);
            }
            eprintln!(
                "[{} exit {} in {} ms]",
                path, result.exit_code, result.duration_ms
            );
            std::process::exit(result.exit_code);
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    }
}
