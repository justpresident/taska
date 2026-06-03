//! User configuration, persisted to `.taska/config.toml`.
//!
//! The split mirrors how the values are consumed: `[compaction]` tunes how much
//! history `ta compact` leaves in the log, `[workflow]` describes task semantics
//! used by the engine/graph layer, and `[merge]` controls how the git merge
//! driver reconciles concurrent branches. Every key here is honored somewhere —
//! this file is not a list of aspirations.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DynError;

/// Smallest `keep_events` we accept in production.
///
/// Retaining fewer than this risks folding away events a concurrent branch still
/// needs to reconcile on merge, which is unrecoverable, so `enforce_config`
/// rejects any smaller value on every store-backed command.
pub const MIN_KEEP_EVENTS: usize = 100;

/// The `config.toml` written by `ta init`.
///
/// Rendered from [`Config::default`] so the values live in exactly one place
/// (the `Default` impls) while the file keeps its explanatory comments — TOML
/// serialization alone can't emit those. A test asserts the rendered file
/// round-trips back to `Config::default()`, which also catches template typos
/// such as a misspelled key.
pub fn default_toml() -> String {
    let Config {
        compaction,
        workflow,
        merge,
        display,
    } = Config::default();
    let on_conflict = merge.on_conflict.as_str();
    let columns = display
        .columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // The `[display.column_max_width]` sub-table, one `name = width` per entry.
    // Keys are quoted so a column name with TOML-special characters round-trips.
    let column_max_width = display
        .column_max_width
        .iter()
        .map(|(k, v)| format!("\"{k}\" = {v}"))
        .collect::<Vec<_>>()
        .join("\n");
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
# Columns shown by list/search/ready in human format — and the field set and
# order used by `--format json`. "id" and "deps" are built-ins; any other name
# is a task field (blank when a task lacks it). Override per command with
# `--columns a,b,c` or `--full`.
columns = [{columns}]
# Truncate long human cell values to this many characters (0 = no limit). This
# is the global fallback for any column not overridden below.
max_width = {max_width}

# Per-column truncation overrides. A column listed here is truncated to its own
# width instead of max_width (0 = no limit). `--full` ignores these entirely.
[display.column_max_width]
{column_max_width}
"#,
        min_keep = MIN_KEEP_EVENTS,
        keep_events = compaction.keep_events,
        keep_days = compaction.keep_days,
        status_field = workflow.status_field,
        done_status = workflow.done_status,
        on_conflict = on_conflict,
        columns = columns,
        max_width = display.max_width,
        column_max_width = column_max_width,
    )
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Config {
    pub compaction: CompactionConfig,
    pub workflow: WorkflowConfig,
    pub merge: MergeConfig,
    pub display: DisplayConfig,
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
            keep_events: 1000,
            keep_days: 30,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct WorkflowConfig {
    pub status_field: String,
    pub done_status: String,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            status_field: "status".to_string(),
            done_status: "closed".to_string(),
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

/// How `list`/`search`/`ready` present tasks.
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

    /// Reject configurations that would corrupt the store. The CLI calls this on
    /// every store-backed command so a bad edit is reported on the very next
    /// `ta` invocation rather than silently at the next compaction.
    pub fn validate(&self) -> Result<(), DynError> {
        if self.compaction.keep_events < MIN_KEEP_EVENTS {
            return Err(format!(
                "config error: compaction.keep_events = {} is below the minimum of {}. \
                 Retaining fewer events risks discarding history still needed to reconcile \
                 merges. Edit .taska/config.toml.",
                self.compaction.keep_events, MIN_KEEP_EVENTS
            )
            .into());
        }
        Ok(())
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
}
