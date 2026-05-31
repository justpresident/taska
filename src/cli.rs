//! `ta` command-line surface: argument parsing, dispatch, and presentation.
//!
//! Command handlers depend on the [`EventStore`] abstraction rather than the
//! concrete [`FileStore`], so they can be exercised against any store.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::{CompactionConfig, WorkflowConfig};
use crate::engine::Engine;
use crate::error::DynError;
use crate::git;
use crate::graph;
use crate::merge;
use crate::model::{MutationEvent, OpType, TaskState};
use crate::storage::{EventStore, FileStore};

#[derive(Parser)]
#[command(name = "ta", version = "0.1.0", about = "Taska Event Log Engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a taska repository environment
    Init,
    /// Create a new schema-agnostic task: `ta create <id> [field=value ...]`
    Create {
        id: String,
        /// Custom fields as `key=value` pairs (values parsed as JSON when possible)
        fields: Vec<String>,
    },
    /// Update fields on an existing task: `ta update <id> [field=value ...]`
    Update { id: String, fields: Vec<String> },
    /// Bind a block constraint: `ta block <task_id> <depends_on>`
    Block { task_id: String, depends_on: String },
    /// Remove a block constraint: `ta unblock <task_id> <depends_on>`
    Unblock { task_id: String, depends_on: String },
    /// Delete a task: `ta delete <id>`
    Delete { id: String },
    /// List all tasks
    List,
    /// Search tasks by field value: `ta search <key> <val>`
    Search { key: String, val: String },
    /// Show tasks ready to work on (deps satisfied, not done)
    Ready,
    /// Fold the mutation log into the baseline snapshot
    Compact,
    /// Git custom merge driver entrypoint (invoked by Git, not humans)
    #[command(name = "git-merge")]
    GitMerge {
        ancestor: String,
        current: String,
        incoming: String,
        /// Original pathname (%P); accepted for Git compatibility, unused.
        #[arg(default_value = "")]
        path: String,
    },
}

/// Parse args and dispatch. `main` maps the result to an exit code.
pub fn run() -> Result<(), DynError> {
    let cli = Cli::parse();
    match cli.command {
        // Commands that don't operate on an existing store.
        Commands::Init => cmd_init(),
        Commands::GitMerge {
            ancestor,
            current,
            incoming,
            path: _,
        } => merge::execute_git_merge(&ancestor, &current, &incoming),

        // Store-backed commands: resolve the store, then hand off to a handler
        // that only knows the `EventStore` abstraction.
        Commands::Create { id, fields } => cmd_create(&FileStore::discover()?, id, fields),
        Commands::Update { id, fields } => cmd_update(&FileStore::discover()?, id, fields),
        Commands::Block { task_id, depends_on } => {
            cmd_dep(&FileStore::discover()?, task_id, depends_on, OpType::AddDep)
        }
        Commands::Unblock { task_id, depends_on } => {
            cmd_dep(&FileStore::discover()?, task_id, depends_on, OpType::RemoveDep)
        }
        Commands::Delete { id } => cmd_delete(&FileStore::discover()?, id),
        Commands::List => cmd_list(&FileStore::discover()?),
        Commands::Search { key, val } => cmd_search(&FileStore::discover()?, key, val),
        Commands::Ready => {
            let store = FileStore::discover()?;
            let workflow = store.config().workflow.clone();
            cmd_ready(&store, &workflow)
        }
        Commands::Compact => {
            let store = FileStore::discover()?;
            let cfg = store.config().compaction.clone();
            cmd_compact(&store, &cfg, Utc::now())
        }
    }
}

/// Load and materialize the current task map from any store.
fn state_of(store: &impl EventStore) -> Result<HashMap<String, TaskState>, DynError> {
    Ok(Engine::materialize_state(
        store.load_baseline()?,
        store.load_mutations()?,
    ))
}

