//! End-to-end tests driving the real `ta` binary against throwaway git repos.
//!
//! Cargo builds the binary and hands us its path via `CARGO_BIN_EXE_ta`. Each
//! test gets its own throwaway subdirectory under the *system* temp dir —
//! deliberately outside the project tree, so `ta`'s walk-up store discovery
//! can't climb into the repo's own `.taska` (e.g. a dogfooding store) and
//! read/write that instead of the test's isolated one. The git merge-driver
//! test needs git to find `ta` on `PATH`, so every spawned command runs with the
//! binary's directory prepended to `PATH`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ta_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ta")
}

fn bin_dir() -> PathBuf {
    Path::new(ta_bin()).parent().unwrap().to_path_buf()
}

/// `PATH` with the test binary's directory prepended, so git's merge driver
/// (`ta git-merge ...`) resolves to the binary under test.
fn path_with_bin() -> OsString {
    let mut dirs = vec![bin_dir()];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap()
}

/// A fresh, empty scratch directory unique to `name`, under the system temp dir
/// (NOT `CARGO_TARGET_TMPDIR`, which lives inside the project tree where `ta`'s
/// store discovery would find the repo's own `.taska` in an ancestor).
fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("taska-e2e").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{} {}`: {}", program, args.join(" "), e))
}

/// Run `ta`, asserting success and returning stdout.
fn ta(dir: &Path, args: &[&str]) -> String {
    let out = run(ta_bin(), dir, args);
    assert!(
        out.status.success(),
        "`ta {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `git`, asserting success and returning stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("git", dir, args);
    assert!(
        out.status.success(),
        "`git {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Whether `id` appears as a listed task. The human table puts the id in the
/// first column, so we check each line's first whitespace-delimited token —
/// robust to alignment padding and to a later `deps` column naming the id.
fn lists_task(output: &str, id: &str) -> bool {
    output
        .lines()
        .any(|l| l.split_whitespace().next() == Some(id))
}

/// Initialize a git repo with a deterministic default branch and an identity.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@taska.dev"]);
    git(dir, &["config", "user.name", "Taska Test"]);
}

#[test]
fn init_creates_config_and_registers_merge_driver() {
    let dir = fresh_dir("init");
    init_repo(&dir);

    let out = ta(&dir, &["init"]);
    assert!(out.contains("Initialized taska store"), "got: {out}");

    let cfg = fs::read_to_string(dir.join(".taska/config.toml")).unwrap();
    assert!(
        cfg.contains("[compaction]") && cfg.contains("[workflow]"),
        "config: {cfg}"
    );
    assert!(cfg.contains("keep_events = 1000"), "config: {cfg}");
    assert!(cfg.contains("status_field = \"status\""), "config: {cfg}");

    let attrs = fs::read_to_string(dir.join(".gitattributes")).unwrap();
    assert!(
        attrs.contains("mutations.jsonl merge=taska-merge-driver"),
        "attrs: {attrs}"
    );

    let driver = git(
        &dir,
        &["config", "--get", "merge.taska-merge-driver.driver"],
    );
    assert!(driver.contains("ta git-merge"), "driver: {driver}");
}

#[test]
fn crud_search_and_ready_workflow() {
    let dir = fresh_dir("crud");
    init_repo(&dir);
    ta(&dir, &["init"]);

    ta(&dir, &["create", "db", "status=closed"]);
    ta(&dir, &["create", "api", "status=open", "priority=3"]);
    ta(&dir, &["block", "api", "db"]);

    // The human table lists ids; `--full --format json` exposes every field —
    // priority coerced to a JSON number, and deps as a JSON array.
    assert!(
        lists_task(&ta(&dir, &["list"]), "api"),
        "api should be listed"
    );
    let json = ta(&dir, &["list", "--full", "--format", "json"]);
    assert!(json.contains(r#""priority":3"#), "json: {json}");
    assert!(json.contains(r#""deps":["db"]"#), "json: {json}");

    let search = ta(&dir, &["search", "status=open"]);
    assert!(lists_task(&search, "api"), "search: {search}");
    assert!(!lists_task(&search, "db"), "db is done, not open: {search}");

    // db is done, so api's only dependency is satisfied -> api is ready.
    let ready = ta(&dir, &["ready"]);
    assert!(lists_task(&ready, "api"), "ready: {ready}");

    // Once api is done too, nothing is ready.
    ta(&dir, &["update", "api", "status=closed"]);
    assert_eq!(ta(&dir, &["ready"]).trim(), "(nothing ready)");

    ta(&dir, &["delete", "db"]);
    assert!(!lists_task(&ta(&dir, &["list"]), "db"), "db should be gone");
}

#[test]
fn update_with_no_fields_fails_and_appends_nothing() {
    let dir = fresh_dir("empty-update");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "api", "status=open"]);

    let log = dir.join(".taska").join("mutations.jsonl");
    let before = rows(&log);

    // `ta update api` with no field=value args must fail (non-zero exit) and
    // must NOT append a no-op empty Update event.
    let out = run(ta_bin(), &dir, &["update", "api"]);
    assert!(
        !out.status.success(),
        "`ta update api` with no fields should exit non-zero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(rows(&log), before, "no event should have been appended");
}

#[test]
fn output_format_columns_and_json() {
    let dir = fresh_dir("output");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(
        &dir,
        &["create", "a", "title=Alpha", "status=open", "priority=3"],
    );

    // Human: uppercase header + the title column value.
    let human = ta(&dir, &["list"]);
    assert!(
        human.contains("ID") && human.contains("STATUS"),
        "header: {human}"
    );
    assert!(human.contains("Alpha"), "title column: {human}");

    // Default json: the default columns (id,title,status,deps) as an array,
    // but NOT priority (not a default column).
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(json.trim_start().starts_with('['), "json array: {json}");
    assert!(json.contains(r#""status":"open""#), "status shown: {json}");
    assert!(
        !json.contains("priority"),
        "priority not a default column: {json}"
    );

    // --full exposes every field; --columns selects an explicit set.
    assert!(
        ta(&dir, &["list", "--full", "--format", "json"]).contains(r#""priority":3"#),
        "--full shows priority"
    );
    let cols = ta(
        &dir,
        &["list", "--columns", "id,priority", "--format", "json"],
    );
    assert!(
        cols.contains(r#""priority":3"#) && !cols.contains("status"),
        "columns select + restrict: {cols}"
    );
}

#[test]
fn show_displays_full_task_and_rejects_unknown_id() {
    let dir = fresh_dir("show");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(
        &dir,
        &["create", "a", "title=Alpha", "status=open", "priority=3"],
    );
    ta(&dir, &["create", "dep"]);
    ta(&dir, &["block", "a", "dep"]);

    // `show` defaults to the FULL task: every field, even ones that are not
    // default `list` columns (e.g. priority), plus deps.
    let human = ta(&dir, &["show", "a"]);
    assert!(
        lists_task(&human, "a"),
        "show should list the task: {human}"
    );
    assert!(human.contains("Alpha"), "title field: {human}");
    assert!(human.contains("PRIORITY"), "priority header shown: {human}");
    assert!(human.contains("dep"), "deps shown: {human}");

    // json emits the same fields (a one-element array is fine, as for list).
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(json.trim_start().starts_with('['), "json array: {json}");
    assert!(
        json.contains(r#""priority":3"#),
        "priority in show json: {json}"
    );
    assert!(
        json.contains(r#""status":"open""#),
        "status in show json: {json}"
    );

    // An explicit --columns still restricts.
    let cols = ta(
        &dir,
        &["show", "a", "--columns", "id,status", "--format", "json"],
    );
    assert!(
        cols.contains(r#""status":"open""#) && !cols.contains("priority"),
        "explicit columns restrict show: {cols}"
    );

    // An unknown id exits non-zero.
    let out = run(ta_bin(), &dir, &["show", "missing"]);
    assert!(
        !out.status.success(),
        "show of an unknown id must exit non-zero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn full_flag_disables_truncation_in_human_output() {
    let dir = fresh_dir("full-no-truncate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A title well past the default title cap (80), so it would otherwise be cut.
    let long = "This title is considerably longer than eighty characters in total, well past the per-column title override, so it still gets truncated by default";
    ta(&dir, &["create", "a", &format!("title={long}")]);

    // Default human view truncates with an ellipsis and drops the tail.
    let default = ta(&dir, &["list"]);
    assert!(default.contains('…'), "default truncates: {default}");
    assert!(!default.contains(long), "default drops the tail: {default}");

    // --full prints the whole value, no ellipsis.
    let full = ta(&dir, &["list", "--full"]);
    assert!(full.contains(long), "--full prints untruncated: {full}");
    assert!(!full.contains('…'), "--full adds no ellipsis: {full}");
}

#[test]
fn full_view_uses_canonical_column_order_in_both_formats() {
    let dir = fresh_dir("canonical-order");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Configure a deliberate column order; deps in the middle. Disable the
    // computed timestamp columns so this test sees only the fields under test.
    fs::write(
        dir.join(".taska/config.toml"),
        "[display]\ncolumns = [\"id\", \"status\", \"deps\"]\nmax_width = 0\n\
         [timestamps]\ncreate_time = \"\"\nupdate_time = \"\"\nclose_time = \"\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "dep"]);
    // Custom fields supplied out of alphabetical order.
    ta(&dir, &["create", "a", "zeta=1", "status=open", "alpha=2"]);
    ta(&dir, &["block", "a", "dep"]);

    // Human --full: configured columns first (id,status,deps), then the extra
    // custom fields alphabetically (alpha, zeta).
    let human = ta(&dir, &["list", "--full"]);
    let header: Vec<&str> = human.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        ["ID", "STATUS", "DEPS", "ALPHA", "ZETA"],
        "human: {human}"
    );

    // JSON --full: the keys appear in that identical order, for one object.
    let json = ta(&dir, &["list", "--full", "--format", "json"]);
    let a_obj = json.lines().find(|l| l.contains("\"a\"")).unwrap();
    let order: Vec<usize> = ["id", "status", "deps", "alpha", "zeta"]
        .iter()
        .map(|k| a_obj.find(&format!("\"{k}\"")).unwrap())
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "json key order: {a_obj}"
    );

    // `show` shares the same default order.
    let show = ta(&dir, &["show", "a", "--format", "json"]);
    let sorder: Vec<usize> = ["id", "status", "deps", "alpha", "zeta"]
        .iter()
        .map(|k| show.find(&format!("\"{k}\"")).unwrap())
        .collect();
    assert!(sorder.windows(2).all(|w| w[0] < w[1]), "show order: {show}");
}

#[test]
fn per_column_max_width_overrides_the_global_default() {
    let dir = fresh_dir("per-column-width");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Global cap of 10, but `title` overridden to 80.
    fs::write(
        dir.join(".taska/config.toml"),
        "[display]\ncolumns = [\"id\", \"title\", \"notes\"]\nmax_width = 10\n\
         [display.column_max_width]\ntitle = 80\n",
    )
    .unwrap();

    let long_title = "A title that is far longer than ten characters but under eighty";
    let long_notes = "Notes that also exceed ten characters and should be cut";
    ta(
        &dir,
        &[
            "create",
            "a",
            &format!("title={long_title}"),
            &format!("notes={long_notes}"),
        ],
    );

    let human = ta(&dir, &["list"]);
    // title uses its own 80-wide cap, so the whole value survives...
    assert!(
        human.contains(long_title),
        "title kept under its override: {human}"
    );
    // ...while notes falls back to the global 10 and is truncated.
    assert!(!human.contains(long_notes), "notes truncated: {human}");
    assert!(
        human.contains('…'),
        "ellipsis from the notes column: {human}"
    );

    // --full still ignores the per-column map and prints everything.
    let full = ta(&dir, &["list", "--full"]);
    assert!(
        full.contains(long_notes),
        "--full prints notes whole: {full}"
    );
    assert!(!full.contains('…'), "--full adds no ellipsis: {full}");
}

fn rows(path: &Path) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn auto_timestamps_lifecycle_search_and_compaction() {
    let dir = fresh_dir("timestamps");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "api", "status=open", "title=API"]);

    // create_time + update_time are materialized; close_time only once done.
    let open = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(
        open.contains("create_time") && open.contains("update_time"),
        "{open}"
    );
    assert!(
        !open.contains("close_time"),
        "open task has no close_time: {open}"
    );

    ta(&dir, &["update", "api", "status=closed"]);
    assert!(
        ta(&dir, &["show", "api", "--format", "json"]).contains("close_time"),
        "closing sets close_time"
    );

    // Reopening clears close_time (the user's 'cleared on reopen' choice).
    ta(&dir, &["update", "api", "status=open"]);
    assert!(
        !ta(&dir, &["show", "api", "--format", "json"]).contains("close_time"),
        "reopen clears close_time"
    );

    // The times are ordinary string fields: searchable and selectable as columns.
    assert!(
        lists_task(&ta(&dir, &["search", "create_time~^20"]), "api"),
        "searchable by create_time"
    );

    // Renaming/disabling via config: blank name hides the column entirely.
    fs::write(
        dir.join(".taska/config.toml"),
        "[timestamps]\ncreate_time = \"made_at\"\nupdate_time = \"\"\n",
    )
    .unwrap();
    let renamed = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(
        renamed.contains("made_at"),
        "create_time renamed: {renamed}"
    );
    assert!(
        !renamed.contains("update_time"),
        "update_time disabled: {renamed}"
    );

    // create_time survives compaction (it is folded into the baseline). Generate
    // enough events to fold the original Create past the keep_events floor.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 100\nkeep_days = 0\n",
    )
    .unwrap();
    for i in 0..120 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    ta(&dir, &["compact"]);
    // api's Create is now folded away, yet its create_time persists via baseline.
    assert!(
        ta(&dir, &["show", "api", "--format", "json"]).contains("create_time"),
        "create_time survives compaction"
    );
}

#[test]
fn jsonl_output_across_commands_omits_absent_fields() {
    let dir = fresh_dir("jsonl");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "api", "status=open", "priority=3"]);
    ta(&dir, &["create", "db", "status=closed"]);
    ta(&dir, &["block", "api", "db"]);

    // list/search/ready/show all speak jsonl: one bare object per line, no array
    // wrapper, and never a null for an absent field.
    for args in [
        vec!["list", "--full", "--format", "jsonl"],
        vec!["search", "status=open", "--format", "jsonl"],
        vec!["ready", "--format", "jsonl"],
        vec!["show", "api", "--format", "jsonl"],
    ] {
        let out = ta(&dir, &args);
        // No top-level array wrapper (a `deps` value may still be an array).
        assert!(
            !out.trim_start().starts_with('['),
            "no array wrapper in `{}`: {out}",
            args.join(" ")
        );
        assert!(
            !out.contains("null"),
            "no null in `{}`: {out}",
            args.join(" ")
        );
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                line.starts_with('{') && line.ends_with('}'),
                "bare object: {line}"
            );
        }
    }

    // The full row for api carries its present fields and omits the title it lacks.
    let full = ta(&dir, &["list", "--full", "--format", "jsonl"]);
    let api = full.lines().find(|l| l.contains("\"api\"")).unwrap();
    assert!(
        api.contains(r#""priority":3"#) && !api.contains("title"),
        "api: {api}"
    );

    // status --format jsonl is the single summary object.
    let status = ta(&dir, &["status", "--format", "jsonl"]);
    assert_eq!(
        status.lines().count(),
        1,
        "status jsonl is one line: {status}"
    );
    assert!(status.contains(r#""total":2"#), "status: {status}");
}

#[test]
fn search_supports_regex_negation_and_combined_criteria() {
    let dir = fresh_dir("search-improve");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(
        &dir,
        &[
            "create",
            "api",
            "status=open",
            "priority=3",
            "title=API work",
        ],
    );
    ta(
        &dir,
        &[
            "create",
            "db",
            "status=closed",
            "priority=1",
            "title=DB migration",
        ],
    );
    ta(
        &dir,
        &["create", "web", "status=open", "priority=2", "title=Web UI"],
    );
    ta(&dir, &["block", "web", "api"]);

    // Multiple criteria are AND-combined.
    let both = ta(&dir, &["search", "status=open", "priority=3"]);
    assert!(
        lists_task(&both, "api") && !lists_task(&both, "web"),
        "AND: {both}"
    );

    // `~` is a regex over the field's string form; numbers match too.
    let re = ta(&dir, &["search", r"priority~^[12]$"]);
    assert!(
        lists_task(&re, "db") && lists_task(&re, "web") && !lists_task(&re, "api"),
        "regex on numeric field: {re}"
    );

    // Negation, and querying built-in id / deps fields.
    let ne = ta(&dir, &["search", "status!=open"]);
    assert!(
        lists_task(&ne, "db") && !lists_task(&ne, "api"),
        "negation: {ne}"
    );
    assert!(
        lists_task(&ta(&dir, &["search", "deps=api"]), "web"),
        "deps query"
    );
    assert!(
        lists_task(&ta(&dir, &["search", "id~^a"]), "api"),
        "id regex"
    );

    // A malformed criterion or bad regex is rejected (non-zero exit).
    assert!(!run(ta_bin(), &dir, &["search", "nooperator"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["search", "title~["]).status.success());
}

#[test]
fn config_get_set_list_validates_and_preserves_comments() {
    let dir = fresh_dir("config-cmd");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let cfg_path = dir.join(".taska/config.toml");

    // get reads effective values, including defaults and nested sub-tables.
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "1000"
    );
    assert_eq!(
        ta(&dir, &["config", "get", "workflow.status_field"]).trim(),
        "status"
    );

    // set coerces by TOML grammar and persists the change.
    ta(&dir, &["config", "set", "compaction.keep_events", "500"]);
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "500"
    );
    ta(&dir, &["config", "set", "merge.on_conflict", "ours"]);
    assert_eq!(
        ta(&dir, &["config", "get", "merge.on_conflict"]).trim(),
        "ours"
    );

    // The documented comments survive a rewrite (toml_edit, not re-serialization).
    let text = fs::read_to_string(&cfg_path).unwrap();
    assert!(
        text.contains("Keep at least this many"),
        "comment kept: {text}"
    );

    // Invalid edits are rejected and leave the file untouched.
    for bad in [
        ["config", "set", "compaction.keep_events", "50"], // below the floor
        ["config", "set", "merge.on_conflict", "bogus"],   // unknown enum variant
        ["config", "set", "workflow.bogus_key", "x"],      // unknown key (typo guard)
    ] {
        let out = run(ta_bin(), &dir, &bad);
        assert!(!out.status.success(), "`ta {}` should fail", bad.join(" "));
    }
    // keep_events is still 500 — no rejected edit slipped through.
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "500"
    );

    // list prints every effective value as sorted dotted keys.
    let list = ta(&dir, &["config", "list"]);
    assert!(
        list.contains("compaction.keep_events = 500"),
        "list: {list}"
    );
    assert!(list.contains("merge.on_conflict = ours"), "list: {list}");
    assert!(
        list.contains("display.column_max_width.title = 80"),
        "nested key flattened: {list}"
    );
}

#[test]
fn status_summarizes_counts_blocked_and_ready() {
    let dir = fresh_dir("status");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // db is done; api depends on the open web, so api is blocked and web is ready.
    ta(&dir, &["create", "db", "status=closed"]);
    ta(&dir, &["create", "web", "status=open"]);
    ta(&dir, &["create", "api", "status=open"]);
    ta(&dir, &["block", "api", "web"]);

    let human = ta(&dir, &["status"]);
    assert!(human.contains("Total"), "total line: {human}");
    assert!(human.contains("By status:"), "status section: {human}");
    assert!(
        human.contains("open") && human.contains("closed"),
        "per-status buckets discovered from data: {human}"
    );
    assert!(
        human.contains("Blocked") && human.contains("Ready"),
        "{human}"
    );

    // JSON form is a single object with the computed fields.
    let json = ta(&dir, &["status", "--format", "json"]);
    assert!(json.contains(r#""total":3"#), "json total: {json}");
    assert!(json.contains(r#""closed":1"#), "one done task: {json}");
    assert!(
        json.contains(r#""blocked":1"#),
        "api blocked by web: {json}"
    );
    assert!(json.contains(r#""ready":1"#), "only web is ready: {json}");
    assert!(
        json.contains(r#""by_status":{"closed":1,"open":2}"#),
        "buckets sorted, counted: {json}"
    );
}

#[test]
fn compact_folds_log_and_appends_resume() {
    let dir = fresh_dir("compact");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A valid retention floor (>= MIN_KEEP_EVENTS = 100). We then generate MORE
    // events than that so compaction actually folds the old prefix and retains
    // exactly the recent suffix — the fold-and-resume path.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 100\nkeep_days = 0\n",
    )
    .unwrap();

    // 150 creates > keep_events (100), so 50 fold into the baseline and 100 stay.
    for i in 0..150 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    ta(&dir, &["compact"]);

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        100,
        "keep_events most-recent events retained in the log"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "the folded remainder (150 - keep_events) is in the baseline"
    );

    // Appends overlay the baseline after compaction, and the older folded tasks
    // are still visible — fold-and-resume keeps everything reachable.
    ta(&dir, &["create", "resumed"]);
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t75", "t149", "resumed"] {
        assert!(lists_task(&list, id), "missing {id} in list:\n{list}");
    }
}

#[test]
fn compact_retains_recent_events_for_merge() {
    let dir = fresh_dir("retain");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Valid retention (>= MIN_KEEP_EVENTS). With more events than keep_events, the
    // recent suffix is retained in the log so divergent branches can still merge.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 120\nkeep_days = 0\n",
    )
    .unwrap();

    // 150 creates > keep_events (120): 30 fold into baseline, 120 newest retained.
    for i in 0..150 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("kept 120 recent event(s)"), "got: {out}");

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        120,
        "kept keep_events recent events for merge reconciliation"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        30,
        "folded the oldest 30 into baseline"
    );

    // The retained log holds the most recent creations (not the oldest, which
    // were folded away). The newest task is in the log; the oldest is not.
    let mutations = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        mutations.contains(r#""task_id":"t149""#),
        "expected the newest event retained: {mutations}"
    );
    assert!(
        !mutations.contains(r#""task_id":"t0""#),
        "the oldest event should have been folded out of the log: {mutations}"
    );

    // All tasks remain visible (baseline + retained log), old and new alike.
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t29", "t30", "t149"] {
        assert!(lists_task(&list, id), "missing {id}:\n{list}");
    }
}

#[test]
fn low_keep_events_is_rejected_on_the_next_command() {
    let dir = fresh_dir("validate");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // A hand-edited, unreasonably small retention value.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 50\n",
    )
    .unwrap();

    // Without the test hatch, even a plain `ta list` must refuse and explain —
    // the error surfaces immediately, not weeks later at compaction time.
    let out = Command::new(ta_bin())
        .args(["list"])
        .current_dir(&dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap();
    assert!(!out.status.success(), "list should fail on invalid config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keep_events") && stderr.contains("100"),
        "stderr: {stderr}"
    );
}

#[test]
fn compact_is_noop_below_threshold() {
    let dir = fresh_dir("compact_noop");
    init_repo(&dir);
    ta(&dir, &["init"]); // default keep_events = 1000

    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("Nothing to compact"), "got: {out}");

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        2,
        "log untouched"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        0,
        "baseline still empty"
    );
}

#[test]
fn git_merge_driver_resolves_divergent_appends() {
    let dir = fresh_dir("merge");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "base", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Branch off, then append a distinct task on each branch.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["create", "on-main"]);
    git(&dir, &["commit", "-aqm", "main task"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "on-feature"]);
    git(&dir, &["commit", "-aqm", "feature task"]);

    // Both branches edited mutations.jsonl; the driver must union them cleanly.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "merge should auto-resolve, got:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list"]);
    for id in ["base", "on-main", "on-feature"] {
        assert!(list.contains(id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn surface_conflict_fails_merge_and_resolve_clears_it() {
    let dir = fresh_dir("conflict");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches set the SAME field of the SAME task to different values.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    // Default policy is `surface`, so the driver must fail the merge.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        !merge.status.success(),
        "surface policy must fail the merge"
    );
    assert!(
        dir.join(".taska/merge-conflict.json").exists(),
        "a conflict marker should be written"
    );

    // `ta resolve` reports the conflict (per-field) and clears the marker.
    let resolved = ta(&dir, &["resolve"]);
    assert!(
        resolved.contains("conflict"),
        "resolve should report the conflict: {resolved}"
    );
    assert!(
        resolved.contains("status"),
        "resolve should name the conflicting field: {resolved}"
    );
    assert!(
        resolved.contains("kept ours"),
        "surface resolves tentatively as ours: {resolved}"
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "marker should be cleared"
    );

    // A second resolve is a clean no-op.
    let again = ta(&dir, &["resolve"]);
    assert!(again.contains("Nothing to resolve"), "got: {again}");
}

#[test]
fn orphaned_events_warn_on_read_and_resolve_drops_them() {
    let dir = fresh_dir("orphan");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Create then delete `a`, then update the (now gone) `a`. Handlers don't check
    // existence, so the update appends an Update event whose target no longer
    // exists at replay time — an orphan that applies to nothing.
    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["delete", "a"]);
    ta(&dir, &["update", "a", "status=x"]);

    let log = dir.join(".taska").join("mutations.jsonl");
    let before = rows(&log);

    // A read command warns about the orphan on STDERR (without failing).
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        out.status.success(),
        "list should still succeed despite orphans"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("orphaned event") && stderr.contains("ta resolve"),
        "list should warn about the orphan: {stderr}"
    );

    // `ta resolve --force` drops the orphan and rewrites the log without it.
    let resolved = ta(&dir, &["resolve", "--force"]);
    assert!(
        resolved.contains("orphaned event"),
        "resolve should report dropping the orphan: {resolved}"
    );
    assert_eq!(
        rows(&log),
        before - 1,
        "exactly the one orphan event was removed from the log"
    );

    // The warning is gone on the next read, and a second resolve is a clean no-op.
    let after = run(ta_bin(), &dir, &["list"]);
    assert!(after.status.success(), "list should still succeed");
    assert!(
        !String::from_utf8_lossy(&after.stderr).contains("orphaned event"),
        "no orphan warning after resolve: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    let again = ta(&dir, &["resolve"]);
    assert!(again.contains("Nothing to resolve"), "got: {again}");
}

#[test]
fn null_value_unsets_a_field() {
    let dir = fresh_dir("null-unset");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "x", "owner=bob", "status=open"]);
    // Setting a field to null removes it (the field-unset convention).
    ta(&dir, &["update", "x", "owner=null"]);
    let json = ta(&dir, &["show", "x", "--format", "json"]);
    assert!(json.contains("\"status\":\"open\""), "status kept: {json}");
    assert!(!json.contains("owner"), "owner unset by null: {json}");
}

#[test]
fn resolve_orphans_requires_confirmation() {
    let dir = fresh_dir("resolve-confirm");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["delete", "a"]);
    ta(&dir, &["update", "a", "status=x"]); // orphan: update to a deleted task
    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);
    // No --force and no stdin (EOF) declines: the orphan is listed but kept.
    let out = ta(&dir, &["resolve"]);
    assert!(out.contains("would be dropped"), "verbose listing: {out}");
    assert_eq!(rows(&log), before, "without --force the log is unchanged");
    // --force drops it.
    ta(&dir, &["resolve", "--force"]);
    assert!(rows(&log) < before, "--force drops the orphan");
}

#[test]
fn theirs_policy_resolves_conflict_without_failing() {
    let dir = fresh_dir("theirs");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Opt into silent resolution: the branch merged IN wins conflicts.
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "theirs policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "auto resolution leaves no marker"
    );

    // Merging feature INTO main with `theirs` keeps feature's value.
    let list = ta(&dir, &["list", "--format", "json"]);
    assert!(
        list.contains("\"status\":\"feature\""),
        "theirs (feature) should win: {list}"
    );
}

#[test]
fn per_field_merge_keeps_disjoint_fields_and_resolves_overlap() {
    let dir = fresh_dir("perfield");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "X", "status=new"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // main and feature overlap on status+owner, but each adds a disjoint field.
    git(&dir, &["branch", "feature"]);
    ta(
        &dir,
        &[
            "update",
            "X",
            "status=closed",
            "owner=alice",
            "scope=project",
        ],
    );
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(
        &dir,
        &["update", "X", "status=open", "owner=bob", "priority=3"],
    );
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "should resolve: {}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list", "--full", "--format", "json"]);
    // Overlapping fields go to theirs (feature); disjoint fields both survive.
    assert!(
        list.contains("\"status\":\"open\""),
        "status -> theirs: {list}"
    );
    assert!(
        list.contains("\"owner\":\"bob\""),
        "owner -> theirs: {list}"
    );
    assert!(
        list.contains("\"scope\":\"project\""),
        "ours-only scope survives: {list}"
    );
    assert!(
        list.contains("\"priority\":3"),
        "theirs-only priority survives: {list}"
    );
}

