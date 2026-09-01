//! Stamp the build with the commit it came from.
//!
//! A version number alone cannot tell a tagged release from a local build
//! many commits past it, and "which build is that peer running" is a question
//! the fabric asks constantly. Absent git (a tarball, a vendored build) this
//! is simply empty and version_string() falls back to the crate version.

use std::path::Path;
use std::process::Command;

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
    if let Some(gitdir) = git(&dir, &["rev-parse", "--absolute-git-dir"]) {
        let head = Path::new(&gitdir).join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
        // The branch ref HEAD names advances on every commit. It can be
        // missing as a loose file (packed refs after a gc) — committing
        // recreates it, and the HEAD/source watches cover until then.
        if let Some(refname) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
            let r = Path::new(&gitdir).join(&refname);
            if r.exists() {
                println!("cargo:rerun-if-changed={}", r.display());
            }
        }
    }

    let Some(short) = git(&dir, &["rev-parse", "--short", "HEAD"]) else {
        return;
    };
    // An uncommitted tree matches no commit at all, so say so rather than
    // claiming the commit it was branched from.
    let dirty = match git(&dir, &["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => "-dirty",
        _ => "",
    };
    println!("cargo:rustc-env=KMPLIFY_BUILD={short}{dirty}");
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
