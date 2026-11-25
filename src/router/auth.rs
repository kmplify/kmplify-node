//! The cluster key, and the signatures that prove a caller holds it.
//!
//! Every request to the node-info surface carries an HMAC-SHA256 signature
//! made with the cluster key, and every answer carries one back. The key
//! itself never crosses the network, so a person sniffing the LAN learns no
//! secret, cannot sign a request of their own, and cannot slip a forged
//! report to a node (the answer's signature binds it to the request's
//! nonce). A captured request replays at most once inside the
//! [`MAX_SKEW_MS`] window, and not at all to the same node, whose
//! [`Verifier`] remembers the nonces it saw.
//!
//! The key lives in `router.key` in the node directory (see [`super::key`]
//! for the store an embedding application brings instead), owner-only, beside
//! the node's credential and never inside `settings.json` (owner rule: no
//! secret in a settings file). Nodes join one cluster by holding the same
//! key: `kmplify-node router key` shows it on the first machine, and
//! `kmplify-node router join` reads it on the others from standard input,
//! so it never lands in a shell history.
//!
//! In protocol v1 requests travel in plain HTTP: hardware facts and meters,
//! authenticated but readable on the wire. From v2 the same signed request
//! and answer travel inside mutual TLS between members ([`super::tls`]); v1
//! is still served for older nodes and apps on this computer. See
//! docs/ROUTER.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};

use super::key::{hex, unhex, DOMAIN};
pub use super::key::{ClusterKey, KEY_FILE};

type HmacSha256 = Hmac<sha2::Sha256>;

/// Request header: `v1.<unix ms>.<nonce hex>.<signature hex>`, or for a
/// request with a body (the settings write) `v2.` and the same parts, the
/// signature then also covering the SHA-256 of the body
/// ([`sign_request_with_body`]).
pub const AUTH_HEADER: &str = "x-kmplify-router-auth";
/// Response header: the signature over the request's nonce and the body.
pub const SIG_HEADER: &str = "x-kmplify-router-sig";
/// How far a request's clock may be from this node's.
pub const MAX_SKEW_MS: u64 = 30_000;
/// Nonces remembered, at most: a flood of valid requests (which needs the
/// key) cannot grow the cache without bound.
const MAX_NONCES: usize = 4096;

fn mac(key: &ClusterKey) -> HmacSha256 {
    // HMAC takes a key of any length; 32 bytes is never refused.
    HmacSha256::new_from_slice(key.bytes()).expect("HMAC accepts any key length")
}

pub fn key_path(node_dir: &Path) -> PathBuf {
    node_dir.join(KEY_FILE)
}

/// The cluster key of this node, if it has one. An unreadable or malformed
/// file is no key (the router then stays local), never a panic.
pub fn load_key(node_dir: &Path) -> Option<ClusterKey> {
    let s = std::fs::read_to_string(key_path(node_dir)).ok()?;
    ClusterKey::parse(&s).ok()
}

/// Store `key` as this node's cluster key, owner-only.
pub async fn save_key(node_dir: &Path, key: &ClusterKey) -> std::io::Result<()> {
    crate::fabric_worker::write_private(
        &key_path(node_dir),
        format!("{}\n", key.reveal()).into_bytes(),
    )
    .await
}

/// The bytes a request signature covers.
fn request_preimage(method: &str, path: &str, ts_ms: u64, nonce: &str) -> String {
    format!("{DOMAIN}\nreq\n{method}\n{path}\n{ts_ms}\n{nonce}")
}

/// The bytes a body-bound (`v2.`) request signature covers: the v1 fields,
/// a different label, and the SHA-256 of the body, so a captured request
/// cannot carry another body.
fn request_preimage_with_body(
    method: &str,
    path: &str,
    ts_ms: u64,
    nonce: &str,
    body: &[u8],
) -> String {
    use sha2::Digest;
    let digest = hex(&sha2::Sha256::digest(body));
    format!("{DOMAIN}\nreq-body\n{method}\n{path}\n{ts_ms}\n{nonce}\n{digest}")
}

/// The header value for one request with a body: `v2.<ts>.<nonce>.<sig>`.
pub fn sign_request_with_body(
    key: &ClusterKey,
    method: &str,
    path: &str,
    ts_ms: u64,
    nonce: &str,
    body: &[u8],
) -> String {
    let mut m = mac(key);
    m.update(request_preimage_with_body(method, path, ts_ms, nonce, body).as_bytes());
    format!("v2.{ts_ms}.{nonce}.{}", hex(&m.finalize().into_bytes()))
}

