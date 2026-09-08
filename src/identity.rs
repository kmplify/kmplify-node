//! This node's public identity, published so nothing has to read its secret.
//!
//! A node's credential file (`fabric_node.json`) holds three things: the node
//! id, which is public and is the handle consumers pin and invite; the gateway
//! token, which is the node itself as far as the gateway is concerned; and,
//! since protocol v3.7, the seed of the node's **identity key** — an Ed25519
//! keypair that is what makes this node *this* node rather than "whoever
//! holds the token". Anything that wants the public half currently has to
//! open a file containing the secret — the Chaingence payment plugin does
//! exactly that today, and so would any other companion an operator installs.
//!
//! That is a bad shape for a machine that is supposed to be lending hardware to
//! strangers: it teaches operators that "read the node's credential file" is a
//! normal thing for a program to do. So the node publishes the public half on
//! its own, in `identity.json`, and the contract in [`docs/REWARDS.md`] is
//! blunt about it: **a companion reads this file, never the credential.**
//!
//! # The identity key (KIP-1, protocol v3.7)
//!
//! The key follows the ecosystem-wide KMPLIFY identity contract
//! (`kmplify-infrastructure/docs/IDENTITY_KEYS.md`): Ed25519, a bech32m
//! address with the `kmpn` prefix, and domain-separated signatures over
//! canonical JSON. The node signs its registration and every hello, so a
//! gateway can tell a node from a copy of its token, and a person can claim
//! the node as theirs by signing its address with their own identity key
//! (that claim lives in their KMPLIFY account, never on the fabric — the
//! node stays anonymous here).
//!
//! This is NOT a wallet key. It signs statements about the node; it never
//! holds, receives or moves anything, which keeps the boundary in
//! `docs/REWARDS.md` exactly where it was.
//!
//! Nothing here knows what a companion does with it. This module publishes
//! facts about this node; rewards, wallets and tokens are somebody else's
//! repository, by design (see the boundary rules in the same document).

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// File name inside the node directory.
pub const IDENTITY_FILE: &str = "identity.json";

/// Protocol tag every signature's preimage starts with.
pub const PROTOCOL: &str = "KMPLIFY-ID-v1";
/// bech32m human-readable part of a NODE address.
pub const HRP_NODE: &str = "kmpn";
/// Signatures older or newer than this are refused by verifiers.
pub const MAX_CLOCK_SKEW_S: u64 = 300;

pub const PURPOSE_NODE_REGISTER: &str = "node-register";
pub const PURPOSE_NODE_HELLO: &str = "node-hello";

/// The node's Ed25519 identity key. Holds the secret; hand out only what the
/// accessors give you.
#[derive(Clone)]
pub struct NodeKey {
    signing: SigningKey,
}

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the seed, not even in a debug dump.
        f.debug_struct("NodeKey").field("address", &self.address()).finish()
    }
}

impl NodeKey {
    pub fn generate() -> Self {
        Self { signing: SigningKey::generate(&mut rand_core::OsRng) }
    }

    pub fn from_seed_hex(seed_hex: &str) -> Result<Self, String> {
        let bytes = crate::functions::hex_decode(seed_hex.trim()).ok_or("identity seed is not hex")?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| "identity seed must be 32 bytes".to_string())?;
        Ok(Self { signing: SigningKey::from_bytes(&seed) })
    }

    pub fn seed_hex(&self) -> String {
        hex_encode(&self.signing.to_bytes())
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn public_hex(&self) -> String {
        hex_encode(&self.public_bytes())
    }

    /// `kmpn1…` — the address consumers and operators refer to this node by.
    pub fn address(&self) -> String {
        bech32m_encode(HRP_NODE, &self.public_bytes())
    }

    /// Hex Ed25519 signature over `KMPLIFY-ID-v1/<purpose>\n` || canonical.
    pub fn sign(&self, purpose: &str, canonical: &[u8]) -> String {
        hex_encode(&self.signing.sign(&signing_input(purpose, canonical)).to_bytes())
    }

    /// The signed body of a `POST /fabric/register` (protocol v3.7).
    pub fn register_fields(&self, gateway: &str, ts: u64) -> serde_json::Value {
        let sig = self.sign(PURPOSE_NODE_REGISTER, &canonical_register(gateway, &self.public_hex(), ts));
        serde_json::json!({ "pubkey": self.public_hex(), "gateway": gateway, "ts": ts, "sig": sig })
    }

    /// The signed identity fields of a hello frame (protocol v3.7).
    pub fn hello_fields(&self, node_id: &str, ts: u64) -> serde_json::Value {
        let sig = self.sign(PURPOSE_NODE_HELLO, &canonical_hello(node_id, &self.public_hex(), ts));
        serde_json::json!({ "pubkey": self.public_hex(), "ts": ts, "sig": sig })
    }
}

/// `KMPLIFY-ID-v1/<purpose>\n` followed by the canonical JSON bytes.
pub fn signing_input(purpose: &str, canonical: &[u8]) -> Vec<u8> {
    let mut out = format!("{PROTOCOL}/{purpose}\n").into_bytes();
    out.extend_from_slice(canonical);
    out
}

