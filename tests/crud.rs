mod common;
use common::names::*;
use common::*;
use taska::model::{DEPS_KEY, ID_KEY, STATUS_KEY};

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
    assert!(
        cfg.contains(&format!("status_field = \"{STATUS_KEY}\"")),
        "config: {cfg}"
    );

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
fn init_commits_store_and_leaves_a_clean_tree() {
    // A fresh init version-controls the store from the first command: .taska, the
    // .gitattributes merge-driver registration, and the agent file init wrote are
    // all committed, so `git status` is clean with nothing left for the user to add.
    let dir = fresh_dir("init-commit");
    init_repo(&dir);
    let out = ta(&dir, &["init"]);
    assert!(
        out.contains("Committed taska store"),
        "init reports the commit: {out}"
    );

    let status = git(&dir, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "clean tree after init: {status:?}"
    );

    let tracked = git(&dir, &["ls-files"]);
    for path in [
        ".taska/config.toml",
        ".taska/mutations.jsonl",
        ".gitattributes",
        "AGENTS.md",
    ] {
        assert!(
            tracked.lines().any(|l| l == path),
            "{path} is committed: {tracked}"
        );
    }
    let subject = git(&dir, &["log", "-1", "--pretty=%s"]);
    assert!(
        subject.contains("Initialize taska store"),
        "commit subject: {subject}"
    );
}

#[test]
fn reinit_makes_no_empty_commit() {
    // Re-running init when nothing changed must not pile up empty commits.
    let dir = fresh_dir("init-recommit");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let before = git(&dir, &["rev-list", "--count", "HEAD"]);
    ta(&dir, &["init"]);
    let after = git(&dir, &["rev-list", "--count", "HEAD"]);
    assert_eq!(
        before.trim(),
        after.trim(),
        "no new commit on a no-op re-init"
    );
    assert!(
        git(&dir, &["status", "--porcelain"]).trim().is_empty(),
        "still clean after re-init"
    );
}

#[test]
fn init_no_commit_leaves_the_store_uncommitted() {
    // --no-commit is the escape hatch for scripted setups: the store is created
    // but nothing is committed (or even staged), so the tree is dirty afterwards.
    let dir = fresh_dir("init-nocommit");
    init_repo(&dir);
    let out = ta(&dir, &["init", "--no-commit"]);
    assert!(!out.contains("Committed"), "no commit reported: {out}");
    assert_eq!(
        git(&dir, &["rev-list", "--all", "--count"]).trim(),
        "0",
        "no commits exist"
    );
    let status = git(&dir, &["status", "--porcelain"]);
    assert!(status.contains(".taska/"), "store left untracked: {status}");
}

#[test]
fn init_commit_skips_gitignored_paths() {
    // A repo may gitignore a file init would otherwise commit (here the agent
    // file). Explicitly `git add`-ing an ignored path errors and half-stages the
    // rest, so init must skip the ignored ones and still commit what it can -
    // never aborting the whole commit.
    let dir = fresh_dir("init-commit-ignored");
    init_repo(&dir);
    fs::write(dir.join(".gitignore"), "AGENTS.md\n").unwrap();

    let out = ta(&dir, &["init"]);
    assert!(
        out.contains("Committed taska store"),
        "still commits: {out}"
    );

    let tracked = git(&dir, &["ls-files"]);
    assert!(
        tracked.lines().any(|l| l == ".taska/config.toml"),
        "store committed: {tracked}"
    );
    assert!(
        tracked.lines().any(|l| l == ".gitattributes"),
        ".gitattributes committed: {tracked}"
    );
    assert!(
        !tracked.lines().any(|l| l == "AGENTS.md"),
        "gitignored agent file is not committed: {tracked}"
    );
}

#[test]
fn init_makes_no_commit_when_everything_is_gitignored() {
    // If every path init would commit is gitignored (here the whole store), there
    // is nothing to version-control: init must not error or make a partial commit.
    let dir = fresh_dir("init-all-ignored");
    init_repo(&dir);
    fs::write(
        dir.join(".gitignore"),
        ".taska/\n.gitattributes\nAGENTS.md\n",
    )
    .unwrap();

    let out = ta(&dir, &["init"]);
    assert!(!out.contains("Committed"), "nothing committed: {out}");
    assert_eq!(
        git(&dir, &["rev-list", "--all", "--count"]).trim(),
        "0",
        "no commit created"
    );
}

