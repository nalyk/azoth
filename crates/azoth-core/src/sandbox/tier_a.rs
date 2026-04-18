//! Tier A: user-ns + net-ns + Landlock + seccompiler, unprivileged sequence.
//!
//! In v1 the setup runs in a forked child immediately before `execve` of the
//! tool workload. The in-process dispatcher path used by azoth-core's v1
//! tools (which are all `Observe`, trivially safe) does *not* exercise the
//! fork path — it only records that Tier A is the required tier and relies
//! on the tool implementation not performing writes. This keeps the safety
//! guarantee monotonic: adding a real out-of-process exec strictly narrows
//! the permitted syscalls.
//!
//! `spawn_jailed` is intentionally not yet wired into the runtime dispatcher;
//! it is exercised by the `sandbox_tier_a_smoke` integration test. Wiring
//! Tier A into the runtime requires serializing Tool Input/Output across a
//! process boundary, which is a separate follow-up.

use super::{Sandbox, SandboxError};
use crate::schemas::SandboxTier;

#[cfg(target_os = "linux")]
use std::path::PathBuf;

pub struct TierA {}

impl TierA {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for TierA {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for TierA {
    fn tier(&self) -> SandboxTier {
        SandboxTier::A
    }

    fn prepare(&self) -> Result<(), SandboxError> {
        #[cfg(not(target_os = "linux"))]
        {
            return Err(SandboxError::Unsupported);
        }
        #[cfg(target_os = "linux")]
        Ok(())
    }
}

/// Landlock allowlist carried into `spawn_jailed`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub allow_read: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
    /// When `true`, install the narrow seccomp allowlist designed for
    /// `/bin/true`-grade workloads (the v1 smoke target). When
    /// `false`, skip seccomp entirely — Landlock is still active,
    /// but the child keeps the host's syscall surface. This is what
    /// v2.1's bash-tool wiring wants: bash fails under the narrow
    /// allowlist on the first `clone()` or `wait4()`, so the
    /// effective mechanism is Landlock on the FS namespace rather
    /// than a syscall deny-list.
    pub strict_seccomp: bool,
}

/// A handle to a child spawned via `spawn_jailed`. Built from a raw pid
/// because `std::process::Child` cannot be constructed from the outside.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct SandboxChild {
    pid: nix::unistd::Pid,
    reaped: bool,
}

