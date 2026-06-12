// Renamed-config test fixtures, the SINGLE source of truth for the non-default
// names and configs. `include!`d by both the in-crate unit-test support
// (`src/test_support.rs`) and the e2e harness (`tests/common/mod.rs`) - two
// separate compilation contexts (the lib's `#[cfg(test)]` items aren't visible
// to the e2e binaries, and vice-versa), so a shared source file `include!`d in
// both is the way to keep the renamed tokens and config TOML in one place. Each
// side wraps these with its own builders: the e2e harness writes the TOML to a
// `.taska/config.toml` and runs the real binary; the unit support parses it into
// an in-memory `Config`/`InMemoryStore`. (Regular `//` comments, not `//!`, so
// the file is valid `include!`d anywhere in a module.)

/// The non-default tokens every renamed test drives through (never a hardcoded
/// `"state"`/`"needs"`/...). The NAME constants hold for both configs; the VALUE
/// constants (`DEFAULT_STATUS`/`MID_STATUS`/`DONE_STATUS`/`TASK_TYPE`/`TITLE`/
/// `NOTES`) describe the schema'd [`RENAMED_SCHEMA_CONFIG`]; the schema-less
/// [`RENAMED_OPEN_CONFIG`] leaves status values free (literal todo/closed).
pub mod names {
    // --- NAMES (both configs) ---
    pub const STATUS_FIELD: &str = "state"; // default: status
    pub const TYPE_FIELD: &str = "kind"; // default: type
    pub const BLOCKER: &str = "needs"; // default: depends_on
    pub const BLOCKER_INV: &str = "feeds"; // default: blocks
    pub const HIER: &str = "contains"; // default: has_subtask
    pub const HIER_INV: &str = "part_of"; // default: subtask_of
    pub const INFO: &str = "related"; // default: relates_to (symmetric)
    pub const DUP: &str = "dup"; // default: duplicates (one-way)
    pub const CREATE_TIME: &str = "made_at"; // default: create_time
    pub const UPDATE_TIME: &str = "touched_at"; // default: update_time
    pub const CLOSE_TIME: &str = "shipped_at"; // default: close_time
    // --- VALUES (the schema'd `RENAMED_SCHEMA_CONFIG`) ---
    pub const DEFAULT_STATUS: &str = "backlog"; // default: todo
    pub const MID_STATUS: &str = "building"; // a non-default, non-done status
    pub const DONE_STATUS: &str = "shipped"; // default: closed
    pub const TASK_TYPE: &str = "story"; // a declared type name
    pub const TITLE: &str = "headline"; // a required string field
    pub const NOTES: &str = "body"; // a required string field
}

/// A schema-LESS config that renames every configurable NAME (untyped tasks
/// allowed, no declared types). Status VALUES stay free, so tests on it use the
/// name constants with literal status values (todo/closed).
pub const RENAMED_OPEN_CONFIG: &str = r#"
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

/// A SCHEMA'D config that renames every configurable thing AND declares the
/// `story` type (required `headline`/`body`, a `state` enum, an optional `rank`),
/// with the status enum values `backlog`/`building`/`shipped`. Use the VALUE
/// constants in [`names`] with it.
pub const RENAMED_SCHEMA_CONFIG: &str = r#"
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
