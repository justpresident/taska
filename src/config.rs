//! User configuration, persisted to `.taska/config.toml`.
//!
//! The split mirrors how the values are consumed: `[compaction]` tunes how much
//! history `ta compact` leaves in the log, `[workflow]` describes task semantics
//! used by the engine/graph layer, and `[merge]` controls how the git merge
//! driver reconciles concurrent branches. Every key here is honored somewhere —
//! this file is not a list of aspirations.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::error::DynError;
use crate::model::{TaskState, DEPENDS_ON};

/// Smallest `keep_events` we accept in production.
///
/// Retaining fewer than this risks folding away events a concurrent branch still
/// needs to reconcile on merge, which is unrecoverable, so `enforce_config`
/// rejects any smaller value on every store-backed command.
pub const MIN_KEEP_EVENTS: usize = 300;

/// The `config.toml` written by `ta init`.
///
/// Rendered from [`Config::default`] so the values live in exactly one place
/// (the `Default` impls) while the file keeps its explanatory comments — TOML
/// serialization alone can't emit those. A test asserts the rendered file
/// round-trips back to `Config::default()`, which also catches template typos
/// such as a misspelled key.
// A prose template that grows with each config option — line count is not a
// useful signal here, unlike for real logic.
#[allow(clippy::too_many_lines)]
pub fn default_toml() -> String {
    let Config {
        compaction,
        workflow,
        merge,
        display,
        timestamps,
        relationships,
        // Default-empty: the template documents [task_types] with a commented
        // example block instead of rendering the (empty) map.
        task_types: _,
    } = Config::default();
    let on_conflict = merge.on_conflict.as_str();
    let columns = render_columns(&display.columns);
    let column_max_width = render_column_widths(&display.column_max_width);
    let relationships_toml = render_relationships(&relationships);
    format!(
        r#"# taska configuration
#
# Created by `ta init`. Edit freely — `ta init` will not overwrite this file
# once it exists. Missing keys fall back to the defaults shown below.

[compaction]
# `ta compact` folds old events into the baseline snapshot to keep the log
# small. These settings control how much recent history stays in the log.
#
# Recent events must be retained: the git merge driver reconciles divergent
# branches from the events still in the log, so anything already folded into the
# baseline can no longer be reconciled. Keep enough history to cover your
# longest-lived branch.
#
# Keep at least this many of the most recent events (minimum {min_keep}); also
# the minimum log size before compaction does anything.
keep_events = {keep_events}
# Also keep every event from at least this many days back (0 disables the time
# window). An event is retained if it is recent by either rule.
keep_days = {keep_days}

[workflow]
# The DISPLAY name of the status field, and the value that marks a task
# complete. `ta list --ready` treats a dependency as satisfied once it reaches
# `done_status`. On disk the status always lives under the canonical key
# `status`, so renaming this is free at any time (no data migration) — just
# update [display] columns to the new name too.
status_field = "{status_field}"
done_status = "{done_status}"
# Status stamped onto a new task when `ta create` doesn't set one. Set to "" to
# create statusless tasks (the status field stays absent until you set it).
default_status = "{default_status}"
# DISPLAY name of the task-type discriminator used by [task_types] schemas
# (`ta create x type=bug`). Stored canonically as `task_type`, so renaming this
# is free, like status_field.
type_field = "{type_field}"
# Read commands print ONE warning when the store holds tasks that don't conform
# to their [task_types] schema (old data is read-tolerated by design; writes to
# such a task must bring it into conformance). Set false to silence.
warn_nonconforming = {warn_nonconforming}
# Tasks WITHOUT a type while [task_types] schemas are declared: "allow"
# (sanctioned — untouched by schemas, never reported), "warn" (tolerated, but
# counted in the non-conformance warning), or "deny" (a type is mandatory: any
# write to an untyped task is rejected until one is set). A long migration
# typically walks allow -> warn -> deny.
untyped_tasks = "{untyped_tasks}"

[merge]
# What to do when concurrent branches change the SAME field (or dependency) to
# incompatible values. Non-overlapping changes always merge cleanly; this only
# governs genuine contradictions, and it resolves them per-field:
#   "surface" — stop the merge and let a human resolve it (run `ta resolve`)
#   "latest"  — keep the most recently written value (by timestamp)
#   "ours"    — keep the value on the branch being merged INTO
#   "theirs"  — keep the value from the branch being merged IN
on_conflict = "{on_conflict}"

[display]
# Columns shown by list/ready in human format — and the field set and
# order used by `--format json`. "id" and "deps" are built-ins; any other name
# is a task field (blank when a task lacks it). Override per command with
# `--columns a,b,c` or `--full`.
columns = [{columns}]
# Truncate long human cell values to this many characters (0 = no limit). This
# is the global fallback for any column not overridden below.
max_width = {max_width}
# Default column to sort list/ready by (ascending; --sort overrides,
# --reverse flips). Any field, "id", or "deps"; empty/unknown falls back to id.
sort = "{sort}"
# Default human layout per command: "table" (aligned columns, one task per row)
# or "list" (a vertical `field: value` record per task). `--layout` overrides.
list_layout = "{list_layout}"
show_layout = "{show_layout}"

# Per-column truncation overrides. A column listed here is truncated to its own
# width instead of max_width (0 = no limit). `--full` ignores these entirely.
[display.column_max_width]
{column_max_width}

[timestamps]
# Computed timestamp fields materialized onto every task from the event log
# (never user-set). These name the columns they surface under; set a name to ""
# to disable that timestamp. create_time = the Create event's time; update_time
# = the latest touching event's time; close_time = the most recent time status
# reached done_status (cleared while the task is currently not done). They are
# available to --columns/--full/show and as a --sort key.
create_time = "{create_time}"
update_time = "{update_time}"
close_time = "{close_time}"

# Relationship types. `ta dep <a> <type>=<b>` adds an edge; an undeclared type is
# rejected. Each type's `kind` says how it behaves: "blocker" makes the target a
# prerequisite (feeds `ta list --ready` and cycle detection); "hierarchy" is a
# parent/child (subtask) edge that gates like a blocker but renders distinctly;
# "info" is informational. inverse names the reverse direction and is OPTIONAL —
# omit it for a one-way type; the type's own name makes it symmetric
# (`a relates_to b` reads both ways); else it labels the inverse.
{relationships_toml}

# Per-type task schemas — OFF while no [task_types.<name>] is declared (the
# store stays fully schema-agnostic). Once declared, the `type` field (see
# workflow.type_field) selects a task's schema, enforced on every create/update
# (whole-task: every violation reported in one error). Field kinds: string,
# bool, int, uint, float, datetime, enum, any, array<T>, set<T>. Example:
# [task_types.bug]
# closed = true                       # no fields beyond the declared ones
# [task_types.bug.fields]
# points = "uint"                     # shorthand: just the kind
# tags = "array<string>"
# [task_types.bug.fields.severity]    # long form when constraints are needed
# type = "enum"
# values = ["low", "medium", "high"]
# required = true
"#,
        min_keep = MIN_KEEP_EVENTS,
        keep_events = compaction.keep_events,
        keep_days = compaction.keep_days,
        status_field = workflow.status_field,
        done_status = workflow.done_status,
        default_status = workflow.default_status,
        type_field = workflow.type_field,
        warn_nonconforming = workflow.warn_nonconforming,
        untyped_tasks = workflow.untyped_tasks.as_str(),
        on_conflict = on_conflict,
        columns = columns,
        max_width = display.max_width,
        sort = display.sort,
        list_layout = display.list_layout.as_str(),
        show_layout = display.show_layout.as_str(),
        column_max_width = column_max_width,
        create_time = timestamps.create_time,
        update_time = timestamps.update_time,
        close_time = timestamps.close_time,
        relationships_toml = relationships_toml,
    )
}

