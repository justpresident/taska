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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    assert!(cfg.contains("keep_events = 5000"), "config: {cfg}");
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
    ta(&dir, &["dep", "add", "api", "depends_on=db"]);

    // The human table lists ids; `--full --format json` exposes every field —
    // priority coerced to a JSON number, and deps as a JSON array.
    assert!(
        lists_task(&ta(&dir, &["list"]), "api"),
        "api should be listed"
    );
    let json = ta(&dir, &["list", "--full", "--format", "json"]);
    assert!(json.contains(r#""priority":3"#), "json: {json}");
    assert!(json.contains(r#""deps":["db"]"#), "json: {json}");

    let search = ta(&dir, &["list", "status=open"]);
    assert!(lists_task(&search, "api"), "search: {search}");
    assert!(!lists_task(&search, "db"), "db is done, not open: {search}");

    // db is done, so api's only dependency is satisfied -> api is ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&ready, "api"), "ready: {ready}");

    // Once api is done too, nothing is ready.
    ta(&dir, &["update", "api", "status=closed"]);
    assert_eq!(ta(&dir, &["list", "--ready"]).trim(), "(nothing ready)");

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
    ta(&dir, &["dep", "add", "a", "depends_on=dep"]);

    // `show`'s human output is a vertical record: one `field: value` line each,
    // every field (even non-default columns like priority), plus deps.
    let human = ta(&dir, &["show", "a"]);
    assert!(
        human
            .lines()
            .any(|l| l.starts_with("id:") && l.split_whitespace().last() == Some("a")),
        "vertical id line: {human}"
    );
    assert!(
        human
            .lines()
            .any(|l| l.starts_with("title:") && l.contains("Alpha")),
        "title field: {human}"
    );
    assert!(
        human
            .lines()
            .any(|l| l.starts_with("priority:") && l.contains('3')),
        "priority field: {human}"
    );
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
    ta(&dir, &["dep", "add", "a", "depends_on=dep"]);

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
fn sort_flag_orders_rows_with_reverse_and_configurable_default() {
    let dir = fresh_dir("sort");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "b", "priority=2"]);
    ta(&dir, &["create", "a", "priority=3"]);
    ta(&dir, &["create", "c", "priority=1"]);

    // Collect the id column (first token of each row after the header).
    let ids = |out: &str| -> Vec<String> {
        out.lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect()
    };

    // --sort over a numeric column sorts numerically, and --reverse flips it.
    let asc = ta(&dir, &["list", "--sort", "priority", "--columns", "id"]);
    assert_eq!(ids(&asc), ["c", "b", "a"], "ascending by priority: {asc}");
    let desc = ta(
        &dir,
        &["list", "--sort", "priority", "--reverse", "--columns", "id"],
    );
    assert_eq!(ids(&desc), ["a", "b", "c"], "reversed: {desc}");

    // A task missing the sort column sorts last.
    ta(&dir, &["create", "d"]);
    let missing = ta(&dir, &["list", "--sort", "priority", "--columns", "id"]);
    assert_eq!(
        ids(&missing),
        ["c", "b", "a", "d"],
        "missing last: {missing}"
    );

    // search honors --sort too.
    let s = ta(
        &dir,
        &[
            "list",
            "priority~.",
            "--sort",
            "priority",
            "--columns",
            "id",
        ],
    );
    assert_eq!(ids(&s), ["c", "b", "a"], "search sorted: {s}");

    // The default sort column is configurable (no --sort given).
    fs::write(dir.join(".taska/config.toml"), "[display]\nsort = \"id\"\n").unwrap();
    let by_id = ta(&dir, &["list", "--columns", "id"]);
    assert_eq!(
        ids(&by_id),
        ["a", "b", "c", "d"],
        "config default sort=id: {by_id}"
    );
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
        lists_task(&ta(&dir, &["list", "create_time~^20"]), "api"),
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
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();
    for i in 0..350 {
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
    ta(&dir, &["dep", "add", "api", "depends_on=db"]);

    // list/search/ready/show all speak jsonl: one bare object per line, no array
    // wrapper, and never a null for an absent field.
    for args in [
        vec!["list", "--full", "--format", "jsonl"],
        vec!["list", "status=open", "--format", "jsonl"],
        vec!["list", "--ready", "--format", "jsonl"],
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
fn list_supports_regex_negation_and_combined_criteria() {
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
    ta(&dir, &["dep", "add", "web", "depends_on=api"]);

    // Multiple criteria are AND-combined.
    let both = ta(&dir, &["list", "status=open", "priority=3"]);
    assert!(
        lists_task(&both, "api") && !lists_task(&both, "web"),
        "AND: {both}"
    );

    // `~` is a regex over the field's string form; numbers match too.
    let re = ta(&dir, &["list", r"priority~^[12]$"]);
    assert!(
        lists_task(&re, "db") && lists_task(&re, "web") && !lists_task(&re, "api"),
        "regex on numeric field: {re}"
    );

    // Negation, and querying built-in id / deps fields.
    let ne = ta(&dir, &["list", "status!=open"]);
    assert!(
        lists_task(&ne, "db") && !lists_task(&ne, "api"),
        "negation: {ne}"
    );
    assert!(
        lists_task(&ta(&dir, &["list", "deps=api"]), "web"),
        "deps query"
    );
    assert!(lists_task(&ta(&dir, &["list", "id~^a"]), "api"), "id regex");

    // A malformed criterion or bad regex is rejected (non-zero exit).
    assert!(!run(ta_bin(), &dir, &["list", "nooperator"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["list", "title~["]).status.success());
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
        "5000"
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
    ta(&dir, &["dep", "add", "api", "depends_on=web"]);

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
fn create_stamps_configurable_default_status() {
    let dir = fresh_dir("default-status");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // A bare create gets the out-of-the-box default status.
    ta(&dir, &["create", "a"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(r#""status":"todo""#),
        "bare create defaults status to todo"
    );

    // An explicit status still wins over the default.
    ta(&dir, &["create", "b", "status=open"]);
    assert!(
        ta(&dir, &["show", "b", "--format", "json"]).contains(r#""status":"open""#),
        "explicit status overrides the default"
    );

    // The default is configurable.
    ta(
        &dir,
        &["config", "set", "workflow.default_status", "backlog"],
    );
    ta(&dir, &["create", "c"]);
    assert!(
        ta(&dir, &["show", "c", "--format", "json"]).contains(r#""status":"backlog""#),
        "configured default status is applied"
    );

    // Setting it empty restores statusless creation.
    ta(&dir, &["config", "set", "workflow.default_status", ""]);
    ta(&dir, &["create", "d"]);
    assert!(
        !ta(&dir, &["show", "d", "--format", "json"]).contains("status"),
        "empty default_status leaves the task statusless"
    );
}

#[test]
fn compact_folds_log_and_appends_resume() {
    let dir = fresh_dir("compact");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A valid retention floor (>= MIN_KEEP_EVENTS = 300). We then generate MORE
    // events than that so compaction actually folds the old prefix and retains
    // exactly the recent suffix — the fold-and-resume path.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300), so 50 fold into the baseline and 300 stay.
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    ta(&dir, &["compact"]);

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        300,
        "keep_events most-recent events retained in the log"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "the folded remainder (350 - keep_events) is in the baseline"
    );

    // Appends overlay the baseline after compaction, and the older folded tasks
    // are still visible — fold-and-resume keeps everything reachable.
    ta(&dir, &["create", "resumed"]);
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t75", "t349", "resumed"] {
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
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300): 50 fold into baseline, 300 newest retained.
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("kept 300 recent event(s)"), "got: {out}");

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        300,
        "kept keep_events recent events for merge reconciliation"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "folded the oldest 50 into baseline"
    );

    // The retained log holds the most recent creations (not the oldest, which
    // were folded away). The newest task is in the log; the oldest is not.
    let mutations = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        mutations.contains(r#""task_id":"t349""#),
        "expected the newest event retained: {mutations}"
    );
    assert!(
        !mutations.contains(r#""task_id":"t0""#),
        "the oldest event should have been folded out of the log: {mutations}"
    );

    // All tasks remain visible (baseline + retained log), old and new alike.
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t49", "t50", "t349"] {
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
        stderr.contains("keep_events") && stderr.contains("300"),
        "stderr: {stderr}"
    );
}

#[test]
fn compact_is_noop_below_threshold() {
    let dir = fresh_dir("compact_noop");
    init_repo(&dir);
    ta(&dir, &["init"]); // default keep_events = 5000

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
    ta(&dir, &["dep", "add", "api", "depends_on=db"]);

    // With the override, db counts as done, so api becomes ready.
    let ready = ta(&dir, &["list", "--ready"]);
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
fn dep_remove_makes_a_blocked_task_ready() {
    let dir = fresh_dir("dep-remove-ready");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // `api` depends on `db`, and `db` is still open, so `api` is blocked: only
    // `db` itself is ready.
    ta(&dir, &["create", "db", "status=open"]);
    ta(&dir, &["create", "api", "status=open"]);
    ta(&dir, &["dep", "add", "api", "depends_on=db"]);
    let before = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&before, "db"), "db ready: {before}");
    assert!(
        !lists_task(&before, "api"),
        "api blocked by open db: {before}"
    );

    // Removing the dependency lifts the block, so `api` becomes ready too.
    let msg = ta(&dir, &["dep", "remove", "api", "depends_on=db"]);
    assert!(
        msg.contains("Removed 1 edge(s)"),
        "dep remove should confirm: {msg}"
    );
    let after = ta(&dir, &["list", "--ready"]);
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
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);

    let out = run(ta_bin(), &dir, &["list", "--ready"]);
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
    assert_eq!(ta(&dir, &["list", "--ready"]).trim(), "(nothing ready)");
    assert_eq!(
        ta(&dir, &["list", "--ready", "--format", "json"]).trim(),
        "[]"
    );

    // A search that matches nothing has its own placeholder and empty array.
    ta(&dir, &["create", "a", "status=open"]);
    assert_eq!(ta(&dir, &["list", "status=closed"]).trim(), "(no matches)");
    assert_eq!(
        ta(&dir, &["list", "status=closed", "--format", "json"]).trim(),
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
        lists_task(&ta(&dir, &["list", "owner=bob"]), "x"),
        "owner=bob should match before unset"
    );

    // Unset via the null convention; the value disappears from every read path.
    ta(&dir, &["update", "x", "owner=null"]);
    assert_eq!(
        ta(&dir, &["list", "owner=bob"]).trim(),
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
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300): 50 fold into the baseline, 300 stay.
    for i in 0..350 {
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

    // ours' baseline is kept verbatim (the 50 folded tasks), and the log driver
    // still reconciles the recent suffix, so every task — old, new, and feature's
    // late `extra` — remains visible after the merge.
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "keep-ours retains our own baseline depth"
    );
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t349", "extra"] {
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

#[test]
fn revert_to_empty_log_is_handled() {
    // Reverting the commit that introduced the only task empties (or removes)
    // mutations.jsonl. The CLI must treat that degenerate empty / None-watermark
    // state as "no tasks", never erroring.
    let dir = fresh_dir("revert-empty");
    init_repo(&dir);
    ta(&dir, &["init"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    ta(&dir, &["create", "only"]);
    git(&dir, &["commit", "-aqm", "add only"]);
    // Reverting the create drops its line, leaving the log empty.
    git(&dir, &["revert", "--no-edit", "HEAD"]);

    assert!(
        !lists_task(&ta(&dir, &["list"]), "only"),
        "the reverted task must be gone and `list` must not error"
    );
    assert!(
        ta(&dir, &["status", "--format", "json"]).contains(r#""total":0"#),
        "an emptied log reports zero tasks"
    );
}

#[test]
fn merge_warns_when_one_branch_reverts_a_shared_event() {
    // main and feature share a committed task `shared`; main alone reverts it.
    // The merge reconciles (the revert wins) but must WARN that a shared event was
    // reverted on one branch and kept on the other — not silently drop it.
    let dir = fresh_dir("revert-warn");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "base"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    ta(&dir, &["create", "shared"]);
    git(&dir, &["commit", "-aqm", "add shared"]);

    // Branch BEFORE reverting, so feature keeps `shared` while main drops it.
    // main only reverts and does NOT create afterwards: the freed seq stays unused,
    // so this is the pure presence-divergence the detector catches (a later create
    // would reuse the seq and surface as a content mismatch instead). The revert
    // auto-commits.
    git(&dir, &["branch", "feature"]);
    git(&dir, &["revert", "--no-edit", "HEAD"]); // main reverts the "add shared" commit

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "on_feature"]);
    git(&dir, &["commit", "-aqm", "feature task"]);

    git(&dir, &["checkout", "-q", "main"]);
    let m = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        m.status.success(),
        "merge should succeed (warn, not fail): {}",
        String::from_utf8_lossy(&m.stderr)
    );
    assert!(
        String::from_utf8_lossy(&m.stderr).contains("reverted on one branch"),
        "expected the shared-revert warning on stderr, got: {}",
        String::from_utf8_lossy(&m.stderr)
    );

    // The revert wins convergently: `shared` is gone, everything else survives.
    let list = ta(&dir, &["list"]);
    assert!(
        !lists_task(&list, "shared"),
        "reverted shared task is gone: {list}"
    );
    for id in ["base", "on_feature"] {
        assert!(lists_task(&list, id), "missing {id}: {list}");
    }
}

#[test]
fn field_value_from_file_and_stdin() {
    let dir = fresh_dir("field-input");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // A value that's hostile to argv: quotes, backticks, a $(...) and newlines.
    let note = "Title: \"big\" job\n\n- uses `ta` and $(whoami)\n- 'apostrophes' too";
    let note_path = dir.join("note.md");
    fs::write(&note_path, note).unwrap();

    // `@file` reads the value verbatim — no shell expansion, no quoting needed.
    ta(
        &dir,
        &["create", "t1", &format!("notes=@{}", note_path.display())],
    );
    let json = ta(&dir, &["show", "t1", "--format", "json"]);
    for frag in ["whoami", "apostrophes"] {
        assert!(json.contains(frag), "note fragment {frag} missing: {json}");
    }
    assert!(
        json.contains("$(whoami)"),
        "file content is literal, never shell-expanded: {json}"
    );

    // `@-` reads the value from stdin.
    let mut child = Command::new(ta_bin())
        .args(["update", "t1", "summary=@-"])
        .current_dir(&dir)
        .env("PATH", path_with_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"summary piped from stdin\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stdin update failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        ta(&dir, &["show", "t1", "--format", "json"]).contains("summary piped from stdin"),
        "stdin value (trailing newline trimmed) stored"
    );

    // `@@x` is a literal `@x`, not a file read.
    ta(&dir, &["create", "t2", "owner=@@alice"]);
    assert!(
        ta(&dir, &["show", "t2", "--format", "json"]).contains(r#""owner":"@alice""#),
        "double-@ escapes to a literal @ value"
    );
}

#[test]
fn append_op_accumulates_a_text_log() {
    let dir = fresh_dir("append");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "task"]);
    ta(&dir, &["update", "task", "log+=started"]);
    ta(&dir, &["update", "task", "log+=made progress"]);
    // The two entries accumulate, newline-joined, instead of overwriting.
    let json = ta(&dir, &["show", "task", "--format", "json"]);
    assert!(
        json.contains(r#""log":"started\nmade progress""#),
        "append accumulates a log: {json}"
    );
}

#[test]
fn update_mixes_set_and_append_in_one_command() {
    let dir = fresh_dir("update-mixed");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "t", "status=open"]);
    // One command: set `status` (=) and append to `log` (+=).
    ta(
        &dir,
        &["update", "t", "status=closed", "log+=did the thing"],
    );
    let json = ta(&dir, &["show", "t", "--format", "json"]);
    assert!(
        json.contains(r#""status":"closed""#) && json.contains(r#""log":"did the thing""#),
        "set and append in one update: {json}"
    );
    // A further append accumulates onto it.
    ta(&dir, &["update", "t", "log+=and another"]);
    assert!(
        ta(&dir, &["show", "t", "--format", "json"])
            .contains(r#""log":"did the thing\nand another""#),
        "subsequent append accumulates"
    );
}

#[test]
fn concurrent_appends_merge_without_conflict() {
    let dir = fresh_dir("append-merge");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "log"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["branch", "feature"]);

    // Each branch appends to the SAME field since the fork.
    ta(&dir, &["update", "log", "notes+=from main"]);
    git(&dir, &["commit", "-aqm", "main note"]);
    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "log", "notes+=from feature"]);
    git(&dir, &["commit", "-aqm", "feature note"]);

    // Default on_conflict=surface FAILS the merge on a real conflict — so a clean
    // merge here proves appends commute. Both entries must survive.
    git(&dir, &["checkout", "-q", "main"]);
    let m = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        m.status.success(),
        "concurrent appends must merge cleanly: {}",
        String::from_utf8_lossy(&m.stderr)
    );
    let json = ta(&dir, &["show", "log", "--format", "json"]);
    assert!(
        json.contains("from main") && json.contains("from feature"),
        "both appends present after merge: {json}"
    );
}

#[test]
fn dep_command_adds_and_removes_typed_edges() {
    let dir = fresh_dir("dep-cmd");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    // Add a depends_on edge (shows in the deps column) and a typed relates_to edge.
    ta(&dir, &["dep", "add", "a", "depends_on=b", "relates_to=c"]);
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(r#""deps":["b"]"#),
        "depends_on shows in deps: {json}"
    );
    // The relates_to edge is recorded as a typed AddDep event.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""type":"relates_to""#) && log.contains(r#""dep":"c""#),
        "typed relates_to edge in the log: {log}"
    );

    // Remove the depends_on edge.
    ta(&dir, &["dep", "remove", "a", "depends_on=b"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(r#""deps":[]"#),
        "depends_on edge removed"
    );

    // An undeclared relationship type is rejected with a helpful error.
    let out = run(ta_bin(), &dir, &["dep", "add", "a", "bogus=b"]);
    assert!(!out.status.success(), "undeclared type must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown relationship type"),
        "error names the problem: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dep_list_shows_forward_and_inverse_edges() {
    let dir = fresh_dir("dep-list");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    // `a depends_on b` (inverse `blocks`) and `a relates_to c` (self-inverse).
    ta(&dir, &["dep", "add", "a", "depends_on=b", "relates_to=c"]);

    // `a` lists its own forward edges.
    let a = ta(&dir, &["dep", "list", "a"]);
    assert!(a.contains("depends_on: b"), "a forward depends_on: {a}");
    assert!(a.contains("relates_to: c"), "a forward relates_to: {a}");

    // `b` never named `a`, but the inverse of `depends_on` surfaces as `blocks`.
    let b = ta(&dir, &["dep", "list", "b"]);
    assert!(b.contains("blocks: a"), "b inverse blocks: {b}");

    // `relates_to` is self-inverse, so `c` shows the symmetric edge back to `a`.
    let c = ta(&dir, &["dep", "list", "c"]);
    assert!(c.contains("relates_to: a"), "c symmetric relates_to: {c}");
}

#[test]
fn dep_remove_by_inverse_name_drops_the_forward_edge() {
    let dir = fresh_dir("dep-remove-inverse");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    assert!(ta(&dir, &["dep", "list", "b"]).contains("blocks: a"));

    // Remove the relationship from b's side using the inverse name `blocks`.
    ta(&dir, &["dep", "remove", "b", "blocks=a"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(r#""deps":[]"#),
        "inverse removal dropped a's depends_on edge"
    );
    let b = ta(&dir, &["dep", "list", "b"]);
    assert!(!b.contains("blocks: a"), "inverse edge gone from b: {b}");
}

#[test]
fn dep_tree_nests_dependencies_and_collapses_shared_nodes() {
    let dir = fresh_dir("dep-tree");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["a", "b", "c", "d", "e"] {
        ta(&dir, &["create", id]);
    }
    // a → {b, c}; both b and c → d (a shared/diamond node); d → e.
    ta(&dir, &["dep", "add", "a", "depends_on=b", "depends_on=c"]);
    ta(&dir, &["dep", "add", "b", "depends_on=d"]);
    ta(&dir, &["dep", "add", "c", "depends_on=d"]);
    ta(&dir, &["dep", "add", "d", "depends_on=e"]);

    let tree = ta(&dir, &["dep", "tree", "a"]);
    assert!(tree.contains("├─ b"), "first child branch: {tree}");
    assert!(tree.contains("└─ c"), "last child branch: {tree}");
    assert!(tree.contains("└─ e"), "e nested under d: {tree}");
    // d (with its e subtree) is reached again under c, but was already expanded
    // under b — the second occurrence collapses rather than reprinting.
    assert!(tree.contains("d …"), "shared node collapsed: {tree}");
}

#[test]
fn dep_cycles_reports_circular_dependencies() {
    let dir = fresh_dir("dep-cycles");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    // No cycle yet.
    assert!(ta(&dir, &["dep", "cycles"]).contains("No dependency cycles"));

    // Close a → b → a into a cycle.
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);
    let cycles = ta(&dir, &["dep", "cycles"]);
    assert!(cycles.contains("a ↔ b"), "cycle members reported: {cycles}");

    // The tree marks the back-edge rather than looping forever.
    assert!(
        ta(&dir, &["dep", "tree", "a"]).contains("(cycle)"),
        "tree flags the cycle"
    );
}

#[test]
fn custom_blocker_relationship_gates_readiness() {
    let dir = fresh_dir("blocker-type");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Declare a second blocker-typed relationship beyond depends_on.
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[relationships.requires]\ntype = \"blocker\"\ninverse = \"required_by\"\n");
    fs::write(&cfg, text).unwrap();

    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["create", "b", "status=open"]);
    ta(&dir, &["dep", "add", "a", "requires=b"]);

    // `requires` is a blocker, so `a` is gated by still-open `b`: only `b` ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&ready, "b"), "b ready: {ready}");
    assert!(!lists_task(&ready, "a"), "a blocked by requires=b: {ready}");

    // The tree walks the typed blocker edge and labels it.
    let tree = ta(&dir, &["dep", "tree", "a"]);
    assert!(
        tree.contains("b [requires]"),
        "typed blocker labelled: {tree}"
    );

    // A cycle through the custom blocker type is detected too.
    ta(&dir, &["dep", "add", "b", "requires=a"]);
    assert!(
        ta(&dir, &["dep", "cycles"]).contains("a ↔ b"),
        "custom-blocker cycle reported"
    );
    ta(&dir, &["dep", "remove", "b", "requires=a"]);

    // Close `b`, and `a` unblocks.
    ta(&dir, &["update", "b", "status=closed"]);
    assert!(
        lists_task(&ta(&dir, &["list", "--ready"]), "a"),
        "a ready after requires-dep done"
    );
}

#[test]
fn informational_relationship_does_not_gate_readiness() {
    let dir = fresh_dir("info-rel");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // `relates_to` is type=info in the default config.
    ta(&dir, &["create", "x", "status=open"]);
    ta(&dir, &["create", "y", "status=open"]);
    ta(&dir, &["dep", "add", "x", "relates_to=y"]);

    // An informational edge must not block: both are ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "x"),
        "x ready despite relates_to: {ready}"
    );
    assert!(lists_task(&ready, "y"), "y ready: {ready}");
}

#[test]
fn config_validate_flags_an_undeclared_relationship_type() {
    let dir = fresh_dir("cfg-validate-undeclared");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["dep", "add", "a", "relates_to=b"]);

    // A consistent store validates clean.
    assert!(
        ta(&dir, &["config", "validate"]).contains("Config OK"),
        "clean store validates"
    );

    // Drop `relates_to` from the config while task `a` still uses it.
    let cfg = dir.join(".taska/config.toml");
    fs::write(
        &cfg,
        "[relationships.depends_on]\ntype = \"blocker\"\ninverse = \"blocks\"\n",
    )
    .unwrap();
    let out = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(
        !out.status.success(),
        "undeclared-type config must be rejected"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("relates_to") && err.contains("not declared"),
        "error names the undeclared type: {err}"
    );
}

