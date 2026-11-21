//! KMPLIFY Credits as an operator sees them: the real binary, a status file
//! on disk, and a small local fake of the gateway's node endpoint.
//!
//! What these hold: with the credit system off at the gateway, or with a
//! gateway that does not know it, the binary prints exactly what it prints
//! for a node that never heard of credits. With it on, `status` and `rewards`
//! show what the gateway reported, and an unlinked node is told how to link.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use kmplify_node::credits::Report;
use kmplify_node::status::{self, Link, Snapshot};

const BIN: &str = env!("CARGO_BIN_EXE_kmplify-node");

/// A node directory of this test's own, removed when the test ends.
struct NodeDir(PathBuf);

impl NodeDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kmplify-node-cli-{name}-{}-{}",
            std::process::id(),
            status::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Publish a status file as a running node would have a moment ago.
    fn publish(&self, credits: Option<Report>) {
        let now = status::now_ms();
        let snap = Snapshot {
            schema: 1,
            version: "0.7.0".into(),
            pid: 4242,
            node_id: "44802ebc90e24de18825b90af320edfe".into(),
            gateway: "https://fabric.example.test".into(),
            link: Link::Online,
            // Two hours and half a minute: the printed uptime cannot tick
            // over between the two runs a test compares.
            started_at_ms: now - (2 * 3600 + 30) * 1000,
            connected_at_ms: now - 60_000,
            published_at_ms: now,
            models: vec!["qwen2.5:7b".into()],
            credits,
            ..Default::default()
        };
        std::fs::write(
            status::status_path(&self.0),
            serde_json::to_vec(&snap).unwrap(),
        )
        .unwrap();
    }

    /// A stored credential, so `rewards` has a token to ask with. The seed
    /// is a fixed test value; nothing real is behind it.
    fn register(&self) {
        std::fs::write(
            self.0.join("fabric_node.json"),
            format!(
                r#"{{"node_id":"44802ebc90e24de18825b90af320edfe","token":"test-node-token","seed_hex":"{}"}}"#,
                "07".repeat(32)
            ),
        )
        .unwrap();
    }
}

impl Drop for NodeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(args)
        .arg("--node-dir")
        .arg(dir)
        .env_remove("CHAINGENCE_PLUGIN")
        .env_remove("PROVIDER_GATEWAY_URL")
        .output()
        .expect("the binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// The fake: answers every request with one status and body, and records the
/// request heads. Returns its URL.
fn fake_gateway(status: u16, body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if sock.read_exact(&mut byte).is_err() {
                    break;
                }
                head.push(byte[0]);
            }
            let _ = tx.send(String::from_utf8_lossy(&head).to_string());
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

const LINKED: &str = r#"{"enabled":true,"linked":true,"gpu_class":"m","vram_gb":24,
    "credits_per_minute":6,"platform_fee_percent":15,
    "earned_this_month":1240,"earned_total":5678,"held":0,
    "history":[{"at":"2026-11-21T20:14:03Z","kind":"earning","credits":153,
                "session_id":"7f3a9c1e22aa","gpu_class":"m","seconds":1800,"fee_credits":27}]}"#;

const UNLINKED: &str = r#"{"enabled":true,"linked":false,"gpu_class":"l","vram_gb":48,
    "credits_per_minute":10,"platform_fee_percent":15,
    "earned_this_month":0,"earned_total":0,"held":420}"#;

fn report(body: &str) -> Report {
    serde_json::from_str(body).unwrap()
}

#[test]
fn status_shows_earnings_and_is_unchanged_without_them() {
    let dir = NodeDir::new("status");

    dir.publish(None);
    let (code, plain) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "{plain}");
    for word in ["credits", "account", "rate  ", "NOT LINKED"] {
        assert!(!plain.contains(word), "`{word}` without a report:\n{plain}");
    }

    dir.publish(Some(report(LINKED)));
    let (code, with) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "{with}");
    // Everything a node without credits prints, untouched, and three lines
    // after it.
    let added = with.strip_prefix(plain.as_str()).unwrap_or_else(|| {
        panic!("the existing output changed:\n--- before\n{plain}--- after\n{with}")
    });
    assert_eq!(
        added,
        "  credits  : 1240 earned this month · 5678 in total\n\
         \x20 rate     : class M (24 GB) · 6 credits per minute · platform fee 15 % · about 5.1 per minute to this node\n\
         \x20 account  : linked\n"
    );
}

#[test]
fn status_tells_an_unlinked_node_that_earnings_wait() {
    let dir = NodeDir::new("unlinked");
    dir.publish(Some(report(UNLINKED)));
    let (code, out) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "  account  : NOT LINKED: 420 credits wait until it is (`kmplify-node rewards` says how)"
        ),
        "{out}"
    );
    assert!(
        out.contains("class L (48 GB) · 10 credits per minute"),
        "{out}"
    );
}

