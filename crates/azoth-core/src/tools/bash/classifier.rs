//! Argv-level classifier for `BashTool` invocations.
//!
//! The bash tool's STATIC `effect_class()` is `ApplyLocal` — the
//! worst-case shape a shell command can take (writes, removes, moves,
//! network fetch, package install, etc). That's the right default for
//! the sandbox tier (a fuse-overlayfs jail is the blast-radius cap),
//! but it makes the CONTRACT BUDGET useless: the model's first triage
//! round fires 8–20 read-only `grep`/`find`/`ls` calls and the run is
//! over before any real work starts.
//!
//! This module refines per invocation. Given the raw `command` string
//! from the tool input, it returns:
//!
//! - `EffectClass::Observe` — argv parses as a bare invocation of a
//!   READ-ONLY command from `READ_ONLY_COMMANDS` (including a small
//!   set of `git <subcommand>` / `cargo <subcommand>` combinations).
//!   The command string contains NO shell metacharacters.
//!
//! - `EffectClass::ApplyLocal` — everything else. Missing from the
//!   allowlist, contains a metacharacter, empty, or unknown shape.
//!
//! ## Safety model
//!
//! The two-layer pattern (memory: `pattern_two_layer_safety_verify_independence.md`):
//!
//! 1. **Mechanical layer (sandbox, Landlock)** — always engaged via
//!    the tier selected from the STATIC `Tool::effect_class()`. Reads
//!    can't escape the jail; writes to `/etc/passwd` or out-of-repo
//!    paths fail with EACCES regardless of the classifier decision.
//!
//! 2. **Policy layer (budget counter)** — this classifier. A misclassified
//!    `Observe` on an actually-destructive command would only mean
//!    "we didn't count it against the budget" — the sandbox still
//!    refuses the bad syscall.
//!
//! Because of that, false-DOWNGRADES here are a cost/UX bug, not a
//! safety bug. False-UPGRADES (misclassifying a bare `grep` as
//! `ApplyLocal`) preserve the pre-α status quo — acceptable.
//!
//! ## Why the metachar allowlist is restrictive
//!
//! A single shell metacharacter (`;`, `|`, `&`, `>`, etc.) can smuggle
//! a second command: `grep foo; rm -rf /`. The classifier refuses to
//! reason about those by returning `ApplyLocal` the instant it sees
//! ANY forbidden byte — no context, no escape handling, no trying
//! to be clever. Staying stupid is the point: the argv is either
//! trivially safe (bare token + args) or it's worst-case.

use crate::schemas::EffectClass;

/// Bare commands that read but do not write, from the POSIX/GNU
/// and cargo/rust ecosystems. Entries must be argv[0] match ONLY —
/// subcommand gating happens per-family below.
///
/// Edit this list with care: every addition must be read-only
/// under any argv combination. If the command has subcommands with
/// different side effects (`git`, `cargo`), do NOT put the bare name
/// here; use the per-family helper.
const READ_ONLY_COMMANDS: &[&str] = &[
    // POSIX read-only core
    "grep",
    "rg",
    "find",
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "file",
    "du",
    "df",
    "stat",
    "which",
    "sha256sum",
    "md5sum",
    "xxd",
    "od",
    "env",
    "date",
    "pwd",
    "test",
    "true",
    "false",
    "sleep",
];

/// `git` subcommands that are read-only by default (no `--` escape
/// hatch, no `-c` config mutation, no `fetch` / `pull` / `push` /
/// `commit` / `checkout` / `reset` / `rebase` etc).
///
/// `git config --get` is allowed; `git config` bare is NOT
/// (defaults to setting). `git log`/`show`/`diff`/`status`/`blame`/
/// `rev-parse`/`branch`/`tag`/`ls-files`/`ls-tree` read refs or the
/// objects DB.
const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "log",
    "show",
    "diff",
    "status",
    "blame",
    "rev-parse",
    "branch",
    "tag",
    "ls-files",
    "ls-tree",
];

/// `cargo` subcommands that read without building a writable
/// artifact in the repo tree. (`cargo check` writes target/ — but
/// target/ is gitignored scratch and doesn't mutate the repo from
/// the model's perspective; the user's build cache is fair game.)
const CARGO_READ_ONLY_SUBCOMMANDS: &[&str] = &["check", "metadata", "tree", "version"];

