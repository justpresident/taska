//! End-to-end tests for `ta edit` - the `$EDITOR` round-trip and its re-edit
//! loop. A throwaway shell script stands in for the editor: it receives the temp
//! file as `$1` and rewrites it, exactly as a real editor's save would.

mod common;
use common::names::*;
use common::*;
use taska::model::STATUS_KEY;

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
fn edit_create_creates_a_missing_task() {
    let dir = setup("edit_create_creates_a_missing_task");
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf 'title = \"Created in editor\"\\npriority = 2\\n' > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "new", "--create"], &ed, "", &[]);
    assert!(
        out.status.success(),
        "edit --create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Created task `new`"));
    let json = show_json(&dir, "new");
    assert!(json.contains("\"title\":\"Created in editor\""));
    assert!(json.contains("\"priority\":2"));
}

#[test]
fn edit_create_empty_save_does_not_create() {
    let dir = setup("edit_create_empty_save_does_not_create");
    // TRUNCATE the file - an editor that saves nothing at all.
    let ed = editor_script(&dir, "ed.sh", "#!/bin/sh\n: > \"$1\"\n");

    let out = run_edit(&dir, &["edit", "new", "--create"], &ed, "", &[]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Empty file - discarded"),
        "an emptied file is the documented discard: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let show = run(ta_bin(), &dir, &["show", "new"]);
    assert!(
        !show.status.success(),
        "empty save must not create the task"
    );
}

#[test]
fn edit_create_comments_only_save_does_not_create() {
    let dir = setup("edit_create_comments_only_save_does_not_create");
    // A file with bytes but no FIELDS is just as empty - it must not create a
    // task with zero fields.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf '# everything commented out\\n' > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "ghost", "--create"], &ed, "", &[]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Empty file - discarded"),
        "a fieldless save is a discard: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let show = run(ta_bin(), &dir, &["show", "ghost"]);
    assert!(
        !show.status.success(),
        "no task may be created: {}",
        String::from_utf8_lossy(&show.stdout)
    );
}

#[test]
fn edit_create_keeps_the_default_status_when_the_line_is_gone() {
    let dir = setup("edit_create_keeps_the_default_status");
    // The editor replaces the whole template (a select-all retype, or simply
    // deleting the prefilled `status` line). The workflow default must still be
    // stamped - a statusless task is invisible to every status filter.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf 'title = \"Created in editor\"\\n' > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "fresh", "--create"], &ed, "", &[]);
    assert!(
        out.status.success(),
        "edit --create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = show_json(&dir, "fresh");
    assert!(
        json.contains("\"status\":\"todo\""),
        "default status must survive an omitted line: {json}"
    );
    assert!(
        ta(&dir, &["list", "--open"]).contains("fresh"),
        "the task must be reachable by a status filter"
    );
}

#[test]
fn edit_blanking_a_field_unsets_it() {
    let dir = setup("edit_blanking_a_field_unsets_it");
    ta(&dir, &["create", "b1", "title=A", "owner=bob"]);
    // `""` is the repository's unset value everywhere else, so it must work in
    // the editor too - the way to suppress a value without deleting the line.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nout=$(sed 's/^owner = .*/owner = \"\"/' \"$1\")\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "b1"], &ed, "", &[]);
    assert!(
        out.status.success(),
        "edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = show_json(&dir, "b1");
    assert!(
        !json.contains("bob"),
        "blanked field should be unset: {json}"
    );
}

#[test]
fn edit_deleting_a_template_only_line_writes_nothing() {
    let dir = setup("edit_deleting_a_template_only_line_writes_nothing");
    ta(&dir, &["create", "seed", "title=Seed", "owner=bob"]);
    ta(&dir, &["create", "t", "title=T"]);
    let before = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    // `owner` is in the template (another task uses it) but this task stores no
    // value for it, so deleting the line has nothing to unset.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nout=$(grep -v '^owner' \"$1\")\nprintf '%s\\n' \"$out\" > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "t"], &ed, "", &[]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No changes"),
        "nothing to write must report no change, not a seq: {stdout}"
    );
    assert!(
        !stdout.contains("[seq:0]"),
        "a write that never happened has no seq: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap(),
        before,
        "the log must be untouched"
    );
}

#[test]
fn edit_eof_at_the_reedit_prompt_discards() {
    let dir = setup("edit_eof_at_the_reedit_prompt_discards");
    ta(&dir, &["create", "t", "title=A"]);
    // An always-broken editor with NO stdin: EOF at the re-edit prompt must
    // discard, not re-launch the editor forever.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf 'broke = = =\\n' > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "t"], &ed, "", &[]);
    assert!(out.status.success(), "a clean discard is not an error");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Discarded"),
        "EOF declines the re-edit: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        show_json(&dir, "t").contains("\"title\":\"A\""),
        "task unchanged"
    );
}

