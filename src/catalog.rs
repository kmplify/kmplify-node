//! The signed workload catalog, verified on the node (protocol v3.1).
//!
//! Until this existed, which image may run for a template was decided by one
//! compiled-in table (`IMAGE_PINS`). That is the right property and the wrong
//! distribution: a new template needed a gateway release, a node build, and
//! every provider to upgrade. A gateway's word was never an option, because a
//! node does not trust the gateway: it is the far end of a socket, asking to
//! run containers on someone else's machine.
//!
//! So the catalog is a document signed by a PUBLISHER, and this node believes
//! it only under a key its owner configured (`KMPLIFY_TRUSTED_PUBLISHERS`).
//! KMPLIFY is one possible publisher, not a privileged one; nothing here
//! hardcodes a trust relationship with anybody. With no trusted key set, this
//! module is inert and the node behaves exactly as before.
//!
//! What a catalog can and cannot do:
//!
//! - It can ADD a template (id, repository, network ceiling) this build
//!   predates.
//! - It can REVOKE a template, compiled-in ones included.
//! - It can pin a DIGEST, and then the node pulls those exact bytes and
//!   ignores whatever tag the gateway sent.
//! - It cannot change the repository of a compiled-in pin. That conflict is
//!   the one shape a catalog-substitution attack takes, so the entry is
//!   refused and logged and the compiled-in pin stands.
//! - It cannot go backwards. The highest verified `catalog_version` is kept on
//!   disk, and an older document is refused however well it is signed, or a
//!   hostile gateway could replay yesterday's catalog to bring back a template
//!   that was revoked this morning.
//!
//! Consent is untouched: a signed entry is permission to run an image, never
//! an instruction to. The owner still opts in to template ids one by one.
//!
//! The signature covers the exact bytes of the `catalog` string in the
//! envelope. Verify first, parse second: no canonical form has to be agreed
//! between this crate and the gateway's Python.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use crate::fabric_worker::Network;

/// One signed entry, reduced to what this node enforces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Normalised the same way the compiled-in pins are compared.
    pub repository: String,
    /// The most network this template may be given. Absent or unknown reads
    /// as `None`: a typo in a catalog must not read as permission.
    pub network: Network,
    /// `sha256:<64 hex>` when the publisher pinned exact bytes.
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub version: u64,
    pub publisher: String,
    pub entries: HashMap<String, Entry>,
    pub revoked: HashSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// No publisher key is configured: the catalog lane is off.
    NoTrustedPublisher,
    Malformed(&'static str),
    UntrustedPublisher,
    BadSignature,
    /// Older than (or equal to a different document than) what was verified
    /// before. Carries (offered, held).
    Rollback(u64, u64),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoTrustedPublisher => write!(f, "no trusted publisher is configured"),
            Refused::Malformed(why) => write!(f, "malformed catalog: {why}"),
            Refused::UntrustedPublisher => {
                write!(f, "signed by a publisher this node does not trust")
            }
            Refused::BadSignature => write!(f, "signature does not verify"),
            Refused::Rollback(offered, held) => write!(
                f,
                "catalog v{offered} is older than v{held} already verified (rollback refused)"
            ),
        }
    }
}

/// Publisher keys from `KMPLIFY_TRUSTED_PUBLISHERS` (comma separated hex).
/// Anything that is not 32 bytes of hex is dropped, never half-trusted.
pub fn parse_trusted(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|k| k.trim().to_ascii_lowercase())
        .filter(|k| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect()
}

