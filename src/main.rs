//! `ta` — taska binary entrypoint.

use std::process::ExitCode;

mod cli;
mod engine;
mod graph;
mod merge;
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