fn render_columns(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `[display.column_max_width]` sub-table, one `name = width` per entry.
/// Keys are quoted so a column name with TOML-special characters round-trips.
fn render_column_widths(widths: &BTreeMap<String, usize>) -> String {
    widths
        .iter()
        .map(|(k, v)| format!("\"{k}\" = {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `[relationships.<name>]` sub-table per declared relationship type.
fn render_relationships(relationships: &RelationshipConfig) -> String {
    relationships
        .types
        .iter()
        .map(|(name, k)| {
            // `inverse` is optional: omit it for a one-way relationship.
            let inverse = if k.inverse.is_empty() {
                String::new()
            } else {
                format!("\ninverse = \"{}\"", k.inverse)
            };
            format!(
                "[relationships.{name}]\nkind = \"{}\"{inverse}",
                k.kind.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Config {
    pub compaction: CompactionConfig,
    pub workflow: WorkflowConfig,
    pub merge: MergeConfig,
    pub display: DisplayConfig,
    pub timestamps: TimestampConfig,
    pub relationships: RelationshipConfig,
    pub task_types: TaskTypesConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct CompactionConfig {
    /// Minimum number of most-recent events to retain in the log after
    /// compaction; also the minimum log size before compaction does anything.
    pub keep_events: usize,
    /// Also retain events newer than this many days (0 disables the time
    /// window). An event is kept if it is recent by either rule.
    pub keep_days: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            keep_events: 5000,
            keep_days: 30,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct WorkflowConfig {
    /// DISPLAY name of the status field. Storage is always the canonical
    /// [`crate::model::STATUS_KEY`]: commands map this name to the key on
    /// write, `state_of` maps it back on read — so renaming it is free.
    pub status_field: String,
    pub done_status: String,
    /// Status stamped onto a new task when `ta create` doesn't set the status
    /// field. Empty means create statusless tasks (the status field is simply
    /// absent until set).
    pub default_status: String,
    /// DISPLAY name of the task-type discriminator (`ta create x type=bug`)
    /// that selects a task's `[task_types]` schema. Same display-vs-storage
    /// split as `status_field`: storage is always the canonical
    /// [`crate::model::TASK_TYPE_KEY`], so renaming this is free too.
    pub type_field: String,
    /// Whether read commands print the one-line warning when the store holds
    /// tasks that don't conform to their `[task_types]` schema (grandfathered
    /// data is read-tolerated by design; the warning is the signal to run the
    /// repair). `false` silences it.
    pub warn_nonconforming: bool,
    /// Policy for tasks WITHOUT a task type while `[task_types]` schemas are
    /// declared — the migration ladder: `allow` (sanctioned: untouched by
    /// schemas, never reported), `warn` (tolerated, but counted in the
    /// non-conformance report), `deny` (a type is mandatory: any write to an
    /// untyped task is rejected until one is set).
    pub untyped_tasks: UntypedTasks,
}

/// See [`WorkflowConfig::untyped_tasks`].
///
/// Typed tasks always validate fully — this only governs tasks missing the
/// discriminator entirely (a task with an UNKNOWN type name is a violation
/// under every policy).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum UntypedTasks {
    Allow,
    Warn,
    #[default]
    Deny,
}

impl UntypedTasks {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Deny => "deny",
        }
    }
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            status_field: "status".to_string(),
            done_status: "closed".to_string(),
            default_status: "todo".to_string(),
            type_field: "type".to_string(),
            warn_nonconforming: true,
            untyped_tasks: UntypedTasks::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct MergeConfig {
    /// What the git merge driver does when two concurrent branches make
    /// incompatible changes to the same field or dependency.
    pub on_conflict: OnConflict,
}

/// Policy for a genuine merge contradiction.
///
/// A contradiction is the same field or dependency set to different values on
/// both branches. Commuting concurrent edits always auto-merge regardless of
/// this setting; it only governs true conflicts, and it applies per-field —
/// each conflicting field is resolved on its own.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    /// Stop the merge and let a human resolve it (`ta resolve`).
    #[default]
    Surface,
    /// Keep the most recently written value, by timestamp.
    Latest,
    /// Keep the value on the branch being merged into (Git's `%A`).
    Ours,
    /// Keep the value from the branch being merged in (Git's `%B`).
    Theirs,
}

impl OnConflict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Surface => "surface",
            Self::Latest => "latest",
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        }
    }
}

/// Presentation for human `list`/`show` output: a columns table or a vertical
/// per-task record.
///
/// SERIALIZATION CONTRACT: the lowercase names are config values users write.
#[derive(Serialize, Deserialize, ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Aligned columns, one task per row (the classic `list` view).
    Table,
    /// A vertical `field: value` record per task (the classic `show` view).
    List,
}

impl Layout {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::List => "list",
        }
    }
}

/// How `list` (including `--ready`) presents tasks.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct DisplayConfig {
    /// Columns shown in human format and the field order used by `--format json`,
    /// in order. `id` and `deps` are built-ins; any other name is a task field.
    pub columns: Vec<String>,
    /// Truncate human cell values to this many characters (0 = no limit). The
    /// global fallback for any column not listed in `column_max_width`.
    pub max_width: usize,
    /// Default layout for `ta list` (`table` or `list`); `--layout` overrides.
    pub list_layout: Layout,
    /// Default layout for `ta show` (`table` or `list`); `--layout` overrides.
    pub show_layout: Layout,
    /// Per-column truncation overrides: a column named here truncates to its own
    /// width instead of `max_width` (0 = no limit). `--full` ignores these. A
    /// `BTreeMap` so the rendered config is deterministically ordered.
    pub column_max_width: BTreeMap<String, usize>,
    /// Default column to sort `list` rows by (ascending). Any
    /// field name, `id`, or `deps`; `--sort` overrides per command. An empty
    /// value (or an unknown column) falls back to ordering by `id`.
    pub sort: String,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            columns: ["id", "title", "status", "deps"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_width: 40,
            // A table scans best for many tasks; a single `show` reads best as a
            // vertical record.
            list_layout: Layout::Table,
            show_layout: Layout::List,
            // Titles are usually the longest field, so give them more room than
            // the global default out of the box.
            column_max_width: std::iter::once(("title".to_string(), 80)).collect(),
            // Oldest-first is the most useful default order for a task list.
            sort: "create_time".to_string(),
        }
    }
}

