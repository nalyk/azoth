//! Tainted values flow through Azoth whenever data originates from something
//! other than Azoth itself. The dispatcher is the only place that strips the
//! taint, and only via a policy-checked `Extractor`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    User,
    Contract,
    ToolOutput,
    RepoFile,
    WebFetch,
    ModelOutput,
}

/// Wraps a value whose provenance is not Azoth itself. The inner value can
/// only be read by an `Extractor` that the runtime trusts for this Origin.
///
/// There is intentionally no public `into_inner`. Constructors are restricted
/// to this crate (`pub(crate)`), so tool authors cannot mint arbitrary
/// `Tainted<T>` of the wrong origin.
#[derive(Debug, Clone)]
pub struct Tainted<T> {
    origin: Origin,
    inner: T,
}

impl<T> Tainted<T> {
    pub(crate) fn new(origin: Origin, inner: T) -> Self {
        Self { origin, inner }
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Only visible within azoth-core. Callers outside the crate must go
    /// through an `Extractor`.
    pub(crate) fn inner_ref(&self) -> &T {
        &self.inner
    }

    pub(crate) fn into_parts(self) -> (Origin, T) {
        (self.origin, self.inner)
    }
}

#[derive(Debug, Error)]
pub enum ExtractionError {
    #[error("origin {0:?} is not permitted by extractor {1}")]
    OriginNotPermitted(Origin, &'static str),
    #[error("schema validation failed: {0}")]
    Schema(String),
    #[error("deserialize error: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// The dispatcher consults `Extractor` implementations to convert
/// `Tainted<T>` into a tool-specific typed input. Extractors are the *only*
/// legal way to strip taint.
pub trait Extractor<T, U>: Send + Sync {
    fn name(&self) -> &'static str;
    fn permitted_origins(&self) -> &'static [Origin];
    fn extract(&self, input: Tainted<T>) -> Result<U, ExtractionError>;
}

/// Default JSON extractor: deserializes `serde_json::Value` into any
/// `DeserializeOwned` type, after checking the origin against the allowlist.
pub struct JsonExtractor<U> {
    origins: &'static [Origin],
    _phantom: std::marker::PhantomData<fn() -> U>,
}

impl<U> JsonExtractor<U> {
    pub const fn new(origins: &'static [Origin]) -> Self {
        Self {
            origins,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<U> Extractor<serde_json::Value, U> for JsonExtractor<U>
where
    U: serde::de::DeserializeOwned + Send,
{
    fn name(&self) -> &'static str {
        "json"
    }

    fn permitted_origins(&self) -> &'static [Origin] {
        self.origins
    }

    fn extract(&self, input: Tainted<serde_json::Value>) -> Result<U, ExtractionError> {
        if !self.origins.contains(&input.origin()) {
            return Err(ExtractionError::OriginNotPermitted(input.origin(), "json"));
        }
        let (_origin, value) = input.into_parts();
        serde_json::from_value(value).map_err(ExtractionError::from)
    }
}

/// Crate-internal constructor shim used by the dispatcher and adapter layer
/// to mint the initial Tainted wrappers on ingress.
pub(crate) fn taint<T>(origin: Origin, inner: T) -> Tainted<T> {
    Tainted::new(origin, inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Input {
        q: String,
    }

    #[test]
    fn extractor_accepts_permitted_origin() {
        let ex: JsonExtractor<Input> = JsonExtractor::new(&[Origin::ModelOutput]);
        let raw = taint(Origin::ModelOutput, serde_json::json!({ "q": "hello" }));
        let out = ex.extract(raw).unwrap();
        assert_eq!(out, Input { q: "hello".into() });
    }

    #[test]
    fn extractor_rejects_foreign_origin() {
        let ex: JsonExtractor<Input> = JsonExtractor::new(&[Origin::User]);
        let raw = taint(Origin::ModelOutput, serde_json::json!({ "q": "hello" }));
        let err = ex.extract(raw).unwrap_err();
        assert!(matches!(
            err,
            ExtractionError::OriginNotPermitted(Origin::ModelOutput, _)
        ));
    }
}
