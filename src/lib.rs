//! `taska` — local-first, git-native task & dependency tracker.
//!
//! The core (model, engine, storage, merge, graph, config, error) plus the CLI
//! surface live in this library crate; the `ta` binary is a thin wrapper that
//! calls [`cli::run`].

#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo,
    clippy::unwrap_used,
    clippy::panic,
    clippy::dbg_macro,
    clippy::missing_const_for_fn,
    clippy::needless_pass_by_value,
    clippy::redundant_pub_crate
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::multiple_crate_versions,
    clippy::missing_panics_doc
)]

pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod format;
pub mod git;
pub mod graph;
pub mod merge;
pub mod migrate;
pub mod model;
pub mod schema;
pub mod storage;

#[cfg(test)]
mod test_support;
