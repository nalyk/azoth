//! Host capability probes. Used by the runtime to decide whether a given
//! sandbox tier can be applied on this host, or whether to fall back.

/// Returns `true` iff this host supports unprivileged `CLONE_NEWUSER`.
///
/// We probe by forking a child that attempts the unshare; we do **not**
/// collapse the current process into a new user namespace — that would
/// permanently alter the parent's credential view for the rest of its life.
#[cfg(target_os = "linux")]
pub fn probe_unprivileged_userns() -> bool {
    use nix::sched::{unshare, CloneFlags};
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, ForkResult};

    // SAFETY: the probe must be called from a single-threaded context. The
    // azoth runtime invokes it before spawning the tokio runtime; tests call
    // it from a synchronous `#[test]`. The child does no allocation beyond
    // the unshare syscall and exits immediately.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let rc = unshare(CloneFlags::CLONE_NEWUSER);
            unsafe { nix::libc::_exit(if rc.is_ok() { 0 } else { 1 }) };
        }
        Ok(ForkResult::Parent { child }) => {
            matches!(waitpid(child, None), Ok(WaitStatus::Exited(_, 0)))
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_unprivileged_userns() -> bool {
    false
}
