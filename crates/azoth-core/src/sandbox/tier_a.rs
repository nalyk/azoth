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
/// Async-signal-safe jail sequence invoked from `pre_exec` in the
/// forked child. Error path uses `io::Error::from_raw_os_error` so
/// failures allocate nothing either.
#[cfg(target_os = "linux")]
fn jail_preexec_async_signal_safe(
    uid_map: &[u8],
    gid_map: &[u8],
    landlock_opt: &mut Option<landlock::RulesetCreated>,
    seccomp_bpf: Option<&seccompiler::BpfProgram>,
) -> std::io::Result<()> {
    use nix::sched::{unshare, CloneFlags};

    unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    write_proc_path(PROC_SETGROUPS, b"deny")?;
    write_proc_path(PROC_UID_MAP, uid_map)?;
    write_proc_path(PROC_GID_MAP, gid_map)?;
    unshare(CloneFlags::CLONE_NEWNET).map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    if let Some(rs) = landlock_opt.take() {
        let status = rs
            .restrict_self()
            .map_err(|_| std::io::Error::from_raw_os_error(nix::libc::EPERM))?;
        if status.ruleset == landlock::RulesetStatus::NotEnforced {
            return Err(std::io::Error::from_raw_os_error(nix::libc::EPERM));
        }
    }
    if let Some(bpf) = seccomp_bpf {
        seccompiler::apply_filter(bpf)
            .map_err(|_| std::io::Error::from_raw_os_error(nix::libc::EPERM))?;
    }
    Ok(())
}

/// Compile-time CStr constants for `/proc` writes — the child
/// allocates nothing.
#[cfg(target_os = "linux")]
const PROC_SETGROUPS: &std::ffi::CStr = c"/proc/self/setgroups";
#[cfg(target_os = "linux")]
const PROC_UID_MAP: &std::ffi::CStr = c"/proc/self/uid_map";
#[cfg(target_os = "linux")]
const PROC_GID_MAP: &std::ffi::CStr = c"/proc/self/gid_map";

