//! One serving node per node directory, with the real binary: while another
//! process holds the directory's serving lock, `run` is refused with its own
//! exit code and a clear message, and the commands that only read or change
//! settings keep working.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kmplify_node::node_dir::{ServeLock, LOCK_FILE};
use kmplify_node::startup::TRACE_FILE;

const BIN: &str = env!("CARGO_BIN_EXE_kmplify-node");
const EXIT_ALREADY_RUNNING: i32 = 4;

struct NodeDir(PathBuf);

impl NodeDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kmplify-node-single-{name}-{}-{}",
            std::process::id(),
            kmplify_node::status::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for NodeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("KMPLIFY_NODE_DIR", dir)
        // Nowhere: if the lock ever failed to refuse, the node would not
        // reach a real gateway from a test.
        .env("PROVIDER_GATEWAY_URL", "http://127.0.0.1:9")
        .env("KMPLIFY_NODE_UI_OPEN", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs")
}

#[test]
fn a_second_node_in_one_dir_is_refused_and_reads_still_work() {
    let dir = NodeDir::new("refuse");
    // The "first node": this test process holds the lock, exactly as a
    // serving kmplify-node does.
    let held = ServeLock::acquire(&dir.0).expect("lock a fresh dir");
    std::fs::write(dir.0.join(TRACE_FILE), "first node's trace\n").unwrap();

    let out = node(&dir.0, &["run", "--ui", "none"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(EXIT_ALREADY_RUNNING), "{err}");
    assert!(
        err.contains("another kmplify-node is already running in"),
        "{err}"
    );
    assert!(err.contains("KMPLIFY_NODE_DIR"), "{err}");
    // The running node's trace is kept, with the refusal added to it.
    let trace = std::fs::read_to_string(dir.0.join(TRACE_FILE)).unwrap();
    assert!(trace.starts_with("first node's trace\n"), "{trace}");
    assert!(trace.contains("a second start"), "{trace}");
    assert!(trace.contains("was refused"), "{trace}");

    // Read and settings commands next to the running node.
    let set = node(&dir.0, &["set", "router=local"]);
    assert!(
        set.status.success(),
        "set: {}",
        String::from_utf8_lossy(&set.stderr)
    );
    let status = node(&dir.0, &["status", "--json"]);
    assert_ne!(
        status.status.code(),
        Some(EXIT_ALREADY_RUNNING),
        "status: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let link = node(&dir.0, &["link-code", "--json"]);
    assert_ne!(link.status.code(), Some(EXIT_ALREADY_RUNNING));

    drop(held);
    assert!(dir.0.join(LOCK_FILE).exists());
}

#[test]
fn the_lock_is_free_again_when_its_holder_is_gone() {
    let dir = NodeDir::new("free");
    drop(ServeLock::acquire(&dir.0).unwrap());
    // A refused `run` holds nothing either.
    let _ = ServeLock::acquire(&dir.0).expect("free after the holder dropped it");
}
