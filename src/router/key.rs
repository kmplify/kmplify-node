//! The cluster key and where it is kept.
//!
//! Compiled in every build (also without the `router` feature), because the
//! views' key actions ("New cluster key", "Join a cluster") and the
//! fingerprint on every screen need it, and an embedding application keeps
//! the key somewhere of its own ([`KeyStore`]).
//!
//! The node keeps it in `router.key` in the node directory, owner-only
//! ([`FileKeyStore`]), never in `settings.json` (owner rule: no secret in a
//! settings file). kmplify-desktop keeps it in its own credentials store and
//! hands the router a [`KeyStore`] of its own; the key's format and its
//! fingerprint are the same everywhere, so a desktop and a node join one
//! cluster with the same 64 hex characters.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// File in the node directory holding the cluster key, hex.
pub const KEY_FILE: &str = "router.key";

pub(crate) const DOMAIN: &str = "kmplify-router-v1";

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The shared secret of one cluster: 32 random bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct ClusterKey([u8; 32]);

impl std::fmt::Debug for ClusterKey {
    // Never the bytes, wherever a key ends up being debug-printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClusterKey({})", self.fingerprint())
    }
}

impl ClusterKey {
    /// A fresh key from the operating system's random source.
    pub fn generate() -> Self {
        use rand_core::RngCore;
        let mut b = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut b);
        Self(b)
    }

    /// 64 hex characters, as `router key` prints it and `router join` reads it.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bytes = unhex(s).filter(|b| b.len() == 32).ok_or(
            "a cluster key is 64 hexadecimal characters, as `kmplify-node router key` prints it",
        )?;
        let mut b = [0u8; 32];
        b.copy_from_slice(&bytes);
        Ok(Self(b))
    }

    /// The key as text, for the person who asked to see it, or for a store
    /// that keeps it. Never logged.
    pub fn reveal(&self) -> String {
        hex(&self.0)
    }

    /// Eight hex characters of SHA-256 over the key: enough to tell two
    /// clusters apart on a screen or in an mDNS record, useless for finding
    /// the key.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest([DOMAIN.as_bytes(), b"/fingerprint/", &self.0].concat());
        hex(&digest[..4])
    }

    #[cfg(feature = "router")]
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }
}

/// Where a router keeps its cluster key.
///
/// `load` is asked once a second by a running router, so a store that is
/// slow to read (an OS credential store) caches. An error is not "no key":
/// the router then neither makes a new key nor overwrites the stored one,
/// and says why.
pub trait KeyStore: Send + Sync {
    fn load(&self) -> Result<Option<ClusterKey>, String>;
    fn save(&self, key: &ClusterKey) -> Result<(), String>;
    /// Where the key is kept, in words, for messages.
    fn place(&self) -> String;
}

/// The node's store: `router.key` in the node directory, owner-only (0600;
/// on Windows the current user and SYSTEM only).
#[derive(Clone, Debug)]
pub struct FileKeyStore {
    dir: PathBuf,
}

impl FileKeyStore {
    pub fn new(node_dir: &Path) -> Self {
        Self {
            dir: node_dir.to_path_buf(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }
}

impl KeyStore for FileKeyStore {
    /// An unreadable or malformed file is no key (the router then stays
    /// local and makes a new one when a mode needs it), as in phase 1.
    fn load(&self) -> Result<Option<ClusterKey>, String> {
        Ok(std::fs::read_to_string(self.path())
            .ok()
            .and_then(|s| ClusterKey::parse(&s).ok()))
    }

    fn save(&self, key: &ClusterKey) -> Result<(), String> {
        // The node's one owner-only writer is async (and on Windows runs
        // icacls). A key is saved rarely, from a view's button, the CLI or
        // the router's first start, so it gets a thread and a small runtime
        // of its own: safe from inside a runtime and from plain threads.
        let path = self.path();
        let bytes = format!("{}\n", key.reveal()).into_bytes();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(crate::fabric_worker::write_private(&path, bytes))
                .map_err(|e| e.to_string())
        })
        .join()
        .map_err(|_| "the key writer stopped".to_string())?
        .map_err(|e| format!("cannot store the cluster key in {KEY_FILE}: {e}"))
    }

    fn place(&self) -> String {
        KEY_FILE.into()
    }
}

/// The fingerprint of the key in `store`, or "" without one.
pub fn fingerprint_of(store: &dyn KeyStore) -> String {
    store
        .load()
        .ok()
        .flatten()
        .map(|k| k.fingerprint())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_store_is_owner_only_and_forgiving() {
        let dir = std::env::temp_dir().join(format!("kmplify-router-ks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileKeyStore::new(&dir);
        assert_eq!(store.load(), Ok(None));
        assert_eq!(fingerprint_of(&store), "");
        let k = ClusterKey::generate();
        store.save(&k).unwrap();
        assert_eq!(store.load(), Ok(Some(k.clone())));
        assert_eq!(fingerprint_of(&store), k.fingerprint());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(store.path(), "garbage").unwrap();
        assert_eq!(store.load(), Ok(None), "a broken file is no key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The format and fingerprint other implementations (kmplify-desktop)
    /// must reproduce: 64 lower-case hex, fingerprint = first 4 bytes of
    /// SHA-256("kmplify-router-v1/fingerprint/" || key) as hex.
    #[test]
    fn the_fingerprint_is_stable() {
        let k = ClusterKey::from_bytes([0u8; 32]);
        assert_eq!(k.reveal(), "0".repeat(64));
        let want = hex(&Sha256::digest(
            [b"kmplify-router-v1/fingerprint/".as_slice(), &[0u8; 32]].concat(),
        )[..4]);
        assert_eq!(k.fingerprint(), want);
        assert_eq!(k.fingerprint().len(), 8);
    }
}
