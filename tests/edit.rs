//! End-to-end tests for `ta edit` - the `$EDITOR` round-trip and its re-edit
//! loop. A throwaway shell script stands in for the editor: it receives the temp
//! file as `$1` and rewrites it, exactly as a real editor's save would.

mod common;
use common::*;

use std::os::unix::fs::PermissionsExt;

/// Write an executable fake-editor script and return its path.
fn editor_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// Run `ta <args>` with a fake `$EDITOR`, a canned stdin (for the re-edit
/// prompt), and any extra env the editor script needs.
fn run_edit(dir: &Path, args: &[&str], editor: &Path, stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(ta_bin());
    cmd.args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        .env("EDITOR", editor)
        .env_remove("VISUAL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn setup(name: &str) -> PathBuf {
    let dir = fresh_dir(name);
    init_repo(&dir);
    ta(&dir, &["init"]);
    dir
}

fn show_json(dir: &Path, id: &str) -> String {
    ta(dir, &["show", id, "--format", "json"])
}

#[test]
fn edit_changes_a_field() {
    let dir = setup("edit_changes_a_field");
    ta(&dir, &["create", "t1", "title=A", "status=todo"]);
    // Portable in-place edit (no `sed -i`, which differs GNU vs BSD).
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nout=$(sed 's/\"todo\"/\"in_progress\"/' \"$1\")\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );
    let out = run_edit(&dir, &["edit", "t1"], &ed, "", &[]);
    assert!(
        out.status.success(),
        "edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Updated task `t1`"));
    assert!(
        show_json(&dir, "t1").contains("\"in_progress\""),
        "status not applied"
    );
}

#[test]
fn edit_unsets_a_deleted_field() {
    let dir = setup("edit_unsets_a_deleted_field");
    ta(&dir, &["create", "t2", "title=A", "priority=2"]);
    // Drop the `priority` line - a removed field unsets via the null convention.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nout=$(grep -v '^priority' \"$1\")\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );
    let out = run_edit(&dir, &["edit", "t2"], &ed, "", &[]);
    assert!(out.status.success());
    let json = show_json(&dir, "t2");
    assert!(
        !json.contains("priority"),
        "deleted field should be unset: {json}"
    );
    assert!(json.contains("\"title\""), "other fields survive");
}

#[test]
fn edit_json_format() {
    let dir = setup("edit_json_format");
    ta(&dir, &["create", "t3", "title=A", "status=todo"]);
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nout=$(sed 's/\"todo\"/\"done\"/' \"$1\")\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );
    let out = run_edit(&dir, &["edit", "t3", "--json"], &ed, "", &[]);
    assert!(
        out.status.success(),
        "json edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(show_json(&dir, "t3").contains("\"done\""));
}

#[test]
fn edit_no_change_is_a_noop() {
    let dir = setup("edit_no_change_is_a_noop");
    ta(&dir, &["create", "t4", "title=A"]);
    // An editor that saves the file untouched.
    let ed = editor_script(&dir, "ed.sh", "#!/bin/sh\n:\n");
    let out = run_edit(&dir, &["edit", "t4"], &ed, "", &[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No changes"));
}

#[test]
fn edit_reedits_after_syntax_error() {
    let dir = setup("edit_reedits_after_syntax_error");
    ta(&dir, &["create", "t5", "title=A", "status=todo"]);
    let counter = dir.join("count");
    // First save is broken TOML; after the user answers `y`, the second save is
    // valid - exercising the re-edit loop on the SAME file.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nn=$(cat \"$COUNTER\" 2>/dev/null || echo 0)\nn=$((n+1))\necho \"$n\" > \"$COUNTER\"\n\
         if [ \"$n\" -eq 1 ]; then\n  printf 'not = = valid toml\\n' > \"$1\"\nelse\n  \
         printf 'status = \"in_progress\"\\ntitle = \"A\"\\n' > \"$1\"\nfi\n",
    );
    let out = run_edit(
        &dir,
        &["edit", "t5"],
        &ed,
        "y\n",
        &[("COUNTER", counter.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "re-edit should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid TOML"),
        "diagnostic on stderr"
    );
    assert!(
        show_json(&dir, "t5").contains("\"in_progress\""),
        "second, valid edit applied"
    );
}

#[test]
fn edit_reedits_after_schema_violation() {
    let dir = setup("edit_reedits_after_schema_violation");
    // Declare a schema so the write gate rejects an out-of-enum status - a
    // different error source than syntax, but the same re-edit loop.
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str(
        "\n[task_types.task]\nclosed = false\nfields = { title = { type = \"string\", \
         required = true }, status = { type = \"enum\", values = [\"todo\", \"done\", \
         \"closed\"], required = true } }\n",
    );
    fs::write(&cfg, text).unwrap();
    ta(
        &dir,
        &["create", "t7", "type=task", "title=A", "status=todo"],
    );

    let counter = dir.join("count");
    // First save violates the enum; the second fixes it.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nn=$(cat \"$COUNTER\" 2>/dev/null || echo 0)\nn=$((n+1))\necho \"$n\" > \"$COUNTER\"\n\
         if [ \"$n\" -eq 1 ]; then\n  out=$(sed 's/\"todo\"/\"bogus\"/' \"$1\")\nelse\n  \
         out=$(sed 's/\"bogus\"/\"done\"/' \"$1\")\nfi\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );
    let out = run_edit(
        &dir,
        &["edit", "t7"],
        &ed,
        "y\n",
        &[("COUNTER", counter.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "re-edit should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("status"),
        "schema diagnostic names the field: {stderr}"
    );
    assert!(
        show_json(&dir, "t7").contains("\"done\""),
        "fixed value applied"
    );
}

#[test]
fn edit_discard_leaves_task_unchanged() {
    let dir = setup("edit_discard_leaves_task_unchanged");
    ta(&dir, &["create", "t6", "title=A", "status=todo"]);
    // Always-broken save; answering `n` discards.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf 'broke = = =\\n' > \"$1\"\n",
    );
    let out = run_edit(&dir, &["edit", "t6"], &ed, "n\n", &[]);
    assert!(out.status.success(), "a clean discard is not an error");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Discarded"));
    assert!(
        show_json(&dir, "t6").contains("\"todo\""),
        "task must be unchanged"
    );
}