/// Bytes that force an immediate `ApplyLocal` fallback — the presence
/// of any one of these in the raw command string means the classifier
/// will not try to reason about argv at all.
///
/// Covered:
/// - command chaining / subshell: `;`, `&&`, `||`, backtick, `$(`
/// - I/O redirection: `>`, `<`, `|`
/// - backgrounding: `&` (bare; `&&` caught earlier)
/// - escapes / line continuation: `\`
/// - whitespace that can splice commands: newline (`\n`), tab (`\t`)
///
/// NOT covered (intentional): spaces (used for arg separation),
/// single/double quotes (not metachars in the control sense — they
/// group, they don't redirect). Quoting does not change the set of
/// reachable commands, only how argv is split. Since we only look at
/// argv[0] + argv[1] for family dispatch, quoting inside later args
/// is irrelevant to the classification.
fn has_forbidden_metachar(cmd: &str) -> bool {
    // `&&` and `||` are caught by the single-char `&` / `|` check.
    // `$(` is caught by the single-char `$` check (plus `(` — we
    // want even lone `$` or `(` to trip the fallback because they're
    // never needed for a bare read-only call).
    cmd.bytes().any(|b| {
        matches!(
            b,
            b';' | b'|'
                | b'&'
                | b'>'
                | b'<'
                | b'`'
                | b'$'
                | b'('
                | b')'
                | b'\n'
                | b'\t'
                | b'\\'
                | b'\r'
        )
    })
}