#[cfg(target_os = "linux")]
impl SandboxChild {
    pub fn id(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    pub fn wait(&mut self) -> Result<std::process::ExitStatus, SandboxError> {
        use nix::sys::wait::{waitpid, WaitStatus};
        use std::os::unix::process::ExitStatusExt;

        if self.reaped {
            return Err(SandboxError::Syscall("child already reaped".into()));
        }
        loop {
            match waitpid(self.pid, None) {
                Ok(WaitStatus::Exited(_, code)) => {
                    self.reaped = true;
                    return Ok(std::process::ExitStatus::from_raw(code << 8));
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    self.reaped = true;
                    return Ok(std::process::ExitStatus::from_raw(sig as i32));
                }
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(SandboxError::Syscall(format!("waitpid: {e}"))),
            }
        }
    }

    pub fn kill(&mut self) -> Result<(), SandboxError> {
        use nix::sys::signal::{kill, Signal};
        kill(self.pid, Signal::SIGKILL).map_err(|e| SandboxError::Syscall(format!("kill: {e}")))
    }
}

/// The unprivileged namespace + Landlock + seccomp sequence described in
/// draft_plan §"Sandbox tiers — honest mechanism stack". Forks a child, the
/// child narrows itself, then `execvp`s `argv[0]`. The parent returns a
/// [`SandboxChild`] or a classified [`SandboxError`] if the pre-exec
/// sequence failed.
///
/// SAFETY: must be called from a single-threaded context. `fork` in a
/// multi-threaded process is UB per POSIX. The v1 callers (integration
/// tests, future synchronous wiring) enforce this.
#[cfg(target_os = "linux")]
pub fn spawn_jailed(tool_argv: &[&str], opts: &SpawnOptions) -> Result<SandboxChild, SandboxError> {
    use nix::fcntl::OFlag;
    use nix::unistd::{fork, getgid, getuid, pipe2, ForkResult};
    use std::io::Read;
    use std::os::fd::AsRawFd;

    if tool_argv.is_empty() {
        return Err(SandboxError::Syscall("empty argv".into()));
    }

    // Capture outer credentials before fork; after unshare(CLONE_NEWUSER)
    // the child sees a different uid/gid view.
    let outer_uid = getuid().as_raw();
    let outer_gid = getgid().as_raw();

    let (r_fd, w_fd) =
        pipe2(OFlag::O_CLOEXEC).map_err(|e| SandboxError::Syscall(format!("pipe2: {e}")))?;

    // SAFETY: parent is single-threaded (contract of the function). The child
    // must only use async-signal-safe operations plus the narrow set of
    // syscalls required for namespace setup and execve.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            drop(r_fd);
            let w_raw = w_fd.as_raw_fd();
            // Run the jailing sequence; on any error, write a classification
            // byte to the parent and _exit(1). The byte tags the failing step
            // so the parent can map it into a specific `SandboxError`.
            let code = child_jail(tool_argv, opts, outer_uid, outer_gid);
            // child_jail returns only on error; on success it execvps.
            let byte: [u8; 1] = [code];
            unsafe {
                nix::libc::write(w_raw, byte.as_ptr() as *const _, 1);
                nix::libc::_exit(1);
            }
        }
        Ok(ForkResult::Parent { child }) => {
            drop(w_fd);
            let mut file = std::fs::File::from(r_fd);
            let mut buf = [0u8; 1];
            match file.read(&mut buf) {
                Ok(0) => {
                    // EOF without data: child successfully reached execvp
                    // (write end was O_CLOEXEC, closed atomically by exec).
                    Ok(SandboxChild {
                        pid: child,
                        reaped: false,
                    })
                }
                Ok(_) => {
                    // Reap the failed child so it doesn't linger as a zombie.
                    let _ = nix::sys::wait::waitpid(child, None);
                    Err(classify_child_error(buf[0]))
                }
                Err(e) => {
                    let _ = nix::sys::wait::waitpid(child, None);
                    Err(SandboxError::Syscall(format!("parent pipe read: {e}")))
                }
            }
        }
        Err(e) => Err(SandboxError::Syscall(format!("fork: {e}"))),
    }
}

// Error classification bytes written from the child to the parent over the
// status pipe. Kept small and stable; only the parent maps them to
// `SandboxError` variants.
#[cfg(target_os = "linux")]
const ERR_UNSHARE_USER: u8 = 1;
#[cfg(target_os = "linux")]
const ERR_UID_MAP: u8 = 2;
#[cfg(target_os = "linux")]
const ERR_UNSHARE_NET: u8 = 3;
#[cfg(target_os = "linux")]
const ERR_LANDLOCK: u8 = 4;
#[cfg(target_os = "linux")]
const ERR_SECCOMP: u8 = 5;
#[cfg(target_os = "linux")]
const ERR_EXECVP: u8 = 6;

#[cfg(target_os = "linux")]
fn classify_child_error(b: u8) -> SandboxError {
    match b {
        ERR_UNSHARE_USER => SandboxError::MissingDependency("CLONE_NEWUSER"),
        ERR_UID_MAP => SandboxError::Syscall("write uid/gid map".into()),
        ERR_UNSHARE_NET => SandboxError::MissingDependency("CLONE_NEWNET"),
        ERR_LANDLOCK => SandboxError::MissingDependency("landlock"),
        ERR_SECCOMP => SandboxError::Syscall("seccomp install".into()),
        ERR_EXECVP => SandboxError::Syscall("execvp".into()),
        _ => SandboxError::Syscall(format!("unknown child error byte {b}")),
    }
}

