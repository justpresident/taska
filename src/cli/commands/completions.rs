//! `ta completions <shell>` - print a static shell-completion script to stdout.
//!
//! The script is generated from the live clap [`Command`] (passed in by `run()` as
//! `Cli::command()`), so it always matches the real subcommands and flags - no
//! hand-maintained list to drift. Static only: it completes subcommands/flags, not
//! task ids/values (dynamic completion is a separate, store-aware follow-up).

use clap::Command;
use clap_complete::{generate, Shell};

/// Write the completion script for `shell` to stdout. Infallible (`generate`
/// swallows write errors), so the caller wraps the `Ok(())` for dispatch.
pub fn cmd_completions(shell: Shell, command: &mut Command) {
    generate(shell, command, "ta", &mut std::io::stdout());
}