#[test]
fn workflow_config_override_changes_ready_semantics() {
    let dir = fresh_dir("config_override");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Rename the status convention to state/closed via config.
    fs::write(
        dir.join(".taska/config.toml"),
        "[workflow]\nstatus_field = \"state\"\ndone_status = \"closed\"\n",
    )
    .unwrap();

    ta(&dir, &["create", "db", "state=closed"]);
    ta(&dir, &["create", "api", "state=open"]);
    ta(&dir, &["block", "api", "db"]);

    // With the override, db counts as done, so api becomes ready.
    let ready = ta(&dir, &["ready"]);
    assert!(lists_task(&ready, "api"), "api should be ready: {ready}");
    assert!(!lists_task(&ready, "db"), "db is closed/done: {ready}");
}

#[test]
fn reinit_is_idempotent_and_preserves_edited_config() {
    let dir = fresh_dir("reinit");
    init_repo(&dir);
    ta(&dir, &["init"]);

    fs::write(
        dir.join(".taska/config.toml"),
        "[workflow]\ndone_status = \"closed\"\n",
    )
    .unwrap();

    let out = ta(&dir, &["init"]);
    assert!(out.contains("already present"), "should reuse store: {out}");

    let cfg = fs::read_to_string(dir.join(".taska/config.toml")).unwrap();
    assert!(
        cfg.contains("closed"),
        "edited config must survive re-init: {cfg}"
    );
}