/// Child-side of `spawn_jailed`. Returns the error classification byte on
/// any failure; on success this function does not return (`execvp` replaces
/// the process image).
#[cfg(target_os = "linux")]
fn child_jail(tool_argv: &[&str], opts: &SpawnOptions, outer_uid: u32, outer_gid: u32) -> u8 {
    use nix::sched::{unshare, CloneFlags};

    // 1. user namespace.
    if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
        return ERR_UNSHARE_USER;
    }

    // 2. setgroups=deny (must precede gid_map write), then uid_map, gid_map.
    if std::fs::write("/proc/self/setgroups", "deny").is_err() {
        return ERR_UID_MAP;
    }
    if std::fs::write("/proc/self/uid_map", format!("0 {outer_uid} 1\n")).is_err() {
        return ERR_UID_MAP;
    }
    if std::fs::write("/proc/self/gid_map", format!("0 {outer_gid} 1\n")).is_err() {
        return ERR_UID_MAP;
    }

    // 3. network namespace. On WSL2 configs without net backend support this
    // may fail; we tag it separately so callers can choose to retry without
    // the net isolation.
    if unshare(CloneFlags::CLONE_NEWNET).is_err() {
        return ERR_UNSHARE_NET;
    }

    // (cgroup v2 slice and Tier B fuse-overlayfs deliberately skipped in v1.)

    // 5. Landlock.
    if install_landlock(opts).is_err() {
        return ERR_LANDLOCK;
    }

    // 6. seccomp (optional — see SpawnOptions::strict_seccomp).
    if opts.strict_seccomp && install_seccomp().is_err() {
        return ERR_SECCOMP;
    }

    // 7. execvp.
    use std::ffi::CString;
    let Ok(argv0) = CString::new(tool_argv[0]) else {
        return ERR_EXECVP;
    };
    let argv_cstrings: Vec<CString> = tool_argv
        .iter()
        .filter_map(|s| CString::new(*s).ok())
        .collect();
    if argv_cstrings.len() != tool_argv.len() {
        return ERR_EXECVP;
    }
    let argv_refs: Vec<&std::ffi::CStr> = argv_cstrings.iter().map(|c| c.as_c_str()).collect();
    if nix::unistd::execvp(&argv0, &argv_refs).is_err() {
        return ERR_EXECVP;
    }
    // Unreachable on success.
    ERR_EXECVP
}

/// Run the jail sequence inside `std::process::Command::pre_exec`.
/// Same steps as `child_jail` minus the `execvp` tail — std invokes
/// `execve` for us after the closure returns.
///
/// SAFETY: `pre_exec` runs in the forked child after `fork()` and
/// before `execve()`. The closure must be async-signal-safe, which
/// matches the `child_jail` sequence: `unshare`, `write`, and
/// seccomp-bpf / Landlock loads are all permitted in that context.
#[cfg(target_os = "linux")]
fn jail_preexec_inner(opts: &SpawnOptions, outer_uid: u32, outer_gid: u32) -> std::io::Result<()> {
    use nix::sched::{unshare, CloneFlags};
    use std::io::Error;

    unshare(CloneFlags::CLONE_NEWUSER)
        .map_err(|e| Error::other(format!("unshare(NEWUSER): {e}")))?;
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/uid_map", format!("0 {outer_uid} 1\n"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {outer_gid} 1\n"))?;
    unshare(CloneFlags::CLONE_NEWNET).map_err(|e| Error::other(format!("unshare(NEWNET): {e}")))?;
    install_landlock(opts).map_err(|_| Error::other("landlock install failed"))?;
    if opts.strict_seccomp {
        install_seccomp().map_err(|_| Error::other("seccomp install failed"))?;
    }
    Ok(())
}

/// Build a `tokio::process::Command` that will run `program args...`
/// inside the unprivileged jail sequence. The caller still owns the
/// stdio setup (piped/null/etc.), the working directory, and env —
/// everything layered on the returned `Command` runs *before* the
/// fork, which is exactly what you want for stdout/stderr pipes.
///
/// `cwd` is applied *after* `pre_exec` via the `Command` builder,
/// so it takes effect in the post-exec process. For Tier B wiring,
/// pass `<OverlayWorkspace>.merged` as the cwd so bash lands inside
/// the writable merge view.
///
/// This is the v2.1 Gap 2 entrypoint for tools that need async
/// stdio. The synchronous `spawn_jailed` stays for tests and
/// non-tokio callers.
#[cfg(target_os = "linux")]
pub fn build_jailed_tokio_command(
    program: &str,
    args: &[&str],
    opts: &SpawnOptions,
    cwd: Option<&std::path::Path>,
) -> tokio::process::Command {
    use nix::unistd::{getgid, getuid};
    // The `pre_exec` method is provided by
    // `std::os::unix::process::CommandExt`. We route through
    // `tokio::process::Command`, which exposes `pre_exec` directly
    // as an inherent method — no import needed.

    let outer_uid = getuid().as_raw();
    let outer_gid = getgid().as_raw();
    let opts_clone = opts.clone();
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    // SAFETY: `pre_exec` runs post-fork pre-exec. The closure owns
    // `opts_clone` and only calls async-signal-safe operations plus
    // the Landlock / seccomp crate setup calls — both of which are
    // deliberately designed for this phase (see the sandboxing
    // crate docs).
    unsafe {
        cmd.pre_exec(move || jail_preexec_inner(&opts_clone, outer_uid, outer_gid));
    }
    cmd
}

