//! Shared harness for the end-to-end test binaries.
//!
//! Each `tests/<theme>.rs` drives the real compiled `ta` binary (path from
//! `CARGO_BIN_EXE_ta`) against throwaway git repos under the *system* temp dir,
//! outside the project tree, so `ta`'s walk-up store discovery can't climb
//! into the repo's own `.taska`. The merge-driver tests prepend the binary's
//! directory to `PATH`. Start each theme file with `mod common; use common::*;`
//! (a `tests/common.rs` would wrongly be its own empty test binary; the
//! subdirectory module is not compiled as one).
// Each theme file is its own test binary that includes this module, so not every
// helper or re-export is used by every binary.
#![allow(dead_code, unused_imports)]

pub use std::ffi::OsString;
pub use std::fs;
pub use std::io::Write;
pub use std::path::{Path, PathBuf};
pub use std::process::{Command, Output, Stdio};

pub fn ta_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ta")
}

pub fn bin_dir() -> PathBuf {
    Path::new(ta_bin()).parent().unwrap().to_path_buf()
}

/// `PATH` with the test binary's directory prepended, so git's merge driver
/// (`ta git-merge ...`) resolves to the binary under test.
pub fn path_with_bin() -> OsString {
    let mut dirs = vec![bin_dir()];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap()
}