fn digest_ok(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Verify an envelope and turn it into a catalog. `held_version` is the
/// highest version this node has verified before.
///
/// `floor` answers "which repository does the compiled-in table pin for this
/// template", so a conflicting entry can be refused. `normalise` reduces an
/// image reference to the comparable repository form.
pub fn verify(
    envelope: &Value,
    trusted: &[String],
    held_version: u64,
    floor: &dyn Fn(&str) -> Option<String>,
    normalise: &dyn Fn(&str) -> String,
) -> Result<(Catalog, Vec<String>), Refused> {
    if trusted.is_empty() {
        return Err(Refused::NoTrustedPublisher);
    }
    let text = envelope["catalog"]
        .as_str()
        .ok_or(Refused::Malformed("no catalog string"))?;
    let pubkey = envelope["pubkey"]
        .as_str()
        .ok_or(Refused::Malformed("no pubkey"))?
        .to_ascii_lowercase();
    let sig_hex = envelope["signature"]
        .as_str()
        .ok_or(Refused::Malformed("no signature"))?;
    if !trusted.contains(&pubkey) {
        return Err(Refused::UntrustedPublisher);
    }
    let key: [u8; 32] = crate::functions::hex_decode(&pubkey)
        .and_then(|b| b.try_into().ok())
        .ok_or(Refused::Malformed("pubkey is not 32 bytes of hex"))?;
    let key =
        VerifyingKey::from_bytes(&key).map_err(|_| Refused::Malformed("pubkey is not a key"))?;
    let sig = crate::functions::hex_decode(sig_hex)
        .and_then(|b| Signature::from_slice(&b).ok())
        .ok_or(Refused::Malformed("signature is not 64 bytes of hex"))?;
    // The bytes of the string exactly as received. Nothing is re-serialised.
    key.verify(text.as_bytes(), &sig)
        .map_err(|_| Refused::BadSignature)?;

    // Only now is the content worth reading.
    let doc: Value =
        serde_json::from_str(text).map_err(|_| Refused::Malformed("catalog is not JSON"))?;
    let version = doc["catalog_version"]
        .as_u64()
        .filter(|v| *v >= 1)
        .ok_or(Refused::Malformed("no catalog_version"))?;
    if version < held_version {
        return Err(Refused::Rollback(version, held_version));
    }

    let mut catalog = Catalog {
        version,
        publisher: doc["publisher"].as_str().unwrap_or_default().to_string(),
        ..Default::default()
    };
    let mut notes = Vec::new();
    for e in doc["entries"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let (Some(id), Some(repo)) = (e["id"].as_str(), e["repository"].as_str()) else {
            continue;
        };
        if id.is_empty() || repo.is_empty() {
            continue;
        }
        let repository = normalise(repo);
        if let Some(pinned) = floor(id) {
            if pinned != repository {
                // The substitution attack, or a publisher's honest mistake.
                // Either way the build's own pin wins, loudly.
                notes.push(format!(
                    "catalog v{version}: entry '{id}' names {repository}, this build pins \
                     {pinned} — entry refused, the compiled-in pin stands"
                ));
                continue;
            }
        }
        let digest = match e["digest"].as_str() {
            None => None,
            Some(d) if digest_ok(d) => Some(d.to_string()),
            Some(_) => {
                // A digest we cannot read is not "no digest": the publisher
                // meant to pin bytes, and running unpinned would betray that.
                notes.push(format!(
                    "catalog v{version}: entry '{id}' has an unreadable digest — entry refused"
                ));
                continue;
            }
        };
        catalog.entries.insert(
            id.to_string(),
            Entry {
                repository,
                network: Network::from_frame(e["network"].as_str()),
                digest,
            },
        );
    }
    for r in doc["revoked"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if let Some(id) = r.as_str() {
            catalog.revoked.insert(id.to_string());
        }
    }
    Ok((catalog, notes))
}

/// `image` rewritten to pull exactly `digest`: registry and repository kept,
/// any tag or digest it came with dropped.
pub fn with_digest(image: &str, digest: &str) -> String {
    let base = image.split_once('@').map_or(image, |(b, _)| b);
    let cut = match base.rfind('/') {
        Some(slash) => base[slash..].find(':').map(|c| slash + c),
        None => base.find(':'),
    };
    format!("{}@{digest}", &base[..cut.unwrap_or(base.len())])
}

// ----- the catalog this process holds ---------------------------------------

fn held() -> &'static RwLock<Option<Catalog>> {
    static HELD: OnceLock<RwLock<Option<Catalog>>> = OnceLock::new();
    HELD.get_or_init(|| RwLock::new(None))
}

pub fn current() -> Option<Catalog> {
    held().read().ok().and_then(|g| g.clone())
}

pub fn install(catalog: Catalog) {
    if let Ok(mut g) = held().write() {
        *g = Some(catalog);
    }
}

#[cfg(test)]
pub fn clear() {
    if let Ok(mut g) = held().write() {
        *g = None;
    }
}

pub fn version() -> u64 {
    current().map_or(0, |c| c.version)
}

/// Is this template revoked by the verified catalog?
pub fn revoked(template: &str) -> bool {
    current().is_some_and(|c| c.revoked.contains(template))
}

