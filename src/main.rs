//! `ta` — taska binary entrypoint.

use std::process::ExitCode;

mod cli;
mod config;
mod engine;
mod error;
mod git;
mod graph;
mod merge;
mod model;
mod storage;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::FAILURE
        }
    }
}
