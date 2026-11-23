//! Golden conformance of the `hello` and `pong` frames against PROTOCOL.md.
//!
//! The real worker (`fabric_worker::run`, the same code the binary and every
//! embedding application run) connects to a gateway served inside this test
//! process. Its frames are compared with the reference tables in PROTOCOL.md
//! ("Reference: every key of `hello` and `pong`"): a key the table does not
//! list fails the test, and so does a key the table says is always sent and
//! the frame lacks. The hello's identity signature is verified as a gateway
//! would, and the embedding hooks (`host::ProviderHost`) are exercised on the
//! way: what an embedder's `configure` sets must be what the hello says.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use kmplify_node::fabric_worker::{self, WorkerConfig};
use kmplify_node::host::ProviderHost;
use kmplify_node::identity;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;

/// (key, always sent) rows of the table under `heading` in PROTOCOL.md.
fn table(heading: &str) -> Vec<(String, bool)> {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/PROTOCOL.md"))
        .expect("PROTOCOL.md");
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("PROTOCOL.md has no section {heading:?}"));
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in doc[start..].lines().skip(1) {
        let line = line.trim();
        if line.starts_with('|') {
            in_table = true;
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if let Some(key) = cells
                .first()
                .and_then(|c| c.strip_prefix('`'))
                .and_then(|c| c.strip_suffix('`'))
            {
                let always = cells.get(1).is_some_and(|s| s.starts_with("always"));
                rows.push((key.to_string(), always));
            }
        } else if in_table {
            break;
        }
    }
    assert!(!rows.is_empty(), "no rows under {heading:?}");
    rows
}

fn check_frame(frame: &Value, heading: &str) {
    let rows = table(heading);
    let documented: BTreeSet<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    let sent: BTreeSet<&str> = frame
        .as_object()
        .expect("frame is an object")
        .keys()
        .map(String::as_str)
        .collect();
    let undocumented: Vec<&&str> = sent.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "{heading}: the worker sends {undocumented:?}, which PROTOCOL.md does not list. \
         Add the key to the reference table in the same change."
    );
    for (key, always) in &rows {
        if *always {
            assert!(
                sent.contains(key.as_str()),
                "{heading}: PROTOCOL.md says `{key}` is always sent; this frame has no such key: {frame}"
            );
        }
    }
}

/// A gateway on loopback: `/fabric/connect` upgrades to a WebSocket whose
/// frames are handed to the test, every other request is a 404 (the worker
/// asks for invitations, the catalog and its own listing on the side).
async fn fake_gateway() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut head = String::new();
                for _ in 0..50 {
                    let n = sock.peek(&mut buf).await.unwrap_or(0);
                    head = String::from_utf8_lossy(&buf[..n]).to_string();
                    if head.contains("\r\n\r\n") || n == buf.len() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if head.to_ascii_lowercase().contains("upgrade: websocket") {
                    if let Ok(ws) = tokio_tungstenite::accept_async(sock).await {
                        let _ = tx.send(ws);
                    }
                } else {
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\
                              Content-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .await;
                }
            });
        }
    });
    (format!("http://{addr}"), rx)
}

#[derive(Default)]
struct Seen {
    configured: bool,
    started_in: Option<PathBuf>,
    hardware: bool,
    log_lines: usize,
}

struct TestHost(Arc<Mutex<Seen>>);