#[cfg(target_os = "linux")]
fn install_landlock(opts: &SpawnOptions) -> Result<(), ()> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    let abi = ABI::V2;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);
    let access_write = AccessFs::from_write(abi) | AccessFs::from_read(abi);

    let mut created = Ruleset::default()
        .handle_access(access_all)
        .map_err(|_| ())?
        .create()
        .map_err(|_| ())?;

    for p in &opts.allow_read {
        let fd = PathFd::new(p).map_err(|_| ())?;
        created = created
            .add_rule(PathBeneath::new(fd, access_read))
            .map_err(|_| ())?;
    }
    for p in &opts.allow_write {
        let fd = PathFd::new(p).map_err(|_| ())?;
        created = created
            .add_rule(PathBeneath::new(fd, access_write))
            .map_err(|_| ())?;
    }

    let status = created.restrict_self().map_err(|_| ())?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_seccomp() -> Result<(), ()> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
    use std::convert::TryInto;

    // Minimal allowlist to let `/bin/true` and similar trivial workloads
    // reach clean exit. This is deliberately not the final policy; the
    // runtime wiring PR will tighten and parametrize this per effect class.
    let allowed: &[i64] = &[
        nix::libc::SYS_read,
        nix::libc::SYS_write,
        nix::libc::SYS_close,
        nix::libc::SYS_exit,
        nix::libc::SYS_exit_group,
        nix::libc::SYS_rt_sigreturn,
        nix::libc::SYS_rt_sigaction,
        nix::libc::SYS_rt_sigprocmask,
        nix::libc::SYS_execve,
        nix::libc::SYS_openat,
        nix::libc::SYS_newfstatat,
        nix::libc::SYS_fstat,
        nix::libc::SYS_mmap,
        nix::libc::SYS_mprotect,
        nix::libc::SYS_munmap,
        nix::libc::SYS_brk,
        nix::libc::SYS_arch_prctl,
        nix::libc::SYS_set_tid_address,
        nix::libc::SYS_set_robust_list,
        nix::libc::SYS_rseq,
        nix::libc::SYS_prlimit64,
        nix::libc::SYS_getrandom,
        nix::libc::SYS_uname,
        nix::libc::SYS_readlink,
        nix::libc::SYS_readlinkat,
        nix::libc::SYS_pread64,
        nix::libc::SYS_access,
        nix::libc::SYS_faccessat,
        nix::libc::SYS_faccessat2,
        nix::libc::SYS_getuid,
        nix::libc::SYS_getgid,
        nix::libc::SYS_geteuid,
        nix::libc::SYS_getegid,
        nix::libc::SYS_futex,
        nix::libc::SYS_gettid,
        nix::libc::SYS_tgkill,
        nix::libc::SYS_getpid,
        nix::libc::SYS_clock_gettime,
        nix::libc::SYS_poll,
        nix::libc::SYS_ppoll,
        nix::libc::SYS_lseek,
    ];

    let rules = allowed.iter().map(|s| (*s, vec![])).collect();
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into().map_err(|_| ())?,
    )
    .map_err(|_| ())?
    .try_into()
    .map_err(|_| ())?;

    seccompiler::apply_filter(&filter).map_err(|_| ())
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub allow_read: Vec<std::path::PathBuf>,
    pub allow_write: Vec<std::path::PathBuf>,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct SandboxChild {}

#[cfg(not(target_os = "linux"))]
pub fn spawn_jailed(
    _tool_argv: &[&str],
    _opts: &SpawnOptions,
) -> Result<SandboxChild, SandboxError> {
    Err(SandboxError::Unsupported)
}
