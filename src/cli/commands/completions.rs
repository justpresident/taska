//! `ta completions <shell>` - print the DYNAMIC completion registration for a
//! shell. Sourced into the shell rc, it makes the shell call back into `ta` at
//! completion time (the `COMPLETE` env var set), so completion is store-aware
//! (task ids, filter fields, columns) and always matches the live CLI.
//!
//! Install, e.g.: bash `echo 'source <(ta completions bash)' >> ~/.bashrc`.

use clap_complete::env::Shells;
use clap_complete::Shell;

use crate::error::DynError;

/// Write the registration shim for `shell` to stdout.
pub fn cmd_completions(shell: Shell) -> Result<(), DynError> {
    let name = shell.to_string();
    let shells = Shells::builtins();
    let completer = shells
        .completer(&name)
        .ok_or_else(|| format!("no completion support for `{name}`"))?;
    // var, name (identifier), bin (completed), completer (callback binary).
    completer.write_registration("COMPLETE", "ta", "ta", "ta", &mut std::io::stdout())?;
    Ok(())
}
