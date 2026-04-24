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
    // POSIX read-only core. Entries here must be read-only under
    // EVERY argv combination the classifier will accept.
    //
    // Removals across R1–R3 (gemini + codex found the class bug,
    // I fixed each occurrence):
    //   - R1: `find` (`-exec` / `-delete` / `-fprintf`), `env`
    //     (`env VAR=val cmd` runs arbitrary command).
    //   - R2: `xxd` (`-r` reverse mode writes binary).
    //   - R3: `date` (`-s STRING` sets system time under root /
    //     CAP_SYS_TIME; bypass is environment-dependent but still
    //     a budget escape on elevated sandbox tiers).
    //
    // The false-UPGRADE on legitimate read-only use (bare
    // `date`, `xxd file`, `find . -name x`) is acceptable per the
    // safety model at the top of this file — cost/UX bug, not a
    // safety bug.
    "grep",
    "rg",
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
    "od",
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
// gemini R0 HIGH (2026-04-24) removed `branch` and `tag`: both
// mutate refs by default (`git branch -D foo`, `git tag -d foo`).
// codex R3 P1 (PR #30, 2026-04-24) removed `diff` and `status`:
// both can write `.git/index` via `refresh_index()` when the stat
// cache is stale (happens after any fs_write to a tracked path).
// The write is semantically benign — it's git's internal stat
// cache, not a repo-logical mutation — but it's still a local
// write, and the consistency argument with find/env/xxd/date
// wins: one real write → budget counts. `--no-optional-locks` is
// the flag that suppresses the refresh; requiring it via classifier
// rewrite would violate "stay stupid" (we'd be spelling shell
// commands for the model), so the cheaper path is exclusion.
//
// The remaining entries don't touch the working tree or refresh
// the index. `log`, `show`, `blame` read commit history and
// objects; `rev-parse`, `ls-files`, `ls-tree` read refs / index
// metadata only. `--output` family flag writes for log/show are
// caught by `has_write_flag` per R1.
const GIT_READ_ONLY_SUBCOMMANDS: &[&str] =
    &["log", "show", "blame", "rev-parse", "ls-files", "ls-tree"];

// `cargo` has NO read-only subcommand allowlist in R3. codex R2
// P1 (PR #30, 2026-04-24) flagged `cargo check --target-dir <DIR>`
// as a write escape — `--target-dir` lets the model point the
// compiler's artifact output at ANY writable path, and neither the
// static effect_class nor the Observe budget catch it. Plain
// `cargo metadata` / `cargo tree` can also rewrite `Cargo.lock` in
// an unlocked workspace. Same class of bug as `find -exec`,
// `env VAR=val cmd`, `xxd -r`, and `date -s` — one flag flips the
// tool from read to write. Per this file's safety model, I remove
// the entry rather than maintain a per-flag denylist: bare `cargo
// check` now counts as ApplyLocal (false-UPGRADE, cost/UX bug not
// a safety bug). If a later sprint decides the UX tax is too high,
// restore a targeted allowlist that scans for `--target-dir`,
// `--offline`, and the locked-invariant family.

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
/// - quoting: `'`, `"` (gemini R1 HIGH, PR #30, 2026-04-24)
///
/// ## Why quotes are rejected
///
/// R0 docstring claimed "quoting does not change the set of reachable
/// commands" — that was wrong for the R1-added `has_write_flag` scan.
/// Since the bash tool runs commands via a real shell (`sh -c`), the
/// shell strips quotes before exec. A payload like
/// `git log "--output=file"` splits by whitespace into argv tokens
/// `[git, log, "--output=file"]`; my `has_write_flag` check looks for
/// `t == "--output" || t.starts_with("--output=")` and misses the
/// quoted token — but the shell strips the quotes and writes the
/// file anyway. Rejecting any `'` / `"` byte closes this bypass
/// without making me do shell-lexer work.
///
/// This DOES penalize common legitimate forms like `grep "foo bar"
/// src/` — they now classify as ApplyLocal rather than Observe.
/// False-UPGRADE per the safety model: a cost/UX bug, not a safety
/// bug. The model can spell the same query without quotes in the
/// overwhelming majority of cases (`grep 'foo bar'` → `grep
/// "foo bar"` → regex alternatives / `rg -F`), and budget survival
/// is more load-bearing than shaving one Observe token off quoted
/// greps.
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
                | b'\''
                | b'"'
        )
    })
}

