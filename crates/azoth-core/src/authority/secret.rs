//! `SecretHandle` — an opaque string value that never implements `Serialize`,
//! never leaks into evidence, and redacts itself under `Debug`.

use std::fmt;
use std::sync::Arc;

#[derive(Clone)]
pub struct SecretHandle(Arc<str>);

impl SecretHandle {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    /// The only escape hatch. Callers that truly need the cleartext —
    /// typically the HTTP header injector in an adapter — take an explicit
    /// `&SecretHandle` and call this at the last moment.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretHandle([REDACTED])")
    }
}

impl fmt::Display for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

// Intentionally no `Serialize`/`Deserialize` impls. Presence of a
// `SecretHandle` in any struct derived `Serialize` is a compile error — which
// is exactly the bug we want to catch.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretHandle::new("sk-abc123");
        let d = format!("{:?}", s);
        assert!(!d.contains("sk-abc123"));
        assert!(d.contains("REDACTED"));
    }

    #[test]
    fn expose_returns_cleartext() {
        let s = SecretHandle::new("sk-abc123");
        assert_eq!(s.expose(), "sk-abc123");
    }
}