/// `open → write → close` via raw libc. Every step is
/// async-signal-safe per POSIX. No Rust allocation, no `std::fs`
/// indirection.
#[cfg(target_os = "linux")]
fn write_proc_path(path: &std::ffi::CStr, bytes: &[u8]) -> std::io::Result<()> {
    // SAFETY: `path` is a static, valid C string. `bytes` is a
    // live byte slice with a correct len. `close` is always safe
    // on an fd returned by `open`.
    let fd = unsafe { nix::libc::open(path.as_ptr(), nix::libc::O_WRONLY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = unsafe { nix::libc::write(fd, bytes.as_ptr() as *const _, bytes.len()) };
    unsafe { nix::libc::close(fd) };
    if n < 0 || (n as usize) != bytes.len() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Build a `tokio::process::Command` that will run `program args...`
/// inside the unprivileged jail sequence. The caller still owns the
/// stdio setup (piped/null/etc.), the working directory, and env —
/// everything layered on the returned `Command` runs *before* the
/// fork, which is exactly what you want for stdout/stderr pipes.
///
/// `cwd` is applied via the `Command` builder, so it takes effect
/// in the post-exec process. For Tier B wiring, pass
/// `<OverlayWorkspace>.merged` as the cwd so bash lands inside the
/// writable merge view.
///
/// Returns `Err` when the Landlock ruleset or seccomp BPF program
/// cannot be built in the parent — the child never forks in that
/// case.
///
/// ## Async-signal-safety discipline (PR #14 bot HIGH/P1 fix)
///
/// `pre_exec` runs post-fork pre-exec. In a multi-threaded Tokio
/// process, any allocator or libc lock held by another thread at
/// fork time stays held in the child; calling Rust code that takes
/// those locks deadlocks the child. So this function does ALL
/// Rust-level allocation up front in the parent (landlock
/// `RulesetCreated` with every `add_rule` applied; seccomp
/// `BpfProgram` built; uid/gid map bytes formatted into owned
/// `Vec<u8>`). The pre_exec closure itself calls only:
///
/// - `nix::sched::unshare` — raw `unshare(2)`, async-signal-safe
/// - `libc::open` / `libc::write` / `libc::close` — the only
///   touches to `/proc/self/{setgroups,uid_map,gid_map}`; paths
///   are compile-time `CStr`s so the child allocates nothing
/// - `RulesetCreated::restrict_self` — `prctl` + `landlock_restrict_self`
/// - `seccompiler::apply_filter(&BpfProgram)` — `prctl` + `seccomp`
///
/// `std::fs::write`, `format!`, `Vec::push`, `PathBuf::from`, and
/// any `Error::other(format!(...))` stay in parent-only code above.
#[cfg(target_os = "linux")]
pub fn build_jailed_tokio_command(
    program: &str,
    args: &[&str],
    opts: &SpawnOptions,
    cwd: Option<&std::path::Path>,
) -> Result<tokio::process::Command, SandboxError> {
    use nix::unistd::{getgid, getuid};

    let outer_uid = getuid().as_raw();
    let outer_gid = getgid().as_raw();

    // Parent-side, pre-fork: do ALL Rust allocation here.
    let uid_map: Vec<u8> = format!("0 {outer_uid} 1\n").into_bytes();
    let gid_map: Vec<u8> = format!("0 {outer_gid} 1\n").into_bytes();
    let landlock_rs =
        build_landlock_ruleset(opts).map_err(|_| SandboxError::MissingDependency("landlock"))?;
    let seccomp_bpf: Option<seccompiler::BpfProgram> = if opts.strict_seccomp {
        Some(build_seccomp_program().map_err(|_| SandboxError::Syscall("seccomp build".into()))?)
    } else {
        None
    };

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }

    // `pre_exec` binds the closure as `FnMut`, so consuming
    // `landlock_rs` directly would trip the borrow checker.
    // `.take()` from an `Option` is a pure pointer swap — no
    // allocation — so it stays signal-safe.
    let mut landlock_opt: Option<landlock::RulesetCreated> = Some(landlock_rs);

    // SAFETY: see the function-level docstring — every operation
    // in the closure is async-signal-safe, and every artifact it
    // reads was built pre-fork by the parent.
    unsafe {
        cmd.pre_exec(move || {
            jail_preexec_async_signal_safe(
                &uid_map,
                &gid_map,
                &mut landlock_opt,
                seccomp_bpf.as_ref(),
            )
        });
    }
    Ok(cmd)
}

/// Build the Landlock `RulesetCreated` pre-fork (fd opened + every
/// `add_rule` applied). `restrict_self()` is the one step deferred
/// to the child because it's the commit that gags the current
/// process — calling it in the parent would gag the parent.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(opts: &SpawnOptions) -> Result<landlock::RulesetCreated, ()> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
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
    // Individual `allow_read` / `allow_write` entries may not
    // exist on every host (e.g. `/dev/random` missing on a bare
    // container) — skip missing-path entries rather than failing
    // the whole ruleset build. The cost is a subset of the
    // intended allow-list; the alternative is the tool refusing
    // to run at all on a minor FS difference.
    for p in &opts.allow_read {
        let Ok(fd) = PathFd::new(p) else { continue };
        created = created
            .add_rule(PathBeneath::new(fd, access_read))
            .map_err(|_| ())?;
    }
    for p in &opts.allow_write {
        let Ok(fd) = PathFd::new(p) else { continue };
        created = created
            .add_rule(PathBeneath::new(fd, access_write))
            .map_err(|_| ())?;
    }
    Ok(created)
}

/// Build the seccomp BPF program pre-fork. `strict_seccomp=false`
/// skips this entirely (BashTool path).
#[cfg(target_os = "linux")]
fn build_seccomp_program() -> Result<seccompiler::BpfProgram, ()> {
    use seccompiler::{SeccompAction, SeccompFilter};
    use std::convert::TryInto;

    // Narrow `/bin/true`-grade allowlist — reused by the
    // `sandbox_tier_a_smoke` integration test. Subprocess-heavy
    // tools (bash) set `strict_seccomp = false`, so this is never
    // built for them; Landlock carries the enforcement.
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
    let bpf: seccompiler::BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into().map_err(|_| ())?,
    )
    .map_err(|_| ())?
    .try_into()
    .map_err(|_| ())?;
    Ok(bpf)
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
