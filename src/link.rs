//! Link codes: a short-lived proof, signed by this node's identity key, that
//! whoever holds the machine wants it linked to a KMPLIFY account.
//!
//! A provider links a machine to their account so the marketplace can show
//! their @alias on its card. The account service used to accept the node id
//! itself as the proof. That held while cards showed only part of the id;
//! once the fabric publishes the whole id, knowing it proves nothing. A link
//! code proves possession of the key the fabric gateway bound to that id at
//! the node's first signed hello (protocol v3.7), which nobody but the
//! machine has.
//!
//! # Format (link proof, KIP-1 purpose `node-link`)
//!
//! ```text
//! kmplink1.<base64url(payload)>.<base64url(signature)>      (no padding)
//! ```
//!
//! `payload` is canonical JSON (sorted keys, no whitespace, ASCII only):
//!
//! | key       | value                                                      |
//! |-----------|------------------------------------------------------------|
//! | `exp`     | unix seconds after which the code is refused               |
//! | `iat`     | unix seconds the code was made                             |
//! | `jti`     | 32 hex characters, random: a code is accepted once          |
//! | `node_id` | the node's public id, 32 lowercase hex characters          |
//! | `nonce`   | optional: a challenge the account service handed out, which binds the code to that account |
//! | `pubkey`  | the node's Ed25519 identity key, 64 lowercase hex          |
//! | `purpose` | always `"link"`                                            |
//! | `v`       | always `1`                                                 |
//!
//! The signature is Ed25519 over `KMPLIFY-ID-v1/node-link\n` followed by the
//! payload bytes (KIP-1 domain separation): no other signature this node
//! makes (`node-hello`, `node-register`, `node-receipt`, ...) can be passed
//! off as a link code, and a link code is good for nothing else.
//!
//! A verifier checks the signature, the times (`exp - iat` at most
//! [`MAX_TTL_S`], [`MAX_CLOCK_SKEW_S`] of tolerance either way), that the
//! payload is canonical, that `jti` was not seen before, and then asks the
//! fabric gateway whether `pubkey` is the key bound to `node_id`. The last
//! step is what makes it a proof; [`decode`] does everything before it.
//!
//! The code carries no personal data: a random id, a public key, two times
//! and, when used, an opaque challenge.
//!
//! Part of the library surface (`provider-core`), so an embedder such as
//! KMPLIFY Desktop makes exactly the same code as the `kmplify-node
//! link-code` command and the dashboards.

use std::path::Path;

use base64::Engine;
use serde::Serialize;

use crate::identity::{self, NodeKey};

/// KIP-1 purpose of a link code's signature.
pub const PURPOSE_NODE_LINK: &str = "node-link";
/// First part of every link code; the `1` is the format version.
pub const PREFIX: &str = "kmplink1";
/// How long a code made by [`for_credentials`] is good for.
pub const DEFAULT_TTL_S: u64 = 600;
/// The longest life a verifier accepts (`exp - iat`).
pub const MAX_TTL_S: u64 = 900;
/// Tolerance for clocks that disagree, both ways (KIP-1's limit).
pub const MAX_CLOCK_SKEW_S: u64 = identity::MAX_CLOCK_SKEW_S;
/// Length of the public node id a code names.
pub const PUBLIC_ID_LEN: usize = 32;
/// A nonce is 8 to 64 characters of `[A-Za-z0-9_-]`.
pub const NONCE_MIN: usize = 8;
pub const NONCE_MAX: usize = 64;
/// Upper bound on a decoded payload; real ones are about 220 bytes.
const MAX_PAYLOAD: usize = 1024;

/// A code made for a person to copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LinkCode {
    pub code: String,
    /// The public node id the code names (32 characters).
    pub node_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

/// What a code says, once [`decode`] accepted its form and signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub node_id: String,
    pub pubkey: String,
    pub nonce: Option<String>,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A nonce as the account service hands it out.
pub fn valid_nonce(s: &str) -> bool {
    (NONCE_MIN..=NONCE_MAX).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The public id of a credential's node id: its first 32 characters, lower
/// case (the gateway publishes exactly that).
pub fn public_id(node_id: &str) -> Result<String, String> {
    let id = node_id.trim().to_ascii_lowercase();
    if id.len() < PUBLIC_ID_LEN || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("this node's id is not a fabric node id (32 hex characters)".into());
    }
    Ok(id[..PUBLIC_ID_LEN].to_string())
}

