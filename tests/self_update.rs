//! e2e: `ta self-update` - the in-place binary updater.
//!
//! The actual download/replace is network- and release-dependent, so it isn't
//! driven here. We cover the offline-knowable contract: the help text, and that
//! the command reports the running binary's own version before reaching out.

mod common;
use common::*;

#[test]
fn self_update_help_documents_the_in_place_update_and_cargo_fallback() {
    let dir = fresh_dir("self-update-help");
    let out = run(ta_bin(), &dir, &["self-update", "--help"]);
    assert!(out.status.success(), "self-update --help should succeed");
    let help = String::from_utf8_lossy(&out.stdout);
    // The two load-bearing promises: replaces the RUNNING binary, and there's a
    // cargo fallback for platforms with no prebuilt asset.
    assert!(
        help.contains("current_exe"),
        "help explains it updates the running binary: {help}"
    );
    assert!(
        help.contains("cargo install taska"),
        "help points unsupported platforms at cargo: {help}"
    );
    assert!(
        help.contains("--check") && help.contains("--force"),
        "help lists the flags: {help}"
    );
}

#[test]
fn self_update_check_reports_the_running_version_first() {
    let dir = fresh_dir("self-update-check");
    // `--check` prints the running binary's version line BEFORE the network
    // lookup of the latest release, so this holds whether or not the host can
    // reach GitHub (we don't assert the exit status, which depends on the net).
    let out = run(ta_bin(), &dir, &["self-update", "--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("ta {} (running from", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.contains(&expected),
        "expected running-version line `{expected}` in stdout:\n{stdout}"
    );
}