#[test]
fn config_validate_flags_a_blocker_cycle_and_set_runs_the_same_check() {
    let dir = fresh_dir("cfg-validate-cycle");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "x"]);
    ta(&dir, &["create", "y"]);
    ta(&dir, &["dep", "add", "x", "depends_on=y"]);
    ta(&dir, &["dep", "add", "y", "depends_on=x"]);

    let out = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(!out.status.success(), "a blocker cycle must be reported");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cycle"),
        "error mentions the cycle: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `config set` runs the same validation, so it too refuses while the graph
    // is inconsistent (the cheap struct-only path keeps fixing commands usable).
    let set = run(
        ta_bin(),
        &dir,
        &["config", "set", "merge.on_conflict", "ours"],
    );
    assert!(!set.status.success(), "config set runs graph validation");
}

#[test]
fn dep_plan_lists_remaining_prerequisites_in_order() {
    let dir = fresh_dir("dep-plan");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["build", "test", "ship"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // ship depends_on test depends_on build.
    ta(&dir, &["dep", "add", "ship", "depends_on=test"]);
    ta(&dir, &["dep", "add", "test", "depends_on=build"]);

    let plan = ta(&dir, &["dep", "plan", "ship"]);
    let (pb, pt, ps) = (
        plan.find("build").unwrap(),
        plan.find("test").unwrap(),
        plan.find("ship").unwrap(),
    );
    assert!(
        pb < pt && pt < ps,
        "prerequisites before dependents: {plan}"
    );
    assert!(plan.contains("3 task(s) remaining"), "count: {plan}");

    // A done prerequisite drops out of the plan as satisfied.
    ta(&dir, &["update", "build", "status=closed"]);
    let plan = ta(&dir, &["dep", "plan", "ship"]);
    assert!(!plan.contains("build"), "done prereq dropped: {plan}");
    assert!(plan.contains("2 task(s) remaining"), "count: {plan}");

    // With everything done there's nothing left to do.
    ta(&dir, &["update", "test", "status=closed"]);
    ta(&dir, &["update", "ship", "status=closed"]);
    assert!(
        ta(&dir, &["dep", "plan", "ship"]).contains("Nothing to do"),
        "all done -> nothing to do"
    );

    // An unknown goal is an error.
    let out = run(ta_bin(), &dir, &["dep", "plan", "nope"]);
    assert!(!out.status.success(), "unknown goal must fail");
}

#[test]
fn dep_plan_critical_shows_the_longest_chain() {
    let dir = fresh_dir("dep-plan-critical");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["ship", "a1", "a2", "a3", "c1"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // ship has a long branch (a3 -> a2 -> a1 -> ship) and a short one (c1 -> ship).
    ta(&dir, &["dep", "add", "ship", "depends_on=a1"]);
    ta(&dir, &["dep", "add", "a1", "depends_on=a2"]);
    ta(&dir, &["dep", "add", "a2", "depends_on=a3"]);
    ta(&dir, &["dep", "add", "ship", "depends_on=c1"]);

    // The full plan lists all five remaining tasks.
    let plan = ta(&dir, &["dep", "plan", "ship"]);
    assert!(
        plan.contains("c1"),
        "full plan includes the short branch: {plan}"
    );
    assert!(plan.contains("5 task(s) remaining"), "count: {plan}");

    // --critical narrows to the longest chain (a3,a2,a1,ship), dropping the short
    // branch, in dependency order.
    let crit = ta(&dir, &["dep", "plan", "ship", "--critical"]);
    assert!(!crit.contains("c1"), "short branch excluded: {crit}");
    let (p3, p2, p1, ps) = (
        crit.find("a3").unwrap(),
        crit.find("a2").unwrap(),
        crit.find("a1").unwrap(),
        crit.find("ship").unwrap(),
    );
    assert!(
        p3 < p2 && p2 < p1 && p1 < ps,
        "longest chain in order: {crit}"
    );
    assert!(
        crit.contains("critical path: 4 of 5"),
        "critical-path count: {crit}"
    );
}

#[test]
fn list_unblocks_and_blocked_by_columns() {
    let dir = fresh_dir("unblocks-cols");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["a", "b", "c"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // Chain c -> b -> a (c depends_on b depends_on a).
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);
    ta(&dir, &["dep", "add", "c", "depends_on=b"]);

    let line_for = |out: &str, id: &str| -> String {
        out.lines()
            .find(|l| l.contains(&format!("\"id\":\"{id}\"")))
            .unwrap_or_else(|| panic!("no line for {id} in {out}"))
            .to_string()
    };

    // Requested as columns, the transitive counts are exact.
    let json = ta(
        &dir,
        &[
            "list",
            "--columns",
            "id,unblocks,blocked_by",
            "--format",
            "jsonl",
        ],
    );
    assert!(
        line_for(&json, "a").contains(r#""unblocks":2"#),
        "a unblocks b,c: {json}"
    );
    assert!(
        line_for(&json, "a").contains(r#""blocked_by":0"#),
        "a: {json}"
    );
    assert!(
        line_for(&json, "c").contains(r#""blocked_by":2"#),
        "c blocked by a,b: {json}"
    );
    assert!(
        line_for(&json, "b").contains(r#""unblocks":1"#),
        "b unblocks c: {json}"
    );

    // --sort unblocks --reverse surfaces the highest-leverage task first.
    let human = ta(
        &dir,
        &[
            "list",
            "--columns",
            "id,unblocks",
            "--sort",
            "unblocks",
            "--reverse",
        ],
    );
    let order: Vec<&str> = human
        .lines()
        .skip(1) // header
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(order, ["a", "b", "c"], "ordered by leverage desc: {human}");

    // Opt-in only: a plain list never carries the computed columns.
    let default = ta(&dir, &["list", "--format", "jsonl"]);
    assert!(
        !default.contains("unblocks"),
        "default omits computed columns: {default}"
    );

    // Done prerequisites stop counting: closing `a` drops c's blocked_by to 1.
    ta(&dir, &["update", "a", "status=closed"]);
    let json = ta(
        &dir,
        &["list", "--columns", "id,blocked_by", "--format", "jsonl"],
    );
    assert!(
        line_for(&json, "c").contains(r#""blocked_by":1"#),
        "done prereq excluded: {json}"
    );
}

#[test]
fn subtask_hierarchy_gates_readiness_and_mirrors_both_ways() {
    let dir = fresh_dir("subtask");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "build-form", "wire-auth"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // Add from the parent side, and from the child side via the inverse — both
    // land as `has_subtask` edges on the parent.
    ta(&dir, &["dep", "add", "epic", "has_subtask=build-form"]);
    ta(&dir, &["dep", "add", "wire-auth", "subtask_of=epic"]);

    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""type":"has_subtask""#) && log.contains(r#""dep":"wire-auth""#),
        "inverse add stored as has_subtask on epic: {log}"
    );

    // dep list shows both directions: parent -> has_subtask, child -> subtask_of.
    let e = ta(&dir, &["dep", "list", "epic"]);
    assert!(
        e.contains("has_subtask:") && e.contains("build-form") && e.contains("wire-auth"),
        "epic lists its subtasks: {e}"
    );
    assert!(
        ta(&dir, &["dep", "list", "build-form"]).contains("subtask_of: epic"),
        "child mirrors the parent"
    );

    // Hierarchy gates like a blocker: the parent isn't ready until its subtasks are.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "build-form") && lists_task(&ready, "wire-auth"),
        "subtasks are ready: {ready}"
    );
    assert!(
        !lists_task(&ready, "epic"),
        "epic blocked by subtasks: {ready}"
    );

    // Close both subtasks -> the parent becomes ready.
    ta(&dir, &["update", "build-form", "status=closed"]);
    ta(&dir, &["update", "wire-auth", "status=closed"]);
    assert!(
        lists_task(&ta(&dir, &["list", "--ready"]), "epic"),
        "epic ready once its subtasks are done"
    );
}

#[test]
fn dep_tree_marks_subtasks_and_rolls_up_progress() {
    let dir = fresh_dir("subtask-tree");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "a", "b", "dep1"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "epic", "has_subtask=a"]);
    ta(&dir, &["dep", "add", "epic", "has_subtask=b"]);
    ta(&dir, &["dep", "add", "epic", "depends_on=dep1"]);
    ta(&dir, &["update", "a", "status=closed"]); // 1 of 2 subtasks done

    let tree = ta(&dir, &["dep", "tree", "epic"]);
    assert!(
        tree.contains("epic [subtasks 1/2]"),
        "parent rolls up child completion: {tree}"
    );
    assert!(tree.contains("a [subtask]"), "subtask tagged: {tree}");
    assert!(tree.contains("b [subtask]"), "subtask tagged: {tree}");
    // A plain depends_on edge is a dependency, not a subtask — never tagged.
    assert!(
        tree.contains("dep1") && !tree.contains("dep1 [subtask]"),
        "plain dependency untagged: {tree}"
    );
}

#[test]
fn list_subtasks_column_shows_parent_progress() {
    let dir = fresh_dir("subtask-col");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "a", "b", "solo"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "epic", "has_subtask=a"]);
    ta(&dir, &["dep", "add", "epic", "has_subtask=b"]);
    ta(&dir, &["update", "a", "status=closed"]); // 1 of 2

    let json = ta(
        &dir,
        &["list", "--columns", "id,subtasks", "--format", "jsonl"],
    );
    let line_for = |id: &str| -> String {
        json.lines()
            .find(|l| l.contains(&format!("\"id\":\"{id}\"")))
            .unwrap_or_else(|| panic!("no line for {id}"))
            .to_string()
    };
    assert!(
        line_for("epic").contains(r#""subtasks":"1/2""#),
        "parent progress: {json}"
    );
    // A task with no subtasks omits the column (absent, not "0/0").
    assert!(
        !line_for("solo").contains("subtasks"),
        "no subtasks omitted: {json}"
    );

    // Opt-in: a plain list never carries the computed column.
    assert!(
        !ta(&dir, &["list", "--format", "jsonl"]).contains("subtasks"),
        "default omits computed column"
    );
}

#[test]
fn show_surfaces_typed_relationships_forward_and_inverse() {
    let dir = fresh_dir("show-rels");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "child", "other", "a", "b"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "epic", "has_subtask=child"]);
    ta(&dir, &["dep", "add", "epic", "relates_to=other"]);
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);

    // The parent's record shows its typed relationships, grouped by type.
    let epic = ta(&dir, &["show", "epic"]);
    assert!(
        epic.contains("has_subtask:") && epic.contains("child"),
        "{epic}"
    );
    assert!(
        epic.contains("relates_to:") && epic.contains("other"),
        "{epic}"
    );

    // The child shows the inverse-mirrored edge (subtask_of), in json too.
    assert!(
        ta(&dir, &["show", "child", "--format", "json"]).contains(r#""subtask_of":["epic"]"#),
        "child mirrors subtask_of"
    );

    // depends_on stays the `deps` built-in — never duplicated as a field; its
    // inverse `blocks` surfaces on the depended-upon task.
    let aj = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        aj.contains(r#""deps":["b"]"#) && !aj.contains("depends_on"),
        "depends_on not duplicated: {aj}"
    );
    assert!(
        ta(&dir, &["show", "b", "--format", "json"]).contains(r#""blocks":["a"]"#),
        "inverse blocks surfaced on b"
    );
}

#[test]
fn dep_add_enforces_single_blocker_and_single_parent() {
    let dir = fresh_dir("subtask-constraints");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["a", "b", "e1", "e2", "c"] {
        ta(&dir, &["create", id]);
    }

    // At most one blocking relationship between two tasks.
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    let out = run(ta_bin(), &dir, &["dep", "add", "a", "has_subtask=b"]);
    assert!(
        !out.status.success(),
        "second blocking edge must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only one blocking relationship"),
        "error names the rule: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A task may have only one parent.
    ta(&dir, &["dep", "add", "e1", "has_subtask=c"]);
    let out = run(ta_bin(), &dir, &["dep", "add", "e2", "has_subtask=c"]);
    assert!(!out.status.success(), "second parent must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only one parent"),
        "error names the rule: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Same constraint when added from the child side via the inverse.
    let out = run(ta_bin(), &dir, &["dep", "add", "c", "subtask_of=e2"]);
    assert!(!out.status.success(), "inverse second-parent also rejected");

    // Re-adding the exact same edge is idempotent, not a conflict.
    assert!(
        run(ta_bin(), &dir, &["dep", "add", "e1", "has_subtask=c"])
            .status
            .success(),
        "idempotent re-add allowed"
    );
}

#[test]
fn human_output_is_uncolored_when_not_a_tty() {
    let dir = fresh_dir("no-color-pipe");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);

    // The test harness captures stdout (a pipe, not a TTY), so color auto-disables
    // — no ANSI escape bytes leak into output that might be piped or grepped.
    for args in [
        vec!["list"],
        vec!["list", "--format", "json"],
        vec!["show", "a"],
        vec!["show", "a", "--format", "jsonl"],
    ] {
        let out = ta(&dir, &args);
        assert!(
            !out.contains('\x1b'),
            "`ta {}` must not emit ANSI escapes off-TTY: {out:?}",
            args.join(" ")
        );
    }
    // `--no-color` is accepted (and a no-op here since already uncolored).
    assert!(!ta(&dir, &["list", "--no-color"]).contains('\x1b'));
}

#[test]
fn output_to_a_closed_pipe_does_not_panic() {
    let dir = fresh_dir("broken-pipe");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A field larger than the OS pipe buffer (64 KiB) so the write outlives a
    // reader that closes after a few bytes.
    let path = dir.join("big.txt");
    fs::write(&path, "x".repeat(300_000)).unwrap();
    ta(
        &dir,
        &["create", "big", &format!("notes=@{}", path.display())],
    );

    // `head -c 32` closes the pipe almost immediately; `ta` must terminate via
    // SIGPIPE, not a Rust panic + backtrace.
    let out = Command::new("sh")
        .arg("-c")
        .arg("ta show big --format json | head -c 32 >/dev/null")
        .current_dir(&dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("failed printing"),
        "ta panicked writing to a closed pipe: {stderr}"
    );
}