/// Names of the computed timestamp fields materialized onto every task.
///
/// They are never user-set (replay computes them from the event log); these
/// settings only control the *column names* they surface under. An empty string
/// disables that timestamp entirely (neither computed-for-display nor shown).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct TimestampConfig {
    /// Column name for the `Create` event's timestamp.
    pub create_time: String,
    /// Column name for the latest touching event's timestamp.
    pub update_time: String,
    /// Column name for the most recent transition of status into `done_status`
    /// (cleared while the task is currently not done).
    pub close_time: String,
}

impl Default for TimestampConfig {
    fn default() -> Self {
        Self {
            create_time: "create_time".to_string(),
            update_time: "update_time".to_string(),
            close_time: "close_time".to_string(),
        }
    }
}

/// What a relationship type does to the dependency graph.
///
/// SERIALIZATION CONTRACT: the lowercase names are config values users write.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RelKind {
    /// `A <type> B` makes `B` a prerequisite of `A`: `A` is ready only once `B`
    /// is done. Feeds `ta list --ready` and cycle detection (the dependency DAG).
    Blocker,
    /// A parent/child containment edge: `A <type> B` makes `B` a subtask of `A`.
    /// Gates the graph exactly like a `blocker` (the parent isn't done until its
    /// children are), but is rendered distinctly as a hierarchy, not a plain
    /// dependency.
    Hierarchy,
    /// No effect on readiness or cycles — purely informational.
    #[default]
    Info,
}

impl RelKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Hierarchy => "hierarchy",
            Self::Info => "info",
        }
    }
}

/// One relationship type's semantics.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RelationshipDef {
    /// `blocker` (gates readiness/cycles), `hierarchy` (parent/child — gates like
    /// a blocker but renders as subtasks), or `info` (informational). The config
    /// key is `kind`; the pre-rename key `type` still loads as an alias.
    #[serde(alias = "type")]
    pub kind: RelKind,
    /// Name the reverse edge is shown under. **Optional**; empty (the default)
    /// means a one-way relationship whose reverse isn't surfaced (e.g. a small
    /// task that `duplicates` part of a bigger one). The type's OWN name means
    /// **symmetric** — `a relates_to b` also reads as `b relates_to a`, removable
    /// from either side. Any other name labels the inverse direction
    /// (`depends_on`'s reverse is `blocks`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub inverse: String,
}

/// Declared relationship types, by name.
///
/// `ta dep <a> <type>=<b>` rejects a type not listed here. A newtype over the map
/// so it gets a non-empty `Default` and serializes transparently as
/// `[relationships.<name>]` sub-tables.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct RelationshipConfig {
    pub types: BTreeMap<String, RelationshipDef>,
}

