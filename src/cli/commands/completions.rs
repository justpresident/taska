//! `ta completions <shell>` - shell completion. By default prints the DYNAMIC
//! registration shim (sourced/auto-loaded, it makes the shell call `ta` back at
//! completion time so task ids / fields / columns complete live from the store).
//! `--install [user|system]` writes the shim into the shell's auto-loaded
//! completion file instead, falling back to `sudo` when the target needs root.
//!
//! `offer_install` is the interactive helper `ta init` calls.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap_complete::env::Shells;
use clap_complete::Shell;

use crate::error::DynError;

/// Where to install completions.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum InstallScope {
    /// Just for the current user (no root needed)
    User,
    /// System-wide (writes under /usr; may prompt for sudo)
    System,
}

/// `ta completions <shell> [--install [scope]]`.
///
/// `install`: `None` prints the shim; `Some(None)` installs after asking where;
/// `Some(Some(scope))` installs for that scope. (The nested `Option` is clap's
/// idiom for a flag with an optional value: absent / `--install` / `--install x`.)
#[allow(clippy::option_option)]
pub fn cmd_completions(
    shell: Shell,
    install: Option<Option<InstallScope>>,
) -> Result<(), DynError> {
    let shim = shim(shell)?;
    let Some(scope) = install else {
        print!("{shim}");
        return Ok(());
    };
    let scope = scope.unwrap_or_else(prompt_scope);
    install_shim(shell, scope, &shim)
}

/// Offer (interactively, on a TTY) to install completions for the user's shell -
/// the hook `ta init` runs. A no-op when non-interactive, the shell is unknown or
/// unsupported, or completions are already installed.
pub fn offer_install() {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return;
    }
    let Some(shell) = login_shell() else {
        return;
    };
    // Already installed for either scope? Don't nag.
    if [InstallScope::User, InstallScope::System]
        .into_iter()
        .filter_map(|s| target_path(shell, s).ok())
        .any(|p| p.exists())
    {
        return;
    }
    if !ask(&format!("Set up `ta` tab-completion for {shell}?"), false) {
        return;
    }
    let Ok(shim) = shim(shell) else { return };
    if let Err(e) = install_shim(shell, prompt_scope(), &shim) {
        eprintln!("completion setup skipped: {e}");
    }
}

// --- shim + install --------------------------------------------------------

/// The registration shim for `shell`, as a string.
fn shim(shell: Shell) -> Result<String, DynError> {
    let name = shell.to_string();
    let shells = Shells::builtins();
    let completer = shells
        .completer(&name)
        .ok_or_else(|| format!("no completion support for `{name}`"))?;
    let mut buf = Vec::new();
    completer.write_registration("COMPLETE", "ta", "ta", "ta", &mut buf)?;
    Ok(String::from_utf8(buf)?)
}

/// The auto-loaded completion file for `shell`/`scope`.
fn target_path(shell: Shell, scope: InstallScope) -> Result<PathBuf, DynError> {
    let home = || -> Result<PathBuf, DynError> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".into())
    };
    let xdg = |var: &str, default: &str| -> Result<PathBuf, DynError> {
        match std::env::var_os(var) {
            Some(d) => Ok(PathBuf::from(d)),
            None => Ok(home()?.join(default)),
        }
    };
    let path = match (shell, scope) {
        (Shell::Bash, InstallScope::User) => {
            xdg("XDG_DATA_HOME", ".local/share")?.join("bash-completion/completions/ta")
        }
        (Shell::Bash, InstallScope::System) => {
            PathBuf::from("/usr/share/bash-completion/completions/ta")
        }
        (Shell::Zsh, InstallScope::User) => {
            xdg("XDG_DATA_HOME", ".local/share")?.join("zsh/site-functions/_ta")
        }
        (Shell::Zsh, InstallScope::System) => PathBuf::from("/usr/share/zsh/site-functions/_ta"),
        (Shell::Fish, InstallScope::User) => {
            xdg("XDG_CONFIG_HOME", ".config")?.join("fish/completions/ta.fish")
        }
        (Shell::Fish, InstallScope::System) => {
            PathBuf::from("/usr/share/fish/vendor_completions.d/ta.fish")
        }
        (other, _) => {
            return Err(format!(
                "--install doesn't support `{other}` (no standard completion dir); \
                 source `ta completions {other}` from your shell config instead"
            )
            .into());
        }
    };
    Ok(path)
}