/// Detect flag-level write escapes in an argv sequence.
///
/// Several otherwise-read-only tools expose a `--output`-family
/// flag that writes to an arbitrary file without using shell
/// redirection. `git log --output=<file>`, `git diff --output
/// <file>`, `git show --output=<file>` all fall here. gemini R0
/// HIGH (PR #30, 2026-04-24) flagged the git forms.
///
/// The match is two-shaped, NOT a bare prefix:
///   - token exactly `--output` → the file arg is argv[n+1]
///   - token begins with `--output=` → filename is embedded
///
/// Naive `starts_with("--output")` would over-reject legitimate
/// read-only flags like `--output-format json` (cargo tree, jq, …)
/// and `--output-indicator-new=X` (git diff), turning common
/// structured-read patterns into ApplyLocal. The tightened form
/// preserves those as Observe.
///
/// `-o` is intentionally NOT rejected — in POSIX it usually means
/// "only-match" (grep), "or" (find), or "long format w/o group"
/// (ls), not "output to file." If a specific tool ever uses `-o
/// <file>` for output in an allowlisted position, add a targeted
/// check rather than broadening this prefix.
fn has_write_flag<'a, I: IntoIterator<Item = &'a str>>(args: I) -> bool {
    args.into_iter()
        .any(|t| t == "--output" || t.starts_with("--output="))
}

