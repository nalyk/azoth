//! Tier A smoke test: fork a child, apply user-ns + Landlock + seccomp,
//! run `/bin/true` to exit=0. Synchronous `#[test]` on purpose — `fork`
//! in a multi-threaded process is UB, and `#[tokio::test]` spawns workers
//! before the test body runs.

#![cfg(target_os = "linux")]

use azoth_core::sandbox::{probe_unprivileged_userns, tier_a::spawn_jailed, tier_a::SpawnOptions};
use std::path::PathBuf;

#[test]
fn spawn_jailed_runs_bin_true() {
    if std::env::var_os("AZOTH_SKIP_TIER_A").is_some() {
        eprintln!("skip: AZOTH_SKIP_TIER_A set");
        return;
    }
    if !probe_unprivileged_userns() {
        eprintln!("skip: host lacks unprivileged CLONE_NEWUSER");
        return;
    }
    let opts = SpawnOptions {
        allow_read: vec![
            PathBuf::from("/bin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/usr"),
            PathBuf::from("/etc"),
        ],
        allow_write: vec![],
    };
    let mut child = spawn_jailed(&["/bin/true"], &opts).expect("spawn_jailed");
    let status = child.wait().expect("wait");
    assert!(status.success(), "jailed /bin/true exited with {status:?}");
}