/// Write `shim` to the right file for `shell`/`scope` (via `sudo` if it needs
/// root), then - for the one fiddly case, zsh user - put the dir on `$fpath`.
fn install_shim(shell: Shell, scope: InstallScope, shim: &str) -> Result<(), DynError> {
    let path = target_path(shell, scope)?;
    write_file(&path, shim)?;
    if matches!((shell, scope), (Shell::Zsh, InstallScope::User)) {
        if let Some(dir) = path.parent() {
            ensure_zsh_fpath(dir)?;
        }
    }
    println!("Installed {shell} completions to {}", path.display());
    println!("Start a new shell (or `exec {shell}`) to use them.");
    Ok(())
}

/// Write `content` to `path`, creating parents. If that's denied, retry through
/// `sudo` (which prompts for the password) rather than failing.
fn write_file(path: &Path, content: &str) -> Result<(), DynError> {
    let direct = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, content));
    if direct.is_ok() {
        return Ok(());
    }
    sudo_write(path, content)
}

/// `sudo mkdir -p <dir>` then pipe `content` to `sudo tee <path>`.
fn sudo_write(path: &Path, content: &str) -> Result<(), DynError> {
    let no_sudo = || -> DynError {
        format!(
            "can't write {} and `sudo` is unavailable; write it yourself, e.g.\n  \
             ta completions <shell> | sudo tee {}",
            path.display(),
            path.display()
        )
        .into()
    };
    if let Some(dir) = path.parent() {
        let ok = Command::new("sudo")
            .args(["mkdir", "-p"])
            .arg(dir)
            .status()
            .map_err(|_| no_sudo())?
            .success();
        if !ok {
            return Err(format!("`sudo mkdir -p {}` failed", dir.display()).into());
        }
    }
    let mut child = Command::new("sudo")
        .arg("tee")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|_| no_sudo())?;
    child
        .stdin
        .take()
        .ok_or("sudo: no stdin")?
        .write_all(content.as_bytes())?;
    if child.wait()?.success() {
        Ok(())
    } else {
        Err(format!("`sudo tee {}` failed", path.display()).into())
    }
}

/// Add `dir` to `$fpath` and run `compinit` from `~/.zshrc` (idempotent), so the
/// just-written `_ta` is actually picked up - the user site-functions dir isn't on
/// the default `$fpath`.
fn ensure_zsh_fpath(dir: &Path) -> Result<(), DynError> {
    let rc = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?
        .join(".zshrc");
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains("taska completion fpath") {
        return Ok(());
    }
    let block = format!(
        "\n# taska completion fpath (added by `ta`)\nfpath=(\"{}\" $fpath)\nautoload -U compinit && compinit\n",
        dir.display()
    );
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc)?;
    f.write_all(block.as_bytes())?;
    println!("Added {} to $fpath in {}", dir.display(), rc.display());
    Ok(())
}

// --- prompts ---------------------------------------------------------------

/// Ask `user` vs `system` (default user); non-interactive -> user.
fn prompt_scope() -> InstallScope {
    if !std::io::stdin().is_terminal() {
        return InstallScope::User;
    }
    print!("Install for (u)ser or (s)ystem? [u] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_ok() {
        let a = line.trim();
        if a.eq_ignore_ascii_case("s") || a.eq_ignore_ascii_case("system") {
            return InstallScope::System;
        }
    }
    InstallScope::User
}

/// A `[y/N]` (or `[Y/n]`) prompt; non-interactive returns `default`.
fn ask(question: &str, default: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        return default;
    }
    print!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

/// The login shell as a completion `Shell`, from `$SHELL`.
fn login_shell() -> Option<Shell> {
    let shell = std::env::var_os("SHELL")?;
    let name = Path::new(&shell).file_name()?.to_str()?;
    match name {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        _ => None,
    }
}
