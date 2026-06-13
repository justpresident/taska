//! e2e: `ta completions <shell>` - the static shell-completion scripts.
//!
//! Drives the real binary; completions need no store, so a bare throwaway dir is
//! enough. Where the shell is installed we also syntax-check the script (`bash -n`
//! / `zsh -n`); otherwise we settle for "non-empty, has the expected markers".

mod common;
use common::*;

/// Whether `shell` is installed (so we can syntax-check against it).
fn shell_present(shell: &str) -> bool {
    Command::new(shell)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// When `shell` is installed, assert it accepts `script` (`<shell> -n <file>`).
fn assert_syntax_ok(dir: &Path, shell: &str, name: &str, script: &str) {
    if !shell_present(shell) {
        return;
    }
    let file = dir.join(name);
    fs::write(&file, script).unwrap();
    let out = Command::new(shell).arg("-n").arg(&file).output().unwrap();
    assert!(
        out.status.success(),
        "`{shell} -n` rejected the generated script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn completions_emit_valid_scripts_per_shell() {
    let dir = fresh_dir("completions");

    // bash: a `_ta` completion function that `bash -n` parses (bash is always here).
    let bash = ta(&dir, &["completions", "bash"]);
    assert!(bash.contains("_ta"), "bash completion names `_ta`: {bash}");
    assert_syntax_ok(&dir, "bash", "ta.bash", &bash);

    // zsh: a `#compdef` autoload script; syntax-check it when zsh is installed.
    let zsh = ta(&dir, &["completions", "zsh"]);
    assert!(
        zsh.contains("#compdef"),
        "zsh script has a #compdef header: {zsh}"
    );
    assert_syntax_ok(&dir, "zsh", "_ta", &zsh);

    // The other clap_complete shells generate too - just check they're non-empty.
    for shell in ["fish", "powershell", "elvish"] {
        assert!(
            !ta(&dir, &["completions", shell]).trim().is_empty(),
            "{shell} completion is non-empty"
        );
    }

    // An unknown shell is rejected by clap (non-zero exit).
    let bad = run(ta_bin(), &dir, &["completions", "tcsh"]);
    assert!(!bad.status.success(), "unknown shell rejected");
}
