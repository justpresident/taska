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
    let Ok(store) = FileStore::discover() else {
        return Vec::new();
    };
    let Ok(session) = crate::action::read(&store) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = session.state.keys().cloned().collect();
    ids.sort();
    candidates(ids)
}

/// Built-in, computed, configured, and in-use column/field names (for
/// `--columns`/`--sort`).
pub fn columns() -> Vec<CompletionCandidate> {
    candidates(column_names())
}

/// A `list` criterion / `dep` edge: complete the field/type name, or - once an
/// operator (`=`, `>`, `=~`, ...) is typed - the value for that field.
pub fn criteria(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    if let Some(op) = cur.find(|c: char| "=!<>~".contains(c)) {
        let field = &cur[..op];
        // Keep the field + the operator the user already typed; complete the value.
        let value_at = op
            + cur[op..]
                .chars()
                .take_while(|c| "=!<>~".contains(*c))
                .map(char::len_utf8)
                .sum::<usize>();
        let prefix = &cur[..value_at];
        return candidates(
            value_strings(field)
                .into_iter()
                .map(|v| format!("{prefix}{v}")),
        );
    }
    candidates(field_names())
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
