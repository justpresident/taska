//! e2e: `ta completions <shell>` (the dynamic registration shim) and the live,
//! store-aware completion it drives (task ids, filter fields, columns).
//!
//! Sourcing the shim makes the shell call `ta` back with `COMPLETE` set; we drive
//! that path directly with the env the shim would pass.

mod common;
use common::names::*;
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
fn assert_syntax_ok(dir: &Path, shell: &str, script: &str) {
    if !shell_present(shell) {
        return;
    }
    let file = dir.join(format!("ta.{shell}"));
    fs::write(&file, script).unwrap();
    let out = Command::new(shell).arg("-n").arg(&file).output().unwrap();
    assert!(
        out.status.success(),
        "`{shell} -n` rejected the shim:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Drive one completion request the way the shim would: `COMPLETE=bash` plus the
/// index of the word under the cursor. Returns the candidate lines.
fn complete(dir: &Path, index: usize, words: &[&str]) -> String {
    let out = Command::new(ta_bin())
        .arg("--")
        .args(words)
        .current_dir(dir)
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", index.to_string())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn completions_emit_valid_registration_shims() {
    let dir = fresh_dir("completions");

    // bash/zsh: a non-empty shim that wires the COMPLETE callback and parses.
    let bash = ta(&dir, &["completions", "bash"]);
    assert!(
        bash.contains("COMPLETE=") && bash.contains("complete "),
        "bash shim wires COMPLETE: {bash}"
    );
    assert_syntax_ok(&dir, "bash", &bash);

    let zsh = ta(&dir, &["completions", "zsh"]);
    assert!(zsh.contains("COMPLETE="), "zsh shim wires COMPLETE: {zsh}");
    assert_syntax_ok(&dir, "zsh", &zsh);

    // The other clap_complete shells emit too - just check they're non-empty.
    for shell in ["fish", "powershell", "elvish"] {
        assert!(
            !ta(&dir, &["completions", shell]).trim().is_empty(),
            "{shell} shim is non-empty"
        );
    }

    // An unknown shell is rejected by clap (non-zero exit).
    assert!(
        !run(ta_bin(), &dir, &["completions", "tcsh"])
            .status
            .success(),
        "unknown shell rejected"
    );
}

#[test]
fn dynamic_completion_offers_task_ids_fields_and_columns() {
    let dir = fresh_dir("complete-dyn");
    init_renamed_open(&dir); // status displayed as `state`, blocker as `needs`
    ta(&dir, &["create", "alpha", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["create", "beta", &format!("{STATUS_FIELD}=closed")]);
    ta(&dir, &["dep", "add", "alpha", &format!("{BLOCKER}=beta")]);

    // `ta show <TAB>` -> task ids.
    let ids = complete(&dir, 2, &["ta", "show", ""]);
    assert!(
        ids.lines().any(|l| l == "alpha") && ids.lines().any(|l| l == "beta"),
        "task ids: {ids}"
    );
    // `ta show be<TAB>` -> prefix-filtered to `beta`.
    let pre = complete(&dir, 2, &["ta", "show", "be"]);
    assert!(
        pre.lines().any(|l| l == "beta") && !pre.lines().any(|l| l == "alpha"),
        "prefix filter: {pre}"
    );

    // `ta list <TAB>` -> filter field names (renamed status, blocker, computed).
    let fields = complete(&dir, 2, &["ta", "list", ""]);
    for needle in [STATUS_FIELD, BLOCKER, "unblocks"] {
        assert!(
            fields.lines().any(|l| l == needle),
            "field `{needle}` in: {fields}"
        );
    }

    // `ta list state=<TAB>` -> the values that field holds, op preserved.
    let vals = complete(&dir, 2, &["ta", "list", &format!("{STATUS_FIELD}=")]);
    for needle in [
        format!("{STATUS_FIELD}=open"),
        format!("{STATUS_FIELD}=closed"),
    ] {
        assert!(
            vals.lines().any(|l| l == needle),
            "value `{needle}` in: {vals}"
        );
    }

    // `ta list --columns <TAB>` -> column names.
    let cols = complete(&dir, 3, &["ta", "list", "--columns", ""]);
    assert!(
        cols.lines().any(|l| l == STATUS_FIELD) && cols.lines().any(|l| l == "unblocks"),
        "columns: {cols}"
    );
}

#[test]
fn install_writes_the_shim_to_the_user_completion_dir() {
    let dir = fresh_dir("completions-install");
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();

    let out = Command::new(ta_bin())
        .args(["completions", "bash", "--install", "user"])
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = home.join(".local/share/bash-completion/completions/ta");
    let content = fs::read_to_string(&file).expect("shim written to the bash user dir");
    assert!(
        content.contains("COMPLETE="),
        "the installed file is the dynamic shim"
    );

    // A shell without a standard auto-load dir is rejected.
    let bad = Command::new(ta_bin())
        .args(["completions", "powershell", "--install", "user"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!bad.status.success(), "unsupported shell rejected");
}

#[test]
fn dash_c_completion_targets_the_flagged_store() {
    // Two stores: one in the cwd we complete from, one we point at with -C.
    let cwd_store = fresh_dir("complete-c-cwd");
    init_renamed_open(&cwd_store);
    ta(
        &cwd_store,
        &["create", "in-cwd-store", &format!("{STATUS_FIELD}=open")],
    );

    let other_store = fresh_dir("complete-c-other");
    init_renamed_open(&other_store);
    ta(
        &other_store,
        &["create", "in-other-store", &format!("{STATUS_FIELD}=open")],
    );

    // `ta -C <other> show <TAB>`, completed from cwd_store's dir, must offer the
    // -C store's ids - not the cwd's. words: ta(0) -C(1) <path>(2) show(3) ""(4).
    let other = other_store.to_str().unwrap();
    let cands = complete(&cwd_store, 4, &["ta", "-C", other, "show", ""]);
    assert!(
        cands.lines().any(|l| l == "in-other-store"),
        "completes the -C store's ids: {cands}"
    );
    assert!(
        !cands.lines().any(|l| l == "in-cwd-store"),
        "not the cwd store's ids: {cands}"
    );
}
