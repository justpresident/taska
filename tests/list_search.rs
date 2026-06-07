mod common;
use common::*;

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

    // `deps=<x>` matches a target under ANY relationship type, info included —
    // the filter sees exactly what the deps column shows.
    ta(&dir, &["dep", "add", "web", "relates_to=db"]);
    let info = ta(&dir, &["list", "deps=db"]);
    assert!(lists_task(&info, "web"), "info edge matches deps=: {info}");
}

#[test]
fn relationship_names_and_computed_columns_filter() {
    let dir = fresh_dir("rel-filters");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "c1", "c2", "other"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(
        &dir,
        &["dep", "add", "epic", "has_subtask=c1", "has_subtask=c2"],
    );
    ta(&dir, &["dep", "add", "c2", "depends_on=c1"]);
    ta(&dir, &["dep", "add", "c1", "relates_to=other"]);

    // Forward: a declared type name filters by that type's edges.
    let dependents = ta(&dir, &["list", "depends_on=c1"]);
    assert!(
        lists_task(&dependents, "c2") && !lists_task(&dependents, "epic"),
        "only the depends_on edge matches: {dependents}"
    );

    // Inverse names resolve the reverse direction: children of the umbrella,
    // and what a task blocks — the queries from the motivating session.
    let children = ta(&dir, &["list", "subtask_of=epic", "--sort", "id"]);
    assert!(
        lists_task(&children, "c1")
            && lists_task(&children, "c2")
            && !lists_task(&children, "other"),
        "subtask_of finds the children: {children}"
    );
    let blockers = ta(&dir, &["list", "blocks=c2"]);
    assert!(
        lists_task(&blockers, "c1") && !lists_task(&blockers, "epic"),
        "blocks resolves what c2 depends on: {blockers}"
    );

    // Symmetric relates_to matches from both sides of the stored edge.
    assert!(lists_task(&ta(&dir, &["list", "relates_to=other"]), "c1"));
    assert!(lists_task(&ta(&dir, &["list", "relates_to=c1"]), "other"));

    // Regex operators compose with edge fields.
    let re = ta(&dir, &["list", "subtask_of~^ep"]);
    assert!(lists_task(&re, "c1") && lists_task(&re, "c2"), "{re}");

    // A computed column used ONLY as a filter is injected: c1 transitively
    // unblocks c2 and epic (no --columns/--sort needed to make this work).
    let unblocks = ta(&dir, &["list", "unblocks=2"]);
    assert!(
        lists_task(&unblocks, "c1") && !lists_task(&unblocks, "other"),
        "computed column filters without being displayed: {unblocks}"
    );

    // A malformed criterion or bad regex is rejected (non-zero exit).
    assert!(!run(ta_bin(), &dir, &["list", "nooperator"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["list", "title~["]).status.success());
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

    // JSON form is a single object with the computed fields (jsonl = one compact
    // line; `--format json` is the same data pretty-printed).
    let json = ta(&dir, &["status", "--format", "jsonl"]);
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
