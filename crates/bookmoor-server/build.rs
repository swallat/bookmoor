//! Bakes the git commit into the binary so the web UI can show what is running.
//! Docker builds have no .git, so BOOKMOOR_COMMIT (a build arg) wins.
use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=BOOKMOOR_COMMIT");
    // HEAD changes on checkout, its reflog on every commit.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    let commit = std::env::var("BOOKMOOR_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            // Only trust git when this workspace is the repository itself,
            // not a tarball that happens to sit inside another checkout.
            let top = std::fs::canonicalize(git(&["rev-parse", "--show-toplevel"])?).ok()?;
            let ws = std::fs::canonicalize(Path::new("../..")).ok()?;
            (top == ws).then(|| git(&["rev-parse", "HEAD"])).flatten()
        })
        .unwrap_or_default();
    let short: String = commit.chars().take(7).collect();
    println!("cargo:rustc-env=BOOKMOOR_GIT_COMMIT={short}");
}
