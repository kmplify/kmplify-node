//! Stamp the build with the commit it came from.
//!
//! A version number alone cannot tell a tagged release from a local build
//! many commits past it, and "which build is that peer running" is a question
//! the fabric asks constantly. Absent git (a tarball, a vendored build) this
//! is simply empty and version_string() falls back to the crate version.

use std::path::Path;
use std::process::Command;

/// The paths whose contents reach the binary: the crate's sources, its
/// manifests, this script, and NOTICE, which `src/lib.rs` embeds with
/// `include_str!`. Everything else in the repository can be edited without
/// changing a single byte of the output, so it must not flip the stamp.
const BUILD_INPUTS: [&str; 5] = ["Cargo.toml", "Cargo.lock", "build.rs", "src", "NOTICE"];

fn main() {
    // Emitting ANY rerun-if directive replaces Cargo's default "rerun when
    // any file in the package changed" — rerun-if-env-changed included,
    // despite what an earlier revision here believed. With only the env
    // directive the script never reran on an incremental rebuild, so a
    // binary built right after a commit still carried the PREVIOUS commit's
    // stamp until a cargo clean. So the inputs that decide the stamp are
    // declared explicitly instead: the sources whose edits flip the dirty
    // flag, and the git files that move when HEAD does.
    println!("cargo:rerun-if-env-changed=KMPLIFY_BUILD");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src");

    if std::env::var("KMPLIFY_BUILD").is_ok() {
        return;
    }

    // Resolve against THIS crate rather than the current directory. As a path
    // dependency the script runs inside someone else's build, and a bare
    // `git rev-parse` walks up: with no .git of our own (a tarball, or
    // `cargo vendor`) it would happily stamp the ENCLOSING repository's
    // commit, which is worse than not stamping at all.
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    if dir.is_empty() || !Path::new(&dir).join(".git").exists() {
        return;
    }

    // Rerun when HEAD moves — a commit, a checkout — even when no source
    // file changed, so the stamp follows the tree out of "-dirty". The
    // gitdir is resolved through git rather than assumed at .git/: in a
    // worktree or submodule .git is a FILE, and declaring a path that never
    // exists made cargo rerun the script every single build, which is the
    // trap the old always-default behaviour fell into from the other side.
    if let Some(gitdir) = git(&dir, &["rev-parse", "--absolute-git-dir"], &[]) {
        let head = Path::new(&gitdir).join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
        // The branch ref HEAD names advances on every commit. Ask git where
        // that ref's file IS rather than joining it onto the gitdir: in a
        // linked worktree the gitdir is .git/worktrees/<name>, while refs
        // live in the COMMON dir, so the joined path never existed and the
        // watch was silently dropped — a commit made in a worktree then
        // changed no watched file and the next build stamped the PREVIOUS
        // commit. `--git-path` knows the split; its answer can be relative
        // to the crate root, so resolve it there.
        if let Some(refname) = git(&dir, &["symbolic-ref", "-q", "HEAD"], &[]) {
            if let Some(p) = git(&dir, &["rev-parse", "--git-path", &refname], &[]) {
                let r = Path::new(&dir).join(p);
                // Can be absent when refs are packed (after a gc);
                // committing writes the loose file again, and the HEAD and
                // source watches cover until then.
                if r.exists() {
                    println!("cargo:rerun-if-changed={}", r.display());
                }
            }
        }
    }

    let Some(short) = git(&dir, &["rev-parse", "--short", "HEAD"], &[]) else {
        return;
    };
    // An uncommitted tree matches no commit at all, so say so rather than
    // claiming the commit it was branched from.
    //
    // Only the files this binary is COMPILED FROM count. A whole-tree
    // `git status` is wrong wherever the build sees a partial checkout of
    // the repository: packaging/Dockerfile.node-build copies exactly these
    // paths plus .git into the image, so git found the other 38 tracked
    // files "deleted" and every containerised build stamped -dirty on a
    // pristine commit (seen on the EX44 node: 0.6.1+1d40365-dirty from a
    // tree with nothing modified). A stamp nobody can trust is worse than
    // no stamp, because "which build is that peer running" is answered
    // from it. Editing a README genuinely does not change the binary.
    let dirty = match git(&dir, &["status", "--porcelain", "--"], &BUILD_INPUTS) {
        Some(s) if !s.is_empty() => "-dirty",
        _ => "",
    };
    println!("cargo:rustc-env=KMPLIFY_BUILD={short}{dirty}");
}

fn git(dir: &str, args: &[&str], paths: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir])
        .args(args)
        .args(paths)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