/// Classify a raw `bash` command string.
///
/// Returns `Observe` for bare invocations of a read-only command
/// from the allowlist. Returns `ApplyLocal` for anything else: any
/// metacharacter, any unknown command, any unknown subcommand of
/// a family-gated command, or an empty / whitespace-only string.
///
/// This is pure and deterministic — no I/O, no side effects. Unit
/// tested below + adversarial-tested in
/// `tests/bash_classifier_adversarial.rs`.
pub fn classify_bash_command(cmd: &str) -> EffectClass {
    if has_forbidden_metachar(cmd) {
        return EffectClass::ApplyLocal;
    }

    let mut tokens = cmd.split_whitespace();
    let Some(argv0) = tokens.next() else {
        // Empty or whitespace-only command: fall through to worst-
        // case. The tool itself will almost certainly error, but we
        // still don't want to downgrade it — avoids a weird "we
        // reserve zero budget for a noop" free pass.
        return EffectClass::ApplyLocal;
    };

    // `git <subcommand>` and `cargo <subcommand>` require an argv[1]
    // match against the per-family allowlist. Bare `git` / `cargo`
    // with no subcommand prints usage — still read-only, but the
    // downstream model might just be probing; keep it cheap.
    match argv0 {
        "git" => {
            let Some(sub) = tokens.next() else {
                return EffectClass::Observe;
            };
            if GIT_READ_ONLY_SUBCOMMANDS.contains(&sub) {
                return EffectClass::Observe;
            }
            // `git config --get <key>` is read-only. `git config` or
            // `git config --set` mutates .git/config. Keep the gate
            // narrow: require `--get` as argv[2].
            if sub == "config" {
                if let Some(flag) = tokens.next() {
                    if flag == "--get" {
                        return EffectClass::Observe;
                    }
                }
            }
            EffectClass::ApplyLocal
        }
        "cargo" => {
            let Some(sub) = tokens.next() else {
                return EffectClass::Observe;
            };
            if CARGO_READ_ONLY_SUBCOMMANDS.contains(&sub) {
                return EffectClass::Observe;
            }
            EffectClass::ApplyLocal
        }
        "rustc" => {
            // `rustc --version` is read-only. `rustc <srcfile>`
            // compiles. Require an explicit `--version`.
            if tokens.next() == Some("--version") {
                return EffectClass::Observe;
            }
            EffectClass::ApplyLocal
        }
        other => {
            if READ_ONLY_COMMANDS.contains(&other) {
                EffectClass::Observe
            } else {
                EffectClass::ApplyLocal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_observe(cmd: &str) {
        assert_eq!(
            classify_bash_command(cmd),
            EffectClass::Observe,
            "expected Observe for {cmd:?}"
        );
    }

    fn assert_apply_local(cmd: &str) {
        assert_eq!(
            classify_bash_command(cmd),
            EffectClass::ApplyLocal,
            "expected ApplyLocal for {cmd:?}"
        );
    }

    #[test]
    fn read_only_core_commands_classify_as_observe() {
        for cmd in READ_ONLY_COMMANDS {
            assert_observe(cmd);
        }
    }

    #[test]
    fn read_only_commands_with_args_classify_as_observe() {
        assert_observe("grep foo src/");
        assert_observe("rg pattern crates/");
        assert_observe("find . -name '*.rs'");
        assert_observe("ls -la");
        assert_observe("cat Cargo.toml");
        assert_observe("wc -l src/main.rs");
    }

    #[test]
    fn leading_and_trailing_whitespace_is_tolerated() {
        assert_observe("   grep foo");
        assert_observe("ls   ");
        assert_observe("  cat   Cargo.toml  ");
    }

    #[test]
    fn empty_and_whitespace_only_fall_through() {
        assert_apply_local("");
        assert_apply_local("   ");
        assert_apply_local("\t\t");
    }

    #[test]
    fn git_read_only_subcommands_classify_as_observe() {
        for sub in GIT_READ_ONLY_SUBCOMMANDS {
            assert_observe(&format!("git {sub}"));
        }
        assert_observe("git log --oneline -5");
        assert_observe("git show HEAD");
        assert_observe("git diff main..HEAD");
        assert_observe("git status");
        assert_observe("git rev-parse --short HEAD");
        assert_observe("git ls-files 'src/*.rs'");
    }

    #[test]
    fn git_write_subcommands_classify_as_apply_local() {
        assert_apply_local("git push");
        assert_apply_local("git pull");
        assert_apply_local("git commit -m 'x'");
        assert_apply_local("git checkout main");
        assert_apply_local("git reset --hard HEAD");
        assert_apply_local("git rebase main");
        assert_apply_local("git fetch");
        assert_apply_local("git merge main");
    }

    #[test]
    fn bare_git_and_cargo_classify_as_observe() {
        // Bare `git` / `cargo` print usage — read-only.
        assert_observe("git");
        assert_observe("cargo");
    }

    #[test]
    fn git_config_requires_get_flag() {
        assert_observe("git config --get user.email");
        assert_apply_local("git config user.email foo@bar.com");
        assert_apply_local("git config --set user.email foo@bar.com");
        assert_apply_local("git config --unset user.email");
    }

    #[test]
    fn cargo_read_only_subcommands_classify_as_observe() {
        assert_observe("cargo check");
        assert_observe("cargo metadata --format-version 1");
        assert_observe("cargo tree");
        assert_observe("cargo version");
    }

    #[test]
    fn cargo_write_subcommands_classify_as_apply_local() {
        assert_apply_local("cargo build");
        assert_apply_local("cargo test");
        assert_apply_local("cargo run");
        assert_apply_local("cargo install ripgrep");
        assert_apply_local("cargo clean");
        assert_apply_local("cargo update");
    }

    #[test]
    fn rustc_version_is_observe_anything_else_is_apply_local() {
        assert_observe("rustc --version");
        assert_apply_local("rustc");
        assert_apply_local("rustc src/main.rs");
        assert_apply_local("rustc --edition 2021 src/main.rs");
    }

    #[test]
    fn single_metachar_forces_apply_local() {
        for bad in &[
            "grep foo; rm -rf /",
            "ls | cat",
            "cat Cargo.toml > out",
            "cat < /etc/passwd",
            "echo `whoami`",
            "echo $(whoami)",
            "ls && rm x",
            "ls || rm x",
            "ls & echo done",
            "grep foo\\ bar",
            "ls\nrm x",
            "ls\trm x",
            "grep foo $HOME",
            "ls (nested)",
        ] {
            assert_apply_local(bad);
        }
    }

    #[test]
    fn unknown_commands_classify_as_apply_local() {
        assert_apply_local("rm -rf /");
        assert_apply_local("mv a b");
        assert_apply_local("cp a b");
        assert_apply_local("mkdir foo");
        assert_apply_local("touch foo");
        assert_apply_local("chmod 777 foo");
        assert_apply_local("curl http://example.com");
        assert_apply_local("wget http://example.com");
        assert_apply_local("python -c 'import os; os.remove(\".git\")'");
        assert_apply_local("node -e 'require(\"fs\").rmSync(\".git\")'");
    }
}
