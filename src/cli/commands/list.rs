//! `ta list` — tasks rendered per the display args, optionally filtered.
//!
//! The filtered set comes from [`crate::action::list`] (positional
//! `field<op>value` criteria, `--open`, `--ready`); this file resolves the
//! display columns that drive computed-column injection, picks the
//! empty-placeholder text, and renders.

use crate::action::{list_tasks, ListQuery};
use crate::cli::print_warnings;
use crate::config::DisplayConfig;
use crate::error::DynError;
use crate::format::{print_tasks, referenced_columns, DisplayArgs};
use crate::model::TaskState;
use crate::storage::EventStore;

pub fn cmd_list(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    // Which columns the table will show/sort by drives the action's lazy
    // injection of the graph-computed columns (it adds criterion fields itself).
    let display_columns = referenced_columns(display, cfg);
    let outcome = list_tasks(
        store,
        &ListQuery {
            criteria,
            open,
            ready,
            display_columns: &display_columns,
        },
    )?;
    print_warnings(&outcome.warnings);

    // A bare `list` shows "(no tasks)"; `--ready` with nothing actionable reads
    // as "(nothing ready)"; any other filter that matched nothing "(no matches)".
    let empty = if ready {
        "(nothing ready)"
    } else if criteria.is_empty() && !open {
        "(no tasks)"
    } else {
        "(no matches)"
    };

    let blockers = store.config().relationships.blocker_types();
    // Resolve the effective layout (flag, else `[display].list_layout`).
    let mut display = display.clone();
    display.layout = Some(display.layout.unwrap_or(cfg.list_layout));
    let tasks: Vec<&TaskState> = outcome.tasks.iter().collect();
    print_tasks(tasks, &display, cfg, &blockers, empty);
    Ok(())
}