/// The header value for one request.
pub fn sign_request(key: &ClusterKey, method: &str, path: &str, ts_ms: u64, nonce: &str) -> String {
    let mut m = mac(key);
    m.update(request_preimage(method, path, ts_ms, nonce).as_bytes());
    format!("v1.{ts_ms}.{nonce}.{}", hex(&m.finalize().into_bytes()))
}

/// A fresh nonce: 16 random bytes, hex.
pub fn new_nonce() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    hex(&b)
}

/// The signature of an answer to the request that carried `nonce`.
pub fn sign_response(key: &ClusterKey, nonce: &str, body: &[u8]) -> String {
    let mut m = mac(key);
    m.update(format!("{DOMAIN}\nres\n{nonce}\n").as_bytes());
    m.update(body);
    hex(&m.finalize().into_bytes())
}

/// Check an answer's signature, in constant time.
pub fn verify_response(key: &ClusterKey, nonce: &str, body: &[u8], sig_hex: &str) -> bool {
    let Some(sig) = unhex(sig_hex) else {
        return false;
    };
    let mut m = mac(key);
    m.update(format!("{DOMAIN}\nres\n{nonce}\n").as_bytes());
    m.update(body);
    m.verify_slice(&sig).is_ok()
}

/// Why a request was refused. Deliberately coarse on the wire: the caller
/// gets 401 either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// No header, or not in the `v1.` shape.
    Missing,
    Malformed,
    /// The clock is too far off: a stale capture, or a broken clock.
    Skew,
    /// The nonce was seen already.
    Replay,
    /// The signature does not match: another cluster's key, or forged.
    BadSignature,
}

/// Checks request signatures for one node, remembering the nonces of the
/// last [`MAX_SKEW_MS`] so no request is accepted twice.
#[derive(Default)]
pub struct Verifier {
    seen: HashMap<String, u64>,
}

impl Verifier {
    /// Verify the `header` of a `method path` request at `now_ms`. On
    /// success the nonce, which the answer's signature must cover.
    pub fn verify(
        &mut self,
        key: &ClusterKey,
        header: Option<&str>,
        method: &str,
        path: &str,
        now_ms: u64,
    ) -> Result<String, Denied> {
        self.check(key, header, "v1", now_ms, |ts, nonce| {
            request_preimage(method, path, ts, nonce)
        })
    }

    /// Verify a body-bound (`v2.`) header over `method path` and `body`.
    /// Only this form is accepted for a request that changes something: a
    /// v1 header does not cover the body.
    pub fn verify_with_body(
        &mut self,
        key: &ClusterKey,
        header: Option<&str>,
        method: &str,
        path: &str,
        body: &[u8],
        now_ms: u64,
    ) -> Result<String, Denied> {
        self.check(key, header, "v2", now_ms, |ts, nonce| {
            request_preimage_with_body(method, path, ts, nonce, body)
        })
    }

