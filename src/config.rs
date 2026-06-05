//! User configuration, persisted to `.taska/config.toml`.
//!
//! The split mirrors how the values are consumed: `[compaction]` tunes how much
//! history `ta compact` leaves in the log, `[workflow]` describes task semantics
//! used by the engine/graph layer, and `[merge]` controls how the git merge
//! driver reconciles concurrent branches. Every key here is honored somewhere —
//! this file is not a list of aspirations.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DynError;
use crate::model::TaskState;

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
# The custom field that records a task's status, and the value that marks a
# task complete. `ta ready` treats a dependency as satisfied once it reaches
# `done_status`.
status_field = "{status_field}"
done_status = "{done_status}"
# Status stamped onto a new task when `ta create` doesn't set one. Set to "" to
# create statusless tasks (the status field stays absent until you set it).
default_status = "{default_status}"

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
# rejected. type = "blocker" makes the target a prerequisite (feeds `ta ready` and
# cycle detection); "info" is informational. inverse names the reverse direction
# and is OPTIONAL — omit it for a one-way type; the type's own name makes it
# symmetric (`a relates_to b` reads both ways); else it labels the inverse.
{relationships_toml}
"#,
        min_keep = MIN_KEEP_EVENTS,
        keep_events = compaction.keep_events,
        keep_days = compaction.keep_days,
        status_field = workflow.status_field,
        done_status = workflow.done_status,
        default_status = workflow.default_status,
        on_conflict = on_conflict,
        columns = columns,
        max_width = display.max_width,
        sort = display.sort,
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
                "[relationships.{name}]\ntype = \"{}\"{inverse}",
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
    pub status_field: String,
    pub done_status: String,
    /// Status stamped onto a new task when `ta create` doesn't set the status
    /// field. Empty means create statusless tasks (the status field is simply
    /// absent until set).
    pub default_status: String,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            status_field: "status".to_string(),
            done_status: "closed".to_string(),
            default_status: "todo".to_string(),
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

/// How `list`/`ready` present tasks.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct DisplayConfig {
    /// Columns shown in human format and the field order used by `--format json`,
    /// in order. `id` and `deps` are built-ins; any other name is a task field.
    pub columns: Vec<String>,
    /// Truncate human cell values to this many characters (0 = no limit). The
    /// global fallback for any column not listed in `column_max_width`.
    pub max_width: usize,
    /// Per-column truncation overrides: a column named here truncates to its own
    /// width instead of `max_width` (0 = no limit). `--full` ignores these. A
    /// `BTreeMap` so the rendered config is deterministically ordered.
    pub column_max_width: BTreeMap<String, usize>,
    /// Default column to sort `list`/`ready` rows by (ascending). Any
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
pub enum RelType {
    /// `A <type> B` makes `B` a prerequisite of `A`: `A` is ready only once `B`
    /// is done. Feeds `ta ready` and cycle detection (the dependency DAG).
    Blocker,
    /// No effect on readiness or cycles — purely informational.
    #[default]
    Info,
}

impl RelType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Info => "info",
        }
    }
}

/// One relationship type's semantics.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RelationshipDef {
    /// `blocker` (gates readiness/cycles) or `info` (informational). The config
    /// key is `type`.
    #[serde(rename = "type")]
    pub kind: RelType,
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
    /// dependency tree.
    ///
    /// Every declared `blocker` type, plus the implicit `depends_on` when it
    /// isn't declared at all (legacy stores without a `[relationships]` section
    /// still treat `depends_on` as a blocker). A `depends_on` explicitly set to
    /// `info` is honored and excluded.
    pub fn blocker_types(&self) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = self
            .types
            .iter()
            .filter(|(_, def)| def.kind == RelType::Blocker)
            .map(|(name, _)| name.clone())
            .collect();
        if !self.types.contains_key("depends_on") {
            set.insert("depends_on".to_string());
        }
        set
    }
}

impl Default for RelationshipConfig {
    fn default() -> Self {
        let def = |kind, inverse: &str| RelationshipDef {
            kind,
            inverse: inverse.to_string(),
        };
        Self {
            // `depends_on` blocks (reverse `blocks`); `relates_to` is symmetric
            // (self-inverse); `duplicates` is one-way (no inverse surfaced).
            types: [
                ("depends_on".to_string(), def(RelType::Blocker, "blocks")),
                ("relates_to".to_string(), def(RelType::Info, "relates_to")),
                ("duplicates".to_string(), def(RelType::Info, "")),
            ]
            .into_iter()
            .collect(),
        }
    }
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

        // 3. An inverse name must not collide with a *different* declared type, or
        //    `ta dep` can't tell the forward edge from the inverse one.
        for (name, def) in types {
            if !def.inverse.is_empty() && def.inverse != *name && types.contains_key(&def.inverse) {
                problems.push(format!(
                    "relationship `{name}` has inverse `{}`, which is also a declared type — \
                     ambiguous (use a distinct inverse name, or set inverse = \"{name}\" to make \
                     it symmetric)",
                    def.inverse
                ));
            }
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
    fn relationship_defaults_set_kind_and_inverse() {
        let r = RelationshipConfig::default();
        assert_eq!(
            r.types["depends_on"].kind,
            RelType::Blocker,
            "depends_on blocks"
        );
        assert_eq!(
            r.types["depends_on"].inverse, "blocks",
            "depends_on's reverse is blocks"
        );
        assert_eq!(
            r.types["relates_to"].kind,
            RelType::Info,
            "relates_to informational"
        );
        assert_eq!(
            r.types["relates_to"].inverse, "relates_to",
            "relates_to is symmetric (self-inverse)"
        );
        // A `[relationships.x]` sub-table with only `type` defaults inverse="".
        let parsed: Config = toml::from_str("[relationships.needs]\ntype = \"blocker\"\n").unwrap();
        assert_eq!(parsed.relationships.types["needs"].kind, RelType::Blocker);
        assert_eq!(parsed.relationships.types["needs"].inverse, "");
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
                kind: RelType::Info,
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
}
