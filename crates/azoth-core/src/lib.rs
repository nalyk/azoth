#![allow(dead_code)]
//! Azoth runtime library.
//!
//! Seven invariants (see docs/draft_plan.md):
//!  1. Transcript is not memory.
//!  2. Deterministic controls outrank model output.
//!  3. Every non-trivial run has a contract.
//!  4. Every side effect has a class.
//!  5. Every run leaves structured evidence.
//!  6. Every subsystem is eval-able.
//!  7. Turn-scoped atomicity.

pub mod adapter;
pub mod artifacts;
pub mod authority;
pub mod context;
pub mod contract;
pub mod eval;
pub mod event_store;
pub mod execution;
pub mod impact;
pub mod retrieval;
pub mod sandbox;
pub mod schemas;
pub mod telemetry;
pub mod tools;
pub mod turn;
pub mod validators;

/// Injection-surface red-team tests (Sprint 7 + PR #11 Codex P1).
/// Lives inside `src/` under `#[cfg(test)]` so the tests can reach
/// `pub(crate) Tainted::new` without forcing a public constructor
/// whose compilation visibility would re-open the provenance gate
/// to downstream consumers.
#[cfg(test)]
mod red_team;

pub use schemas::*;