/// Canonical JSON of a claim: keys sorted, no whitespace. Every value is
/// ASCII (hex, digits, the nonce alphabet), so this is byte for byte what
/// `json.dumps(obj, sort_keys=True, separators=(",", ":"))` gives.
pub fn canonical(c: &Claim) -> Vec<u8> {
    let nonce = c
        .nonce
        .as_deref()
        .map(|n| format!("\"nonce\":{},", serde_json::json!(n)))
        .unwrap_or_default();
    format!(
        "{{\"exp\":{},\"iat\":{},\"jti\":{},\"node_id\":{},{nonce}\"pubkey\":{},\"purpose\":\"link\",\"v\":1}}",
        c.exp,
        c.iat,
        serde_json::json!(c.jti),
        serde_json::json!(c.node_id),
        serde_json::json!(c.pubkey),
    )
    .into_bytes()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
}

fn random_jti() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    identity::hex_encode(&b)
}

/// Sign `claim` with `key`. The claim's `pubkey` must be the key's.
fn encode(key: &NodeKey, claim: &Claim) -> String {
    let payload = canonical(claim);
    let sig_hex = key.sign(PURPOSE_NODE_LINK, &payload);
    let sig = crate::functions::hex_decode(&sig_hex).unwrap_or_default();
    format!("{PREFIX}.{}.{}", b64(&payload), b64(&sig))
}

fn make_with(
    key: &NodeKey,
    node_id: &str,
    nonce: Option<&str>,
    now: u64,
    ttl_s: u64,
    jti: String,
) -> Result<LinkCode, String> {
    let node_id = public_id(node_id)?;
    let nonce = match nonce.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if valid_nonce(n) => Some(n.to_string()),
        Some(_) => {
            return Err(format!(
                "a link nonce is {NONCE_MIN} to {NONCE_MAX} characters of letters, digits, - and _"
            ))
        }
        None => None,
    };
    if ttl_s == 0 || ttl_s > MAX_TTL_S {
        return Err(format!(
            "a link code lives 1 to {MAX_TTL_S} seconds, not {ttl_s}"
        ));
    }
    let claim = Claim {
        node_id: node_id.clone(),
        pubkey: key.public_hex(),
        nonce,
        iat: now,
        exp: now + ttl_s,
        jti,
    };
    Ok(LinkCode {
        code: encode(key, &claim),
        node_id,
        issued_at: claim.iat,
        expires_at: claim.exp,
    })
}

/// A fresh link code for `node_id`, signed by `key`, good for `ttl_s`.
pub fn make(
    key: &NodeKey,
    node_id: &str,
    nonce: Option<&str>,
    now: u64,
    ttl_s: u64,
) -> Result<LinkCode, String> {
    make_with(key, node_id, nonce, now, ttl_s, random_jti())
}

/// A fresh link code for the node whose credential file is `creds_path`
/// (`fabric_node.json`). Reads the file and nothing else: no network, no
/// write, and the seed never leaves this function.
pub fn for_credentials(creds_path: &Path, nonce: Option<&str>) -> Result<LinkCode, String> {
    let bytes = std::fs::read(creds_path).map_err(|_| {
        "this machine has not joined the fabric yet: start it once, then make a link code"
            .to_string()
    })?;
    let creds: crate::fabric_worker::Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "the node's credential file is unreadable".to_string())?;
    let key = creds.key().ok_or_else(|| {
        "this node has no identity key yet: start it once (it makes one and the fabric binds it)"
            .to_string()
    })?;
    make(
        &key,
        &creds.node_id,
        nonce,
        identity::now_s(),
        DEFAULT_TTL_S,
    )
}

/// What to do with a code, in the words every view uses (the CLI, the web
/// and terminal dashboards).
pub fn instructions(c: &LinkCode, bound: bool) -> Vec<String> {
    let minutes = c.expires_at.saturating_sub(c.issued_at).div_ceil(60);
    let mut out = vec![
        format!(
            "Link code for node {}: valid for {minutes} minutes, once{}.",
            c.node_id,
            if bound {
                ", for the account that showed the challenge"
            } else {
                ""
            }
        ),
        "Enter it in the KMPLIFY marketplace: Settings > Peer Compute > Link code.".into(),
        "It proves you run this machine and carries no personal data. Make a new one any time."
            .into(),
    ];
    if !bound {
        out.push(format!(
            "Not bound to an account: whoever has this code within {minutes} minutes can redeem it. \
             Binding is recommended: make the code in KMPLIFY Desktop (signed in), or run \
             kmplify-node link-code --nonce CHALLENGE with the challenge from your account \
             (marketplace: Settings > Peer Compute)."
        ));
    }
    out
}