/// Canonical JSON (sorted keys, no whitespace) of the register payload. The
/// gateway rebuilds exactly these bytes; see `canonical()` in functions.rs for
/// why this is spelled out rather than serialised from a struct.
pub fn canonical_register(gateway: &str, pubkey_hex: &str, ts: u64) -> Vec<u8> {
    format!(
        "{{\"gateway\":{},\"pubkey\":{},\"ts\":{}}}",
        serde_json::json!(gateway),
        serde_json::json!(pubkey_hex),
        ts
    )
    .into_bytes()
}

pub fn canonical_hello(node_id: &str, pubkey_hex: &str, ts: u64) -> Vec<u8> {
    format!(
        "{{\"node_id\":{},\"pubkey\":{},\"ts\":{}}}",
        serde_json::json!(node_id),
        serde_json::json!(pubkey_hex),
        ts
    )
    .into_bytes()
}

/// Verify a KIP-1 signature under a hex public key. Used by the tests and
/// available to any companion that wants to check what a node said.
pub fn verify(pubkey_hex: &str, purpose: &str, canonical: &[u8], sig_hex: &str) -> bool {
    let Some(pk) = crate::functions::hex_decode(pubkey_hex) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.try_into() else { return false };
    let Ok(key) = VerifyingKey::from_bytes(&pk) else { return false };
    let Some(sig) = crate::functions::hex_decode(sig_hex) else { return false };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig) else { return false };
    key.verify(&signing_input(purpose, canonical), &sig).is_ok()
}

pub fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ----- bech32m (BIP-350) -------------------------------------------------------

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const BECH32M_CONST: u32 = 0x2bc8_30a3;

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a_57b2, 0x2650_8e6d, 0x1ea1_19fa, 0x3d42_33dd, 0x2a14_62b3];
    let mut chk: u32 = 1;
    for &v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ff_ffff) << 5) ^ u32::from(v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|c| c >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|c| c & 31));
    out
}

fn to_5bit(data: &[u8]) -> Vec<u8> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(data.len() * 8 / 5 + 1);
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (5 - bits)) & 31) as u8);
    }
    out
}

/// bech32m-encode `payload` under `hrp`.
pub fn bech32m_encode(hrp: &str, payload: &[u8]) -> String {
    let data = to_5bit(payload);
    let mut values = hrp_expand(hrp);
    values.extend_from_slice(&data);
    values.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let pm = polymod(&values) ^ BECH32M_CONST;
    let mut s = String::with_capacity(hrp.len() + 1 + data.len() + 6);
    s.push_str(hrp);
    s.push('1');
    for d in &data {
        s.push(CHARSET[*d as usize] as char);
    }
    for i in 0..6 {
        s.push(CHARSET[((pm >> (5 * (5 - i))) & 31) as usize] as char);
    }
    s
}

// ----- the published file -----------------------------------------------------

/// The public half of a node's identity.
///
/// Every field is `#[serde(default)]` so a companion built against a newer or
/// older node still parses what it gets, exactly like the status snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    /// Schema of this file, bumped only on an incompatible change.
    pub schema: u32,
    /// The node id the gateway knows this machine by. Public: consumers pin
    /// it, invitations are addressed from it, and a rewards account binds to
    /// it.
    pub node_id: String,
    /// The node's Ed25519 public key, hex (protocol v3.7). Empty on a node
    /// that has not made a key yet.
    pub public_key: String,
    /// The same key as a `kmpn1…` address — what a person pastes into their
    /// KMPLIFY account to claim this node as theirs.
    pub address: String,
    /// The fabric this node joined. A companion that settles work has to know
    /// which fabric attested it.
    pub gateway: String,
    /// The build that published this, for support and for compatibility
    /// checks.
    pub version: String,
    pub os: String,
    pub arch: String,
    /// When this file was last written, unix ms.
    pub published_at_ms: u64,
}

pub fn path(node_dir: &Path) -> PathBuf {
    node_dir.join(IDENTITY_FILE)
}

impl Identity {
    /// The identity of the node running in this process.
    pub fn of(node_id: &str, gateway: &str, key: Option<&NodeKey>) -> Self {
        Self {
            schema: 2,
            node_id: node_id.to_string(),
            public_key: key.map(NodeKey::public_hex).unwrap_or_default(),
            address: key.map(NodeKey::address).unwrap_or_default(),
            gateway: gateway.to_string(),
            version: crate::version_string().to_string(),
            // From the compiler rather than from a runtime probe: it is the
            // binary that was built for this platform, which is the question
            // a companion is actually asking.
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            published_at_ms: crate::status::now_ms(),
        }
    }

