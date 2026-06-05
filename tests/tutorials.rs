//! Smoke test: run every `tutorials/NN-*.sh` end to end on `cargo test`.
//!
//! The tutorials double as runnable UX walkthroughs, so exercising them here
//! catches drift when a CLI change breaks one (the recent `block`/`unblock` ->
//! `dep` migration would have been caught automatically). Each script spins up
//! its own throwaway repo outside the checkout (lib.sh's `fresh_repo` uses
//! `mktemp` under the system temp dir), so they never touch the repo's own
//! `.taska`. We run them with the built binary's directory prepended to `PATH`
//! (matching how the tutorials expect to find `ta`) and `TUTORIAL_NONINTERACTIVE=1`
//! so every `pause` returns immediately, then assert each exits 0 — naming any
//! that don't.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Directory holding the built `ta` binary (from `CARGO_BIN_EXE_ta`).
fn bin_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_ta"))
        .parent()
        .expect("binary has a parent directory")
        .to_path_buf()
}

/// `PATH` with the built `ta`'s directory prepended, so lib.sh's up-front
/// `command -v ta` check (and every `ta`/`git ta git-merge` call) resolves to
/// the binary under test.
fn path_with_bin() -> OsString {
    let mut dirs = vec![bin_dir()];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).expect("join PATH")
}

/// The crate's `tutorials/` directory.
fn tutorials_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tutorials")
}

/// Every `NN-*.sh` scenario script (two-digit prefix), in sorted order. The
/// `lib.sh` / `run-all.sh` helpers and `README.md` lack the prefix and are
/// excluded.
fn tutorial_scripts() -> Vec<PathBuf> {
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(tutorials_dir())
        .expect("read tutorials directory")
        .map(|e| e.expect("read dir entry").path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let b = name.as_bytes();
            b.len() > 3
                && b[0].is_ascii_digit()
                && b[1].is_ascii_digit()
                && b[2] == b'-'
                && name.ends_with(".sh")
        })
        .collect();
    scripts.sort();
    scripts
}

#[test]
fn every_tutorial_runs_clean() {
    let scripts = tutorial_scripts();
    // Guard against a vacuous pass if discovery ever silently matches nothing.
    assert!(
        !scripts.is_empty(),
        "found no tutorials/NN-*.sh under {}",
        tutorials_dir().display()
    );

    let path = path_with_bin();
    let mut failures = Vec::new();
    for script in &scripts {
        let name = script
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>");
        // `bash <path>` mirrors run-all.sh; the absolute path lets lib.sh's
        // `source "$(dirname "$0")/lib.sh"` resolve. stdin is /dev/null so any
        // stray `read` can't block even if the env guard regressed.
        let out = Command::new("bash")
            .arg(script)
            .env("PATH", &path)
            .env("TUTORIAL_NONINTERACTIVE", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn `bash {}`: {e}", script.display()));
        if !out.status.success() {
            failures.push(format!(
                "--- {name} exited with {} ---\n{}{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} tutorial(s) failed:\n\n{}",
        failures.len(),
        scripts.len(),
        failures.join("\n")
    );
}