#[test]
fn edit_create_prefills_the_schema_template() {
    let dir = fresh_dir("edit_create_prefills_the_schema_template");
    init_renamed(&dir);
    let capture = dir.join("opened.toml");
    let ed = editor_script(&dir, "ed.sh", "#!/bin/sh\ncp \"$1\" \"$CAPTURE\"\n");

    let out = run_edit(
        &dir,
        &["edit", "new", "--create"],
        &ed,
        "",
        &[("CAPTURE", capture.to_str().unwrap())],
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("not created"));

    let opened = fs::read_to_string(capture).unwrap();
    for expected in [
        "body = \"\"",
        "headline = \"\"",
        "kind = \"story\"",
        "rank = \"\"",
        "state = \"backlog\"",
    ] {
        assert!(
            opened.contains(expected),
            "missing `{expected}` in editor template:\n{opened}"
        );
    }
    // Assert on the parsed KEYS: a `starts_with` line scan both over-matches a
    // longer field name and misses a key emitted anywhere but at line start.
    let keys: toml::Table = toml::from_str(&opened).unwrap();
    for computed in ["id", "deps", "made_at", "touched_at", "shipped_at", "needs"] {
        assert!(
            !keys.contains_key(computed),
            "computed/relationship field `{computed}` must not be editable:\n{opened}"
        );
    }
}

#[test]
fn edit_prefills_fields_known_from_other_tasks() {
    let dir = setup("edit_prefills_fields_known_from_other_tasks");
    ta(&dir, &["create", "seed", "title=Seed", "owner=bob"]);
    ta(&dir, &["create", "target", "title=Target"]);
    let capture = dir.join("opened.toml");
    let ed = editor_script(&dir, "ed.sh", "#!/bin/sh\ncp \"$1\" \"$CAPTURE\"\n");

    let out = run_edit(
        &dir,
        &["edit", "target"],
        &ed,
        "",
        &[("CAPTURE", capture.to_str().unwrap())],
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No changes"));

    let opened = fs::read_to_string(capture).unwrap();
    assert!(opened.contains("title = \"Target\""));
    assert!(opened.contains("status = \"todo\""));
    assert!(opened.contains("owner = \"\""));
    assert!(opened.contains("type = \"\""));
}

#[test]
fn edit_missing_without_create_still_errors() {
    let dir = setup("edit_missing_without_create_still_errors");
    let out = run(ta_bin(), &dir, &["edit", "missing"]);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no task `missing`"));
}

#[test]
fn edit_create_rejects_an_existing_task() {
    let dir = setup("edit_create_rejects_an_existing_task");
    ta(&dir, &["create", "existing", "title=Original"]);
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf 'title = \"Changed\"\\n' > \"$1\"\n",
    );

    let out = run_edit(&dir, &["edit", "existing", "--create"], &ed, "", &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task `existing` already exists"));
    assert!(show_json(&dir, "existing").contains("\"title\":\"Original\""));
}

#[test]
fn edit_changes_a_field() {
    let dir = fresh_dir("edit_changes_a_field");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "t1", "title=A", &format!("{STATUS_FIELD}=todo")],
    );
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
        "state not applied"
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
    let dir = fresh_dir("edit_json_format");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "t3", "title=A", &format!("{STATUS_FIELD}=todo")],
    );
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
    let dir = fresh_dir("edit_reedits_after_syntax_error");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "t5", "title=A", &format!("{STATUS_FIELD}=todo")],
    );
    let counter = dir.join("count");
    // First save is broken TOML; an empty RESPONSE (a bare newline - EOF instead
    // would decline) accepts the default `yes`, then the second save is valid -
    // exercising the re-edit loop on the SAME file.
    let ed = editor_script(
        &dir,
        "ed.sh",
        &format!("#!/bin/sh\nn=$(cat \"$COUNTER\" 2>/dev/null || echo 0)\nn=$((n+1))\necho \"$n\" > \"$COUNTER\"\n\
         if [ \"$n\" -eq 1 ]; then\n  printf 'not = = valid toml\\n' > \"$1\"\nelse\n  \
         printf '{STATUS_FIELD} = \"in_progress\"\\ntitle = \"A\"\\n' > \"$1\"\nfi\n"),
    );
    let out = run_edit(
        &dir,
        &["edit", "t5"],
        &ed,
        "\n",
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
        String::from_utf8_lossy(&out.stderr).contains("[Y/n]"),
        "re-edit prompt defaults to yes"
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
        &[
            "create",
            "t7",
            "type=task",
            "title=A",
            &format!("{STATUS_KEY}=todo"),
        ],
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
        stderr.contains(STATUS_KEY),
        "schema diagnostic names the field: {stderr}"
    );
    assert!(
        show_json(&dir, "t7").contains("\"done\""),
        "fixed value applied"
    );
}

#[test]
fn edit_discard_leaves_task_unchanged() {
    let dir = fresh_dir("edit_discard_leaves_task_unchanged");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "t6", "title=A", &format!("{STATUS_FIELD}=todo")],
    );
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

#[test]
fn edit_prompts_to_add_a_new_field_name() {
    let dir = setup("edit_new_field");
    // `a` is the first task (empty-store grace), so it seeds freely.
    ta(&dir, &["create", "a", "title=Alpha"]);

    // The editor adds a brand-new field name; on the now non-empty store that
    // trips the interactive new-field prompt. Answer `y` to add it.
    let ed = editor_script(
        &dir,
        "ed.sh",
        "#!/bin/sh\nprintf '\\nowner = \"bob\"\\n' >> \"$1\"\n",
    );
    let out = run_edit(&dir, &["edit", "a"], &ed, "y\n", &[]);
    assert!(
        out.status.success(),
        "edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no task uses yet") && stderr.contains("owner"),
        "new-field prompt shown: {stderr}"
    );
    assert!(
        show_json(&dir, "a").contains("\"owner\":\"bob\""),
        "field added after confirming"
    );
}
