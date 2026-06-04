//! `ta` — taska binary entrypoint.

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

use std::process::ExitCode;

use taska::cli;

fn main() -> ExitCode {
    // Rust ignores SIGPIPE, so writing to a closed pipe (`ta list | head`) makes
    // the print macros panic instead of the process terminating the usual way.
    // Restore the default action so a broken pipe exits cleanly with a signal
    // status rather than a panic + backtrace.
    #[cfg(unix)]
    unsafe {
        // SAFETY: resetting a signal disposition to SIG_DFL is always sound.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