pub fn entry(template: &str) -> Option<Entry> {
    current().and_then(|c| c.entries.get(template).cloned())
}

// ----- rollback state on disk -----------------------------------------------

pub fn state_path(node_dir: &Path) -> PathBuf {
    node_dir.join("catalog_state.json")
}

/// The highest catalog version this install has ever verified. A missing or
/// unreadable file is 0: a fresh install has nothing to be rolled back from.
pub fn load_held_version(node_dir: &Path) -> u64 {
    std::fs::read_to_string(state_path(node_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["catalog_version"].as_u64())
        .unwrap_or(0)
}

pub fn save_held_version(node_dir: &Path, version: u64) -> std::io::Result<()> {
    let path = state_path(node_dir);
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::json!({"catalog_version": version}).to_string(),
    )?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    // RFC 8032 test vector 1, the same pair the gateway's tests use.
    const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const PUBKEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn envelope(text: &str, seed_hex: &str) -> Value {
        let seed: [u8; 32] = crate::functions::hex_decode(seed_hex)
            .unwrap()
            .try_into()
            .unwrap();
        let key = SigningKey::from_bytes(&seed);
        json!({
            "catalog": text,
            "signature": crate::identity::hex_encode(&key.sign(text.as_bytes()).to_bytes()),
            "pubkey": crate::identity::hex_encode(key.verifying_key().as_bytes()),
        })
    }

    fn floor(id: &str) -> Option<String> {
        (id == "ollama").then(|| "ollama/ollama".to_string())
    }
    fn same(s: &str) -> String {
        s.to_string()
    }
    fn check(text: &str, held: u64) -> Result<(Catalog, Vec<String>), Refused> {
        verify(
            &envelope(text, SEED),
            &[PUBKEY.to_string()],
            held,
            &floor,
            &same,
        )
    }

    const DIGEST: &str = "sha256:abababababababababababababababababababababababababababababababab";

    /// An envelope minted by the gateway's `app/catalog.py` (`catalog.sign`)
    /// with this seed, pasted verbatim: the text AND the signature are the
    /// Python side's output. If the two sides ever disagree about which bytes
    /// are signed, this is where it shows. Ed25519 is deterministic, so the
    /// gateway's own test pins the same signature for the same document.
    #[test]
    fn a_catalog_minted_by_the_gateway_verifies_here() {
        let text = r#"{"catalog_version":3,"entries":[{"accelerator":"cpu","id":"echo-test","isolation":"container","network":"none","port":80,"repository":"traefik/whoami"}],"issued_at":1,"publisher":"kmplify","revoked":[]}"#;
        let env = json!({
            "catalog": text,
            "pubkey": PUBKEY,
            "signature": "f98d1ab15e1692aca04e2bd6155bd76e94934436ed9060f35cb6660b28dd31d4150a632527fa7c53b1aafac5c6eeb8c5839c5efaad6689dbcde959a0e1e69f02",
        });
        let (c, notes) = verify(&env, &[PUBKEY.to_string()], 0, &floor, &same).unwrap();
        assert_eq!(c.version, 3);
        assert_eq!(c.publisher, "kmplify");
        assert!(notes.is_empty());
        assert_eq!(c.entries["echo-test"].repository, "traefik/whoami");
        assert_eq!(c.entries["echo-test"].network, Network::None);
        // And this crate's own signer agrees with Python's, byte for byte.
        assert_eq!(envelope(text, SEED)["signature"], env["signature"]);
    }

    #[test]
    fn without_a_trusted_publisher_the_lane_is_off() {
        let env = envelope(r#"{"catalog_version":1,"entries":[],"revoked":[]}"#, SEED);
        assert_eq!(
            verify(&env, &[], 0, &floor, &same).unwrap_err(),
            Refused::NoTrustedPublisher
        );
    }

    /// A perfectly valid signature by somebody this owner never chose.
    #[test]
    fn a_stranger_s_signature_is_not_trust() {
        let other = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";
        let env = envelope(r#"{"catalog_version":1,"entries":[],"revoked":[]}"#, other);
        assert_eq!(
            verify(&env, &[PUBKEY.to_string()], 0, &floor, &same).unwrap_err(),
            Refused::UntrustedPublisher
        );
    }

    #[test]
    fn one_changed_character_is_refused() {
        let mut env = envelope(r#"{"catalog_version":1,"entries":[],"revoked":[]}"#, SEED);
        env["catalog"] = json!(r#"{"catalog_version":2,"entries":[],"revoked":[]}"#);
        assert_eq!(
            verify(&env, &[PUBKEY.to_string()], 0, &floor, &same).unwrap_err(),
            Refused::BadSignature
        );
    }

    /// Yesterday's catalog, correctly signed, replayed to bring back a
    /// template that was revoked this morning.
    #[test]
    fn a_correctly_signed_older_catalog_is_a_rollback() {
        let text = r#"{"catalog_version":5,"entries":[],"revoked":[]}"#;
        assert_eq!(check(text, 9).unwrap_err(), Refused::Rollback(5, 9));
        assert!(
            check(text, 5).is_ok(),
            "the same version again is not a rollback"
        );
        assert!(check(text, 0).is_ok());
    }

    /// The substitution attack: a signed entry that renames what a
    /// compiled-in template runs.
    #[test]
    fn a_catalog_cannot_replace_a_compiled_in_repository() {
        let text = r#"{"catalog_version":1,"entries":[{"id":"ollama","repository":"attacker/miner","network":"egress"},{"id":"newthing","repository":"good/newthing","network":"egress"}],"revoked":[]}"#;
        let (c, notes) = check(text, 0).unwrap();
        assert!(!c.entries.contains_key("ollama"));
        assert!(c.entries.contains_key("newthing"));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("compiled-in pin stands"));
    }

    #[test]
    fn an_entry_agreeing_with_the_floor_may_add_a_digest() {
        let text = format!(
            r#"{{"catalog_version":1,"entries":[{{"id":"ollama","repository":"ollama/ollama","network":"egress","digest":"{DIGEST}"}}],"revoked":[]}}"#
        );
        let (c, notes) = check(&text, 0).unwrap();
        assert!(notes.is_empty());
        assert_eq!(c.entries["ollama"].digest.as_deref(), Some(DIGEST));
    }

    /// The publisher MEANT to pin bytes. Running unpinned would betray that.
    #[test]
    fn an_unreadable_digest_refuses_the_entry_rather_than_dropping_the_pin() {
        let text = r#"{"catalog_version":1,"entries":[{"id":"x","repository":"a/x","digest":"sha256:short"}],"revoked":[]}"#;
        let (c, notes) = check(text, 0).unwrap();
        assert!(c.entries.is_empty());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn a_typo_in_network_is_not_permission() {
        let text = r#"{"catalog_version":1,"entries":[{"id":"x","repository":"a/x","network":"egres"},{"id":"y","repository":"a/y"}],"revoked":[]}"#;
        let (c, _) = check(text, 0).unwrap();
        assert_eq!(c.entries["x"].network, Network::None);
        assert_eq!(c.entries["y"].network, Network::None);
    }

    #[test]
    fn a_digest_replaces_the_tag_and_keeps_the_registry() {
        assert_eq!(
            with_digest("traefik/whoami:latest", DIGEST),
            format!("traefik/whoami@{DIGEST}")
        );
        assert_eq!(
            with_digest("ghcr.io/a/b:1.2", DIGEST),
            format!("ghcr.io/a/b@{DIGEST}")
        );
        assert_eq!(
            with_digest("localhost:5000/a/b:1", DIGEST),
            format!("localhost:5000/a/b@{DIGEST}")
        );
        assert_eq!(with_digest("a/b", DIGEST), format!("a/b@{DIGEST}"));
        // A digest the gateway sent is not believed over the publisher's.
        assert_eq!(
            with_digest(
                "a/b@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                DIGEST
            ),
            format!("a/b@{DIGEST}")
        );
    }

    #[test]
    fn only_real_keys_are_trusted() {
        assert_eq!(
            parse_trusted(&format!(" {PUBKEY} , nonsense,, {}", "ab".repeat(31))),
            vec![PUBKEY]
        );
        assert!(parse_trusted("").is_empty());
    }

    #[test]
    fn the_held_version_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("kmplify-catalog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_held_version(&dir), 0);
        save_held_version(&dir, 41).unwrap();
        assert_eq!(load_held_version(&dir), 41);
        std::fs::write(state_path(&dir), "not json").unwrap();
        assert_eq!(load_held_version(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