/// A fresh, empty scratch directory unique to `name`, under the system temp dir
/// (NOT `CARGO_TARGET_TMPDIR`, which lives inside the project tree where `ta`'s
/// store discovery would find the repo's own `.taska` in an ancestor).
pub fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("taska-e2e").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{} {}`: {}", program, args.join(" "), e))
}

/// Run `ta`, asserting success and returning stdout.
pub fn ta(dir: &Path, args: &[&str]) -> String {
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
pub fn git(dir: &Path, args: &[&str]) -> String {
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
/// first column, so we check each line's first whitespace-delimited token -
/// robust to alignment padding and to a later `deps` column naming the id.
pub fn lists_task(output: &str, id: &str) -> bool {
    output
        .lines()
        .any(|l| l.split_whitespace().next() == Some(id))
}

/// Initialize a git repo with a deterministic default branch and an identity.
pub fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@taska.dev"]);
    git(dir, &["config", "user.name", "Taska Test"]);
}

/// The non-default config tokens the [`init_renamed`] store uses. A test
/// exercising configurable surfaces should drive EVERYTHING through these names;
/// if production code hardcodes a default (`status`/`depends_on`/`type`/
/// `create_time`/...), a command against this store breaks and the test fails.
/// (A few tests stay on the defaults to cover default initialization itself.)
pub mod names {
    pub const STATUS_FIELD: &str = "state"; // default: status
    pub const DEFAULT_STATUS: &str = "backlog"; // default: todo
    pub const MID_STATUS: &str = "building"; // a non-default, non-done status
    pub const DONE_STATUS: &str = "shipped"; // default: closed
    pub const TYPE_FIELD: &str = "kind"; // default: type
    pub const TASK_TYPE: &str = "story"; // a declared type name
    pub const TITLE: &str = "headline"; // a required string field
    pub const NOTES: &str = "body"; // a required string field
    pub const BLOCKER: &str = "needs"; // default: depends_on
    pub const BLOCKER_INV: &str = "feeds"; // default: blocks
    pub const HIER: &str = "contains"; // default: has_subtask
    pub const HIER_INV: &str = "part_of"; // default: subtask_of
    pub const INFO: &str = "related"; // default: relates_to (symmetric)
    pub const CREATE_TIME: &str = "made_at"; // default: create_time
    pub const UPDATE_TIME: &str = "touched_at"; // default: update_time
    pub const CLOSE_TIME: &str = "shipped_at"; // default: close_time
}

/// A `config.toml` that renames every configurable thing to a distinctive
/// non-default token (see [`names`]). Scalar keys precede sub-tables per section
/// (TOML ordering), and the schema is valid (one blocker relationship, no inverse
/// collisions).
const RENAMED_CONFIG: &str = r#"
[workflow]
status_field = "state"
default_status = "backlog"
done_status = "shipped"
type_field = "kind"
untyped_tasks = "deny"

[timestamps]
create_time = "made_at"
update_time = "touched_at"
close_time = "shipped_at"

[display]
columns = ["id", "headline", "state", "deps"]
max_width = 40
sort = "id"

[relationships]
needs    = { kind = "blocker", inverse = "feeds" }
contains = { kind = "hierarchy", inverse = "part_of" }
related  = { kind = "info", inverse = "related" }
dup      = { kind = "info" }

[task_types.story]
closed = true
fields = { headline = { type = "string", required = true }, body = { type = "string", required = true }, state = { type = "enum", values = ["backlog", "building", "shipped"], required = true }, rank = { type = "enum", values = ["lo", "hi"] } }
"#;

/// Provision a store whose config renames EVERY configurable thing to a
/// distinctive non-default token (see [`names`]): git-init the repo, write the
/// renamed `.taska/config.toml`, then `ta init`. Non-init tests use this so a
/// stray hardcoded default in the code fails them.
pub fn init_renamed(dir: &Path) {
    init_repo(dir);
    fs::create_dir_all(dir.join(".taska")).unwrap();
    fs::write(dir.join(".taska/config.toml"), RENAMED_CONFIG).unwrap();
    ta(dir, &["init"]);
}

/// A schema-LESS renamed config: it renames the configurable NAMES (status field
/// `state`, type field `kind`, timestamp columns, and all relationship types +
/// inverses) but declares no task types and allows untyped tasks, so a theme test
/// converts with only field/relationship token swaps (no required fields to add).
/// Status VALUES stay near the defaults here; the fully-renamed-VALUE coverage
/// lives in `configurable_names.rs` / [`init_renamed`].
const RENAMED_OPEN_CONFIG: &str = r#"
[workflow]
status_field = "state"
default_status = "todo"
done_status = "closed"
type_field = "kind"
untyped_tasks = "allow"

[timestamps]
create_time = "made_at"
update_time = "touched_at"
close_time = "shipped_at"

[display]
columns = ["id", "state", "deps"]
max_width = 40

[relationships]
needs    = { kind = "blocker", inverse = "feeds" }
contains = { kind = "hierarchy", inverse = "part_of" }
related  = { kind = "info", inverse = "related" }
dup      = { kind = "info" }
"#;

/// Provision a store with the schema-less renamed config (see
/// [`RENAMED_OPEN_CONFIG`]): the configurable NAMES are non-default, so a theme
/// converted onto this helper catches any hardcoded `status`/`depends_on`/`type`/
/// `create_time`/... in the code paths it exercises.
pub fn init_renamed_open(dir: &Path) {
    init_repo(dir);
    fs::create_dir_all(dir.join(".taska")).unwrap();
    fs::write(dir.join(".taska/config.toml"), RENAMED_OPEN_CONFIG).unwrap();
    ta(dir, &["init"]);
}

pub fn rows(path: &Path) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Append a raw `Update` event for `task_id` directly to the log, at the next
/// seq. Used to plant an *orphan* (an event whose target task doesn't exist):
/// the write-time gate now rejects mutating a missing task, so orphans only arise
/// from merges/reverts/manual edits - which this simulates.
pub fn append_orphan_update(log: &Path, task_id: &str) {
    let mut content = fs::read_to_string(log).unwrap();
    let next = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    content.push_str(&format!(
        "{{\"seq\":{next},\"timestamp\":\"2026-01-01T00:00:00Z\",\"op\":\"Update\",\
         \"task_id\":\"{task_id}\",\"status\":\"x\"}}\n"
    ));
    fs::write(log, content).unwrap();
}