impl RelationshipConfig {
    /// Relationship-type names that gate readiness, cycle detection, and the
    /// dependency tree: every declared `blocker` *or* `hierarchy` type (both
    /// gate). Config is the sole source of truth — there is no implicit
    /// `depends_on` (`Config::validate` requires at least one `blocker` type).
    pub fn blocker_types(&self) -> BTreeSet<String> {
        self.types
            .iter()
            .filter(|(_, def)| matches!(def.kind, RelKind::Blocker | RelKind::Hierarchy))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The default blocker type new untyped edges and the legacy-event migration
    /// resolve to: the first (by name) declared `blocker`-kind type, if any.
    pub fn default_blocker(&self) -> Option<&str> {
        self.types
            .iter()
            .find(|(_, def)| def.kind == RelKind::Blocker)
            .map(|(name, _)| name.as_str())
    }

    /// Relationship-type names whose edges are parent→child containment
    /// (`kind = "hierarchy"`). These gate like blockers but render distinctly as
    /// subtasks, and a parent rolls up completion over them.
    pub fn hierarchy_types(&self) -> BTreeSet<String> {
        self.types
            .iter()
            .filter(|(_, def)| def.kind == RelKind::Hierarchy)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl Default for RelationshipConfig {
    fn default() -> Self {
        let def = |kind, inverse: &str| RelationshipDef {
            kind,
            inverse: inverse.to_string(),
        };
        Self {
            // `depends_on` blocks (reverse `blocks`); `has_subtask` is a hierarchy
            // (reverse `subtask_of`); `relates_to` is symmetric (self-inverse);
            // `duplicates` is one-way (no inverse surfaced).
            types: [
                (DEPENDS_ON.to_string(), def(RelKind::Blocker, "blocks")),
                (
                    "has_subtask".to_string(),
                    def(RelKind::Hierarchy, "subtask_of"),
                ),
                ("relates_to".to_string(), def(RelKind::Info, "relates_to")),
                ("duplicates".to_string(), def(RelKind::Info, "")),
            ]
            .into_iter()
            .collect(),
        }
    }
}

/// A declared field's value kind, parsed from its spec string (`"uint"`,
/// `"enum"`, `"array<string>"`, `"set<enum>"`, …).
///
/// This is the grammar the schema write gate will enforce; the config layer
/// parses and validates declarations only. `set<T>` is an `array<T>` with
/// unique elements, stored canonically deduped + sorted (canonical bytes are
/// what make concurrent inserts converge on merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    String,
    Bool,
    Int,
    Uint,
    Float,
    Datetime,
    Enum,
    Any,
    Array(Box<Self>),
    Set(Box<Self>),
}

impl FieldKind {
    /// Parse a kind string. Containers take exactly one SCALAR element kind:
    /// `array<…>`/`set<…>` don't nest, and `any` can't be an element.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let scalar = |name: &str| match name {
            "string" => Some(Self::String),
            "bool" => Some(Self::Bool),
            "int" => Some(Self::Int),
            "uint" => Some(Self::Uint),
            "float" => Some(Self::Float),
            "datetime" => Some(Self::Datetime),
            "enum" => Some(Self::Enum),
            _ => None,
        };
        if spec == "any" {
            return Ok(Self::Any);
        }
        if let Some(kind) = scalar(spec) {
            return Ok(kind);
        }
        for (prefix, container) in [
            ("array<", Self::Array as fn(Box<Self>) -> Self),
            ("set<", Self::Set as fn(Box<Self>) -> Self),
        ] {
            if let Some(element) = spec
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix('>'))
            {
                return scalar(element)
                    .map(|kind| container(Box::new(kind)))
                    .ok_or_else(|| {
                        format!(
                            "`{element}` is not a valid element kind for `{prefix}…>` \
                             (expected a scalar: string|bool|int|uint|float|datetime|enum)"
                        )
                    });
            }
        }
        Err(format!(
            "unknown field kind `{spec}` (expected string|bool|int|uint|float|datetime|enum|any, \
             or array<…>/set<…> of a scalar kind)"
        ))
    }

    /// The kind that `values` and value checks attach to: the element kind for
    /// containers, the kind itself otherwise.
    #[must_use]
    pub fn base(&self) -> &Self {
        match self {
            Self::Array(element) | Self::Set(element) => element,
            other => other,
        }
    }

    /// Whether a stored JSON value conforms to this kind. `values` holds the
    /// declared enum values (consulted when [`Self::base`] is `Enum`). The
    /// shared check behind the schema write gate and (later) schema-aware
    /// coercion. `int`/`uint` reject fractional numbers; `float` accepts any
    /// number; `datetime` is an RFC 3339 string; `set<T>` additionally requires
    /// unique elements (compared by their compact JSON form).
    #[must_use]
    pub fn matches_value(&self, value: &serde_json::Value, values: &[String]) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Bool => value.is_boolean(),
            Self::Int => value.is_i64(),
            Self::Uint => value.is_u64(),
            Self::Float => value.is_number(),
            Self::Datetime => value
                .as_str()
                .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok()),
            Self::Enum => value
                .as_str()
                .is_some_and(|s| values.iter().any(|v| v == s)),
            Self::Any => true,
            Self::Array(element) => value
                .as_array()
                .is_some_and(|items| items.iter().all(|i| element.matches_value(i, values))),
            Self::Set(element) => value.as_array().is_some_and(|items| {
                let unique: BTreeSet<String> = items.iter().map(ToString::to_string).collect();
                unique.len() == items.len()
                    && items.iter().all(|i| element.matches_value(i, values))
            }),
        }
    }
}

/// One declared field of a task type.
///
/// Either the shorthand kind string (`points = "uint"`) or the long form with
/// constraints (`[task_types.<t>.fields.<name>]` carrying `type`, `values`,
/// `required`). Loading is permissive (untagged); [`Config::validate`] checks
/// the kind grammar and the enum/`values` consistency.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum FieldSchema {
    /// `name = "<kind>"`.
    Short(String),
    /// A `[task_types.<t>.fields.<name>]` sub-table.
    Full(FieldSpec),
}

impl FieldSchema {
    /// The declared kind string (the shorthand itself, or the long form's `type`).
    #[must_use]
    pub fn kind_str(&self) -> &str {
        match self {
            Self::Short(kind) => kind,
            Self::Full(spec) => &spec.kind,
        }
    }

    /// The declared enum values (empty for the shorthand and non-enum kinds).
    #[must_use]
    pub fn values(&self) -> &[String] {
        match self {
            Self::Short(_) => &[],
            Self::Full(spec) => &spec.values,
        }
    }