    /// Write it, replacing whatever was there.
    ///
    /// Deliberately NOT owner-only. Everything in this file is public — the
    /// node id is on the wire in every hello frame, the public key is in
    /// every signed one, and the gateway URL is a URL — and the point of the
    /// file is that a companion running as another local user can read the
    /// node's id without being handed its token or its seed. The directory's
    /// own permissions still govern who can get to it.
    pub fn publish(&self, node_dir: &Path) -> std::io::Result<()> {
        let target = path(node_dir);
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &target)
    }

    /// Read a node's published identity, or `None` when none has been
    /// published here.
    pub fn read(node_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path(node_dir)).ok()?).ok()
    }
}

/// Publish the identity of the node about to start.
///
/// Idempotent and non-fatal: a node that cannot write this file still serves.
/// Losing a companion's convenience is not a reason to refuse to lend a GPU.
pub fn publish_for(node_dir: &Path, node_id: &str, gateway: &str, key: Option<&NodeKey>) {
    if node_id.is_empty() {
        return;
    }
    if let Err(e) = Identity::of(node_id, gateway, key).publish(node_dir) {
        crate::status::push_log(format!(
            "could not publish {}: {e}",
            path(node_dir).display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cross-language vectors every implementation pins itself to
    // (kmplify-infrastructure/docs/IDENTITY_KEYS.md). RFC 8032 test key 1.
    const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUB: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const NODE_ADDR: &str = "kmpn16adfsqvzky9t042tlmfujeq88g8wzuhnm2nzxfd0qgdx3ac82ydqv0z6f7";

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "kmplify-node-id-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn shared_vectors_pin_key_and_address() {
        let key = NodeKey::from_seed_hex(SEED).unwrap();
        assert_eq!(key.public_hex(), PUB);
        assert_eq!(key.address(), NODE_ADDR);
        assert_eq!(key.seed_hex(), SEED);
        // BIP-350 known vector.
        assert_eq!(bech32m_encode("a", &[]), "a1lqfn3a");
    }

    #[test]
    fn signatures_are_domain_separated_and_verify() {
        let key = NodeKey::from_seed_hex(SEED).unwrap();
        let canonical = canonical_hello("node-1", &key.public_hex(), 1_760_000_000);
        assert_eq!(
            String::from_utf8(canonical.clone()).unwrap(),
            format!("{{\"node_id\":\"node-1\",\"pubkey\":\"{PUB}\",\"ts\":1760000000}}")
        );
        let sig = key.sign(PURPOSE_NODE_HELLO, &canonical);
        assert!(verify(PUB, PURPOSE_NODE_HELLO, &canonical, &sig));
        assert!(!verify(PUB, PURPOSE_NODE_REGISTER, &canonical, &sig));
        let other = canonical_hello("node-2", &key.public_hex(), 1_760_000_000);
        assert!(!verify(PUB, PURPOSE_NODE_HELLO, &other, &sig));
        // The gateway's Python verifier rebuilds this exact preimage.
        assert_eq!(
            signing_input(PURPOSE_NODE_HELLO, b"{}"),
            b"KMPLIFY-ID-v1/node-hello\n{}".to_vec()
        );
    }

    #[test]
    fn generated_keys_are_distinct_and_round_trip() {
        let a = NodeKey::generate();
        let b = NodeKey::generate();
        assert_ne!(a.public_hex(), b.public_hex());
        let back = NodeKey::from_seed_hex(&a.seed_hex()).unwrap();
        assert_eq!(back.public_hex(), a.public_hex());
        assert!(a.address().starts_with("kmpn1"));
        assert!(NodeKey::from_seed_hex("abc").is_err());
    }

    #[test]
    fn what_is_published_is_public_and_only_public() {
        let dir = temp_dir("public");
        let key = NodeKey::from_seed_hex(SEED).unwrap();
        publish_for(&dir, "abc123", "https://fabric.kmplify.io", Some(&key));
        let raw = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(raw.contains("abc123"));
        assert!(raw.contains(NODE_ADDR));
        assert!(raw.contains(PUB));
        // The two things this file must never carry.
        assert!(!raw.contains("token"), "the gateway token must never leak here");
        assert!(!raw.contains(SEED), "the identity seed must never leak here");
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "abc123");
        assert_eq!(back.address, NODE_ADDR);
        assert_eq!(back.gateway, "https://fabric.kmplify.io");
        assert!(!back.version.is_empty());
        assert!(!back.os.is_empty() && !back.arch.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_node_with_no_identity_yet_publishes_nothing() {
        // Before registration there is no id to publish, and an empty one
        // would bind a rewards account to nothing.
        let dir = temp_dir("empty");
        publish_for(&dir, "", "https://fabric.kmplify.io", None);
        assert!(Identity::read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn republishing_replaces_rather_than_appends() {
        let dir = temp_dir("replace");
        publish_for(&dir, "one", "https://a.example", None);
        publish_for(&dir, "two", "https://b.example", None);
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "two");
        assert_eq!(back.gateway, "https://b.example");
        assert_eq!(back.address, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_files_missing_fields_do_not_fail_the_read() {
        let dir = temp_dir("older");
        std::fs::write(path(&dir), br#"{"node_id":"old"}"#).unwrap();
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "old");
        assert_eq!(back.schema, 0);
        assert_eq!(back.public_key, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