    fn check(
        &mut self,
        key: &ClusterKey,
        header: Option<&str>,
        version: &str,
        now_ms: u64,
        preimage: impl FnOnce(u64, &str) -> String,
    ) -> Result<String, Denied> {
        let header = header.ok_or(Denied::Missing)?;
        let mut parts = header.trim().split('.');
        let (Some(v), Some(ts), Some(nonce), Some(sig), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(Denied::Malformed);
        };
        if v != version {
            return Err(Denied::Malformed);
        }
        let ts: u64 = ts.parse().map_err(|_| Denied::Malformed)?;
        if nonce.len() != 32 || !nonce.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Denied::Malformed);
        }
        let sig = unhex(sig)
            .filter(|s| s.len() == 32)
            .ok_or(Denied::Malformed)?;
        if ts.abs_diff(now_ms) > MAX_SKEW_MS {
            return Err(Denied::Skew);
        }
        // The signature before the replay cache: only a request that proves
        // the key may occupy a slot in it.
        let mut m = mac(key);
        m.update(preimage(ts, nonce).as_bytes());
        m.verify_slice(&sig).map_err(|_| Denied::BadSignature)?;
        self.seen.retain(|_, t| t.abs_diff(now_ms) <= MAX_SKEW_MS);
        if self.seen.contains_key(nonce) {
            return Err(Denied::Replay);
        }
        if self.seen.len() >= MAX_NONCES {
            // Full of live nonces: refuse rather than forget one that could
            // then be replayed.
            return Err(Denied::Replay);
        }
        self.seen.insert(nonce.to_string(), ts);
        Ok(nonce.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn key(b: u8) -> ClusterKey {
        ClusterKey::from_bytes([b; 32])
    }

    #[test]
    fn a_key_round_trips_and_never_prints_itself() {
        let k = ClusterKey::generate();
        assert_eq!(ClusterKey::parse(&k.reveal()).unwrap(), k);
        assert_eq!(ClusterKey::parse(&format!(" {}\n", k.reveal())).unwrap(), k);
        assert_ne!(ClusterKey::generate(), k, "two keys are not the same key");
        assert!(!format!("{k:?}").contains(&k.reveal()));
        assert_eq!(k.fingerprint().len(), 8);
        assert_ne!(key(1).fingerprint(), key(2).fingerprint());
        for bad in ["", "abc", &"g".repeat(64), &"a".repeat(62), &"a".repeat(66)] {
            assert!(ClusterKey::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_signed_request_is_accepted_once() {
        let k = key(7);
        let mut v = Verifier::default();
        let nonce = new_nonce();
        let h = sign_request(&k, "GET", "/v1/node-info", NOW, &nonce);
        assert_eq!(
            v.verify(&k, Some(&h), "GET", "/v1/node-info", NOW + 500),
            Ok(nonce)
        );
        assert_eq!(
            v.verify(&k, Some(&h), "GET", "/v1/node-info", NOW + 600),
            Err(Denied::Replay),
            "the same request twice is a replay"
        );
    }

    #[test]
    fn the_wrong_key_path_method_or_clock_is_refused() {
        let k = key(7);
        let mut v = Verifier::default();
        let h = |n: &str, ts: u64| sign_request(&k, "GET", "/v1/node-info", ts, n);
        // Another cluster's key.
        assert_eq!(
            v.verify(
                &key(8),
                Some(&h(&new_nonce(), NOW)),
                "GET",
                "/v1/node-info",
                NOW
            ),
            Err(Denied::BadSignature)
        );
        // Signed for another path or method.
        assert_eq!(
            v.verify(&k, Some(&h(&new_nonce(), NOW)), "GET", "/v1/other", NOW),
            Err(Denied::BadSignature)
        );
        assert_eq!(
            v.verify(
                &k,
                Some(&h(&new_nonce(), NOW)),
                "POST",
                "/v1/node-info",
                NOW
            ),
            Err(Denied::BadSignature)
        );
        // Too old, too new.
        assert_eq!(
            v.verify(
                &k,
                Some(&h(&new_nonce(), NOW - MAX_SKEW_MS - 1)),
                "GET",
                "/v1/node-info",
                NOW
            ),
            Err(Denied::Skew)
        );
        assert_eq!(
            v.verify(
                &k,
                Some(&h(&new_nonce(), NOW + MAX_SKEW_MS + 1)),
                "GET",
                "/v1/node-info",
                NOW
            ),
            Err(Denied::Skew)
        );
        // A tampered timestamp breaks the signature.
        let good = h(&new_nonce(), NOW);
        let tampered = good.replacen(&NOW.to_string(), &(NOW + 1).to_string(), 1);
        assert_eq!(
            v.verify(&k, Some(&tampered), "GET", "/v1/node-info", NOW),
            Err(Denied::BadSignature)
        );
    }

    #[test]
    fn missing_and_malformed_headers_are_refused() {
        let k = key(7);
        let mut v = Verifier::default();
        assert_eq!(v.verify(&k, None, "GET", "/", NOW), Err(Denied::Missing));
        for bad in [
            "",
            "v1",
            "v2.1.2.3",
            "v1.x.0123456789abcdef0123456789abcdef.00",
            "v1.1.short.00",
            &format!("v1.{NOW}.0123456789abcdef0123456789abcdef.zz"),
            &format!(
                "v1.{NOW}.0123456789abcdef0123456789abcdef.{}.extra",
                "0".repeat(64)
            ),
            "Bearer 0123",
        ] {
            assert!(
                matches!(
                    v.verify(&k, Some(bad), "GET", "/", NOW),
                    Err(Denied::Malformed)
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn old_nonces_are_forgotten_and_the_cache_is_bounded() {
        let k = key(7);
        let mut v = Verifier::default();
        for i in 0..MAX_NONCES as u64 {
            let n = format!("{i:032x}");
            let h = sign_request(&k, "GET", "/", NOW, &n);
            assert!(v.verify(&k, Some(&h), "GET", "/", NOW).is_ok());
        }
        let n = new_nonce();
        let h = sign_request(&k, "GET", "/", NOW, &n);
        assert_eq!(
            v.verify(&k, Some(&h), "GET", "/", NOW),
            Err(Denied::Replay),
            "full: refuse rather than forget a live nonce"
        );
        // A minute later the old ones have aged out.
        let later = NOW + 2 * MAX_SKEW_MS + 1;
        let h = sign_request(&k, "GET", "/", later, &n);
        assert!(v.verify(&k, Some(&h), "GET", "/", later).is_ok());
        assert_eq!(v.seen.len(), 1);
    }

    /// Golden value shared with kmplify-rag-system's client
    /// (tests/test_lan_router_admin.py), so the two never drift apart.
    #[test]
    fn the_body_bound_signature_matches_the_golden_value() {
        assert_eq!(
            sign_request_with_body(
                &key(7),
                "PUT",
                "/v2/node-settings",
                1_800_000_000_000,
                "0123456789abcdef0123456789abcdef",
                br#"{"name":"studio"}"#,
            ),
            "v2.1800000000000.0123456789abcdef0123456789abcdef.\
             5582be7da0c48ae5164b91fcfe20fa8f8f8d0f619e59b7d28a5ac51fd11c9a0b"
        );
    }

    #[test]
    fn a_body_bound_request_covers_its_body_and_is_accepted_once() {
        let k = key(7);
        let mut v = Verifier::default();
        let p = "/v2/node-settings";
        let body = br#"{"name":"studio"}"#;
        let nonce = new_nonce();
        let h = sign_request_with_body(&k, "PUT", p, NOW, &nonce, body);
        assert!(h.starts_with("v2."));
        // Another body under the same signature: refused.
        assert_eq!(
            v.verify_with_body(&k, Some(&h), "PUT", p, br#"{"name":"evil"}"#, NOW),
            Err(Denied::BadSignature)
        );
        assert_eq!(
            v.verify_with_body(&k, Some(&h), "PUT", p, body, NOW),
            Ok(nonce)
        );
        assert_eq!(
            v.verify_with_body(&k, Some(&h), "PUT", p, body, NOW),
            Err(Denied::Replay)
        );
        // A v1 header is not accepted where the body must be covered, and a
        // v2 header is not accepted where v1 is expected.
        let v1 = sign_request(&k, "PUT", p, NOW, &new_nonce());
        assert_eq!(
            v.verify_with_body(&k, Some(&v1), "PUT", p, body, NOW),
            Err(Denied::Malformed)
        );
        let v2 = sign_request_with_body(&k, "GET", "/v1/node-info", NOW, &new_nonce(), b"");
        assert_eq!(
            v.verify(&k, Some(&v2), "GET", "/v1/node-info", NOW),
            Err(Denied::Malformed)
        );
        // Another key, another method.
        let h = sign_request_with_body(&key(8), "PUT", p, NOW, &new_nonce(), body);
        assert_eq!(
            v.verify_with_body(&k, Some(&h), "PUT", p, body, NOW),
            Err(Denied::BadSignature)
        );
        let h = sign_request_with_body(&k, "POST", p, NOW, &new_nonce(), body);
        assert_eq!(
            v.verify_with_body(&k, Some(&h), "PUT", p, body, NOW),
            Err(Denied::BadSignature)
        );
    }

    #[test]
    fn an_answer_is_bound_to_its_request_and_its_body() {
        let k = key(7);
        let nonce = new_nonce();
        let body = br#"{"id":"abc"}"#;
        let sig = sign_response(&k, &nonce, body);
        assert!(verify_response(&k, &nonce, body, &sig));
        assert!(
            !verify_response(&k, &new_nonce(), body, &sig),
            "another request"
        );
        assert!(
            !verify_response(&k, &nonce, br#"{"id":"abd"}"#, &sig),
            "another body"
        );
        assert!(!verify_response(&key(8), &nonce, body, &sig), "another key");
        assert!(!verify_response(&k, &nonce, body, "nothex"));
    }

    #[tokio::test]
    async fn the_key_file_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("kmplify-router-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_key(&dir).is_none());
        let k = ClusterKey::generate();
        save_key(&dir, &k).await.unwrap();
        assert_eq!(load_key(&dir), Some(k));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(key_path(&dir), "garbage").unwrap();
        assert!(load_key(&dir).is_none(), "a broken file is no key");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
