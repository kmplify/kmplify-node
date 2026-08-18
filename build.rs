//! Stamp the build with the commit it came from.
//!
//! A version number alone cannot tell a tagged release from a local build
//! many commits past it, and "which build is that peer running" is a question
//! the fabric asks constantly. Absent git (a tarball, a vendored build) this
//! is simply empty and version_string() falls back to the crate version.

use std::path::Path;
use std::process::Command;

fn main() {
    // Deliberately NOT rerun-if-changed on a .git path. Declaring any
    // rerun-if-changed switches Cargo off its default "rerun when any file in
    // the package changed", and the only thing that told us the tree was
    // dirty was the source files themselves: the stamp would keep saying
    // clean while you edited the worker. rerun-if-env-changed does not
    // disable the default, so this keeps it.
    //
    // The cost is re-running on every source change, which is two cheap git
    // calls. The old form had the opposite problem AND was worse in a git
    // worktree or submodule, where .git is a FILE: the declared path never
    // matched, so the script re-ran every single build and dragged a
    // dependent recompile with it.
    println!("cargo:rerun-if-env-changed=KMPLIFY_BUILD");

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
