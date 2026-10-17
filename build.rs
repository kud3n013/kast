//! Records the git commit the binary is built from, for "Built-in · kast 0.1.3 (abc1234)".

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=packages.toml");
    let commit = Command::new("git").args(["rev-parse", "--short=7", "HEAD"]).output().ok().filter(|o| o.status.success());
    if let Some(o) = commit {
        println!("cargo:rustc-env=KAST_COMMIT={}", String::from_utf8_lossy(&o.stdout).trim());
    }
}
