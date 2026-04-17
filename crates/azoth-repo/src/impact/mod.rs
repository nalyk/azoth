//! Sprint 5 — TDAD test impact selection.
//!
//! Concrete selectors live here so `azoth-core` stays free of
//! `cargo`/`git` shell-out deps. The public shape:
//!
//! - [`cargo::CargoTestImpact`] — v2 default for Rust-on-Rust
//!   dogfood (the only ecosystem v2 ships; pytest/jest/go-test in
//!   v2.1).
//! - [`git_status::GitStatusDiffSource`] — `DiffSource` impl that
//!   shells out to `git status --porcelain` in the repo root. Used
//!   by the TUI worker to materialise a `Diff` at the turn's
//!   validate phase.
//! - Both use `std::process::Command` / `tokio::process::Command`
//!   rather than `gix` or `git2` — consistent with the Sprint 3
//!   co-edit graph's "no new git dep" rule (v2 plan §A3, §Sprint 3).

pub mod cargo;
pub mod git_status;

pub use cargo::{discover_cargo_tests, CargoTestImpact, TestUniverse, CARGO_TEST_IMPACT_VERSION};
pub use git_status::{parse_porcelain as parse_porcelain_for_tests, GitStatusDiffSource};
