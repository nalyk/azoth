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

pub mod schemas;
pub mod authority;
pub mod event_store;
pub mod artifacts;
pub mod adapter;
pub mod context;
pub mod retrieval;
pub mod execution;
pub mod tools;
pub mod sandbox;
pub mod turn;
pub mod validators;
pub mod telemetry;
pub mod contract;

pub use schemas::*;
