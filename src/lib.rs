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
pub mod engines;
pub mod fabric_worker;
pub mod functions;
pub mod gpu;
pub mod hostcpu;
pub mod identity;
pub mod peers;
pub mod proc;
pub mod rewards;
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

#[cfg(test)]
mod build_stamp_tests {
    /// build.rs decides "-dirty" from a fixed list of paths, and
    /// packaging/Dockerfile.node-build copies a fixed list of paths into the
    /// build image. They have to agree: a source the Dockerfile copies but
    /// the list omits is a source whose edits silently do NOT flip the
    /// stamp, and a path in the list the image never receives makes every
    /// containerised build dirty again (which is exactly the bug this
    /// guards, EX44 stamping 0.6.1+1d40365-dirty on a pristine tree).
    #[test]
    fn dirty_check_covers_every_source_the_build_image_receives() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let build_rs = std::fs::read_to_string(root.join("build.rs")).unwrap();
        let inputs: Vec<String> = build_rs
            .split_once("const BUILD_INPUTS")
            // Past the `: [&str; N] =` type annotation, whose brackets are
            // not the array literal's.
            .and_then(|(_, tail)| tail.split_once('='))
            .and_then(|(_, tail)| tail.split_once('['))
            .and_then(|(_, tail)| tail.split_once(']'))
            .map(|(list, _)| {
                list.split(',')
                    .filter_map(|s| s.trim().strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                    .map(str::to_owned)
                    .collect()
            })
            .expect("BUILD_INPUTS array in build.rs");
        assert!(inputs.contains(&"src".to_string()), "{inputs:?}");

        let dockerfile =
            std::fs::read_to_string(root.join("packaging/Dockerfile.node-build")).unwrap();
        // Everything the image copies, minus .git (metadata, not a source)
        // and LICENSE (shipped for the build, never compiled in).
        let copied: Vec<String> = dockerfile
            .lines()
            .filter_map(|l| l.trim().strip_prefix("COPY "))
            // `COPY --from=<stage>` moves a build artefact between stages;
            // its operands are container paths, not repository sources.
            .filter(|rest| !rest.contains("--from="))
            .flat_map(|rest| {
                let mut parts: Vec<&str> = rest.split_whitespace().collect();
                parts.pop(); // the destination
                parts
            })
            .map(|p| p.trim_start_matches("./").to_owned())
            .filter(|p| p != ".git" && p != "LICENSE")
            .collect();
        assert!(!copied.is_empty(), "no COPY lines parsed from the Dockerfile");
        for path in &copied {
            assert!(
                inputs.contains(path),
                "packaging/Dockerfile.node-build copies {path:?} into the build, but \
                 build.rs' BUILD_INPUTS does not watch it: edits to it would not mark \
                 the stamp dirty. Add it to BUILD_INPUTS."
            );
        }
        for path in &inputs {
            assert!(
                root.join(path).exists(),
                "BUILD_INPUTS names {path:?}, which does not exist in the crate"
            );
        }
    }
}
