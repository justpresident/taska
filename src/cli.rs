//! `ta` command-line surface: argument parsing and command handlers.

use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::engine::Engine;
use crate::graph;
use crate::merge;
use crate::storage::{DynError, MutationEvent, OpType, Storage, TaskState};

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
    Update {
        id: String,
        fields: Vec<String>,
    },
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

/// Parse args and dispatch. Returns the process result; `main` maps it to an
/// exit code.
pub fn run() -> Result<(), DynError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Create { id, fields } => cmd_create(id, fields),
        Commands::Update { id, fields } => cmd_update(id, fields),
        Commands::Block { task_id, depends_on } => cmd_dep(task_id, depends_on, OpType::AddDep),
        Commands::Unblock { task_id, depends_on } => cmd_dep(task_id, depends_on, OpType::RemoveDep),
        Commands::Delete { id } => cmd_delete(id),
        Commands::List => cmd_list(),
        Commands::Search { key, val } => cmd_search(key, val),
        Commands::Ready => cmd_ready(),
        Commands::Compact => cmd_compact(),
        Commands::GitMerge {
            ancestor,
            current,
            incoming,
            path: _,
        } => merge::execute_git_merge(&ancestor, &current, &incoming),
    }
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

fn cmd_init() -> Result<(), DynError> {
    let cwd = std::env::current_dir()?;
    let storage = Storage::init(&cwd)?;
    println!("Initialized taska store at {}", storage.base_dir.display());
    setup_git_integration(&cwd)?;
    Ok(())
}

/// Wire up `.gitattributes` and the custom merge driver, best-effort.
fn setup_git_integration(repo_root: &std::path::Path) -> Result<(), DynError> {
    use std::io::Write;

    // .gitattributes entry (append if not already present).
    let attrs_path = repo_root.join(".gitattributes");
    let line = ".taska/mutations.jsonl merge=taska-merge-driver";
    let existing = std::fs::read_to_string(&attrs_path).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == line) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&attrs_path)?;
        if !existing.is_empty() && !existing.ends_with('\n') {
            writeln!(f)?;
        }
        writeln!(f, "{}", line)?;
    }

    // Register the merge driver in local git config (best-effort).
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .status()
    };
    let name_ok = git(&[
        "config",
        "merge.taska-merge-driver.name",
        "Taska Auto-Resolution Log Consolidation Driver",
    ]);
    let driver_ok = git(&[
        "config",
        "merge.taska-merge-driver.driver",
        "ta git-merge %O %A %B %P",
    ]);
    match (name_ok, driver_ok) {
        (Ok(a), Ok(b)) if a.success() && b.success() => {
            println!("Configured git merge driver for .taska/mutations.jsonl");
        }
        _ => {
            eprintln!("warning: could not configure git merge driver (is this a git repo?)");
        }
    }
    Ok(())
}

fn cmd_create(id: String, fields: Vec<String>) -> Result<(), DynError> {
    let payload = parse_fields(&fields)?;
    let storage = Storage::discover()?;
    storage.append_events(&[MutationEvent::new(OpType::Create, &id, payload)])?;
    println!("Created task `{}`", id);
    Ok(())
}

fn cmd_update(id: String, fields: Vec<String>) -> Result<(), DynError> {
    let payload = parse_fields(&fields)?;
    let storage = Storage::discover()?;
    storage.append_events(&[MutationEvent::new(OpType::Update, &id, payload)])?;
    println!("Updated task `{}`", id);
    Ok(())
}

fn cmd_dep(task_id: String, depends_on: String, op: OpType) -> Result<(), DynError> {
    let storage = Storage::discover()?;
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(depends_on.clone()));
    let is_add = matches!(op, OpType::AddDep);
    storage.append_events(&[MutationEvent::new(op, &task_id, payload)])?;
    if is_add {
        println!("`{}` now depends on `{}`", task_id, depends_on);
    } else {
        println!("`{}` no longer depends on `{}`", task_id, depends_on);
    }
    Ok(())
}

fn cmd_delete(id: String) -> Result<(), DynError> {
    let storage = Storage::discover()?;
    storage.append_events(&[MutationEvent::new(OpType::Delete, &id, Map::new())])?;
    println!("Deleted task `{}`", id);
    Ok(())
}

fn cmd_list() -> Result<(), DynError> {
    let storage = Storage::discover()?;
    let state = Engine::load(&storage)?;
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

fn cmd_search(key: String, val: String) -> Result<(), DynError> {
    let storage = Storage::discover()?;
    let state = Engine::load(&storage)?;
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

fn cmd_ready() -> Result<(), DynError> {
    let storage = Storage::discover()?;
    let state = Engine::load(&storage)?;
    let ready = graph::ready_tasks(&state)?;
    if ready.is_empty() {
        println!("(nothing ready)");
    } else {
        for id in ready {
            print_task(&state[&id]);
        }
    }
    Ok(())
}

fn cmd_compact() -> Result<(), DynError> {
    let storage = Storage::discover()?;
    let state = Engine::load(&storage)?;
    let mut states: Vec<TaskState> = state.into_values().collect();
    states.sort_by(|a, b| a.id.cmp(&b.id));
    storage.write_baseline(&states)?;
    println!("Compacted {} task(s) into baseline", states.len());
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
