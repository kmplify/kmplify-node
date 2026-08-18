//! Stamp the build with the commit it came from.
//!
//! A version number alone cannot tell a tagged release from a local build
//! many commits past it, and "which build is that peer running" is a question
//! the fabric asks constantly. Absent git (a tarball, a vendored build) this
//! is simply empty and version_string() falls back to the crate version.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=KMPLIFY_BUILD");

    if std::env::var("KMPLIFY_BUILD").is_ok() {
        return;
    }

    let Some(short) = git(&["rev-parse", "--short", "HEAD"]) else {
        return;
    };
    // An uncommitted tree matches no commit at all, so say so rather than
    // claiming the commit it was branched from.
    let dirty = match git(&["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => "-dirty",
        _ => "",
    };
    println!("cargo:rustc-env=KMPLIFY_BUILD={short}{dirty}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