#[test]
fn init_outside_git_is_quiet_and_actionable() {
    // Deliberately NO `git init`: the store must still initialize, with ONE
    // actionable warning - not the raw `fatal: not in a git directory` noise
    // each `git config` child would leak if its stderr were inherited.
    let dir = fresh_dir("init-no-git");
    let out = run(ta_bin(), &dir, &["init"]);
    assert!(out.status.success(), "init works in a plain directory");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("fatal:"), "no raw git stderr: {stderr}");
    assert_eq!(
        stderr.matches("warning:").count(),
        1,
        "exactly one warning: {stderr}"
    );
    assert!(
        stderr.contains("git init"),
        "warning names the remedy: {stderr}"
    );

    // After `git init`, re-running `ta init` configures the drivers cleanly.
    init_repo(&dir);
    let out = run(ta_bin(), &dir, &["init"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("Configured git merge drivers"),
        "drivers configured once a repo exists: {stdout}"
    );
    assert!(!stderr.contains("warning:"), "no more warning: {stderr}");
}

#[test]
fn store_commands_auto_register_drivers_when_gitattributes_present() {
    // Fresh-clone simulation: the repo has `.gitattributes` committed (via `ta init`),
    // but a new clone lacks the per-clone driver *definitions* - a git merge would
    // text-merge the log. Because the committed `.gitattributes` already declares the
    // drivers, the next store command heals the clone SILENTLY rather than nagging:
    // the registered command is a taska-owned constant, so auto-registering it can't
    // run anything the repo chose.
    let dir = fresh_dir("scm-health");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Simulate a fresh clone by unsetting the drivers (`.gitattributes` stays
    // committed and present, but local config has no driver definitions).
    git(
        &dir,
        &["config", "--unset", "merge.taska-merge-driver.driver"],
    );
    git(
        &dir,
        &["config", "--unset", "merge.taska-merge-driver.name"],
    );
    git(
        &dir,
        &["config", "--unset", "merge.taska-baseline-keep-ours.driver"],
    );
    git(
        &dir,
        &["config", "--unset", "merge.taska-baseline-keep-ours.name"],
    );

    let pre = run(
        "git",
        &dir,
        &["config", "--get", "merge.taska-merge-driver.driver"],
    );
    assert!(
        !pre.status.success(),
        "drivers unregistered before any store command runs"
    );

    // ...the first store command registers them silently - no warning, no block.
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(out.status.success(), "health never blocks the command");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("warning:"),
        "auto-registered, not nagged: {stderr}"
    );
    let driver = git(
        &dir,
        &["config", "--get", "merge.taska-merge-driver.driver"],
    );
    assert!(
        driver.contains("ta git-merge"),
        "driver now registered: {driver}"
    );

    // A deleted .gitattributes is the committed half of the setup - we won't
    // silently rewrite a tracked file, so the warning resurfaces with its remedy.
    fs::remove_file(dir.join(".gitattributes")).unwrap();
    let out = run(ta_bin(), &dir, &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".gitattributes") && stderr.contains("`ta init`"),
        "attrs warning: {stderr}"
    );
}

#[test]
fn plain_dir_stays_quiet_and_mercurial_warns_unsupported() {
    // No SCM at all: deliberate plain-dir use - store commands don't nag
    // (`ta init` already warned once at setup time).
    let dir = fresh_dir("scm-none");
    run(ta_bin(), &dir, &["init"]);
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning:"),
        "no SCM -> no nagging"
    );

    // Mercurial detection is a directory stat - no hg binary needed.
    fs::create_dir(dir.join(".hg")).unwrap();
    let out = run(ta_bin(), &dir, &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mercurial") && stderr.contains("only git"),
        "hg warning: {stderr}"
    );
}

