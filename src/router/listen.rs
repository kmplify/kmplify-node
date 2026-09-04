//! One port, two personalities.
//!
//! A TLS handshake begins with the byte `0x16`; no HTTP method does. So a
//! listener peeks at the first byte and hands the connection either to a
//! plaintext HTTP server or to the cluster's mutual TLS acceptor — the same
//! trick PAIR uses, and the reason an endpoint is `http://127.0.0.1:11440`
//! for an application on this machine while the same port serves
//! authenticated TLS to paired nodes. Local clients need no certificates,
//! and inference between machines is never plaintext.
//!
//! What a handler learns about the caller is [`PeerInfo`]: the address,
//! and — only when the handshake presented a pinned certificate — which
//! node it is. Everything about who may do what is decided from that, in
//! the handlers, not here.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

use super::{cluster, lock, Shared};

/// The caller of a request, as the listener established it.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    /// Present only for a mutual-TLS connection whose certificate is
    /// pinned: the node id it belongs to (empty if pinned but not yet in
    /// the member list) and the fingerprint that was presented.
    pub tls: Option<TlsPeer>,
}

#[derive(Clone, Debug)]
pub struct TlsPeer {
    pub node_id: String,
    pub fingerprint: String,
}

impl PeerInfo {
    pub fn is_loopback(&self) -> bool {
        self.addr.ip().is_loopback()
    }

    pub fn is_member(&self) -> bool {
        self.tls.as_ref().is_some_and(|t| !t.node_id.is_empty())
    }
}

impl Connected<PeerInfo> for PeerInfo {
    fn connect_info(target: PeerInfo) -> Self {
        target
    }
}

/// Bind `port` on every interface and serve `app` on it, both
/// personalities. Returns only if the bind fails or the accept loop dies.
pub async fn serve(shared: Shared, port: u16, app: axum::Router, label: &'static str) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let mut r = lock(&shared);
            r.listeners = format!("{label}: cannot bind :{port}: {e}");
            r.push_log(format!("{label} unavailable on :{port}: {e}"));
            return;
        }
    };
    {
        let mut r = lock(&shared);
        r.push_log(format!("{label} on :{port}"));
        if r.listeners == "starting" || r.listeners.is_empty() {
            r.listeners = "listening".into();
        }
    }
    let make_service = app.into_make_service_with_connect_info::<PeerInfo>();
    loop {
        let (stream, remote) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                lock(&shared).push_log(format!("{label}: accept failed: {e}"));
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        let shared = shared.clone();
        let make_service = make_service.clone();
        tokio::spawn(async move {
            let mut first = [0u8; 1];
            let is_tls = matches!(stream.peek(&mut first).await, Ok(1) if first[0] == 0x16);
            if is_tls {
                let config: Option<Arc<rustls::ServerConfig>> = lock(&shared).tls_server.clone();
                let Some(config) = config else {
                    // No certificate here, so no TLS personality; the
                    // handshake simply fails on the other side.
                    return;
                };
                let acceptor = TlsAcceptor::from(config);
                let tls = match acceptor.accept(stream).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                let fingerprint = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|c| c.first())
                    .map(|c| cluster::fingerprint(c.as_ref()))
                    .unwrap_or_default();
                let node_id = lock(&shared)
                    .cluster
                    .member_by_fingerprint(&fingerprint)
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                let info = PeerInfo {
                    addr: remote,
                    tls: Some(TlsPeer { node_id, fingerprint }),
                };
                serve_one(make_service, info, tls).await;
            } else {
                let info = PeerInfo {
                    addr: remote,
                    tls: None,
                };
                serve_one(make_service, info, stream).await;
            }
        });
    }
}

async fn serve_one<IO>(
    make_service: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, PeerInfo>,
    info: PeerInfo,
    io: IO,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Ok(service) = make_service.oneshot(info).await;
    let _ = Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(service))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_handshake_makes_a_member_and_plaintext_never_does() {
        let addr: SocketAddr = "192.168.1.9:5000".parse().unwrap();
        let plain = PeerInfo { addr, tls: None };
        assert!(!plain.is_member());
        assert!(!plain.is_loopback());
        let pinned_unknown = PeerInfo {
            addr,
            tls: Some(TlsPeer { node_id: String::new(), fingerprint: "f".into() }),
        };
        assert!(!pinned_unknown.is_member(), "pinned but not yet listed is not a member");
        let member = PeerInfo {
            addr,
            tls: Some(TlsPeer { node_id: "n".into(), fingerprint: "f".into() }),
        };
        assert!(member.is_member());
        let local = PeerInfo { addr: "127.0.0.1:1".parse().unwrap(), tls: None };
        assert!(local.is_loopback());
    }
}
