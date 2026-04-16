//! Input helpers — typed slash-command parser.
//!
//! The TUI's Enter handler calls [`SlashCommand::parse`] on every non-empty
//! line; when it returns `Some`, the UI handles the command locally and the
//! worker never sees it. A leading `/` is required — anything else returns
//! `None` and falls through to user text.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Status,
    Context,
    /// `/contract [goal text...]` — empty argument shows usage; a non-empty
    /// rest-of-line is treated as the contract goal.
    Contract(Option<String>),
    /// `/approve [tool_name]` — no argument lists active capability tokens;
    /// a tool name pre-grants a session-scoped token for that tool.
    Approve(Option<String>),
    Quit,
    /// `/resume <run_id>` — the argument is `None` when no token follows.
    Resume(Option<String>),
    /// Anything beginning with `/` that didn't match a known verb.
    Unknown(String),
}

impl SlashCommand {
    /// Parse a single input line. Returns `None` unless the trimmed line
    /// starts with `/`.
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let mut parts = trimmed.split_whitespace();
        let head = parts.next()?;
        let name = &head[1..];
        let rest_of_line = || {
            let after = trimmed[head.len()..].trim();
            if after.is_empty() {
                None
            } else {
                Some(after.to_string())
            }
        };
        Some(match name {
            "help" => Self::Help,
            "status" => Self::Status,
            "context" => Self::Context,
            "contract" => Self::Contract(rest_of_line()),
            "approve" => Self::Approve(rest_of_line()),
            "quit" => Self::Quit,
            "resume" => Self::Resume(parts.next().map(|s| s.to_string())),
            other => Self::Unknown(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slash_help() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("  /help  "), Some(SlashCommand::Help));
    }

    #[test]
    fn parses_slash_resume_with_arg() {
        assert_eq!(
            SlashCommand::parse("/resume run_abc123"),
            Some(SlashCommand::Resume(Some("run_abc123".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/resume"),
            Some(SlashCommand::Resume(None))
        );
    }

    #[test]
    fn unknown_returns_unknown() {
        assert_eq!(
            SlashCommand::parse("/foo"),
            Some(SlashCommand::Unknown("foo".to_string()))
        );
    }

    #[test]
    fn non_slash_is_none() {
        assert_eq!(SlashCommand::parse("hello world"), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(SlashCommand::parse("   "), None);
    }

    #[test]
    fn all_known_verbs_parse() {
        assert_eq!(SlashCommand::parse("/status"), Some(SlashCommand::Status));
        assert_eq!(SlashCommand::parse("/context"), Some(SlashCommand::Context));
        assert_eq!(
            SlashCommand::parse("/contract"),
            Some(SlashCommand::Contract(None))
        );
        assert_eq!(
            SlashCommand::parse("/contract fix token refresh"),
            Some(SlashCommand::Contract(Some(
                "fix token refresh".to_string()
            )))
        );
        assert_eq!(
            SlashCommand::parse("/approve"),
            Some(SlashCommand::Approve(None))
        );
        assert_eq!(
            SlashCommand::parse("/approve fs.write"),
            Some(SlashCommand::Approve(Some("fs.write".to_string())))
        );
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
    }
}
