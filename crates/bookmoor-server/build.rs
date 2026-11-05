//! Bakes the git commit into the binary so the web UI can show what is running.
//! Docker builds have no .git, so BOOKMOOR_COMMIT (a build arg) wins.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BOOKMOOR_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let commit = std::env::var("BOOKMOOR_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .unwrap_or_default();
    let short: String = commit.chars().take(7).collect();
    println!("cargo:rustc-env=BOOKMOOR_GIT_COMMIT={short}");
}