/// Check a code's form, times and signature. Does NOT check that the key is
/// the one the fabric bound to the node id, nor that the code is new: those
/// need the gateway and a memory of used codes, which only the verifier has.
pub fn decode(code: &str, now: u64) -> Result<Claim, String> {
    let code = code.trim();
    let mut parts = code.split('.');
    let (Some(prefix), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a link code".into());
    };
    if prefix != PREFIX {
        return Err("not a link code".into());
    }
    let payload = unb64(p).ok_or("not a link code")?;
    let sig = unb64(s).ok_or("not a link code")?;
    if payload.len() > MAX_PAYLOAD || sig.len() != 64 {
        return Err("not a link code".into());
    }
    let v: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| "not a link code".to_string())?;
    let obj = v.as_object().ok_or("not a link code")?;
    const KEYS: &[&str] = &[
        "exp", "iat", "jti", "node_id", "nonce", "pubkey", "purpose", "v",
    ];
    if obj.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return Err("not a link code".into());
    }
    if obj.get("v").and_then(|x| x.as_u64()) != Some(1)
        || obj.get("purpose").and_then(|x| x.as_str()) != Some("link")
    {
        return Err("not a link code".into());
    }
    let s_of = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let n_of = |k: &str| obj.get(k).and_then(|x| x.as_u64());
    let claim = Claim {
        node_id: s_of("node_id").ok_or("not a link code")?,
        pubkey: s_of("pubkey").ok_or("not a link code")?,
        nonce: match obj.get("nonce") {
            None => None,
            Some(x) => Some(x.as_str().ok_or("not a link code")?.to_string()),
        },
        iat: n_of("iat").ok_or("not a link code")?,
        exp: n_of("exp").ok_or("not a link code")?,
        jti: s_of("jti").ok_or("not a link code")?,
    };
    if !is_lower_hex(&claim.node_id, PUBLIC_ID_LEN)
        || !is_lower_hex(&claim.pubkey, 64)
        || !is_lower_hex(&claim.jti, 32)
        || claim.nonce.as_deref().is_some_and(|n| !valid_nonce(n))
    {
        return Err("not a link code".into());
    }
    // One spelling only: what was signed is what is read.
    if canonical(&claim) != payload {
        return Err("not a link code".into());
    }
    if !identity::verify(
        &claim.pubkey,
        PURPOSE_NODE_LINK,
        &payload,
        &identity::hex_encode(&sig),
    ) {
        return Err("the link code's signature is not valid".into());
    }
    if claim.exp <= claim.iat || claim.exp - claim.iat > MAX_TTL_S {
        return Err("the link code's lifetime is not allowed".into());
    }
    if claim.iat > now + MAX_CLOCK_SKEW_S {
        return Err("the link code is from the future: check this machine's clock".into());
    }
    if now > claim.exp + MAX_CLOCK_SKEW_S {
        return Err("the link code has expired: make a new one".into());
    }
    Ok(claim)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8032 test key 1, the KIP-1 cross-language key.
    const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUB: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const NODE: &str = "0b79ed9d11112222aaaabbbbcccc0000";
    const JTI: &str = "00112233445566778899aabbccddeeff";
    const T: u64 = 1_795_000_000;
    /// Known answer: `make_with(SEED, NODE, None, T, 600, JTI)`. The account
    /// service's tests pin the same string (kmplify-rag-system-usermanagement
    /// test_provider_link_proof.py), so both sides agree on every byte.
    const VECTOR: &str = "kmplink1.eyJleHAiOjE3OTUwMDA2MDAsImlhdCI6MTc5NTAwMDAwMCwianRpIjoiMDAxMTIyMzM0NDU1NjY3Nzg4OTlhYWJiY2NkZGVlZmYiLCJub2RlX2lkIjoiMGI3OWVkOWQxMTExMjIyMmFhYWFiYmJiY2NjYzAwMDAiLCJwdWJrZXkiOiJkNzVhOTgwMTgyYjEwYWI3ZDU0YmZlZDNjOTY0MDczYTBlZTE3MmYzZGFhNjIzMjVhZjAyMWE2OGY3MDc1MTFhIiwicHVycG9zZSI6ImxpbmsiLCJ2IjoxfQ.kFtiATJjTLyg6L41efjugbRJ6njPdpxVxabXWmlbkP88Jblc6Nmel2wD6UJf_ND0JmDD53JHLN6Kd11QIjrZDw";

    fn key() -> NodeKey {
        NodeKey::from_seed_hex(SEED).unwrap()
    }

    fn vector() -> LinkCode {
        make_with(&key(), NODE, None, T, DEFAULT_TTL_S, JTI.into()).unwrap()
    }

    /// Replace the payload and re-sign it with `k` under `purpose`.
    fn resign(k: &NodeKey, purpose: &str, payload: &[u8]) -> String {
        let sig = crate::functions::hex_decode(&k.sign(purpose, payload)).unwrap();
        format!("{PREFIX}.{}.{}", b64(payload), b64(&sig))
    }

    #[test]
    fn the_known_answer_matches_every_implementation() {
        let c = vector();
        assert_eq!(key().public_hex(), PUB);
        assert_eq!(c.code, VECTOR, "printed: {}", c.code);
        assert_eq!(c.node_id, NODE);
        assert_eq!((c.issued_at, c.expires_at), (T, T + 600));
    }

    #[test]
    fn a_fresh_code_decodes_to_what_was_signed() {
        let k = NodeKey::generate();
        let c = make(
            &k,
            "0B79ED9D11112222AAAABBBBCCCC0000ffff",
            Some("chal_ABC-123"),
            T,
            600,
        )
        .unwrap();
        let claim = decode(&format!("  {}\n", c.code), T + 30).unwrap();
        // A longer legacy id is named by the 32 characters the gateway publishes.
        assert_eq!(claim.node_id, NODE);
        assert_eq!(claim.pubkey, k.public_hex());
        assert_eq!(claim.nonce.as_deref(), Some("chal_ABC-123"));
        assert_eq!(claim.exp - claim.iat, 600);
        // Two codes never share a jti.
        let other = make(&k, NODE, None, T, 600).unwrap();
        assert_ne!(decode(&other.code, T).unwrap().jti, claim.jti);
        // No personal data and no secret: the payload is exactly the claim.
        assert!(!c.code.contains(&k.seed_hex()));
    }

    #[test]
    fn a_changed_payload_or_another_key_is_refused() {
        let c = vector();
        let claim = decode(&c.code, T).unwrap();
        // Another node id under the original signature.
        let mut forged = claim.clone();
        forged.node_id = "ffffffffffffffffffffffffffffffff".into();
        let sig = c.code.rsplit('.').next().unwrap();
        let tampered = format!("{PREFIX}.{}.{sig}", b64(&canonical(&forged)));
        assert!(decode(&tampered, T).unwrap_err().contains("signature"));
        // Signed by another key, naming the victim's key: signature fails.
        let mallory = NodeKey::generate();
        assert!(decode(&resign(&mallory, PURPOSE_NODE_LINK, &canonical(&claim)), T).is_err());
        // Signed by another key, naming that key: decodes (the form is fine),
        // and only the gateway's key check can refuse it. That is the point
        // of asking the gateway, and why decode() alone is never a proof.
        let mut own = claim.clone();
        own.pubkey = mallory.public_hex();
        let theirs = resign(&mallory, PURPOSE_NODE_LINK, &canonical(&own));
        assert_eq!(decode(&theirs, T).unwrap().pubkey, mallory.public_hex());
    }

    #[test]
    fn another_purpose_never_passes_as_a_link_code() {
        let claim = decode(&vector().code, T).unwrap();
        for purpose in [
            identity::PURPOSE_NODE_HELLO,
            identity::PURPOSE_NODE_REGISTER,
            identity::PURPOSE_NODE_RECEIPT,
        ] {
            let code = resign(&key(), purpose, &canonical(&claim));
            assert!(decode(&code, T).is_err(), "{purpose}");
        }
        // And a hello signature over the hello preimage is no link code
        // either, whatever is wrapped around it.
        let hello = key().hello_fields(NODE, T);
        let sig = crate::functions::hex_decode(hello["sig"].as_str().unwrap()).unwrap();
        let code = format!(
            "{PREFIX}.{}.{}",
            b64(&identity::canonical_hello(NODE, PUB, T)),
            b64(&sig)
        );
        assert!(decode(&code, T).is_err());
    }

    #[test]
    fn times_are_checked_with_tolerance() {
        let c = vector();
        assert!(decode(&c.code, T + 600 + MAX_CLOCK_SKEW_S).is_ok());
        assert!(decode(&c.code, T + 600 + MAX_CLOCK_SKEW_S + 1)
            .unwrap_err()
            .contains("expired"));
        assert!(decode(&c.code, T - MAX_CLOCK_SKEW_S).is_ok());
        assert!(decode(&c.code, T - MAX_CLOCK_SKEW_S - 1)
            .unwrap_err()
            .contains("future"));
        // A long-lived code is refused even when properly signed.
        let mut claim = decode(&c.code, T).unwrap();
        claim.exp = claim.iat + MAX_TTL_S + 1;
        let long = resign(&key(), PURPOSE_NODE_LINK, &canonical(&claim));
        assert!(decode(&long, T).unwrap_err().contains("lifetime"));
        assert!(make(&key(), NODE, None, T, MAX_TTL_S + 1).is_err());
        assert!(make(&key(), NODE, None, T, 0).is_err());
    }

    #[test]
    fn only_the_canonical_spelling_is_read() {
        let claim = decode(&vector().code, T).unwrap();
        let spaced = String::from_utf8(canonical(&claim))
            .unwrap()
            .replace(',', ", ");
        let code = resign(&key(), PURPOSE_NODE_LINK, spaced.as_bytes());
        assert!(decode(&code, T).is_err());
        // Unknown keys, a wrong version, upper-case hex, a bare id.
        for bad in [
            String::from_utf8(canonical(&claim))
                .unwrap()
                .replace("\"v\":1", "\"v\":2"),
            String::from_utf8(canonical(&claim))
                .unwrap()
                .replace("\"v\":1", "\"v\":1,\"x\":1"),
            String::from_utf8(canonical(&claim))
                .unwrap()
                .replace(NODE, &NODE.to_uppercase()),
        ] {
            assert!(decode(&resign(&key(), PURPOSE_NODE_LINK, bad.as_bytes()), T).is_err());
        }
        for junk in [
            "",
            NODE,
            "kmplink1",
            "kmplink1..",
            "kmplink2.a.b",
            "kmplink1.a.b.c",
        ] {
            assert!(decode(junk, T).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn nonces_and_ids_are_checked_when_making_a_code() {
        assert!(make(&key(), NODE, Some("short"), T, 600).is_err());
        assert!(make(&key(), NODE, Some("has space in it"), T, 600).is_err());
        assert!(make(&key(), NODE, Some(&"a".repeat(65)), T, 600).is_err());
        assert!(
            make(&key(), NODE, Some("  "), T, 600).is_ok(),
            "blank means none"
        );
        assert!(make(&key(), "abc", None, T, 600).is_err());
        assert!(make(&key(), "zz79ed9d11112222aaaabbbbcccc0000", None, T, 600).is_err());
    }

    #[test]
    fn an_unbound_code_says_who_can_redeem_it_and_how_to_bind() {
        let c = vector();
        let unbound = instructions(&c, false).join(" ");
        assert!(
            unbound.contains("whoever has this code within 10 minutes"),
            "{unbound}"
        );
        assert!(unbound.contains("KMPLIFY Desktop") && unbound.contains("--nonce"));
        let bound = instructions(&c, true).join(" ");
        assert!(bound.contains("for the account that showed the challenge"));
        assert!(!bound.contains("whoever has this code"));
    }

    #[test]
    fn a_code_is_made_from_the_credential_file_only() {
        let dir = std::env::temp_dir().join(format!("kmplify-node-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fabric_node.json");
        assert!(for_credentials(&path, None)
            .unwrap_err()
            .contains("not joined"));
        std::fs::write(&path, format!("{{\"node_id\":\"{NODE}\",\"token\":\"t\"}}")).unwrap();
        assert!(for_credentials(&path, None)
            .unwrap_err()
            .contains("no identity key"));
        std::fs::write(
            &path,
            format!("{{\"node_id\":\"{NODE}\",\"token\":\"t\",\"seed_hex\":\"{SEED}\"}}"),
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let c = for_credentials(&path, None).unwrap();
        let claim = decode(&c.code, identity::now_s()).unwrap();
        assert_eq!((claim.node_id.as_str(), claim.pubkey.as_str()), (NODE, PUB));
        assert_eq!(c.expires_at - c.issued_at, DEFAULT_TTL_S);
        assert_eq!(std::fs::read(&path).unwrap(), before, "never writes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
