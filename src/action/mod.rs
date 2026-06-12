//! The frontend-agnostic **action layer**: every operation a frontend performs
//! on a store, returning typed data and warnings-*as-data* - never printing.
//!
//! This is the seam that lets more than one frontend (the bundled CLI, a TUI, a
//! library consumer) drive the same functionality and present it differently.
//! An action depends only on the [`EventStore`](crate::storage::EventStore)
//! trait plus the domain layers (engine/schema/graph/config); it knows nothing
//! of `cli` or `format`. Presentation - `println!`, color, prompts, tables -
//! stays entirely in the frontend, which consumes an action's typed result and
//! renders it however it likes.
//!
//! Every command drives through here: `read`, the display actions
//! (`list`/`show`/`status`/`prime`), the `dep` group, `write::{create,update,delete}`,
//! the plan->apply pairs (`undo`/`resolve`), `compact`, `repair`, `config`, and
//! `init`. [`materialize`] is the shared raw-materialization primitive every
//! write/maintenance action uses (so it lives here, not in any one sibling).

use std::collections::HashMap;

use crate::config::Config;
use crate::engine::Engine;
use crate::graph;
use crate::model::{MutationEvent, TaskState, BLOCKED_BY_KEY, SUBTASKS_KEY, UNBLOCKS_KEY};
use crate::storage::EventStore;

pub mod compact;
pub mod config;
pub mod dep;
pub mod init;
pub mod list;
pub mod prime;
pub mod read;
pub mod repair;
pub mod resolve;
pub mod show;
pub mod status;
pub mod undo;
pub mod write;

pub use list::{list_tasks, ListOutcome, ListQuery};
pub use prime::{prime, PrimeFacts, PrimeOutcome};
pub use read::{read, Session, Warning};
pub use show::{show, ShowOutcome};
pub use status::{status, StatusOutcome, StatusSummary};

/// Materialize RAW state from baseline + log slices using `config`'s workflow.
///
/// The write-side counterpart to [`read`]: the `append_checked` verifier closures
/// and the maintenance actions hold slices read under the store lock (not a
/// store) and want raw state - canonical keys, no display shaping. A thin
/// convenience over [`Engine::materialize_state`], at the action root so no
/// action depends on a *sibling* module for it.
pub(crate) fn materialize(
    config: &Config,
    baseline: &[TaskState],
    log: &[MutationEvent],
) -> HashMap<String, TaskState> {
    Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.done_status,
    )
}

/// Inject the graph-computed columns onto `state`, but only those `wanted` by the
/// caller - so they cost nothing unless a query/display actually references one.
///
/// The shared generic-column primitive: `list` (display + criterion columns) and
/// `dep tree` (its configured columns) both call this, so every multi-result
/// action surfaces the same computed columns the same way. They're injected as
/// ordinary fields, so `cell_value`/sorting/filtering/rendering handle them with
/// no special-casing:
/// - `unblocks`/`blocked_by` - transitive not-done dependents / prerequisites
///   over the blocker edges (numbers).
/// - `subtasks` - a parent's `done/total` direct-child completion (string).
pub(crate) fn inject_computed_columns(
    store: &impl EventStore,
    state: &mut HashMap<String, TaskState>,
    wanted: &[&str],
) {
    let wants = |name: &str| wanted.contains(&name);
    let workflow = &store.config().workflow;

    if wants(UNBLOCKS_KEY) || wants(BLOCKED_BY_KEY) {
        let blockers = store.config().relationships.blocker_types();
        let counts = graph::reachability_counts(
            state,
            &blockers,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(unblocks, blocked_by)) = counts.get(id) {
                task.custom_fields
                    .insert(UNBLOCKS_KEY.to_string(), serde_json::json!(unblocks));
                task.custom_fields
                    .insert(BLOCKED_BY_KEY.to_string(), serde_json::json!(blocked_by));
            }
        }
    }

    if wants(SUBTASKS_KEY) {
        let hierarchy = store.config().relationships.hierarchy_types();
        let progress = graph::subtask_progress(
            state,
            &hierarchy,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(done, total)) = progress.get(id) {
                task.custom_fields.insert(
                    SUBTASKS_KEY.to_string(),
                    serde_json::json!(format!("{done}/{total}")),
                );
            }
        }
    }
}