    /// Whether every task of the type must carry this field.
    #[must_use]
    pub const fn required(&self) -> bool {
        match self {
            Self::Short(_) => false,
            Self::Full(spec) => spec.required,
        }
    }
}

/// The long-form field spec. `deny_unknown_fields` makes a typo'd key a load
/// error instead of a silently weaker schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FieldSpec {
    /// The field's value kind (`"enum"`, `"uint"`, `"array<string>"`, …). The
    /// config key is `type` — a field's type and a TASK's type are different
    /// entities; both are legitimately called type.
    #[serde(rename = "type")]
    pub kind: String,
    /// Allowed values when the (element) kind is `enum`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Whether every task of this type must carry the field.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// One declared task type (`[task_types.<name>]`): its field schemas and
/// whether undeclared fields are allowed on tasks of this type.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TaskTypeDef {
    /// `true` = tasks of this type may carry NO field names beyond the declared
    /// ones (plus the discriminator itself). Open (`false`) is the default —
    /// the schema-agnostic ethos.
    pub closed: bool,
    pub fields: BTreeMap<String, FieldSchema>,
}

/// Declared task types, by name.
///
/// Empty (the default) means schemas are off and the store stays fully
/// schema-agnostic. A newtype over the map so it serializes transparently as
/// `[task_types.<name>]` sub-tables.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(transparent)]
pub struct TaskTypesConfig {
    pub types: BTreeMap<String, TaskTypeDef>,
}

impl Config {
    /// Load config from `path`, falling back to defaults if the file is absent.
    /// `#[serde(default)]` means a partial file still loads — only the keys
    /// present override their defaults.
    pub fn load(path: &Path) -> Result<Self, DynError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Cheap, struct-only validation: reject configurations that would corrupt the
    /// store regardless of the task data (today: the `keep_events` floor). The CLI
    /// calls this on every store-backed command so a bad edit is reported on the
    /// very next `ta` invocation rather than silently at the next compaction — and
    /// because it never inspects the graph, it can't lock you out of the commands
    /// (`resolve`, `dep remove`, `config get`) you'd use to fix a deeper problem.
    pub fn validate(&self) -> Result<(), DynError> {
        let mut problems = Vec::new();
        self.collect_struct_problems(&mut problems);
        Self::finish(&problems)
    }

    /// Full validation: the struct-only checks plus consistency against the
    /// materialized task graph. `ta config validate` and `ta config set` run this
    /// so a manual edit (or a `set`) that contradicts the data — an edge of an
    /// undeclared type, a blocker cycle, an inverse name colliding with another
    /// type — is caught up front. This is the hook `type-schemas` extends with
    /// per-type field validation.
    pub fn validate_against(&self, state: &HashMap<String, TaskState>) -> Result<(), DynError> {
        let mut problems = Vec::new();
        self.collect_struct_problems(&mut problems);
        self.collect_graph_problems(state, &mut problems);
        Self::finish(&problems)
    }

    fn collect_struct_problems(&self, problems: &mut Vec<String>) {
        if self.compaction.keep_events < MIN_KEEP_EVENTS {
            problems.push(format!(
                "compaction.keep_events = {} is below the minimum of {}. Retaining fewer \
                 events risks discarding history still needed to reconcile merges.",
                self.compaction.keep_events, MIN_KEEP_EVENTS
            ));
        }
        if self.relationships.default_blocker().is_none() {
            problems.push(
                "[relationships] declares no `kind = \"blocker\"` type. At least one blocking \
                 dependency type is required (the default config declares `depends_on`)."
                    .to_string(),
            );
        }
        self.collect_workflow_name_problems(problems);
        self.collect_name_collision_problems(problems);
        self.collect_task_type_problems(problems);
    }

    /// The relationship-type names + their non-empty inverse display names —
    /// the vocabulary task fields and workflow display names must not shadow.
    fn relationship_names(&self) -> BTreeSet<&str> {
        let mut names: BTreeSet<&str> = self
            .relationships
            .types
            .keys()
            .map(String::as_str)
            .collect();
        for def in self.relationships.types.values() {
            if !def.inverse.is_empty() {
                names.insert(def.inverse.as_str());
            }
        }
        names
    }

    /// The configured display names (`status_field`, `type_field`) must be
    /// non-empty, distinct, and must not shadow another concept's name.
    /// The configured display names (`status_field`, `type_field`) must be
    /// non-empty; everything else about them is covered by the namespace pass.
    fn collect_workflow_name_problems(&self, problems: &mut Vec<String>) {
        let w = &self.workflow;
        for (option, name) in [
            ("workflow.status_field", &w.status_field),
            ("workflow.type_field", &w.type_field),
        ] {
            if name.is_empty() {
                problems.push(format!("{option} must not be empty"));
            }
        }
    }

