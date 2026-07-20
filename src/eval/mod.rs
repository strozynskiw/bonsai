//! Eval harness: runs eval suites against a mock or live provider and writes a
//! byte-stable `report.json` plus a `summary.md`.
//!
//! This module is the wiring/re-export layer. The crate-facing surface is
//! [`EvalCliConfig`], [`EvalMode`], [`EvalRunOutcome`], and [`run`]; everything
//! else is internal and split across the submodules below:
//!
//! - [`cli`] — CLI config + mode parsing.
//! - [`run`] — the runner, suite/task models, and provider setup.
//! - [`fs_util`] — safe relative paths, fixture copying, detail truncation.
//! - [`grader`] — grader specs and the grading helpers.
//! - [`report`] — the serialized report types and summary formatting.
//! - [`mock`] — the deterministic mock provider.
//!
//! Each submodule pulls the shared eval namespace in via `use super::*`, so the
//! cross-module `pub(crate)` items below behave as if they still lived in one
//! file.

mod adapters;
mod baseline;
mod budget;
mod cli;
mod fs_util;
mod grader;
mod mock;
mod report;
mod run;

pub(crate) use adapters::*;
pub(crate) use baseline::*;
pub(crate) use budget::*;
pub(crate) use cli::*;
pub(crate) use fs_util::*;
pub(crate) use grader::*;
pub(crate) use mock::*;
pub(crate) use report::*;
pub(crate) use run::*;
