mod common;
use common::*;

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
fn full_flag_disables_truncation_in_human_output() {
    let dir = fresh_dir("full-no-truncate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A title well past the default title cap (80), so it would otherwise be cut.
    let long = "This title is considerably longer than eighty characters in total, well past the per-column title override, so it still gets truncated by default";
    ta(&dir, &["create", "a", &format!("title={long}")]);

    // Default human view truncates with an ellipsis and drops the tail.
    let default = ta(&dir, &["list"]);
    assert!(default.contains('\u{2026}'), "default truncates: {default}");
    assert!(!default.contains(long), "default drops the tail: {default}");

    // --full prints the whole value, no ellipsis.
    let full = ta(&dir, &["list", "--full"]);
    assert!(full.contains(long), "--full prints untruncated: {full}");
    assert!(
        !full.contains('\u{2026}'),
        "--full adds no ellipsis: {full}"
    );
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
        human.contains('\u{2026}'),
        "ellipsis from the notes column: {human}"
    );

    // --full still ignores the per-column map and prints everything.
    let full = ta(&dir, &["list", "--full"]);
    assert!(
        full.contains(long_notes),
        "--full prints notes whole: {full}"
    );
    assert!(
        !full.contains('\u{2026}'),
        "--full adds no ellipsis: {full}"
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
        human.contains("ThisIsA\u{2026}"),
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
fn output_commands_are_format_and_color_consistent() {
    let dir = fresh_dir("output-consistency");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["create", "b", "status=open"]);
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);

    // Every command on the shared output pipeline must honor `--format` and color
    // identically: human is escape-free off-TTY, json/jsonl parse and never color.
    // (dep tree/plan/cycles join this list as they're migrated.)
    let commands: Vec<Vec<&str>> = vec![
        vec!["list"],
        vec!["show", "a"],
        vec!["status"],
        vec!["prime"],
        vec!["dep", "cycles"],
        vec!["dep", "plan", "a"],
        vec!["dep", "tree"],
    ];
    for base in &commands {
        let label = base.join(" ");
        let no_esc = |out: &str, what: &str| {
            assert!(
                !out.contains('\x1b'),
                "`ta {label}` {what} must be escape-free: {out:?}"
            );
        };

        no_esc(&ta(&dir, base), "human");

        // --no-color is accepted everywhere.
        let mut nc = base.clone();
        nc.push("--no-color");
        assert!(
            run(ta_bin(), &dir, &nc).status.success(),
            "`ta {label} --no-color` ok"
        );

        // --format json: one valid JSON value, never colored.
        let mut j = base.clone();
        j.extend(["--format", "json"]);
        let json = ta(&dir, &j);
        no_esc(&json, "json");
        serde_json::from_str::<serde_json::Value>(&json)
            .unwrap_or_else(|e| panic!("`ta {label} --format json` invalid: {e}: {json}"));

        // --format jsonl: each non-empty line is valid JSON, never colored.
        let mut jl = base.clone();
        jl.extend(["--format", "jsonl"]);
        let jsonl = ta(&dir, &jl);
        no_esc(&jsonl, "jsonl");
        for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("`ta {label} --format jsonl` bad line: {e}: {line}"));
        }
    }
}

#[test]
fn human_output_is_uncolored_when_not_a_tty() {
    let dir = fresh_dir("no-color-pipe");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=open"]);

    // The test harness captures stdout (a pipe, not a TTY), so color auto-disables
    // - no ANSI escape bytes leak into output that might be piped or grepped.
    for args in [
        vec!["list"],
        vec!["list", "--format", "json"],
        vec!["show", "a"],
        vec!["show", "a", "--format", "jsonl"],
        vec!["dep", "tree"],
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
fn layout_flag_and_config_switch_table_and_record() {
    let dir = fresh_dir("layout");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "title=Alpha", "status=open"]);
    ta(&dir, &["create", "b", "title=Beta", "status=open"]);

    // `list` defaults to the aligned table (uppercase headers, no record labels).
    let table = ta(&dir, &["list"]);
    assert!(
        table.contains("ID") && table.contains("STATUS"),
        "table header: {table}"
    );
    assert!(!table.contains("id:"), "not records: {table}");

    // `--layout list` switches to vertical records (one per task).
    let recs = ta(&dir, &["list", "--layout", "list"]);
    assert!(
        recs.lines().any(|l| l.starts_with("id:")),
        "record labels: {recs}"
    );
    assert!(
        !recs.contains("STATUS"),
        "no table header in records: {recs}"
    );

    // `show` defaults to a record; `--layout table` switches to a table.
    assert!(
        ta(&dir, &["show", "a"])
            .lines()
            .any(|l| l.starts_with("id:")),
        "show defaults to record"
    );
    assert!(
        ta(&dir, &["show", "a", "--layout", "table"]).contains("ID"),
        "show --layout table"
    );

    // The per-command default is configurable: flip list to records.
    ta(&dir, &["config", "set", "display.list_layout", "list"]);
    assert!(
        ta(&dir, &["list"]).lines().any(|l| l.starts_with("id:")),
        "config list_layout=list makes `list` render records"
    );
    // An invalid layout value is rejected.
    assert!(
        !run(
            ta_bin(),
            &dir,
            &["config", "set", "display.show_layout", "bogus"]
        )
        .status
        .success(),
        "invalid layout rejected"
    );
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

#[test]
fn deps_column_groups_every_relationship_type() {
    let dir = fresh_dir("deps-groups");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["api", "db", "web", "infra"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(
        &dir,
        &["dep", "add", "api", "depends_on=db", "depends_on=web"],
    );
    ta(&dir, &["dep", "add", "api", "relates_to=infra"]);

    // The human table cell shows EVERY edge as labeled type groups joined by
    // `;` - gating and informational types alike (styling is TTY-only).
    let table = ta(&dir, &["list", "--full"]);
    assert!(
        table.contains("depends_on: db, web; relates_to: infra"),
        "grouped cell: {table}"
    );

    // The record view (`show`) puts one type group per line under `deps:`,
    // continuation lines indented.
    let rec = ta(&dir, &["show", "api"]);
    assert!(
        rec.lines()
            .any(|l| l.starts_with("deps:") && l.ends_with("depends_on: db, web")),
        "first group on the label line: {rec}"
    );
    assert!(
        rec.lines()
            .any(|l| l.starts_with(' ') && l.trim() == "relates_to: infra"),
        "next group indented: {rec}"
    );

    // json/jsonl carry the typed map itself; an edge-free task is `{}`.
    let json = ta(&dir, &["list", "--format", "jsonl"]);
    assert!(
        json.contains(r#""deps":{"depends_on":["db","web"],"relates_to":["infra"]}"#),
        "typed map in jsonl: {json}"
    );
    assert!(json.contains(r#""deps":{}"#), "edge-free task: {json}");
}