    /// Every configured name lives in ONE namespace: the canonical storage keys,
    /// the workflow display names, the timestamp columns, and the relationship
    /// type + inverse names all end up as field/column names, so a name claimed
    /// by two roles — or by the static reserved list — garbles reads (a
    /// timestamp clobbering the status, a relationship shadowed by the `deps`
    /// built-in, …). Schema field declarations are checked separately,
    /// membership-only: two task types sharing a field name is deliberate.
    fn collect_name_collision_problems(&self, problems: &mut Vec<String>) {
        fn claim<'a>(
            claimed: &mut BTreeMap<&'a str, String>,
            problems: &mut Vec<String>,
            name: &'a str,
            role: String,
        ) {
            if name.is_empty() {
                return; // empty = disabled (timestamps) or reported separately
            }
            if crate::model::RESERVED_FIELD_KEYS.contains(&name) {
                problems.push(format!(
                    "{role} `{name}` collides with a reserved/computed field name"
                ));
            } else if let Some(prev) = claimed.get(name) {
                problems.push(format!("`{name}` is used by both {prev} and {role}"));
            } else {
                claimed.insert(name, role);
            }
        }
        let mut claimed: BTreeMap<&str, String> = BTreeMap::new();
        claim(
            &mut claimed,
            problems,
            crate::model::STATUS_KEY,
            "the status storage key".to_string(),
        );
        claim(
            &mut claimed,
            problems,
            crate::model::TASK_TYPE_KEY,
            "the task-type storage key".to_string(),
        );
        // A display name equal to its OWN canonical key is the identity mapping
        // (the default), not a collision.
        let w = &self.workflow;
        if w.status_field != crate::model::STATUS_KEY {
            claim(
                &mut claimed,
                problems,
                &w.status_field,
                "workflow.status_field".to_string(),
            );
        }
        if w.type_field != crate::model::TASK_TYPE_KEY {
            claim(
                &mut claimed,
                problems,
                &w.type_field,
                "workflow.type_field".to_string(),
            );
        }
        let ts = &self.timestamps;
        for (role, name) in [
            ("timestamps.create_time", &ts.create_time),
            ("timestamps.update_time", &ts.update_time),
            ("timestamps.close_time", &ts.close_time),
        ] {
            claim(&mut claimed, problems, name, role.to_string());
        }
        for (name, def) in &self.relationships.types {
            claim(
                &mut claimed,
                problems,
                name,
                format!("relationship type `{name}`"),
            );
            // A symmetric self-inverse is sanctioned; an inverse equal to a
            // DIFFERENT declared type is reported by the dedicated ambiguity
            // check below with a more actionable message.
            if !def.inverse.is_empty()
                && def.inverse != *name
                && !self.relationships.types.contains_key(&def.inverse)
            {
                claim(
                    &mut claimed,
                    problems,
                    &def.inverse,
                    format!("the inverse of relationship `{name}`"),
                );
            }
        }
        // An inverse name colliding with a *different* declared type makes
        // `ta dep` unable to tell the forward edge from the inverse one.
        // (Structural, so it belongs here — every store command checks it.)
        for (name, def) in &self.relationships.types {
            if !def.inverse.is_empty()
                && def.inverse != *name
                && self.relationships.types.contains_key(&def.inverse)
            {
                problems.push(format!(
                    "relationship `{name}` has inverse `{}`, which is also a declared type — \
                     ambiguous (use a distinct inverse name, or set inverse = \"{name}\" to make \
                     it symmetric)",
                    def.inverse
                ));
            }
        }
    }

    /// `[task_types]` declarations: every field name must be usable (not
    /// reserved/computed, not a relationship or timestamp name, not the
    /// discriminator itself), every kind string must parse, and `values` must
    /// be present exactly for enum kinds.
    fn collect_task_type_problems(&self, problems: &mut Vec<String>) {
        let w = &self.workflow;
        let rel_names = self.relationship_names();
        let ts = &self.timestamps;
        let timestamp_names: Vec<&String> = [&ts.create_time, &ts.update_time, &ts.close_time]
            .into_iter()
            .filter(|n| !n.is_empty())
            .collect();
        for (type_name, def) in &self.task_types.types {
            for (field, schema) in &def.fields {
                let ctx = format!("task_types.{type_name}.fields.{field}");
                if crate::model::RESERVED_FIELD_KEYS.contains(&field.as_str()) {
                    problems.push(format!("{ctx}: reserved/computed field name"));
                } else if rel_names.contains(field.as_str()) {
                    problems.push(format!(
                        "{ctx}: collides with a relationship type or inverse name"
                    ));
                } else if timestamp_names.contains(&field) {
                    problems.push(format!("{ctx}: collides with a computed timestamp column"));
                } else if field == &w.type_field || field == crate::model::TASK_TYPE_KEY {
                    problems.push(format!(
                        "{ctx}: the task-type discriminator is implicit — don't declare it"
                    ));
                } else if field == crate::model::STATUS_KEY
                    && w.status_field != crate::model::STATUS_KEY
                {
                    problems.push(format!(
                        "{ctx}: declare the status under its display name `{}`",
                        w.status_field
                    ));
                }
                match FieldKind::parse(schema.kind_str()) {
                    Err(reason) => problems.push(format!("{ctx}: {reason}")),
                    Ok(kind) => {
                        let is_enum = matches!(kind.base(), FieldKind::Enum);
                        if is_enum && schema.values().is_empty() {
                            problems.push(format!(
                                "{ctx}: an enum kind needs a non-empty `values` list"
                            ));
                        } else if !is_enum && !schema.values().is_empty() {
                            problems.push(format!(
                                "{ctx}: `values` only applies to enum kinds (declared `{}`)",
                                schema.kind_str()
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Relationship/graph consistency checks against the materialized tasks.
    fn collect_graph_problems(
        &self,
        state: &HashMap<String, TaskState>,
        problems: &mut Vec<String>,
    ) {
        let types = &self.relationships.types;

        // 1. Every typed relationship edge present in the data must use a declared
        //    type — so renaming/removing a type that tasks still use is caught.
        let mut undeclared: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (id, task) in state {
            for rel in task.relationships.keys() {
                if !types.contains_key(rel) {
                    undeclared.entry(rel).or_default().insert(id);
                }
            }
        }
        for (rel, who) in &undeclared {
            let list: Vec<&str> = who.iter().copied().collect();
            problems.push(format!(
                "relationship type `{rel}` is used by {} task(s) ({}) but is not declared \
                 in [relationships]",
                who.len(),
                list.join(", ")
            ));
        }

        // 2. The blocker graph must be acyclic.
        let blockers = self.relationships.blocker_types();
        for cycle in crate::graph::dependency_cycles(state, &blockers) {
            let shown = if cycle.len() == 1 {
                format!("{} (depends on itself)", cycle[0])
            } else {
                cycle.join(" ↔ ")
            };
            problems.push(format!("blocker dependency cycle: {shown}"));
        }

        // (The inverse-vs-declared-type ambiguity check is structural and lives
        // in `collect_name_collision_problems`, so every store command runs it.)

        // 4. At most one blocking relationship between any two tasks.
        for (task, target, kinds) in crate::graph::duplicate_blocker_edges(state, &blockers) {
            problems.push(format!(
                "`{task}` has more than one blocking relationship to `{target}` ({}); only one \
                 is allowed between two tasks",
                kinds.join(", ")
            ));
        }

        // 5. A task may have at most one parent (one incoming hierarchy edge).
        let hierarchy = self.relationships.hierarchy_types();
        for (child, parents) in crate::graph::multi_parent_tasks(state, &hierarchy) {
            problems.push(format!(
                "`{child}` is a subtask of multiple parents ({}); a task can have only one parent",
                parents.join(", ")
            ));
        }
    }

    fn finish(problems: &[String]) -> Result<(), DynError> {
        if problems.is_empty() {
            return Ok(());
        }
        let mut msg = format!("config validation failed ({} problem(s)):", problems.len());
        for p in problems {
            msg.push_str("\n  - ");
            msg.push_str(p);
        }
        Err(msg.into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        // The rendered template must round-trip to the defaults it was built
        // from — catches a typo'd key or section in the prose template.
        let parsed: Config = toml::from_str(&default_toml()).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn field_kind_grammar_parses_and_rejects() {
        assert_eq!(FieldKind::parse("uint").unwrap(), FieldKind::Uint);
        assert_eq!(FieldKind::parse("any").unwrap(), FieldKind::Any);
        assert_eq!(
            FieldKind::parse("array<string>").unwrap(),
            FieldKind::Array(Box::new(FieldKind::String))
        );
        assert_eq!(
            FieldKind::parse("set<enum>").unwrap(),
            FieldKind::Set(Box::new(FieldKind::Enum))
        );
        // `base` is what values/checks attach to: the element for containers.
        assert_eq!(
            *FieldKind::parse("set<int>").unwrap().base(),
            FieldKind::Int
        );
        assert_eq!(*FieldKind::parse("float").unwrap().base(), FieldKind::Float);
        for bad in ["", "integer", "array<any>", "array<array<int>>", "set<>"] {
            assert!(FieldKind::parse(bad).is_err(), "must reject `{bad}`");
        }
    }

    #[test]
    fn task_type_schemas_parse_shorthand_and_long_form() {
        let cfg: Config = toml::from_str(
            r#"
[task_types.bug]
closed = true
[task_types.bug.fields]
points = "uint"
tags = "array<string>"
[task_types.bug.fields.severity]
type = "enum"
values = ["low", "high"]
required = true
"#,
        )
        .unwrap();
        let bug = &cfg.task_types.types["bug"];
        assert!(bug.closed);
        assert_eq!(bug.fields["points"].kind_str(), "uint");
        assert!(!bug.fields["points"].required(), "shorthand is optional");
        assert!(bug.fields["points"].values().is_empty());
        assert_eq!(bug.fields["severity"].kind_str(), "enum");
        assert_eq!(bug.fields["severity"].values(), ["low", "high"]);
        assert!(bug.fields["severity"].required());
        assert!(cfg.validate().is_ok(), "a sound schema validates");

        // A typo'd long-form key is a LOAD error, not a silently weaker schema.
        assert!(toml::from_str::<Config>(
            "[task_types.t.fields.s]\ntype = \"string\"\nrequird = true\n"
        )
        .is_err());
    }

    #[test]
    fn task_type_validation_catches_bad_declarations() {
        let check = |types_toml: &str, needle: &str| {
            let cfg: Config = toml::from_str(types_toml).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "`{needle}` not in: {err}");
        };
        check("[task_types.t.fields.sev]\ntype = \"enum\"\n", "values");
        check(
            "[task_types.t.fields]\npoints = \"integer\"\n",
            "unknown field kind",
        );
        check(
            "[task_types.t.fields.p]\ntype = \"uint\"\nvalues = [\"x\"]\n",
            "only applies to enum",
        );
        check("[task_types.t.fields]\ndeps = \"string\"\n", "reserved");
        check(
            "[task_types.t.fields]\nblocks = \"string\"\n",
            "relationship",
        );
        check(
            "[task_types.t.fields]\ntype = \"string\"\n",
            "discriminator",
        );
        check(
            "[task_types.t.fields]\ncreate_time = \"datetime\"\n",
            "timestamp",
        );
        // With a renamed status display name, the canonical spelling is the
        // wrong way to declare the status field.
        check(
            "[workflow]\nstatus_field = \"state\"\n[task_types.t.fields]\nstatus = \"string\"\n",
            "display name `state`",
        );
    }

    #[test]
    fn workflow_display_names_validate() {
        let check = |toml_src: &str, needle: &str| {
            let cfg: Config = toml::from_str(toml_src).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "`{needle}` not in: {err}");
        };
        check(
            "[workflow]\nstatus_field = \"x\"\ntype_field = \"x\"\n",
            "used by both",
        );
        check(
            "[workflow]\ntype_field = \"status\"\n",
            "status storage key",
        );
        check(
            "[workflow]\nstatus_field = \"task_type\"\n",
            "task-type storage key",
        );
        check("[workflow]\nstatus_field = \"\"\n", "must not be empty");
        check("[workflow]\ntype_field = \"deps\"\n", "reserved");
        check("[workflow]\ntype_field = \"blocks\"\n", "relationship");
    }

    #[test]
    fn name_namespace_collisions_are_rejected() {
        let check = |toml_src: &str, needle: &str| {
            let cfg: Config = toml::from_str(toml_src).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains(needle), "`{needle}` not in: {err}");
        };
        // The holes from the audit: a relationship (or inverse) shadowing the
        // `deps` built-in; a timestamp clobbering the status; a relationship
        // named after the canonical task-type key; duplicate timestamp names;
        // two relationships sharing one inverse.
        check("[relationships.deps]\nkind = \"info\"\n", "reserved");
        check(
            "[relationships.depends_on]\nkind = \"blocker\"\ninverse = \"deps\"\n",
            "reserved",
        );
        check(
            "[timestamps]\ncreate_time = \"status\"\n",
            "status storage key",
        );
        check(
            "[timestamps]\ncreate_time = \"when\"\nupdate_time = \"when\"\n",
            "used by both",
        );
        check(
            "[relationships.task_type]\nkind = \"info\"\n",
            "task-type storage key",
        );
        check(
            "[relationships.depends_on]\nkind = \"blocker\"\n\
             [relationships.a]\nkind = \"info\"\ninverse = \"rev\"\n\
             [relationships.b]\nkind = \"info\"\ninverse = \"rev\"\n",
            "used by both",
        );
        // A symmetric self-inverse stays sanctioned.
        let symmetric: Config = toml::from_str(
            "[relationships.depends_on]\nkind = \"blocker\"\n\
             [relationships.mirror]\nkind = \"info\"\ninverse = \"mirror\"\n",
        )
        .unwrap();
        assert!(symmetric.validate().is_ok());
        // The inverse-vs-type ambiguity now fires on PLAIN validate, with its
        // actionable message.
        check(
            "[relationships.depends_on]\nkind = \"blocker\"\ninverse = \"has_subtask\"\n\
             [relationships.has_subtask]\nkind = \"hierarchy\"\n",
            "ambiguous",
        );
    }

    #[test]
    fn relationship_defaults_set_kind_and_inverse() {
        let r = RelationshipConfig::default();
        assert_eq!(
            r.types["depends_on"].kind,
            RelKind::Blocker,
            "depends_on blocks"
        );
        assert_eq!(
            r.types["depends_on"].inverse, "blocks",
            "depends_on's reverse is blocks"
        );
        assert_eq!(
            r.types["relates_to"].kind,
            RelKind::Info,
            "relates_to informational"
        );
        assert_eq!(
            r.types["relates_to"].inverse, "relates_to",
            "relates_to is symmetric (self-inverse)"
        );
        // A `[relationships.x]` sub-table with only `kind` defaults inverse="".
        let parsed: Config = toml::from_str("[relationships.needs]\nkind = \"blocker\"\n").unwrap();
        assert_eq!(parsed.relationships.types["needs"].kind, RelKind::Blocker);
        assert_eq!(parsed.relationships.types["needs"].inverse, "");
        // The pre-rename key `type` still loads as an alias of `kind`.
        let legacy: Config =
            toml::from_str("[relationships.needs]\ntype = \"hierarchy\"\n").unwrap();
        assert_eq!(legacy.relationships.types["needs"].kind, RelKind::Hierarchy);
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let parsed: Config = toml::from_str("[workflow]\ndone_status = \"closed\"\n").unwrap();
        assert_eq!(parsed.workflow.done_status, "closed");
        assert_eq!(parsed.workflow.status_field, "status"); // untouched default
        assert_eq!(parsed.compaction, CompactionConfig::default()); // whole section defaulted
        assert_eq!(parsed.merge, MergeConfig::default()); // whole section defaulted
    }

    #[test]
    fn on_conflict_parses_all_variants() {
        for (text, expected) in [
            ("surface", OnConflict::Surface),
            ("latest", OnConflict::Latest),
            ("ours", OnConflict::Ours),
            ("theirs", OnConflict::Theirs),
        ] {
            let cfg: Config =
                toml::from_str(&format!("[merge]\non_conflict = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.merge.on_conflict, expected);
        }
    }

    #[test]
    fn validate_rejects_keep_events_below_minimum() {
        let mut cfg = Config::default();
        assert!(cfg.validate().is_ok(), "the default must be valid");

        cfg.compaction.keep_events = MIN_KEEP_EVENTS - 1;
        assert!(cfg.validate().is_err(), "below the floor must be rejected");

        cfg.compaction.keep_events = MIN_KEEP_EVENTS;
        assert!(cfg.validate().is_ok(), "exactly the floor is allowed");
    }

    #[test]
    fn validate_against_passes_for_defaults_and_flags_inverse_collision() {
        let cfg = Config::default();
        assert!(
            cfg.validate_against(&HashMap::new()).is_ok(),
            "default config + empty graph is valid"
        );

        // Declaring a `blocks` type collides with depends_on's inverse name.
        let mut cfg = Config::default();
        cfg.relationships.types.insert(
            "blocks".to_string(),
            RelationshipDef {
                kind: RelKind::Info,
                inverse: String::new(),
            },
        );
        let err = cfg
            .validate_against(&HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ambiguous"),
            "inverse collision flagged: {err}"
        );
    }

    #[test]
    fn validate_against_flags_double_blocker_and_multiple_parents() {
        use crate::test_support::task;
        let into_state = |tasks: Vec<crate::model::TaskState>| -> HashMap<String, _> {
            tasks.into_iter().map(|t| (t.id.clone(), t)).collect()
        };

        // `a` blocks-by-two: depends_on b (field) and has_subtask b (hierarchy).
        let mut a = task("a", &["b"], &[]);
        a.relationships
            .insert("has_subtask".to_string(), vec!["b".to_string()]);
        let err = Config::default()
            .validate_against(&into_state(vec![a, task("b", &[], &[])]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("only one") && err.contains("blocking"),
            "double blocker flagged: {err}"
        );

        // `c` is a subtask of both p1 and p2.
        let mut p1 = task("p1", &[], &[]);
        p1.relationships
            .insert("has_subtask".to_string(), vec!["c".to_string()]);
        let mut p2 = task("p2", &[], &[]);
        p2.relationships
            .insert("has_subtask".to_string(), vec!["c".to_string()]);
        let err = Config::default()
            .validate_against(&into_state(vec![p1, p2, task("c", &[], &[])]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("one parent"),
            "multiple parents flagged: {err}"
        );
    }
}