#[test]
fn status_json_has_the_block_only_when_the_gateway_reported_one() {
    let dir = NodeDir::new("json");

    dir.publish(None);
    let (_, out) = run(dir.path(), &["status", "--json"]);
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["running"], true);
    assert!(body.get("credits").is_none(), "{out}");

    dir.publish(Some(report(UNLINKED)));
    let (_, out) = run(dir.path(), &["status", "--json"]);
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["credits"]["linked"], false);
    assert_eq!(body["credits"]["held"], 420);
    assert_eq!(body["credits"]["earned_this_month"], 0);
    assert_eq!(body["credits"]["gpu_class"], "l");
    assert_eq!(body["credits"]["credits_per_minute"], 10);
    assert_eq!(body["credits"]["platform_fee_percent"], 15.0);
}

#[test]
fn rewards_is_unchanged_when_the_gateway_is_off_or_older() {
    let dir = NodeDir::new("rewards-off");
    dir.register();
    dir.publish(None);

    // A gateway from before the credit system: no such route.
    let (missing, seen) = fake_gateway(404, r#"{"detail":"Not Found"}"#);
    let (code_missing, out_missing) = run(dir.path(), &["rewards", "--gateway", &missing]);
    // It did ask, at the documented path, with the node's token.
    let head = seen.recv().unwrap().to_ascii_lowercase();
    assert!(head.starts_with("get /fabric/credits/node "), "{head}");
    assert!(
        head.contains("authorization: bearer test-node-token"),
        "{head}"
    );

    // The flag off.
    let (off, _seen) = fake_gateway(200, r#"{"enabled":false}"#);
    let (code_off, out_off) = run(dir.path(), &["rewards", "--gateway", &off]);

    // Nothing answering at all.
    let (code_down, out_down) = run(dir.path(), &["rewards", "--gateway", "http://127.0.0.1:1"]);

    assert_eq!((code_missing, code_off, code_down), (0, 0, 0));
    assert_eq!(out_missing, out_off);
    assert_eq!(out_missing, out_down);
    for word in ["Credits", "credits", "NOT LINKED", "prepaid"] {
        assert!(
            !out_missing.contains(word),
            "`{word}` with credits off:\n{out_missing}"
        );
    }
    // The sections that were always there still are, in order.
    let this = out_missing.find("this node").expect("identity section");
    let delivered = out_missing
        .find("delivered since")
        .expect("delivered section");
    let companion = out_missing
        .find("rewards companion")
        .expect("companion section");
    assert!(this < delivered && delivered < companion, "{out_missing}");
}

#[test]
fn rewards_shows_what_the_gateway_reported() {
    let dir = NodeDir::new("rewards-on");
    dir.register();
    dir.publish(None);
    let (url, _seen) = fake_gateway(200, LINKED);
    let (code, out) = run(dir.path(), &["rewards", "--gateway", &url]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("KMPLIFY Credits (as the gateway reports them)"),
        "{out}"
    );
    assert!(
        out.contains("  earned   : 1240 this month · 5678 in total"),
        "{out}"
    );
    assert!(
        out.contains("class M (24 GB) · 6 credits per minute · platform fee 15 % · about 5.1 per minute to this node"),
        "{out}"
    );
    assert!(
        out.contains("  account  : linked to a KMPLIFY account"),
        "{out}"
    );
    assert!(
        out.contains("session 7f3a9c1e…  M  1800 s  +153 (fee 27)"),
        "{out}"
    );
    assert!(
        out.contains("Credits are a prepaid balance for KMPLIFY services, not a currency. There is no cash payout."),
        "{out}"
    );
    assert!(!out.contains("to link"), "{out}");
    // The companion section is still there, after it.
    assert!(
        out.find("KMPLIFY Credits").unwrap() < out.find("rewards companion").unwrap(),
        "{out}"
    );
}

#[test]
fn rewards_tells_an_unlinked_node_how_to_link() {
    let dir = NodeDir::new("rewards-unlinked");
    dir.register();
    let (url, _seen) = fake_gateway(200, UNLINKED);
    let (code, out) = run(dir.path(), &["rewards", "--gateway", &url]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("NOT LINKED. Earnings wait until this node is linked to a KMPLIFY account."),
        "{out}"
    );
    assert!(out.contains("420 credits are held for this node"), "{out}");
    // The address to paste is printed, derived from the node's own key.
    assert!(out.contains("copy this node's address: kmpn1"), "{out}");
    assert!(out.contains("\"My nodes\""), "{out}");
    assert!(out.contains("\"Claim node\""), "{out}");
    // And the credential itself never is.
    assert!(!out.contains("test-node-token"), "{out}");
}
