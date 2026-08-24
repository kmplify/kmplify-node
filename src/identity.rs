//! This node's public identity, published so nothing has to read its secret.
//!
//! A node's credential file (`fabric_node.json`) holds two things: the node id,
//! which is public and is the handle consumers pin and invite, and the gateway
//! token, which is the node itself. Anything that wants the first currently has
//! to open a file containing the second — the Chaingence payment plugin does
//! exactly that today, and so would any other companion an operator installs.
//!
//! That is a bad shape for a machine that is supposed to be lending hardware to
//! strangers: it teaches operators that "read the node's credential file" is a
//! normal thing for a program to do. So the node publishes the public half on
//! its own, in `identity.json`, and the contract in [`docs/REWARDS.md`] is
//! blunt about it: **a companion reads this file, never the credential.**
//!
//! Nothing here knows what a companion does with it. This module publishes
//! facts about this node; rewards, wallets and tokens are somebody else's
//! repository, by design (see the boundary rules in the same document).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the node directory.
pub const IDENTITY_FILE: &str = "identity.json";

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
    pub fn of(node_id: &str, gateway: &str) -> Self {
        Self {
            schema: 1,
            node_id: node_id.to_string(),
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
    /// node id is on the wire in every hello frame, and the gateway URL is a
    /// URL — and the point of the file is that a companion running as another
    /// local user can read the node's id without being handed its token. The
    /// directory's own permissions still govern who can get to it.
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
pub fn publish_for(node_dir: &Path, node_id: &str, gateway: &str) {
    if node_id.is_empty() {
        return;
    }
    if let Err(e) = Identity::of(node_id, gateway).publish(node_dir) {
        crate::status::push_log(format!(
            "could not publish {}: {e}",
            path(node_dir).display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn what_is_published_is_public_and_only_public() {
        let dir = temp_dir("public");
        publish_for(&dir, "abc123", "https://fabric.kmplify.io");
        let raw = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(raw.contains("abc123"));
        // The one thing this file must never carry, named the way the
        // credential file names it.
        assert!(
            !raw.contains("token"),
            "the gateway token must never leak here"
        );
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "abc123");
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
        publish_for(&dir, "", "https://fabric.kmplify.io");
        assert!(Identity::read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn republishing_replaces_rather_than_appends() {
        let dir = temp_dir("replace");
        publish_for(&dir, "one", "https://a.example");
        publish_for(&dir, "two", "https://b.example");
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "two");
        assert_eq!(back.gateway, "https://b.example");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_files_missing_fields_do_not_fail_the_read() {
        let dir = temp_dir("older");
        std::fs::write(path(&dir), br#"{"node_id":"old"}"#).unwrap();
        let back = Identity::read(&dir).unwrap();
        assert_eq!(back.node_id, "old");
        assert_eq!(back.schema, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