/// Classify a raw `bash` command string.
///
/// Returns `Observe` for bare invocations of a read-only command
/// from the allowlist. Returns `ApplyLocal` for anything else: any
/// metacharacter, any unknown command, any unknown subcommand of
/// a family-gated command, any token past argv0 that begins with
/// `--output` (write-flag escape per `has_write_flag`), or an
/// empty / whitespace-only string.
///
/// This is pure and deterministic — no I/O, no side effects. Unit
/// tested below + adversarial-tested in
/// `tests/bash_classifier_adversarial.rs`.
pub fn classify_bash_command(cmd: &str) -> EffectClass {
    if has_forbidden_metachar(cmd) {
        return EffectClass::ApplyLocal;
    }

    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let Some((argv0, rest)) = tokens.split_first() else {
        // Empty or whitespace-only command: fall through to worst-
        // case. The tool itself will almost certainly error, but we
        // still don't want to downgrade it — avoids a weird "we
        // reserve zero budget for a noop" free pass.
        return EffectClass::ApplyLocal;
    };

    // Flag-level write escape — rejected regardless of which
    // allowlist entry argv0 hits. Catches `git log --output=...`,
    // `git diff --output=...`, and the same family across any
    // other read-only entry that ever acquires a write-flag
    // option in a future ecosystem version.
    if has_write_flag(rest.iter().copied()) {
        return EffectClass::ApplyLocal;
    }

    // `git <subcommand>` and `cargo <subcommand>` require an argv[1]
    // match against the per-family allowlist. Bare `git` / `cargo`
    // with no subcommand prints usage — still read-only, but the
    // downstream model might just be probing; keep it cheap.
    match *argv0 {
        "git" => {
            let Some((sub, git_rest)) = rest.split_first() else {
                return EffectClass::Observe;
            };
            if GIT_READ_ONLY_SUBCOMMANDS.contains(sub) {
                return EffectClass::Observe;
            }
            // `git config --get <key>` is read-only. `git config` or
            // `git config --set` mutates .git/config. Keep the gate
            // narrow: require `--get` as argv[2].
            if *sub == "config" {
                if let Some(flag) = git_rest.first() {
                    if *flag == "--get" {
                        return EffectClass::Observe;
                    }
                }
            }
            EffectClass::ApplyLocal
        }
        "rustc" => {
            // `rustc --version` is read-only. `rustc <srcfile>`
            // compiles. Require an explicit `--version`.
            if rest.first().copied() == Some("--version") {
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
        // `find` deliberately omitted — see READ_ONLY_COMMANDS note
        // (gemini R0 HIGH 2026-04-24): `find -exec`/`-delete` are
        // write escapes.
        assert_observe("ls -la");
        assert_observe("cat Cargo.toml");
        assert_observe("wc -l src/main.rs");
        // Unquoted multi-char args remain fine.
        assert_observe("grep -r pattern src");
        assert_observe("rg --type rust Function");
    }

    #[test]
    fn quoted_args_force_apply_local_after_r1_gemini_high() {
        // gemini R1 HIGH (PR #30, 2026-04-24): quotes in the raw
        // command string let a payload like
        // `git log "--output=file"` sneak past has_write_flag, but
        // the shell strips quotes before exec. Any `'` or `"` in
        // the command now forces ApplyLocal.
        assert_apply_local(r#"git log "--output=/tmp/evil""#);
        assert_apply_local(r#"git log "--output=/tmp/evil" HEAD"#);
        assert_apply_local("git log '--output=/tmp/evil'");
        assert_apply_local(r#"grep "foo bar" src/"#);
        assert_apply_local("rg 'pattern with space' crates/");
    }

    #[test]
    fn find_and_env_classify_as_apply_local() {
        // Removed from allowlist after gemini R0 HIGH (2026-04-24).
        // Bare invocation would have been Observe in R0; now falls
        // through to ApplyLocal like any unknown argv0.
        assert_apply_local("find . -name *.rs");
        assert_apply_local("find . -exec rm {} +");
        assert_apply_local("find . -delete");
        assert_apply_local("env");
        assert_apply_local("env VAR=val grep foo");
    }

    #[test]
    fn xxd_classifies_as_apply_local_after_r1_codex_p1() {
        // codex R1 P1 (PR #30, 2026-04-24): `xxd -r` reverse mode
        // writes binary to an output file. Removed entirely from
        // READ_ONLY_COMMANDS because a single flag flips it from
        // read to write.
        assert_apply_local("xxd");
        assert_apply_local("xxd file");
        assert_apply_local("xxd -r dump.hex target.bin");
    }

    #[test]
    fn date_classifies_as_apply_local_after_r3_codex_p2() {
        // codex R2 P2 (PR #30, 2026-04-24): `date -s STRING` /
        // `date --set=STRING` sets system time (requires root /
        // CAP_SYS_TIME, but the sandbox may allow it on elevated
        // tiers). Removed from READ_ONLY_COMMANDS to close the
        // budget escape; bare `date +%Y` now falls through to
        // ApplyLocal.
        assert_apply_local("date");
        assert_apply_local("date +%Y-%m-%d");
        assert_apply_local("date -s 2020-01-01");
        assert_apply_local("date --set=2020-01-01");
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
        assert_observe("git rev-parse --short HEAD");
        assert_observe("git blame src/main.rs");
        // Unquoted glob — quoted forms (`'src/*.rs'`) go to
        // ApplyLocal after gemini R1 HIGH, see
        // `quoted_args_force_apply_local_after_r1_gemini_high`.
        assert_observe("git ls-files src");
    }

    #[test]
    fn git_diff_and_status_classify_as_apply_local_after_r3_codex_p1() {
        // codex R3 P1 (PR #30, 2026-04-24): `git diff` / `git status`
        // can write `.git/index` via refresh_index() when the stat
        // cache is stale (happens after any fs_write to tracked
        // files). Removed from GIT_READ_ONLY_SUBCOMMANDS.
        assert_apply_local("git diff");
        assert_apply_local("git diff main..HEAD");
        assert_apply_local("git diff --staged");
        assert_apply_local("git status");
        assert_apply_local("git status -s");
        assert_apply_local("git status --porcelain");
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
        // Removed from allowlist after gemini R0 HIGH (2026-04-24):
        // `git branch -D` / `git tag -d` mutate refs.
        assert_apply_local("git branch");
        assert_apply_local("git branch -D stale");
        assert_apply_local("git tag");
        assert_apply_local("git tag -d v0");
    }

    #[test]
    fn git_log_show_diff_with_output_flag_classify_as_apply_local() {
        // gemini R0 HIGH (2026-04-24): `git log --output=<file>`
        // writes regardless of subcommand being otherwise read-only.
        // `has_write_flag` matches `--output` exactly OR the
        // `--output=` prefix (filename embedded).
        assert_apply_local("git log --output=/tmp/evil");
        assert_apply_local("git log --output /tmp/evil");
        assert_apply_local("git diff --output=/tmp/evil");
        assert_apply_local("git diff --output /tmp/evil");
        assert_apply_local("git show --output=/tmp/evil HEAD");
    }

    #[test]
    fn non_write_output_prefix_flags_stay_observe() {
        // `--output-format`, `--output-indicator-new`, and friends
        // change formatting, not destination. The tight matcher
        // must not over-reject these. Use `git log` only because
        // `git diff` was removed from GIT_READ_ONLY_SUBCOMMANDS in
        // R3 (codex P1 on index refresh_index writes).
        assert_observe("git log --output-indicator-new=X");
        assert_observe("git log --output-indicator-new X");
    }

    #[test]
    fn dash_o_short_flag_is_not_over_rejected() {
        // `-o` is commonly non-write (grep only-match, ls long w/o
        // group). Conservative-but-narrow: we reject `--output`,
        // not every `-o`. These must stay Observe.
        assert_observe("grep -o foo src/");
        assert_observe("ls -o");
    }

    #[test]
    fn bare_git_classifies_as_observe_after_r3_cargo_removal() {
        // Bare `git` prints usage — still read-only.
        assert_observe("git");
        // Bare `cargo` used to print usage → Observe in R0–R2.
        // R3 removed the cargo subcommand allowlist entirely
        // (codex R2 P1 on `cargo check --target-dir`), so cargo
        // now falls through to ApplyLocal.
        assert_apply_local("cargo");
    }

    #[test]
    fn git_config_requires_get_flag() {
        assert_observe("git config --get user.email");
        assert_apply_local("git config user.email foo@bar.com");
        assert_apply_local("git config --set user.email foo@bar.com");
        assert_apply_local("git config --unset user.email");
    }

    #[test]
    fn cargo_all_subcommands_classify_as_apply_local_after_r3() {
        // codex R2 P1 (PR #30, 2026-04-24): `cargo check
        // --target-dir <DIR>` writes artifacts to any path, and
        // `cargo metadata` / `cargo tree` can update Cargo.lock
        // in an unlocked workspace. Removed the cargo subcommand
        // allowlist entirely in R3; cargo falls through to
        // ApplyLocal via unknown-argv0 on every invocation.
        assert_apply_local("cargo check");
        assert_apply_local("cargo check --target-dir /tmp/evil");
        assert_apply_local("cargo metadata --format-version 1");
        assert_apply_local("cargo tree");
        assert_apply_local("cargo version");
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