/// Parse `key=value` strings; values are parsed as JSON, falling back to a
/// plain string when that fails (so `status=open` stays a string).
fn parse_fields(fields: &[String]) -> Result<Map<String, Value>, DynError> {
    let mut map = Map::new();
    for raw in fields {
        let (key, val) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid field `{}` (expected key=value)", raw))?;
        let value =
            serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.to_string()));
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// Idempotent: reuse an existing store if one is discoverable from the current
/// directory (e.g. a fresh clone), otherwise create one here. Either way, the
/// git merge driver is (re)registered, so re-running `ta init` is how a clone
/// installs the driver into its local config.
fn cmd_init() -> Result<(), DynError> {
    // Resolve the store directory: reuse an existing one (so re-running from
    // anywhere in the repo is idempotent), else create one in the current dir.
    let base_dir = match FileStore::discover() {
        Ok(existing) => {
            println!("taska store already present at {}", existing.base_dir.display());
            existing.base_dir
        }
        Err(_) => {
            let dir = std::env::current_dir()?.join(".taska");
            println!("Initialized taska store at {}", dir.display());
            dir
        }
    };

    // Provision honors the (possibly user-edited) config, creating any newly
    // configured log files — this is what makes re-running `ta init` the way to
    // apply a change to the `[store]` paths.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?;
    git::setup(repo_root)?;
    Ok(())
}

fn cmd_create(store: &impl EventStore, id: String, fields: Vec<String>) -> Result<(), DynError> {
    let payload = parse_fields(&fields)?;
    store.append_events(&[MutationEvent::new(OpType::Create, &id, payload)])?;
    println!("Created task `{}`", id);
    Ok(())
}

fn cmd_update(store: &impl EventStore, id: String, fields: Vec<String>) -> Result<(), DynError> {
    let payload = parse_fields(&fields)?;
    store.append_events(&[MutationEvent::new(OpType::Update, &id, payload)])?;
    println!("Updated task `{}`", id);
    Ok(())
}

fn cmd_dep(
    store: &impl EventStore,
    task_id: String,
    depends_on: String,
    op: OpType,
) -> Result<(), DynError> {
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(depends_on.clone()));
    let is_add = matches!(op, OpType::AddDep);
    store.append_events(&[MutationEvent::new(op, &task_id, payload)])?;
    if is_add {
        println!("`{}` now depends on `{}`", task_id, depends_on);
    } else {
        println!("`{}` no longer depends on `{}`", task_id, depends_on);
    }
    Ok(())
}

fn cmd_delete(store: &impl EventStore, id: String) -> Result<(), DynError> {
    store.append_events(&[MutationEvent::new(OpType::Delete, &id, Map::new())])?;
    println!("Deleted task `{}`", id);
    Ok(())
}

fn cmd_list(store: &impl EventStore) -> Result<(), DynError> {
    let state = state_of(store)?;
    if state.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }
    let mut ids: Vec<&String> = state.keys().collect();
    ids.sort();
    for id in ids {
        print_task(&state[id]);
    }
    Ok(())
}

fn cmd_search(store: &impl EventStore, key: String, val: String) -> Result<(), DynError> {
    let state = state_of(store)?;
    // Match the query against the same JSON coercion used on write.
    let needle = serde_json::from_str::<Value>(&val).unwrap_or_else(|_| Value::String(val.clone()));
    let mut hits = Engine::filter_tasks(&state, &key, &needle);
    hits.sort_by(|a, b| a.id.cmp(&b.id));
    if hits.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for task in hits {
        print_task(task);
    }
    Ok(())
}

fn cmd_ready(store: &impl EventStore, workflow: &WorkflowConfig) -> Result<(), DynError> {
    let state = state_of(store)?;
    let ready = graph::ready_tasks(&state, &workflow.status_field, &workflow.done_status)?;
    if ready.is_empty() {
        println!("(nothing ready)");
    } else {
        for id in ready {
            print_task(&state[&id]);
        }
    }
    Ok(())
}

