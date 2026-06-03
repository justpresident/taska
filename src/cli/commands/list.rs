//! `ta list` — every task, rendered per the display args.

use crate::cli::state_of;
use crate::config::DisplayConfig;
use crate::error::DynError;
use crate::format::{print_tasks, DisplayArgs};
use crate::model::TaskState;
use crate::storage::EventStore;

pub fn cmd_list(
    store: &impl EventStore,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let tasks: Vec<&TaskState> = state.values().collect();
    print_tasks(tasks, display, cfg, "(no tasks)");
    Ok(())
}
