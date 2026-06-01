//! `taska` — local-first, git-native task & dependency engine.
//!
//! The core (model, engine, storage, merge, graph, config, error) plus the CLI
//! surface live in this library crate; the `ta` binary is a thin wrapper that
//! calls [`cli::run`].

pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod git;
pub mod graph;
pub mod merge;
pub mod model;
pub mod storage;
