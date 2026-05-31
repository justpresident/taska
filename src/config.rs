//! User configuration, persisted to `.taska/config.toml`.
//!
//! The split mirrors how the values are consumed: `[store]` settings are an
//! internal concern of [`crate::storage::FileStore`], while `[workflow]`
//! settings describe task semantics used by the engine/graph layer. Every key
//! here is honored somewhere — this file is not a list of aspirations.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DynError;

/// The `config.toml` written by `ta init`. Rendered from [`Config::default`] so
/// the values live in exactly one place (the `Default` impls) while the file
/// keeps its explanatory comments — TOML serialization alone can't emit those.
/// A test asserts the rendered file round-trips back to `Config::default()`,
/// which also catches template typos such as a misspelled key.
pub fn default_toml() -> String {
    let Config { compaction, workflow } = Config::default();
    format!(
        r#"# taska configuration
#
# Created by `ta init`. Edit freely — `ta init` will not overwrite this file
# once it exists. Missing keys fall back to the defaults shown below.

[compaction]
# `ta compact` folds old events into the baseline snapshot to keep the log
# small. These settings control how much recent history stays in the log.
#
# Recent events must be retained: compaction discards event ids (the baseline is
# keyed by task, not event), and the git merge driver reconciles divergent
# branches by event id. Keep enough history to cover your longest-lived branch.
#
# Keep at least this many of the most recent events. Also the minimum log size
# before compaction does anything.
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
"#,
        keep_events = compaction.keep_events,
        keep_days = compaction.keep_days,
        status_field = workflow.status_field,
        done_status = workflow.done_status,
    )
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Config {
    pub compaction: CompactionConfig,
    pub workflow: WorkflowConfig,
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
        CompactionConfig {
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
        WorkflowConfig {
            status_field: "status".to_string(),
            done_status: "done".to_string(),
        }
    }
}

impl Config {
    /// Load config from `path`, falling back to defaults if the file is absent.
    /// `#[serde(default)]` means a partial file still loads — only the keys
    /// present override their defaults.
    pub fn load(path: &Path) -> Result<Config, DynError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
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
    }
}
