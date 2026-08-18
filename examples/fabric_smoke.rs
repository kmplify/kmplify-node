//! Standalone smoke test for the native fabric worker (no Tauri app needed).
//!
//! Run: cargo run --example fabric_smoke -- [gateway_url] [templates] [seconds]
//!
//!   gateway_url   default http://127.0.0.1:18100
//!   templates     comma-separated workload template ids this node offers
//!                 (e.g. "echo-test"); empty/omitted = inference-only node.
//!                 Offering templates makes this a real session host, which
//!                 is how the v2.1 pull-progress and streamed-relay paths get
//!                 exercised headlessly: schedule echo-test at the gateway
//!                 and watch workload_status frames carry `progress`.
//!   seconds       how long to stay connected (default 15)
use kmplify_node::fabric_worker::{default_creds_path, run, WorkerConfig};

#[tokio::main]
async fn main() {
    let gateway_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:18100".to_string());
    let workload_templates: Vec<String> = std::env::args()
        .nth(2)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    let seconds: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    // Only what this harness actually cares about; everything else comes from
    // WorkerConfig::default(). That trailing `..` is load-bearing: naming
    // every field meant each new one (three so far — the cpu, vram and disk
    // ceilings) broke this example and with it `cargo test` on the branch.
    let cfg = WorkerConfig {
        gateway_url,
        creds_path: default_creds_path(&std::env::temp_dir()),
        // Only what the caller opted into — and never a GPU claim: this
        // harness runs anywhere, so only the CPU template (echo-test) can
        // genuinely be honored.
        workload_templates,
        // Defaults cover the rest: country stays empty so the gateway records
        // "XX" and keeps this throwaway node out of EU-only scheduling; no
        // operator ceilings; cuda false; no event sink, since a headless
        // smoke test has no UI to notify.
        ..Default::default()
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(run(cfg, stop_rx));
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
    let _ = stop_tx.send(true);
    let _ = task.await;
    println!("[smoke] stopped cleanly");
}