impl ProviderHost for TestHost {
    fn configure(&self, cfg: &mut WorkerConfig) {
        cfg.marketplace = true;
        self.0.lock().unwrap().configured = true;
    }
    fn worker_started(&self, node_dir: &std::path::Path) {
        self.0.lock().unwrap().started_in = Some(node_dir.to_path_buf());
    }
    fn log_line(&self, _line: &str) {
        self.0.lock().unwrap().log_lines += 1;
    }
    fn hardware(&self, gpu: &Value, cpu: Option<&Value>) {
        assert!(gpu.is_object() && cpu.is_some());
        self.0.lock().unwrap().hardware = true;
    }
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    within: Duration,
) -> Value {
    loop {
        let msg = tokio::time::timeout(within, ws.next())
            .await
            .expect("the worker went quiet")
            .expect("the worker closed the socket")
            .expect("websocket error");
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).expect("a JSON frame");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_and_pong_match_the_protocol_reference() {
    let dir = std::env::temp_dir().join(format!(
        "kmplify-node-golden-{}-{}",
        std::process::id(),
        identity::now_s()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let creds_path = dir.join("fabric_credentials.json");
    // A registered node with an identity key, so no register round-trip is
    // needed and the hello is signed.
    let key = identity::NodeKey::generate();
    std::fs::write(
        &creds_path,
        json!({
            "node_id": "golden0000000000000000000000000000000000",
            "token": "golden-token",
            "seed_hex": key.seed_hex(),
        })
        .to_string(),
    )
    .unwrap();

    let seen = Arc::new(Mutex::new(Seen::default()));
    kmplify_node::host::install(Arc::new(TestHost(seen.clone())));
    kmplify_node::operator_session::set(Some(kmplify_node::operator_session::OperatorSession {
        session_id: "golden-session-01".into(),
        expires_at: identity::now_s() + 3600,
    }))
    .unwrap();

    let (gateway, mut accepted) = fake_gateway().await;
    let cfg = WorkerConfig {
        gateway_url: gateway,
        // Nothing listens here: the node lends no models, which is a valid
        // hello and keeps the test off any engine on the machine running it.
        ollama_base: "http://127.0.0.1:9".into(),
        creds_path: creds_path.clone(),
        client_version: Some("9.9.9-embedder".into()),
        ..Default::default()
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(fabric_worker::run(cfg, stop_rx));

    let mut ws = tokio::time::timeout(Duration::from_secs(120), accepted.recv())
        .await
        .expect("the worker never dialled the gateway")
        .expect("gateway task ended");

    let hello = next_json(&mut ws, Duration::from_secs(30)).await;
    assert_eq!(hello["type"], "hello");
    check_frame(&hello, "### `hello` (protocol 4)");
    assert_eq!(hello["protocol"], 4);
    assert_eq!(hello["node_id"], "golden0000000000000000000000000000000000");
    assert_eq!(
        hello["version"], "9.9.9-embedder",
        "an embedder's own version"
    );
    assert_eq!(hello["worker_version"], kmplify_node::version_string());
    assert_eq!(hello["marketplace"], true, "what the host's configure set");
    assert_eq!(hello["pubkey"], key.public_hex());
    let ts = hello["ts"].as_u64().expect("ts");
    assert!(identity::verify(
        &key.public_hex(),
        identity::PURPOSE_NODE_HELLO,
        &identity::canonical_hello(hello["node_id"].as_str().unwrap(), &key.public_hex(), ts),
        hello["sig"].as_str().expect("sig"),
    ));
    assert_eq!(hello["operator_session"]["session_id"], "golden-session-01");
    assert!(
        hello["envelope"].is_object(),
        "an identity key offers the envelope"
    );

    ws.send(Message::Text(json!({"type": "welcome"}).to_string()))
        .await
        .unwrap();
    ws.send(Message::Text(json!({"type": "ping"}).to_string()))
        .await
        .unwrap();
    let pong = loop {
        let f = next_json(&mut ws, Duration::from_secs(30)).await;
        if f["type"] == "pong" {
            break f;
        }
    };
    check_frame(&pong, "### `pong` (protocol 4)");
    assert!(pong["loaded_models"].is_array());
    assert_eq!(pong["operator_session"]["session_id"], "golden-session-01");

    let link = fabric_worker::link_state();
    assert!(link.connected, "{link:?}");
    assert_eq!(link.models, 0);

    stop_tx.send(true).unwrap();
    drop(ws);
    let _ = tokio::time::timeout(Duration::from_secs(30), worker).await;
    kmplify_node::host::uninstall();
    kmplify_node::operator_session::set(None).unwrap();

    let seen = seen.lock().unwrap();
    assert!(seen.configured, "configure runs before the connection");
    assert_eq!(seen.started_in.as_deref(), Some(dir.as_path()));
    assert!(seen.hardware, "hardware is reported with the hello");
    assert!(seen.log_lines > 0, "log lines reach the host");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The tables themselves: every row names one key once, and the frames'
/// fixed keys are there.
#[test]
fn the_reference_tables_are_well_formed() {
    for heading in ["### `hello` (protocol 4)", "### `pong` (protocol 4)"] {
        let rows = table(heading);
        let mut seen = BTreeSet::new();
        for (k, _) in &rows {
            assert!(seen.insert(k.clone()), "{heading}: `{k}` listed twice");
        }
        assert!(seen.contains("type"));
    }
    let hello: BTreeSet<String> = table("### `hello` (protocol 4)")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for k in [
        "node_id",
        "token",
        "pubkey",
        "sig",
        "protocol",
        "worker_version",
    ] {
        assert!(hello.contains(k), "hello table lacks `{k}`");
    }
}
