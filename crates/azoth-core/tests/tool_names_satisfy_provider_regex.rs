//! Every Tool registered in the built-in dispatcher MUST have a name
//! that satisfies Anthropic's Messages API tool-name regex:
//!
//!   `^[a-zA-Z0-9_-]{1,128}$`
//!
//! Violating this triggers a live 400 `invalid_request_error` on every
//! Anthropic request — as it did on the first OAuth dogfood attempt
//! after PR #12 landed the Bearer path (tool name was `repo.search`;
//! the dot is not in the allowed character class).
//!
//! This test is the permanent regression gate. Every tool added to the
//! built-in dispatcher list here must pass; registration-time
//! `assert!` in `ToolDispatcher::register` catches it in all other
//! builds too (unit tests, integration tests, production startup).

use azoth_core::execution::ToolDispatcher;
use azoth_core::tools::{
    BashTool, FsWriteTool, RepoReadFileTool, RepoReadSpansTool, RepoSearchTool,
};

/// Anthropic Messages API regex: `^[a-zA-Z0-9_-]{1,128}$`.
/// Implemented without pulling in the `regex` crate — ASCII check only.
fn name_satisfies_provider_regex(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[test]
fn all_builtin_tool_names_match_anthropic_regex() {
    let mut disp = ToolDispatcher::new();
    disp.register(RepoSearchTool);
    disp.register(RepoReadFileTool);
    disp.register(RepoReadSpansTool);
    disp.register(FsWriteTool);
    disp.register(BashTool);
    for name in disp.names() {
        assert!(
            name_satisfies_provider_regex(name),
            "tool name {name:?} violates Anthropic Messages API regex \
             ^[a-zA-Z0-9_-]{{1,128}}$ — rename to use only ASCII \
             letters/digits/underscore/hyphen"
        );
    }
}

#[test]
fn regex_helper_rejects_dot_and_accepts_underscore() {
    // Sanity-check the helper so a future refactor can't silently
    // loosen it and let dotted names through.
    assert!(name_satisfies_provider_regex("repo_search"));
    assert!(name_satisfies_provider_regex("fs_write"));
    assert!(name_satisfies_provider_regex("bash"));
    // Negative cases — a previous global rename broke these literals
    // by rewriting "repo.search" → "repo_search" everywhere. Build the
    // dotted strings from pieces so a future sed sweep can't flatten
    // them again.
    let bad_repo_search: String = format!("{}.{}", "repo", "search");
    let bad_fs_write: String = format!("{}.{}", "fs", "write");
    assert!(!name_satisfies_provider_regex(&bad_repo_search));
    assert!(!name_satisfies_provider_regex(&bad_fs_write));
    assert!(!name_satisfies_provider_regex(""));
    assert!(!name_satisfies_provider_regex(&"x".repeat(129)));
}
