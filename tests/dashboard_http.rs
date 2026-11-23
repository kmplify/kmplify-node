//! The local dashboard, served for real on loopback inside this test
//! process: what it answers with the token, and what it refuses without.

use std::net::SocketAddr;

use kmplify_node::dashboard::{self, Ctx};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn send(addr: SocketAddr, raw: String) -> (u16, String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}\r\n",
        addr.port()
    )
}

fn post(addr: SocketAddr, path: &str, token: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
         X-KMPLIFY-Node-Token: {token}\r\nContent-Length: {}\r\n\r\n{body}",
        addr.port(),
        body.len()
    )
}

#[tokio::test]
async fn the_dashboard_serves_on_loopback_and_only_with_its_token() {
    let dir = std::env::temp_dir().join(format!("kn-dash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = dashboard::new_token();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    // A non-loopback bind is refused before anything listens.
    let refused = dashboard::start(
        "0.0.0.0:0".parse().unwrap(),
        Ctx {
            token: token.clone(),
            node_dir: dir.clone(),
            engine: String::new(),
            gateway: String::new(),
        },
        stop_rx.clone(),
    )
    .await;
    assert!(refused.is_err());

    let d = dashboard::start(
        dashboard::loopback(0),
        Ctx {
            token: token.clone(),
            node_dir: dir.clone(),
            engine: "http://127.0.0.1:11434".into(),
            gateway: "https://fabric.example".into(),
        },
        stop_rx,
    )
    .await
    .unwrap();
    let addr = d.addr;
    assert!(addr.ip().is_loopback());
    assert!(d.url().contains(&format!("?t={token}")));

    // The page, with the token in the URL.
    let (code, head, body) = send(addr, get(addr, &format!("/?t={token}"), "")).await;
    assert_eq!(code, 200, "{head}");
    assert!(head.contains("Content-Security-Policy: default-src 'none'"));
    assert!(body.contains("<title>kmplify-node</title>"));
    assert!(
        body.contains(&token),
        "the page carries the token for its own calls"
    );
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));

    // Without it, or with a wrong one: refused, page and script alike.
    for path in ["/", "/app.js", "/api/state", "/?t=00"] {
        let (code, _, body) = send(addr, get(addr, path, "")).await;
        assert_eq!(code, 401, "{path}");
        assert!(!body.contains(&token));
    }
    // Another site, or another name for this address: refused.
    let (code, _, _) = send(
        addr,
        get(
            addr,
            &format!("/?t={token}"),
            "Origin: https://evil.example\r\n",
        ),
    )
    .await;
    assert_eq!(code, 403);
    let (code, _, _) = send(
        addr,
        format!(
            "GET /?t={token} HTTP/1.1\r\nHost: rebind.example:{}\r\n\r\n",
            addr.port()
        ),
    )
    .await;
    assert_eq!(code, 403);

    // The state, with the token in the header.
    let (code, _, body) = send(
        addr,
        get(
            addr,
            "/api/state",
            &format!("X-KMPLIFY-Node-Token: {token}\r\n"),
        ),
    )
    .await;
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    for key in [
        "node", "machine", "sharing", "trust", "sessions", "settings", "logs",
    ] {
        assert!(v.get(key).is_some(), "{key} missing in {body}");
    }
    assert!(!body.contains(&token), "the state never echoes the token");

    // Writes: a setting the dashboard may change lands in settings.json ...
    let (code, _, body) = send(
        addr,
        post(
            addr,
            "/api/settings",
            &token,
            r#"{"key":"share-cpu","value":true}"#,
        ),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let stored = kmplify_node::settings::Settings::load(&dir);
    assert_eq!(stored.share_cpu, Some(true));
    // ... one it may not is refused, and nothing is written for it.
    let (code, _, _) = send(
        addr,
        post(
            addr,
            "/api/settings",
            &token,
            r#"{"key":"colibri-key","value":"x"}"#,
        ),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(
        kmplify_node::settings::Settings::load(&dir).colibri_api_key,
        None
    );
    // A POST without the token header is refused even with it in the URL.
    let (code, _, _) = send(
        addr,
        format!(
            "POST /api/pause?t={token} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}",
            addr.port()
        ),
    )
    .await;
    assert_eq!(code, 401);
    // An oversized body is refused before it is read.
    let (code, _, _) = send(
        addr,
        format!(
            "POST /api/settings HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
             X-KMPLIFY-Node-Token: {token}\r\nContent-Length: 999999\r\n\r\n",
            addr.port()
        ),
    )
    .await;
    assert_eq!(code, 413);

    // The stop signal ends it.
    stop_tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
