//! Dynamic shell-completion candidates (store-aware) for the args that accept
//! task ids, `list` filter fields, and column names. Each provider discovers the
//! store from the current dir and returns nothing when there is none, so
//! completion never errors outside a store. Wired onto args via
//! `#[arg(add = ...)]`; the value-aware `list` criteria use an `ArgValueCompleter`.

use std::collections::BTreeSet;
use std::ffi::OsStr;

use clap_complete::engine::CompletionCandidate;

use crate::model::{BLOCKED_BY_KEY, DEPS_KEY, ID_KEY, SUBTASKS_KEY, UNBLOCKS_KEY};
use crate::storage::{EventStore, FileStore};

fn candidates(values: impl IntoIterator<Item = String>) -> Vec<CompletionCandidate> {
    values.into_iter().map(CompletionCandidate::new).collect()
}

/// Every task id in the store under the current dir.
pub fn task_ids() -> Vec<CompletionCandidate> {
    candidates(task_id_names())
}

/// Built-in, computed, configured, and in-use column/field names (for
/// `--columns`/`--sort`).
pub fn columns() -> Vec<CompletionCandidate> {
    candidates(column_names())
}

/// A criterion: complete a field name or a task id (a bare id is a criterion
/// too), or - once an operator (`=`, `>`, `=~`, ...) is typed - the value for
/// that field.
pub fn criteria(current: &OsStr) -> Vec<CompletionCandidate> {
    value_after_operator(current).unwrap_or_else(|| {
        let mut names = field_names();
        names.extend(task_id_names());
        candidates(names)
    })
}

/// A `dep add`/`dep remove` edge `type=target`: complete the relationship type,
/// then (after `=`) the target task id.
pub fn edges(current: &OsStr) -> Vec<CompletionCandidate> {
    value_after_operator(current).unwrap_or_else(|| {
        let types: BTreeSet<String> = FileStore::discover()
            .map(|store| store.config().relationships.types.keys().cloned().collect())
            .unwrap_or_default();
        candidates(types)
    })
}

/// Once `field<op>` is typed, the candidate values for that field (keeping the
/// field and operator); `None` while no operator has been typed yet.
fn value_after_operator(current: &OsStr) -> Option<Vec<CompletionCandidate>> {
    let cur = current.to_string_lossy();
    let op = cur.find(|c: char| "=!<>~".contains(c))?;
    let field = &cur[..op];
    // Keep the field + the operator the user already typed; complete the value.
    let value_at = op
        + cur[op..]
            .chars()
            .take_while(|c| "=!<>~".contains(*c))
            .map(char::len_utf8)
            .sum::<usize>();
    let prefix = &cur[..value_at];
    Some(candidates(
        value_strings(field)
            .into_iter()
            .map(|v| format!("{prefix}{v}")),
    ))
}

/// The discovered store's task ids (empty without a readable store).
fn task_id_names() -> BTreeSet<String> {
    FileStore::discover()
        .ok()
        .and_then(|store| crate::action::read(&store).ok())
        .map(|session| session.state.into_keys().collect())
        .unwrap_or_default()
}

// --- shared builders -------------------------------------------------------

/// Every column the store accepts - the shared vocabulary `--columns`/`--sort`
/// are checked against, so what completes is exactly what's accepted.
fn column_names() -> BTreeSet<String> {
    vocabulary().map_or_else(builtin_columns, |v| v.columns().clone())
}

/// Filterable field names: the columns plus relationship type names and their
/// inverses (`depends_on=`, `blocks=`, `subtask_of=`, ...).
fn field_names() -> BTreeSet<String> {
    vocabulary().map_or_else(builtin_columns, |v| v.filter_fields())
}

/// The discovered store's full column vocabulary, if there is a readable store.
fn vocabulary() -> Option<crate::action::columns::Vocabulary> {
    let store = FileStore::discover().ok()?;
    crate::action::columns::full_vocabulary(&store).ok()
}

/// The built-in columns, offered when no store is reachable.
fn builtin_columns() -> BTreeSet<String> {
    [ID_KEY, DEPS_KEY, UNBLOCKS_KEY, BLOCKED_BY_KEY, SUBTASKS_KEY]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Candidate values for `field<op>`: task ids when the field references tasks
/// (`id`, `deps`, a relationship type or inverse), else the distinct values that
/// field already holds across the tasks.
fn value_strings(field: &str) -> Vec<String> {
    let Ok(store) = FileStore::discover() else {
        return Vec::new();
    };
    let Ok(session) = crate::action::read(&store) else {
        return Vec::new();
    };
    let id_like = field == ID_KEY
        || field == DEPS_KEY
        || store
            .config()
            .relationships
            .types
            .iter()
            .any(|(name, def)| field == name || field == def.inverse);
    let mut vals: BTreeSet<String> = BTreeSet::new();
    if id_like {
        vals.extend(session.state.keys().cloned());
    }
    for task in session.state.values() {
        if let Some(v) = task.custom_fields.get(field) {
            vals.insert(v.as_str().map_or_else(|| v.to_string(), str::to_string));
        }
    }
    vals.into_iter().collect()
}