#[test]
fn nested_store_inside_a_repo_is_supported_by_walk_up_detection() {
    // The store and the SCM root need not share a directory: here `.taska`
    // predates the repo and lives two levels below it. Health detection must
    // walk UP from the store's parent rather than expect `.git` beside it.
    let dir = fresh_dir("scm-nested");
    let sub = dir.join("crates").join("app");
    fs::create_dir_all(&sub).unwrap();
    run(ta_bin(), &sub, &["init"]); // plain-dir store first...
    init_repo(&dir); // ...the repo appears ABOVE it later

    // Re-running `ta init` from the store's dir reuses the nested store and
    // registers the drivers (git config resolves from any dir in the repo).
    let out = run(ta_bin(), &sub, &["init"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already present"),
        "nested store reused, not shadowed"
    );
    let out = run(ta_bin(), &sub, &["list"]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning:"),
        "registered nested store is healthy"
    );

    // Unregister a driver (what a fresh clone of this layout looks like): the
    // next store command silently re-registers it via walk-up, even though `.git`
    // is two levels above the store (git config resolves from `sub`).
    git(
        &dir,
        &["config", "--unset", "merge.taska-merge-driver.driver"],
    );
    let out = run(ta_bin(), &sub, &["list"]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning:"),
        "nested store auto-heals via walk-up: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let driver = git(
        &dir,
        &["config", "--get", "merge.taska-merge-driver.driver"],
    );
    assert!(
        driver.contains("ta git-merge"),
        "driver re-registered via walk-up: {driver}"
    );
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
fn init_from_subdirectory_creates_a_new_store_at_the_scm_root() {
    // No store anywhere yet: `ta init` from a repo SUBDIRECTORY must place the
    // new .taska at the SCM root - committed there it travels with the repo -
    // not at the invocation dir.
    let dir = fresh_dir("init-at-root");
    init_repo(&dir);
    let sub = dir.join("src").join("deep");
    fs::create_dir_all(&sub).unwrap();

    let out = ta(&sub, &["init"]);
    assert!(out.contains("Initialized taska store"), "got: {out}");
    assert!(dir.join(".taska").is_dir(), "store at the SCM root");
    assert!(
        !sub.join(".taska").exists(),
        "no store at the invocation dir"
    );
    assert!(
        fs::read_to_string(dir.join(".gitattributes"))
            .unwrap()
            .contains("merge=taska-merge-driver"),
        ".gitattributes at the root too"
    );

    // The store is immediately usable from the subdir (walk-up discovery), and
    // healthy (drivers registered at init time - no warning).
    ta(&sub, &["create", "t1", "title=x"]);
    assert!(lists_task(&ta(&dir, &["list"]), "t1"), "usable from root");
    let out = run(ta_bin(), &sub, &["list"]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning:"),
        "healthy from the subdir"
    );
}

#[test]
fn crud_search_and_ready_workflow() {
    let dir = fresh_dir("crud");
    init_renamed_open(&dir);

    ta(&dir, &["create", "db", &format!("{STATUS_FIELD}=closed")]);
    // `priority` is a field no task uses yet, so introducing it needs --new-field
    // (db is the first task and seeds the vocabulary; api comes after).
    ta(
        &dir,
        &[
            "create",
            "api",
            "--new-field",
            &format!("{STATUS_FIELD}=open"),
            "priority=3",
        ],
    );
    ta(&dir, &["dep", "add", "api", &format!("{BLOCKER}=db")]);

    // The human table lists ids; `--full --format json` exposes every field -
    // priority coerced to a JSON number, and deps as the typed map.
    assert!(
        lists_task(&ta(&dir, &["list"]), "api"),
        "api should be listed"
    );
    let json = ta(&dir, &["list", "--full", "--format", "json"]);
    assert!(json.contains(r#""priority":3"#), "json: {json}");
    assert!(
        json.contains(&format!(r#""{DEPS_KEY}":{{"{BLOCKER}":["db"]}}"#)),
        "json: {json}"
    );

    let search = ta(&dir, &["list", &format!("{STATUS_FIELD}=open")]);
    assert!(lists_task(&search, "api"), "search: {search}");
    assert!(!lists_task(&search, "db"), "db is done, not open: {search}");

    // db is done, so api's only dependency is satisfied -> api is ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&ready, "api"), "ready: {ready}");

    // Once api is done too, nothing is ready.
    ta(&dir, &["update", "api", &format!("{STATUS_FIELD}=closed")]);
    assert_eq!(ta(&dir, &["list", "--ready"]).trim(), "(nothing ready)");

    ta(&dir, &["delete", "db"]);
    assert!(!lists_task(&ta(&dir, &["list"]), "db"), "db should be gone");
}

#[test]
fn update_with_no_fields_fails_and_appends_nothing() {
    let dir = fresh_dir("empty-update");
    init_renamed_open(&dir);
    ta(&dir, &["create", "api", &format!("{STATUS_FIELD}=open")]);

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
fn show_displays_full_task_and_rejects_unknown_id() {
    let dir = fresh_dir("show");
    init_renamed_open(&dir);
    ta(
        &dir,
        &[
            "create",
            "a",
            "title=Alpha",
            &format!("{STATUS_FIELD}=open"),
            "priority=3",
        ],
    );
    ta(&dir, &["create", "dep"]);
    ta(&dir, &["dep", "add", "a", &format!("{BLOCKER}=dep")]);

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
        json.contains(&format!(r#""{STATUS_FIELD}":"open""#)),
        "state in show json: {json}"
    );

    // An explicit --columns still restricts.
    let cols = ta(
        &dir,
        &[
            "show",
            "a",
            "--columns",
            &format!("{ID_KEY},{STATUS_FIELD}"),
            "--format",
            "json",
        ],
    );
    assert!(
        cols.contains(&format!(r#""{STATUS_FIELD}":"open""#)) && !cols.contains("priority"),
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
fn show_accepts_multiple_ids_deduplicated() {
    let dir = fresh_dir("show-multi");
    init_renamed_open(&dir);
    for id in ["a", "b", "c"] {
        ta(&dir, &["create", id]);
    }

    // Two ids -> one vertical record each, separated by a blank line.
    let human = ta(&dir, &["show", "a", "b"]);
    let id_lines = human.lines().filter(|l| l.starts_with("id:")).count();
    assert_eq!(id_lines, 2, "one record per id: {human}");
    assert!(
        human.contains("\n\n"),
        "records separated by a blank line: {human}"
    );

    // Duplicates collapse and first-occurrence order is preserved: `a b a` -> [a, b].
    let json = ta(
        &dir,
        &[
            "show",
            "a",
            "b",
            "a",
            "--columns",
            ID_KEY,
            "--format",
            "json",
        ],
    );
    let (a, b) = (json.find(r#""a""#), json.find(r#""b""#));
    assert!(
        a < b && a.is_some(),
        "first-occurrence order a before b: {json}"
    );
    assert_eq!(
        json.matches(r#""a""#).count(),
        1,
        "duplicate id shown once: {json}"
    );

    // An unknown id alongside known ones still exits non-zero.
    let out = run(ta_bin(), &dir, &["show", "a", "missing"]);
    assert!(
        !out.status.success(),
        "an unknown id among knowns must exit non-zero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
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
        ta(&dir, &["show", "a", "--format", "json"]).contains(&format!(r#""{STATUS_KEY}":"todo""#)),
        "bare create defaults status to todo"
    );

    // An explicit status still wins over the default.
    ta(&dir, &["create", "b", &format!("{STATUS_KEY}=open")]);
    assert!(
        ta(&dir, &["show", "b", "--format", "json"]).contains(&format!(r#""{STATUS_KEY}":"open""#)),
        "explicit status overrides the default"
    );

    // The default is configurable.
    ta(
        &dir,
        &["config", "set", "workflow.default_status", "backlog"],
    );
    ta(&dir, &["create", "c"]);
    assert!(
        ta(&dir, &["show", "c", "--format", "json"])
            .contains(&format!(r#""{STATUS_KEY}":"backlog""#)),
        "configured default status is applied"
    );

    // Setting it empty restores statusless creation.
    ta(&dir, &["config", "set", "workflow.default_status", ""]);
    ta(&dir, &["create", "d"]);
    assert!(
        !ta(&dir, &["show", "d", "--format", "json"]).contains(STATUS_KEY),
        "empty default_status leaves the task statusless"
    );
}

#[test]
fn null_value_unsets_a_field() {
    let dir = fresh_dir("null-unset");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "x", "owner=bob", &format!("{STATUS_FIELD}=open")],
    );
    // Setting a field to null removes it (the field-unset convention).
    ta(&dir, &["update", "x", "owner=null"]);
    let json = ta(&dir, &["show", "x", "--format", "json"]);
    assert!(
        json.contains(&format!("\"{STATUS_FIELD}\":\"open\"")),
        "state kept: {json}"
    );
    assert!(!json.contains("owner"), "owner unset by null: {json}");
}

#[test]
fn empty_value_unsets_an_optional_field_like_null() {
    // Design A: `field=` (empty) clears an optional field just like `field=null`,
    // so callers never need the raw `null` - and the empty-string FILTER then
    // finds the cleared task, keeping select and update consistent.
    let dir = fresh_dir("empty-unset");
    init_renamed_open(&dir);
    ta(
        &dir,
        &["create", "x", "owner=bob", &format!("{STATUS_FIELD}=open")],
    );
    // Clearing with an empty value removes the (optional) field, same as null.
    ta(&dir, &["update", "x", "owner="]);
    let json = ta(&dir, &["show", "x", "--format", "json"]);
    assert!(
        !json.contains("owner"),
        "owner unset by empty value: {json}"
    );

    // Select agrees with update: the empty-string filter matches the cleared task.
    let listed = ta(&dir, &["list", "owner=", "--columns", "id"]);
    assert!(
        listed.lines().any(|l| l.trim() == "x"),
        "empty filter matches the unset field: {listed}"
    );
}

#[test]
fn conditional_update_claims_atomically_and_exits_3_on_loss() {
    // `--if` is a compare-and-swap: two agents race to claim one task, exactly one
    // wins, and the loser fails with exit code 3 and a clear message - even though
    // its intended end-state matches (it must not read as a silent no-op).
    let dir = fresh_dir("claim");
    init_repo(&dir);
    init_renamed_open(&dir);
    ta(&dir, &["create", "t", &format!("{STATUS_FIELD}=todo")]);

    let claim = |d: &std::path::Path| {
        run(
            ta_bin(),
            d,
            &[
                "update",
                "t",
                &format!("{STATUS_FIELD}=in_progress"),
                "--if",
                &format!("{STATUS_FIELD}=todo"),
            ],
        )
    };

    // First agent claims while the task is `todo`: succeeds.
    let first = claim(&dir);
    assert!(
        first.status.success(),
        "first claim wins: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Second agent claims the same target: loses - exit 3, clear message.
    let second = claim(&dir);
    assert_eq!(second.status.code(), Some(3), "lost claim exits 3");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("does not meet"),
        "clear failure message: {second:?}"
    );

    // The lost claim changed nothing.
    let shown = ta(&dir, &["show", "t", "--format", "json"]);
    assert!(
        shown.contains(&format!("\"{STATUS_FIELD}\":\"in_progress\"")),
        "state unchanged by the lost claim: {shown}"
    );

    // A conditional delete honors the guard the same way.
    let bad = run(
        ta_bin(),
        &dir,
        &["delete", "t", "--if", &format!("{STATUS_FIELD}=todo")],
    );
    assert_eq!(bad.status.code(), Some(3), "wrong-condition delete exits 3");
    ta(
        &dir,
        &[
            "delete",
            "t",
            "--if",
            &format!("{STATUS_FIELD}=in_progress"),
        ],
    );
    let gone = run(ta_bin(), &dir, &["show", "t"]);
    assert!(
        !gone.status.success(),
        "conditional delete removed the task"
    );
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

    // `@file` reads the value verbatim - no shell expansion, no quoting needed.
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

    // `@-` reads the value from stdin. `summary` is a new field (t1 used `notes`),
    // so it needs --new-field.
    let mut child = Command::new(ta_bin())
        .args(["update", "t1", "--new-field", "summary=@-"])
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

    // `@@x` is a literal `@x`, not a file read. `owner` is a new field name.
    ta(&dir, &["create", "t2", "--new-field", "owner=@@alice"]);
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
    // First use of `log` introduces it - needs --new-field; later appends don't.
    ta(&dir, &["update", "task", "--new-field", "log+=started"]);
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
    init_renamed_open(&dir);
    ta(&dir, &["create", "t", &format!("{STATUS_FIELD}=open")]);
    // One command: set `state` (=) and append to `log` (+=). `log` is new here.
    ta(
        &dir,
        &[
            "update",
            "t",
            "--new-field",
            &format!("{STATUS_FIELD}=closed"),
            "log+=did the thing",
        ],
    );
    let json = ta(&dir, &["show", "t", "--format", "json"]);
    assert!(
        json.contains(&format!(r#""{STATUS_FIELD}":"closed""#))
            && json.contains(r#""log":"did the thing""#),
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
fn dash_c_drives_a_store_in_another_directory() {
    // A store in one directory...
    let store = fresh_dir("dashc-store");
    init_repo(&store);
    ta(&store, &["init"]);
    ta(&store, &["create", "alpha", "title=Alpha"]);

    // ...driven from an unrelated working directory via `-C` (git's semantics).
    let elsewhere = fresh_dir("dashc-elsewhere");
    let store_path = store.to_str().unwrap();

    // Read: `ta -C <store> list` from `elsewhere` sees the store's task.
    let listed = run(ta_bin(), &elsewhere, &["-C", store_path, "list"]);
    assert!(
        listed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        lists_task(&stdout, "alpha"),
        "-C list sees the store: {stdout}"
    );

    // Write: a create via `-C` lands in the store, and creates NO store in the cwd.
    let created = run(
        ta_bin(),
        &elsewhere,
        &["-C", store_path, "create", "beta", "title=Beta"],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(
        !elsewhere.join(".taska").exists(),
        "no store is created in the -C caller's own cwd"
    );
    assert!(
        lists_task(&ta(&store, &["list"]), "beta"),
        "the write landed in the -C store"
    );

    // A non-existent -C directory is a clean error that names the flag.
    let bad = run(ta_bin(), &elsewhere, &["-C", "/no/such/taska/dir", "list"]);
    assert!(!bad.status.success(), "bad -C dir must fail");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("-C"),
        "error names the flag: {}",
        String::from_utf8_lossy(&bad.stderr)
    );
}