fn cmd_compact(
    store: &impl EventStore,
    cfg: &CompactionConfig,
    now: DateTime<Utc>,
) -> Result<(), DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;

    let split = Engine::retention_split(&mutations, cfg.keep_events, cfg.keep_days, now);
    if split == 0 {
        println!(
            "Nothing to compact ({} event(s) in log, keep_events = {})",
            mutations.len(),
            cfg.keep_events
        );
        return Ok(());
    }

    // Fold the old prefix into the baseline; retain the recent suffix in the log
    // so divergent branches can still be reconciled by event id.
    let (to_fold, to_keep) = mutations.split_at(split);
    let folded = Engine::materialize_state(baseline, to_fold.to_vec());
    let mut new_baseline: Vec<TaskState> = folded.into_values().collect();
    new_baseline.sort_by(|a, b| a.id.cmp(&b.id));

    store.compact(&new_baseline, to_keep)?;
    println!(
        "Compacted {} event(s) into baseline ({} task(s)); kept {} recent event(s)",
        split,
        new_baseline.len(),
        to_keep.len()
    );
    Ok(())
}

fn print_task(task: &TaskState) {
    let fields = serde_json::to_string(&task.custom_fields).unwrap_or_default();
    if task.depends_on.is_empty() {
        println!("{}  {}", task.id, fields);
    } else {
        println!("{}  {}  deps={:?}", task.id, fields, task.depends_on);
    }
}

#[cfg(test)]
mod tests {
    //! These tests are the dividend the `EventStore` trait pays out: command
    //! handlers run against an in-memory fake with no disk, locks, or git.
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct InMemoryStore {
        events: RefCell<Vec<MutationEvent>>,
        baseline: RefCell<Vec<TaskState>>,
    }

    impl EventStore for InMemoryStore {
        fn load_baseline(&self) -> Result<Vec<TaskState>, DynError> {
            Ok(self.baseline.borrow().clone())
        }
        fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError> {
            Ok(self.events.borrow().clone())
        }
        fn append_events(&self, new_events: &[MutationEvent]) -> Result<(), DynError> {
            self.events.borrow_mut().extend_from_slice(new_events);
            Ok(())
        }
        fn compact(&self, baseline: &[TaskState], retained: &[MutationEvent]) -> Result<(), DynError> {
            *self.baseline.borrow_mut() = baseline.to_vec();
            *self.events.borrow_mut() = retained.to_vec();
            Ok(())
        }
    }

    #[test]
    fn create_then_materialize() {
        let store = InMemoryStore::default();
        cmd_create(&store, "api".into(), vec!["status=open".into(), "priority=3".into()]).unwrap();
        let state = state_of(&store).unwrap();
        assert_eq!(state["api"].custom_fields["status"], serde_json::json!("open"));
        // `priority=3` is coerced to a JSON number, not a string.
        assert_eq!(state["api"].custom_fields["priority"], serde_json::json!(3));
    }

    #[test]
    fn compact_folds_log_into_baseline() {
        let store = InMemoryStore::default();
        cmd_create(&store, "a".into(), vec![]).unwrap();
        cmd_create(&store, "b".into(), vec![]).unwrap();
        // keep nothing -> fold the whole log into the baseline.
        let cfg = CompactionConfig { keep_events: 0, keep_days: 0 };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert!(store.load_mutations().unwrap().is_empty(), "log cleared");
        assert_eq!(store.load_baseline().unwrap().len(), 2, "folded into baseline");
        // Appends still work and overlay the baseline post-compaction.
        cmd_create(&store, "c".into(), vec![]).unwrap();
        assert_eq!(state_of(&store).unwrap().len(), 3);
    }

    #[test]
    fn compact_retains_recent_events() {
        let store = InMemoryStore::default();
        for id in ["a", "b", "c", "d", "e"] {
            cmd_create(&store, id.into(), vec![]).unwrap();
        }
        // Keep the 2 most recent, time window off.
        let cfg = CompactionConfig { keep_events: 2, keep_days: 0 };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert_eq!(store.load_mutations().unwrap().len(), 2, "kept 2 recent events");
        assert_eq!(store.load_baseline().unwrap().len(), 3, "folded 3 into baseline");
        assert_eq!(state_of(&store).unwrap().len(), 5, "all tasks still visible");
    }

    #[test]
    fn invalid_field_is_rejected() {
        let store = InMemoryStore::default();
        let err = cmd_create(&store, "x".into(), vec!["no_equals_sign".into()]);
        assert!(err.is_err());
    }
}
