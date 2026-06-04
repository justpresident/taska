//! `ta ready` — not-done tasks whose dependencies are all done.

use crate::cli::state_of;
use crate::config::{DisplayConfig, WorkflowConfig};
use crate::error::DynError;
use crate::format::{print_tasks, DisplayArgs};
use crate::graph;
use crate::model::TaskState;
use crate::storage::EventStore;

pub fn cmd_ready(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let ready = graph::ready_tasks(
        &state,
        &workflow.status_field,
        &workflow.done_status,
        &blockers,
    )?;
    let tasks: Vec<&TaskState> = ready.iter().filter_map(|id| state.get(id)).collect();
    // ready_tasks returns a topological order, but ready tasks never depend on
    // one another (their deps are all done, hence excluded), so re-sorting in
    // print_tasks is free of ordering hazards and gives a consistent order.
    print_tasks(tasks, display, cfg, "(nothing ready)");
    Ok(())
}
