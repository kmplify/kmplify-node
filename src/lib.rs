//! kmplify-node: the KMPLIFY GPU Fabric provider agent.
//!
//! This is the half of the fabric that runs on **your** machine. It dials out
//! to a gateway, advertises what the machine can serve, executes inference
//! jobs against a local model server, and (only if you opt in) hosts
//! container sessions on your GPU. Nothing here ever listens on a port: the
//! node opens one outbound WebSocket and everything travels over it, so
//! joining a fabric never exposes your machine to the internet.
//!
//! The scheduler, registry, billing and marketplace live on the gateway and
//! are not part of this crate. The split is deliberate and permanent: what
//! runs on your hardware is open, what runs on ours is not.
//!
//! See PROTOCOL.md for the wire format and the trust model, and README.md for
//! the operator's view.

pub mod control;
pub mod fabric_worker;
pub mod functions;
pub mod gpu;
pub mod hostcpu;
pub mod peers;
pub mod proc;
pub mod settings;
pub mod status;
pub mod vectors;

/// This crate's NOTICE, compiled in.
///
/// Apache-2.0 section 4(d) obliges anything that redistributes this code to
/// carry it, and an embedder shipping an installer is exactly that. Exposed
/// as a constant so the obligation can be met from the crate itself rather
/// than by copying a file out of a checkout: consumed through a git or
/// registry dependency there IS no checkout to copy from, and a hand-kept
/// copy is one that can silently stop matching what is actually linked.
pub const NOTICE: &str = include_str!("../NOTICE");

/// The public KMPLIFY fabric, used when `PROVIDER_GATEWAY_URL` is unset.
///
/// Pointing this at your own gateway is a supported, first-class setup: the
/// protocol is documented and there is nothing proprietary on this side of
/// the socket.
pub const PUBLIC_FABRIC_URL: &str = "https://fabric.kmplify.io";

/// Set by build.rs from `git describe`, empty for a build from a tarball.
const BUILD_STAMP: &str = match option_env!("KMPLIFY_BUILD") {
    Some(s) => s,
    None => "",
};

/// The version this node reports to the gateway, e.g. `0.1.0+1a2b3c4`.
///
/// Suffixed with the commit, because the version alone cannot tell a tagged
/// release from a local build many commits ahead of it: both say "0.1.0". A
/// peer reported exactly that while carrying unreleased protocol support, and
/// the only way to find out was to probe its behaviour and guess wrong once.
/// A `-dirty` suffix also says the tree had uncommitted edits, so it matches
/// no commit at all.
pub fn version_string() -> &'static str {
    static FULL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FULL.get_or_init(|| {
        if BUILD_STAMP.is_empty() {
            env!("CARGO_PKG_VERSION").to_string()
        } else {
            format!("{}+{}", env!("CARGO_PKG_VERSION"), BUILD_STAMP)
        }
    })
}
