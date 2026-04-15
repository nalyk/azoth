//! Input helpers — slash command resolver, @file completion. v1 stubs.

pub fn resolve_slash(raw: &str) -> Option<&'static str> {
    match raw.trim_start_matches('/') {
        "contract" => Some("contract"),
        "approve" => Some("approve"),
        "status" => Some("status"),
        "context" => Some("context"),
        "resume" => Some("resume"),
        "quit" => Some("quit"),
        "help" => Some("help"),
        _ => None,
    }
}