#[test]
fn undo_uncommitted_truncates_the_tail() {
    let dir = fresh_dir("undo-uncommitted");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // Nothing is committed, so undo just truncates the last (uncommitted) event.
    ta(&dir, &["undo", "--force"]);
    assert_eq!(rows(&log), before - 1, "one event truncated from the log");

    let list = ta(&dir, &["list"]);
    assert!(lists_task(&list, "a"), "a should remain: {list}");
    assert!(!lists_task(&list, "b"), "b should be gone: {list}");
}

#[test]
fn undo_count_removes_the_last_n() {
    let dir = fresh_dir("undo-count");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    ta(&dir, &["undo", "--count", "2", "--force"]);
    assert_eq!(rows(&log), before - 2, "two events truncated");

    let list = ta(&dir, &["list"]);
    assert!(lists_task(&list, "a"), "a remains: {list}");
    assert!(!lists_task(&list, "b"), "b undone: {list}");
    assert!(!lists_task(&list, "c"), "c undone: {list}");
}

#[test]
fn undo_committed_appends_a_compensating_event() {
    let dir = fresh_dir("undo-committed");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);
    // Commit the create AND the update so the undone event is already committed.
    ta(&dir, &["update", "a", "status=closed"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Undo the committed update (no --remove): because the update is committed,
    // the log must GROW (a compensating event is appended), the committed event
    // stays, and the materialized state reverts.
    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    ta(&dir, &["undo", "--force"]);
    assert_eq!(
        rows(&log),
        before + 1,
        "committed undo appends a compensating event (log grows)"
    );

    // status reverts to its prior committed value.
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(r#""status":"open""#),
        "status reverted to open: {json}"
    );
}

#[test]
fn undo_committed_unsets_a_newly_added_field() {
    let dir = fresh_dir("undo-committed-unset");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);
    // Commit the create AND the field-add so the undone event is committed and
    // the compensating (append-only) path runs rather than truncation.
    ta(&dir, &["update", "a", "owner=bob"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // Undo the committed field-add. The compensating Update must UNSET the field
    // (via the null convention), and the log must grow rather than shrink.
    ta(&dir, &["undo", "--force"]);
    assert_eq!(rows(&log), before + 1, "compensating event appended");

    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(!json.contains("owner"), "owner unset after undo: {json}");
    assert!(
        json.contains(r#""status":"open""#),
        "status preserved: {json}"
    );
}

#[test]
fn undo_remove_truncates_committed_and_warns() {
    let dir = fresh_dir("undo-remove");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["update", "a", "status=closed"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // --remove on a committed event truncates (log shrinks) and warns on stderr.
    let out = run(ta_bin(), &dir, &["undo", "--remove", "--force"]);
    assert!(out.status.success(), "undo --remove should succeed");
    assert_eq!(
        rows(&log),
        before - 1,
        "--remove truncates the committed event (log shrinks)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("DANGER"),
        "--remove on a committed event warns loudly: {stderr}"
    );

    // The earlier (still-committed) create remains; status reverts.
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(r#""status":"open""#),
        "the update was removed, leaving the create: {json}"
    );
}

#[test]
fn undo_with_empty_log_is_a_noop() {
    let dir = fresh_dir("undo-empty");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let out = ta(&dir, &["undo", "--force"]);
    assert!(out.contains("Nothing to undo"), "got: {out}");
}

#[test]
fn init_from_subdirectory_reuses_existing_store() {
    let dir = fresh_dir("subdir");
    init_repo(&dir);
    ta(&dir, &["init"]);

    let nested = dir.join("src/deep");
    fs::create_dir_all(&nested).unwrap();

    let out = ta(&nested, &["init"]);
    assert!(out.contains("already present"), "should reuse: {out}");
    assert!(
        !nested.join(".taska").exists(),
        "must not create a nested .taska"
    );
}

#[test]
fn unblock_makes_a_blocked_task_ready() {
    let dir = fresh_dir("unblock");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // `api` depends on `db`, and `db` is still open, so `api` is blocked: only
    // `db` itself is ready.
    ta(&dir, &["create", "db", "status=open"]);
    ta(&dir, &["create", "api", "status=open"]);
    ta(&dir, &["block", "api", "db"]);
    let before = ta(&dir, &["ready"]);
    assert!(lists_task(&before, "db"), "db ready: {before}");
    assert!(
        !lists_task(&before, "api"),
        "api blocked by open db: {before}"
    );

    // Removing the dependency lifts the block, so `api` becomes ready too.
    let msg = ta(&dir, &["unblock", "api", "db"]);
    assert!(
        msg.contains("no longer depends"),
        "unblock should confirm: {msg}"
    );
    let after = ta(&dir, &["ready"]);
    assert!(
        lists_task(&after, "api"),
        "api ready after unblock: {after}"
    );

    // The dependency is gone from the materialized task, not just from `ready`.
    let json = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(json.contains(r#""deps":[]"#), "dep removed: {json}");
}

#[test]
fn dependency_cycle_is_reported_by_ready() {
    let dir = fresh_dir("cycle");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // a -> b and b -> a form a cycle. `ready` runs the topological sort, so it
    // must refuse and name the cycle (it can't order a circular graph).
    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["create", "b", "status=open"]);
    ta(&dir, &["block", "a", "b"]);
    ta(&dir, &["block", "b", "a"]);

    let out = run(ta_bin(), &dir, &["ready"]);
    assert!(
        !out.status.success(),
        "ready must exit non-zero on a dependency cycle, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("cycle"),
        "ready should report the cycle: {stderr}"
    );
}

#[test]
fn config_columns_and_max_width_are_honored() {
    let dir = fresh_dir("display-config");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // The `[display]` config drives both the default column set and human-table
    // truncation, with no command-line flags.
    fs::write(
        dir.join(".taska/config.toml"),
        "[display]\ncolumns = [\"id\", \"note\"]\nmax_width = 8\n",
    )
    .unwrap();
    ta(
        &dir,
        &[
            "create",
            "a",
            "note=ThisIsAVeryLongNoteValue",
            "status=open",
        ],
    );

    let human = ta(&dir, &["list"]);
    // Only the configured columns appear (note, but not the unlisted status), and
    // the long value is truncated to max_width with a trailing ellipsis.
    assert!(
        human.contains("NOTE") && !human.contains("STATUS"),
        "config columns drive the header: {human}"
    );
    assert!(
        human.contains("ThisIsA…"),
        "max_width truncates with an ellipsis: {human}"
    );
    assert!(
        !human.contains("ThisIsAVeryLong"),
        "the full value must not appear once truncated: {human}"
    );

    // json honors the configured column order too (id then note, no status).
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(
        json.contains(r#""note":"ThisIsAVeryLongNoteValue""#),
        "json is not truncated: {json}"
    );
    assert!(
        !json.contains("status"),
        "status not a configured column: {json}"
    );
}

#[test]
fn empty_results_render_placeholders_and_empty_json_array() {
    let dir = fresh_dir("empty-results");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // With no tasks at all, list/ready print their human placeholders and `[]`
    // for json.
    assert_eq!(ta(&dir, &["list"]).trim(), "(no tasks)");
    assert_eq!(ta(&dir, &["list", "--format", "json"]).trim(), "[]");
    assert_eq!(ta(&dir, &["ready"]).trim(), "(nothing ready)");
    assert_eq!(ta(&dir, &["ready", "--format", "json"]).trim(), "[]");

    // A search that matches nothing has its own placeholder and empty array.
    ta(&dir, &["create", "a", "status=open"]);
    assert_eq!(
        ta(&dir, &["search", "status=closed"]).trim(),
        "(no matches)"
    );
    assert_eq!(
        ta(&dir, &["search", "status=closed", "--format", "json"]).trim(),
        "[]"
    );
}

#[test]
fn reserved_field_keys_are_rejected() {
    let dir = fresh_dir("reserved");
    init_repo(&dir);
    ta(&dir, &["init"]);

    let log = dir.join(".taska/mutations.jsonl");
    // Each reserved envelope key must be refused up front (non-zero exit) and
    // append nothing, so it can never shadow the event envelope.
    for key in ["seq", "op", "task_id", "timestamp", "_meta"] {
        let out = run(ta_bin(), &dir, &["create", "x", &format!("{key}=v")]);
        assert!(
            !out.status.success(),
            "reserved key `{key}` must be rejected, got:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("reserved"),
            "stderr should explain the reservation for `{key}`: {stderr}"
        );
        assert!(
            !log.exists() || rows(&log) == 0,
            "a rejected reserved-key create must append nothing"
        );
    }
}

#[test]
fn null_value_unset_is_reflected_in_list_and_search() {
    let dir = fresh_dir("null-list-search");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[display]\ncolumns = [\"id\", \"owner\", \"status\"]\n",
    )
    .unwrap();
    ta(&dir, &["create", "x", "owner=bob", "status=open"]);

    // The field is searchable before the unset.
    assert!(
        lists_task(&ta(&dir, &["search", "owner=bob"]), "x"),
        "owner=bob should match before unset"
    );

    // Unset via the null convention; the value disappears from every read path.
    ta(&dir, &["update", "x", "owner=null"]);
    assert_eq!(
        ta(&dir, &["search", "owner=bob"]).trim(),
        "(no matches)",
        "search no longer finds the unset field"
    );
    let list = ta(&dir, &["list", "--format", "json"]);
    assert!(!list.contains("bob"), "owner value gone from list: {list}");
    // show is a single task: the unset `owner` field vanishes entirely (no key,
    // no null), and JSON never carries a null for an absent field.
    let show = ta(&dir, &["show", "x", "--format", "json"]);
    assert!(!show.contains("owner"), "owner gone from show: {show}");
    assert!(!show.contains("null"), "no null in json: {show}");
    assert!(
        show.contains(r#""status":"open""#),
        "status preserved: {show}"
    );
}

#[test]
fn clean_disjoint_field_merge_has_no_conflict() {
    let dir = fresh_dir("disjoint-fields");
    init_repo(&dir);
    ta(&dir, &["init"]); // default on_conflict = surface
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Each branch sets a DIFFERENT field of the same task: no overlap, so even the
    // strict `surface` policy must merge cleanly with no marker and no failure.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "owner=alice"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "priority=3"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "disjoint-field edits must merge cleanly under surface: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "no conflict marker for a clean merge"
    );

    // Both disjoint edits survive.
    let json = ta(&dir, &["show", "t", "--format", "json"]);
    assert!(
        json.contains(r#""owner":"alice""#),
        "ours field kept: {json}"
    );
    assert!(
        json.contains(r#""priority":3"#),
        "theirs field kept: {json}"
    );
}

#[test]
fn ours_policy_keeps_the_branch_merged_into() {
    let dir = fresh_dir("ours");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"ours\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    // Merge feature INTO main: `ours` keeps main's value, with no marker/failure.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "ours policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "auto resolution leaves no marker"
    );
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(
        json.contains(r#""status":"main""#),
        "ours (main) should win: {json}"
    );
}

#[test]
fn latest_policy_keeps_the_newest_write() {
    let dir = fresh_dir("latest");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"latest\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Write main's edit FIRST, then feature's: the feature write has the later
    // timestamp, so `latest` must keep it regardless of merge direction.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "latest policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(
        json.contains(r#""status":"feature""#),
        "latest (the newer feature write) should win: {json}"
    );
}

#[test]
fn baseline_keep_ours_merges_after_both_branches_compact() {
    let dir = fresh_dir("baseline-merge");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Compaction needs more than keep_events to fold anything into baseline.jsonl,
    // which is what exercises the keep-ours baseline driver on merge.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 100\nkeep_days = 0\n",
    )
    .unwrap();

    // 130 creates > keep_events (100): 30 fold into the baseline, 100 stay.
    for i in 0..130 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches compact independently (folding the shared prefix), so both
    // baseline.jsonl AND mutations.jsonl diverge and must merge via their drivers.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["compact"]);
    git(&dir, &["commit", "-aqm", "main compact"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "extra"]);
    ta(&dir, &["compact"]);
    git(&dir, &["commit", "-aqm", "feature compact"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "compacted baselines must merge cleanly (keep-ours): {}",
        String::from_utf8_lossy(&merge.stderr)
    );

    // ours' baseline is kept verbatim (the 30 folded tasks), and the log driver
    // still reconciles the recent suffix, so every task — old, new, and feature's
    // late `extra` — remains visible after the merge.
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        30,
        "keep-ours retains our own baseline depth"
    );
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t129", "extra"] {
        assert!(lists_task(&list, id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn reverts_converge_regardless_of_merge_direction() {
    // A git revert of the commit that ADDED some tasks must converge to the same
    // surviving set no matter which way the branches are later merged — the merge
    // driver unions both sides' removals. We build the identical history twice and
    // merge it both directions, then assert the materialized task sets match.
    fn build(dir: &Path) {
        init_repo(dir);
        ta(dir, &["init"]);
        ta(dir, &["create", "keep1"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", "c0 base"]);
        ta(dir, &["create", "drop1"]);
        ta(dir, &["create", "drop2"]);
        git(dir, &["commit", "-aqm", "c1 adds drop1 drop2"]);
        ta(dir, &["create", "keep2"]);
        git(dir, &["commit", "-aqm", "c2 adds keep2"]);
        // Revert the commit that introduced drop1/drop2 (its Create events vanish
        // from the log), leaving keep1/keep2.
        git(dir, &["revert", "--no-edit", "HEAD~1"]);
        // Branch and add one distinct task per side.
        git(dir, &["branch", "feature"]);
        ta(dir, &["create", "on_main"]);
        git(dir, &["commit", "-aqm", "main task"]);
        git(dir, &["checkout", "-q", "feature"]);
        ta(dir, &["create", "on_feature"]);
        git(dir, &["commit", "-aqm", "feature task"]);
    }

    fn task_ids(dir: &Path) -> Vec<String> {
        let mut ids: Vec<String> = ta(dir, &["list"])
            .lines()
            .skip(1) // header row
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        ids.sort();
        ids
    }

    // Direction 1: merge feature INTO main.
    let d1 = fresh_dir("revert-fwd");
    build(&d1);
    git(&d1, &["checkout", "-q", "main"]);
    let m1 = run("git", &d1, &["merge", "feature", "-m", "merge"]);
    assert!(
        m1.status.success(),
        "fwd merge: {}",
        String::from_utf8_lossy(&m1.stderr)
    );

    // Direction 2: merge main INTO feature.
    let d2 = fresh_dir("revert-rev");
    build(&d2);
    // Currently on `feature`; merge main in.
    let m2 = run("git", &d2, &["merge", "main", "-m", "merge"]);
    assert!(
        m2.status.success(),
        "rev merge: {}",
        String::from_utf8_lossy(&m2.stderr)
    );

    let fwd = task_ids(&d1);
    let rev = task_ids(&d2);
    assert_eq!(
        fwd, rev,
        "revert must converge both directions: {fwd:?} vs {rev:?}"
    );
    // The reverted tasks are gone; everything else survives, both ways.
    assert_eq!(
        fwd,
        ["keep1", "keep2", "on_feature", "on_main"],
        "surviving set after a reverted add: {fwd:?}"
    );
}
