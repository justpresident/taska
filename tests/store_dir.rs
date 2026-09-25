//! End-to-end tests for `[store] dir` - keeping the log and baseline somewhere
//! other than beside `config.toml`: relative, `$VAR`, and `~` locations, the
//! unset-variable and missing-directory refusals, and the SCM wiring (merge
//! drivers, `undo`'s committed-history check, `init`'s commit) following the
//! data to its new home.
mod common;
use common::*;

use taska::model::STATUS_KEY;

/// A git project at `<root>/proj` holding a default-layout store (`--no-commit`,
/// so the commit graph is the test's). Returns `(root, proj)`.
fn project(name: &str) -> (PathBuf, PathBuf) {
    let root = fresh_dir(name);
    let proj = root.join("proj");
    fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    ta(&proj, &["init", "--no-commit"]);
    (root, proj)
}

/// Point the store at `value` through the real `ta config set` path.
fn set_store_dir(proj: &Path, value: &str) {
    ta(
        proj,
        &["config", "set", "store.dir", &format!("\"{value}\"")],
    );
}

/// Run `ta` in `dir` with `env` applied on top of the harness environment:
/// `Some` sets a variable, `None` removes it.
fn ta_env(dir: &Path, args: &[&str], env: &[(&str, Option<&Path>)]) -> Output {
    let mut cmd = Command::new(ta_bin());
    cmd.args(args).current_dir(dir).env("PATH", path_with_bin());
    for (name, value) in env {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    cmd.output().unwrap()
}

fn succeeded(out: &Output) -> String {
    assert!(
        out.status.success(),
        "expected success:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn failed(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected failure, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The paths a commit touched.
fn committed_files(repo: &Path) -> String {
    git(repo, &["show", "--name-only", "--format=", "HEAD"])
}

#[test]
fn relative_store_dir_holds_the_log_and_baseline() {
    let (_root, proj) = project("store-dir-relative");
    set_store_dir(&proj, "../tasks");

    // The new home doesn't exist yet: a data command must say so (and how to fix
    // it) rather than read as an empty store.
    let err = failed(&run(ta_bin(), &proj, &["list"]));
    assert!(
        err.contains("does not exist") && err.contains("ta init"),
        "{err}"
    );

    let out = ta(&proj, &["init", "--no-commit"]);
    assert!(out.contains("lives in"), "init names the data dir: {out}");
    ta(&proj, &["create", "t"]);

    let data = proj.join("tasks");
    assert_eq!(
        rows(&data.join("mutations.jsonl")),
        1,
        "the event lands there"
    );
    assert!(data.join("baseline.jsonl").is_file());
    assert_eq!(
        rows(&proj.join(".taska/mutations.jsonl")),
        0,
        "the log beside config.toml is no longer written"
    );

    // Discovery still goes through `.taska`, from anywhere in the project.
    let sub = proj.join("src");
    fs::create_dir_all(&sub).unwrap();
    assert!(lists_task(&ta(&sub, &["list"]), "t"));
    // `-C` resolves the same store (and data) from outside the project.
    let outside = ta(&proj.join(".."), &["-C", "proj", "list"]);
    assert!(lists_task(&outside, "t"), "{outside}");
}

#[test]
fn env_var_store_dir_requires_the_variable() {
    const VAR: &str = "TASKA_TEST_STORE_ROOT";
    let (root, proj) = project("store-dir-env");
    let data_root = root.join("elsewhere");
    fs::create_dir_all(&data_root).unwrap();
    set_store_dir(&proj, &format!("${VAR}/proj"));

    let set = [(VAR, Some(data_root.as_path()))];
    succeeded(&ta_env(&proj, &["init", "--no-commit"], &set));
    succeeded(&ta_env(&proj, &["create", "t"], &set));
    assert_eq!(rows(&data_root.join("proj/mutations.jsonl")), 1);
    assert!(lists_task(&succeeded(&ta_env(&proj, &["list"], &set)), "t"));

    // Unset or empty, every command that touches the data refuses, naming the
    // variable - an empty `$VAR/proj` must not quietly become `/proj`.
    let empty = Path::new("");
    for env in [[(VAR, None)], [(VAR, Some(empty))]] {
        for args in [&["list"][..], &["create", "u"], &["status"], &["resolve"]] {
            let err = failed(&ta_env(&proj, args, &env));
            assert!(err.contains(&format!("${VAR}")), "`ta {args:?}`: {err}");
        }
    }

    // `ta config` still works - it's how the setting gets fixed.
    let unset = [(VAR, None)];
    let got = succeeded(&ta_env(&proj, &["config", "get", "store.dir"], &unset));
    assert!(got.contains(&format!("${VAR}/proj")), "{got}");
    let moved = succeeded(&ta_env(
        &proj,
        &["config", "set", "store.dir", "\".\""],
        &unset,
    ));
    assert!(moved.contains('.'), "{moved}");
    succeeded(&ta_env(&proj, &["list"], &unset));
}

#[test]
fn tilde_store_dir_is_home() {
    let (root, proj) = project("store-dir-tilde");
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    set_store_dir(&proj, "~/tasks");

    let with_home = [("HOME", Some(home.as_path()))];
    succeeded(&ta_env(&proj, &["init", "--no-commit"], &with_home));
    succeeded(&ta_env(&proj, &["create", "t"], &with_home));
    assert_eq!(rows(&home.join("tasks/mutations.jsonl")), 1);

    // `~` needs HOME exactly as `$HOME` would.
    let err = failed(&ta_env(&proj, &["list"], &[("HOME", None)]));
    assert!(err.contains("$HOME"), "{err}");
}

#[test]
fn malformed_store_dir_is_rejected() {
    let (_root, proj) = project("store-dir-malformed");
    let config = proj.join(".taska/config.toml");
    let before = fs::read_to_string(&config).unwrap();

    for bad in ["~user/x", "a/$/b", "${X", "$1", ""] {
        let err = failed(&run(
            ta_bin(),
            &proj,
            &["config", "set", "store.dir", &format!("\"{bad}\"")],
        ));
        assert!(err.contains("store.dir"), "`{bad}`: {err}");
    }
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        before,
        "never written"
    );

    // Hand-edited in anyway, the next store command reports it.
    fs::write(&config, "[store]\ndir = \"~user/tasks\"\n").unwrap();
    let err = failed(&run(ta_bin(), &proj, &["list"]));
    assert!(err.contains("`~user`"), "{err}");
}

#[test]
fn undo_respects_history_committed_in_a_relocated_dir() {
    // With the data outside `.taska`, the committed log is `tasks/mutations.jsonl`.
    // Reading the committed count from `.taska/` would see 0 committed events,
    // and undo would TRUNCATE shared history instead of compensating.
    let (_root, proj) = project("store-dir-undo");
    set_store_dir(&proj, "../tasks");
    ta(&proj, &["init", "--no-commit"]);
    ta(&proj, &["create", "a", &format!("{STATUS_KEY}=open")]);
    ta(&proj, &["update", "a", &format!("{STATUS_KEY}=closed")]);
    git(&proj, &["add", "-A"]);
    git(&proj, &["commit", "-qm", "tasks"]);

    let log = proj.join("tasks/mutations.jsonl");
    let before = rows(&log);
    ta(&proj, &["undo", "--force"]);
    assert_eq!(
        rows(&log),
        before + 1,
        "a committed event is compensated (log grows), never truncated"
    );
    let json = ta(&proj, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(&format!(r#""{STATUS_KEY}":"open""#)),
        "{json}"
    );
}

#[test]
fn init_wires_and_commits_a_relocated_dir_in_the_project() {
    let (_root, proj) = project("store-dir-init");
    git(&proj, &["add", "-A"]);
    git(&proj, &["commit", "-qm", "base"]);
    set_store_dir(&proj, "../tasks");
    ta(&proj, &["init"]);

    // A relocated data dir carries its own, root-anchored merge-driver entries.
    let attrs = fs::read_to_string(proj.join("tasks/.gitattributes")).unwrap();
    assert!(
        attrs.contains("/mutations.jsonl merge=taska-merge-driver")
            && attrs.contains("/baseline.jsonl merge=taska-baseline-keep-ours"),
        "{attrs}"
    );

    // The config change and the data files are committed together.
    let files = committed_files(&proj);
    for path in [
        ".taska/config.toml",
        "tasks/mutations.jsonl",
        "tasks/baseline.jsonl",
        "tasks/.gitignore",
        "tasks/.gitattributes",
    ] {
        assert!(files.lines().any(|l| l == path), "{path} in:\n{files}");
    }

    // The health check agrees the relocated log is protected.
    let out = run(ta_bin(), &proj, &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && !stderr.contains("warning"),
        "{stderr}"
    );
}

#[test]
fn data_in_another_repo_is_wired_there_and_not_committed_here() {
    let (root, proj) = project("store-dir-other-repo");
    git(&proj, &["add", "-A"]);
    git(&proj, &["commit", "-qm", "base"]);
    let tasks_repo = root.join("tasks-repo");
    fs::create_dir_all(&tasks_repo).unwrap();
    init_repo(&tasks_repo);
    set_store_dir(&proj, "../../tasks-repo/proj");
    ta(&proj, &["init"]);

    // The drivers are wired in the repo that versions the data...
    let attrs = fs::read_to_string(tasks_repo.join("proj/.gitattributes")).unwrap();
    assert!(
        attrs.contains("/mutations.jsonl merge=taska-merge-driver"),
        "{attrs}"
    );
    let driver = git(&tasks_repo, &["config", "merge.taska-merge-driver.driver"]);
    assert!(driver.contains("ta git-merge"), "{driver}");

    // ...while the project commits only its own config - naming a path from
    // another checkout would have failed the whole commit.
    let files = committed_files(&proj);
    assert!(files.contains(".taska/config.toml"), "{files}");
    assert!(!files.contains("mutations.jsonl"), "{files}");

    // `undo` reads committed history from the data's repo.
    ta(&proj, &["create", "a", &format!("{STATUS_KEY}=open")]);
    ta(&proj, &["update", "a", &format!("{STATUS_KEY}=closed")]);
    git(&tasks_repo, &["add", "-A"]);
    git(&tasks_repo, &["commit", "-qm", "tasks"]);
    let log = tasks_repo.join("proj/mutations.jsonl");
    let before = rows(&log);
    ta(&proj, &["undo", "--force"]);
    assert_eq!(rows(&log), before + 1, "committed there, so compensated");
}

/// Two branches that set the same field of `t` to different values, then a merge
/// of `feature` into `main`. Returns the merge's output.
fn conflicting_merge(proj: &Path) -> Output {
    ta(proj, &["create", "t", &format!("{STATUS_KEY}=open")]);
    git(proj, &["add", "-A"]);
    git(proj, &["commit", "-qm", "init"]);
    git(proj, &["branch", "feature"]);
    ta(proj, &["update", "t", &format!("{STATUS_KEY}=main")]);
    git(proj, &["commit", "-aqm", "main edit"]);
    git(proj, &["checkout", "-q", "feature"]);
    ta(proj, &["update", "t", &format!("{STATUS_KEY}=feature")]);
    git(proj, &["commit", "-aqm", "feature edit"]);
    git(proj, &["checkout", "-q", "main"]);
    run("git", proj, &["merge", "feature", "-m", "merge"])
}

#[test]
fn surfaced_merge_conflict_is_recorded_beside_the_relocated_log() {
    let (_root, proj) = project("store-dir-merge-surface");
    set_store_dir(&proj, "../tasks");
    ta(&proj, &["init", "--no-commit"]);

    let merge = conflicting_merge(&proj);
    assert!(!merge.status.success(), "default `surface` fails the merge");
    let marker = proj.join("tasks/merge-conflict.json");
    assert!(marker.exists(), "marker beside the merged log");
    assert!(!proj.join(".taska/merge-conflict.json").exists());
    // The data dir's .gitignore keeps the marker out of `git add -A`.
    let status = git(&proj, &["status", "--porcelain", "--ignored"]);
    assert!(status.contains("!! tasks/merge-conflict.json"), "{status}");

    let resolved = ta(&proj, &["resolve"]);
    assert!(resolved.contains("conflict"), "{resolved}");
    assert!(!marker.exists(), "resolve clears it");
}

#[test]
fn merge_driver_applies_the_policy_of_the_store_that_relocated_the_log() {
    // The data dir holds no config.toml; the driver must find the store whose
    // `[store] dir` points at it. Proof: `theirs` resolves a conflict that the
    // default `surface` would fail.
    let (_root, proj) = project("store-dir-merge-policy");
    set_store_dir(&proj, "../tasks");
    ta(&proj, &["config", "set", "merge.on_conflict", "theirs"]);
    ta(&proj, &["init", "--no-commit"]);

    let merge = conflicting_merge(&proj);
    assert!(
        merge.status.success(),
        "{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    let json = ta(&proj, &["show", "t", "--format", "json"]);
    assert!(
        json.contains(&format!(r#""{STATUS_KEY}":"feature""#)),
        "{json}"
    );
}

#[test]
fn init_warns_about_data_left_beside_the_config() {
    let (_root, proj) = project("store-dir-stranded");
    ta(&proj, &["create", "t"]);
    set_store_dir(&proj, "../tasks");

    let out = run(ta_bin(), &proj, &["init", "--no-commit"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("mutations.jsonl") && stderr.contains("store.dir points elsewhere"),
        "{stderr}"
    );
    assert!(!lists_task(&ta(&proj, &["list"]), "t"), "not read any more");

    // Moving the log to the new home brings the task back and quiets init.
    fs::rename(
        proj.join(".taska/mutations.jsonl"),
        proj.join("tasks/mutations.jsonl"),
    )
    .unwrap();
    let out = run(ta_bin(), &proj, &["init", "--no-commit"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("store.dir points elsewhere"));
    assert!(lists_task(&ta(&proj, &["list"]), "t"));
}
