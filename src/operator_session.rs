//! Operator session report: which signed-in KMPLIFY session runs this node.
//!
//! A node counts as KMPLIFY-operated only while an operator is signed in on
//! it. The node itself has no account and no login: a person claims a node in
//! their KMPLIFY account by its address, and nothing about that account ever
//! reaches the node. An EMBEDDER that does have a signed-in session (the
//! KMPLIFY desktop app runs this worker in-process) hands the session's
//! PUBLIC id and expiry here with `set`, and clears it on logout. The worker
//! then reports it in every hello and pong as `operator_session`, signed by
//! the node's identity key, so the service that knows the session (it alone
//! can tell whose it is and whether it is still valid) can tie that session
//! to this node and to its liveness.
//!
//! What is sent is deliberately small: the public session id, its expiry and
//! a timestamp. Never the session token, never an account id, a name or an
//! e-mail address. `set` refuses anything shaped like a bearer token (a JWT
//! has dots; tokens are long), so an embedder that passes the wrong field
//! fails loudly instead of leaking a credential into the fabric.
//!
//! The headless binary never sets one: it has no login flow, and inventing
//! one is out of scope. Without a session the key is simply absent.

use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

/// KIP-1 purpose of the signature over the session report.
pub const PURPOSE_NODE_OPERATOR_SESSION: &str = "node-operator-session";

/// The public handle of a signed-in session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorSession {
    /// The session's public id: what the issuing service lists, revokes and
    /// looks up by. Not a credential.
    pub session_id: String,
    /// When the session ends, unix seconds.
    pub expires_at: u64,
}

fn cell() -> &'static RwLock<Option<OperatorSession>> {
    static S: OnceLock<RwLock<Option<OperatorSession>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(None))
}

/// A public id is 8 to 64 characters of `A-Z a-z 0-9 _ -`.
fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Report this session from now on (`Some`), or stop reporting (`None`, on
/// logout). Takes effect on the next pong; no reconnect needed.
pub fn set(session: Option<OperatorSession>) -> Result<(), String> {
    if let Some(s) = &session {
        if !valid_id(&s.session_id) {
            return Err(
                "operator session id must be the session's public id (8 to 64 characters of \
                 letters, digits, '_' and '-'), never its token"
                    .into(),
            );
        }
        if s.expires_at <= crate::identity::now_s() {
            return Err("operator session has already expired".into());
        }
    }
    if let Ok(mut c) = cell().write() {
        *c = session;
    }
    Ok(())
}

/// The session to report at `now`: none once it has expired.
pub fn current(now: u64) -> Option<OperatorSession> {
    cell().read().ok()?.clone().filter(|s| s.expires_at > now)
}

/// Exactly the bytes the identity key signs (sorted keys, no whitespace).
pub fn canonical(s: &OperatorSession, node_id: &str, pubkey_hex: &str, ts: u64) -> Vec<u8> {
    format!(
        "{{\"expires_at\":{},\"node_id\":{},\"pubkey\":{},\"session_id\":{},\"ts\":{}}}",
        s.expires_at,
        json!(node_id),
        json!(pubkey_hex),
        json!(s.session_id),
        ts
    )
    .into_bytes()
}

/// The `operator_session` object for a hello or pong, or `None` when there
/// is no live session or no identity key to sign with (an unsigned claim of
/// a session would be worth nothing).
pub fn fact(key: Option<&crate::identity::NodeKey>, node_id: &str, now: u64) -> Option<Value> {
    let key = key?;
    let s = current(now)?;
    let sig = key.sign(
        PURPOSE_NODE_OPERATOR_SESSION,
        &canonical(&s, node_id, &key.public_hex(), now),
    );
    Some(json!({
        "session_id": s.session_id,
        "expires_at": s.expires_at,
        "ts": now,
        "sig": sig,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, because the session is process-global.
    #[test]
    fn a_public_session_is_reported_signed_and_a_token_is_refused() {
        let now = crate::identity::now_s();
        let key = crate::identity::NodeKey::generate();
        set(None).unwrap();
        assert_eq!(
            fact(Some(&key), "node-1", now),
            None,
            "no session, no key in the frame"
        );

        // A JWT, a long opaque token, an expired session: all refused.
        for bad in [
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
            &"a".repeat(65),
            "short",
        ] {
            assert!(
                set(Some(OperatorSession {
                    session_id: bad.into(),
                    expires_at: now + 60
                }))
                .is_err(),
                "{bad}"
            );
        }
        assert!(set(Some(OperatorSession {
            session_id: "sess_12345678".into(),
            expires_at: now
        }))
        .is_err());

        set(Some(OperatorSession {
            session_id: "sess_12345678".into(),
            expires_at: now + 3600,
        }))
        .unwrap();
        let v = fact(Some(&key), "node-1", now).unwrap();
        assert_eq!(v["session_id"], "sess_12345678");
        assert_eq!(v["expires_at"], now + 3600);
        let s = current(now).unwrap();
        assert!(crate::identity::verify(
            &key.public_hex(),
            PURPOSE_NODE_OPERATOR_SESSION,
            &canonical(&s, "node-1", &key.public_hex(), now),
            v["sig"].as_str().unwrap()
        ));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["expires_at", "session_id", "sig", "ts"]);
        assert_eq!(fact(None, "node-1", now), None, "unsigned is never sent");
        assert_eq!(current(now + 3600), None, "expired reads as none");

        set(None).unwrap();
        assert_eq!(current(now), None, "logout");
    }
}
