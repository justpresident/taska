//! Shared unit-test fixtures.
//!
//! Compiled only under `cfg(test)`. The `EventStore` trait pays its dividend
//! here: command handlers run against [`InMemoryStore`] with no disk, locks, or
//! git, and the small builders keep the per-module tests terse.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::Value;

use crate::config::{Config, TimestampConfig};
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputArgs, OutputFormat};
use crate::model::{MutationEvent, TaskState};
use crate::storage::EventStore;

/// In-memory [`EventStore`] fake: no disk, no locks, no git.
#[derive(Default)]
pub struct InMemoryStore {
    events: RefCell<Vec<MutationEvent>>,
    baseline: RefCell<Vec<TaskState>>,
    pub config: Config,
}

impl EventStore for InMemoryStore {
    fn config(&self) -> &Config {
        &self.config
    }
    fn load_baseline(&self) -> Result<Vec<TaskState>, DynError> {
        Ok(self.baseline.borrow().clone())
    }
    fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError> {
        Ok(self.events.borrow().clone())
    }
    fn append_events(&self, drafts: &[MutationEvent]) -> Result<(), DynError> {
        let mut events = self.events.borrow_mut();
        let start = events.iter().map(|e| e.seq).max().map_or(1, |m| m + 1);
        for (seq, draft) in (start..).zip(drafts) {
            let mut event = draft.clone();
            event.seq = seq;
            events.push(event);
        }
        Ok(())
    }
    fn compact(&self, baseline: &[TaskState], retained: &[MutationEvent]) -> Result<(), DynError> {
        *self.baseline.borrow_mut() = baseline.to_vec();
        *self.events.borrow_mut() = retained.to_vec();
        Ok(())
    }
    fn replace_mutations(&self, events: &[MutationEvent]) -> Result<(), DynError> {
        *self.events.borrow_mut() = events.to_vec();
        Ok(())
    }
}

/// An in-memory store with the computed-timestamp columns disabled, for tests
/// asserting exact field/column sets that shouldn't see injected times.
pub fn store_without_timestamps() -> InMemoryStore {
    let mut store = InMemoryStore::default();
    store.config.timestamps = TimestampConfig {
        create_time: String::new(),
        update_time: String::new(),
        close_time: String::new(),
    };
    store
}

/// An in-memory store whose config DECLARES a schema - the `task` type (required
/// `title`/`notes`, a `status` enum, an optional `priority` enum) and a
/// `depends_on` blocker - for tests that need a rich, non-default vocabulary
/// (`Config::default()` declares no task types, so its status is free-form).
pub fn store_with_schema() -> InMemoryStore {
    let toml = r#"
[workflow]
status_field = "status"
done_status = "closed"
default_status = "todo"
type_field = "type"

[relationships]
depends_on = { kind = "blocker", inverse = "blocks" }

[task_types.task]
closed = true
fields = { title = { type = "string", required = true }, notes = { type = "string", required = true }, status = { type = "enum", values = ["todo", "in_progress", "closed"], required = true }, priority = { type = "enum", values = ["low", "medium", "high"] } }
"#;
    let config = toml::from_str(toml).expect("test schema config parses");
    InMemoryStore {
        config,
        ..Default::default()
    }
}

/// Build a [`TaskState`] from an id, dependency ids, and `(key, value)` fields.
pub fn task(id: &str, deps: &[&str], fields: &[(&str, Value)]) -> TaskState {
    let mut relationships = std::collections::BTreeMap::new();
    if !deps.is_empty() {
        relationships.insert(
            "depends_on".to_string(),
            deps.iter().map(|d| (*d).to_string()).collect(),
        );
    }
    TaskState {
        id: id.to_string(),
        relationships,
        custom_fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        create_time: None,
        update_time: None,
        close_time: None,
    }
}

/// Index a slice of tasks into the materialized-state map shape.
pub fn state(tasks: &[TaskState]) -> HashMap<String, TaskState> {
    tasks.iter().map(|t| (t.id.clone(), t.clone())).collect()
}

/// Build [`DisplayArgs`] with the non-format flags at their defaults.
///
/// `no_color` is forced **on** so rendering is deterministic: the human path
/// resolves color through `want_color`, which ANDs in `stdout().is_terminal()`,
/// so a helper that left color enabled would make every content assertion below
/// TTY-dependent (passing under `cargo test`'s piped output, failing in an
/// interactive terminal). Color wrapping itself is covered separately by calling
/// `render_human`/`render_record` with an explicit `color` bool.
pub fn display(format: OutputFormat, full: bool, columns: Option<&[&str]>) -> DisplayArgs {
    DisplayArgs {
        output: OutputArgs {
            format,
            no_color: true,
        },
        full,
        columns: columns.map(|c| c.iter().map(|s| (*s).to_string()).collect()),
        sort: None,
        reverse: false,
        layout: None,
    }
}
